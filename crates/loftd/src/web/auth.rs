//! Who may use web access (ADR-0016): the pairing token, the sessions it
//! was exchanged for, and a brake on wrong guesses.
//!
//! - The **pairing token** is 256 random bits, kept in `<home>/web/token`
//!   (mode 0600). The desktop app shows it inside a link; a browser trades
//!   it once for a session.
//! - A **session** is another 256 random bits, handed to the browser as an
//!   HttpOnly cookie. Only its SHA-256 is kept (in memory and in
//!   `<home>/web/sessions.json`), so reading that file signs nobody in.
//! - Sessions expire, can be revoked one by one or all at once, and a
//!   revoked session's open connections are told to close.

use base64::Engine as _;
use loft_protocol::WebSession;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq as _;
use tokio::sync::broadcast;

/// Bytes of entropy in the pairing token and in each session secret.
const SECRET_BYTES: usize = 32;
/// Signed-in browsers kept at once; the oldest is dropped beyond this.
const MAX_SESSIONS: usize = 64;

/// Wrong pairing tokens allowed from one address before it has to wait.
pub(crate) const MAX_FAILURES: usize = 5;
/// …counted over this long…
pub(crate) const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// …and how long it then waits.
pub(crate) const LOCKOUT: Duration = Duration::from_secs(60);
/// Wrong tokens from everywhere together before everyone waits (many
/// addresses guessing at once).
const MAX_FAILURES_GLOBAL: usize = 50;
/// Addresses remembered by the limiter.
const MAX_TRACKED_ADDRS: usize = 4096;

type Hash = [u8; 32];

fn sha256(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A fresh secret: 256 bits from the OS, as 43 URL-safe characters.
fn new_secret() -> std::io::Result<String> {
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Compare two secrets without leaking where they differ: both are hashed
/// first (so length doesn't matter either), then compared in constant time.
pub(crate) fn secrets_match(a: &str, b: &str) -> bool {
    sha256(a.as_bytes()).ct_eq(&sha256(b.as_bytes())).into()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    id: String,
    /// SHA-256 of the session secret, hex.
    hash: String,
    label: String,
    addr: String,
    created_ms: i64,
    last_seen_ms: i64,
    expires_ms: i64,
}

#[derive(Debug, Clone)]
struct Session {
    /// Order of signing in (or of loading, after a restart): breaks ties
    /// between sessions seen in the same millisecond.
    seq: u64,
    id: String,
    label: String,
    addr: String,
    created_ms: i64,
    last_seen_ms: i64,
    expires_ms: i64,
}

/// A session a request proved it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Authed {
    pub id: String,
    pub expires_ms: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PairError {
    /// Wrong token.
    Invalid,
    /// Too many wrong tokens; try again after this long.
    RateLimited(Duration),
    Internal(String),
}

/// Sliding-window failure counts with a lockout, per address and overall.
#[derive(Default)]
struct Limiter {
    by_addr: HashMap<IpAddr, Bucket>,
    global: Bucket,
}

#[derive(Default)]
struct Bucket {
    failures: VecDeque<Instant>,
    locked_until: Option<Instant>,
}

impl Bucket {
    fn prune(&mut self, now: Instant) {
        while self
            .failures
            .front()
            .is_some_and(|t| now.duration_since(*t) >= FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        if self.locked_until.is_some_and(|until| now >= until) {
            self.locked_until = None;
        }
    }

    fn wait(&mut self, now: Instant) -> Option<Duration> {
        self.prune(now);
        self.locked_until.map(|until| until - now)
    }

    fn fail(&mut self, now: Instant, max: usize) {
        self.prune(now);
        self.failures.push_back(now);
        if self.failures.len() >= max {
            self.locked_until = Some(now + LOCKOUT);
            self.failures.clear();
        }
    }
}

/// One key per IPv4 address or IPv6 /64 (a single host owns a whole /64).
fn limiter_key(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V4(_) => addr,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut segments = v6.segments();
                segments[4..].fill(0);
                IpAddr::V6(segments.into())
            }
        },
    }
}

impl Limiter {
    /// How long `addr` must wait before trying again, if it must.
    fn wait(&mut self, addr: IpAddr, now: Instant) -> Option<Duration> {
        let global = self.global.wait(now);
        let own = self
            .by_addr
            .get_mut(&limiter_key(addr))
            .and_then(|b| b.wait(now));
        global.max(own)
    }

    fn fail(&mut self, addr: IpAddr, now: Instant) {
        if self.by_addr.len() >= MAX_TRACKED_ADDRS {
            self.by_addr.retain(|_, b| {
                b.prune(now);
                b.locked_until.is_some() || !b.failures.is_empty()
            });
            if self.by_addr.len() >= MAX_TRACKED_ADDRS {
                // Flooded with addresses: fall back to the global brake.
                self.by_addr.clear();
            }
        }
        self.by_addr
            .entry(limiter_key(addr))
            .or_default()
            .fail(now, MAX_FAILURES);
        self.global.fail(now, MAX_FAILURES_GLOBAL);
    }
}

#[derive(Default)]
struct State {
    next_seq: u64,
    token: Option<String>,
    sessions: HashMap<Hash, Session>,
    /// Open connections per session id.
    connections: HashMap<String, u32>,
    limiter: Limiter,
}

/// What a connection hears when sessions are revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Revoked {
    One(String),
    All,
}

pub(crate) struct Auth {
    dir: PathBuf,
    state: Mutex<State>,
    revoked: broadcast::Sender<Revoked>,
}

/// Counts an open connection against its session until dropped.
pub(crate) struct ConnectionGuard {
    auth: Arc<Auth>,
    id: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut state = self.auth.lock();
        if let Some(n) = state.connections.get_mut(&self.id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                state.connections.remove(&self.id);
            }
        }
    }
}

impl Auth {
    /// Load what's on disk under `dir` (`<home>/web`); nothing is created
    /// until web access is first used.
    pub(crate) fn new(dir: PathBuf) -> Arc<Self> {
        let mut state = State::default();
        let now = now_ms();
        if let Ok(text) = std::fs::read_to_string(dir.join("sessions.json"))
            && let Ok(mut stored) = serde_json::from_str::<Vec<StoredSession>>(&text)
        {
            stored.sort_by_key(|s| (s.last_seen_ms, s.created_ms));
            for s in stored {
                if s.expires_ms <= now {
                    continue;
                }
                if let Some(hash) = unhex(&s.hash) {
                    state.next_seq += 1;
                    state.sessions.insert(
                        hash,
                        Session {
                            seq: state.next_seq,
                            id: s.id,
                            label: s.label,
                            addr: s.addr,
                            created_ms: s.created_ms,
                            last_seen_ms: s.last_seen_ms,
                            expires_ms: s.expires_ms,
                        },
                    );
                }
            }
        }
        Arc::new(Self {
            dir,
            state: Mutex::new(state),
            revoked: broadcast::channel(16).0,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_file(&self) -> PathBuf {
        self.dir.join("token")
    }

    /// The pairing token, created on first use.
    pub(crate) fn token(&self) -> std::io::Result<String> {
        let mut state = self.lock();
        if let Some(token) = &state.token {
            return Ok(token.clone());
        }
        let file = self.token_file();
        let token = match std::fs::read_to_string(&file) {
            Ok(text) if text.trim().len() >= 43 => {
                // Tighten a file someone loosened.
                private_mode(&file, 0o600)?;
                text.trim().to_owned()
            }
            _ => {
                let token = new_secret()?;
                write_private(&self.dir, &file, token.as_bytes())?;
                token
            }
        };
        state.token = Some(token.clone());
        Ok(token)
    }

    /// Replace the pairing token and sign every browser out.
    pub(crate) fn rotate_token(&self) -> std::io::Result<()> {
        let token = new_secret()?;
        write_private(&self.dir, &self.token_file(), token.as_bytes())?;
        self.lock().token = Some(token);
        self.revoke_all();
        Ok(())
    }

    /// Trade the pairing token for a session. Returns the session secret
    /// (shown to the browser once) and when it expires.
    pub(crate) fn pair(
        &self,
        candidate: &str,
        addr: IpAddr,
        user_agent: &str,
        ttl: Duration,
    ) -> Result<(String, Authed), PairError> {
        self.pair_at(candidate, addr, user_agent, ttl, Instant::now())
    }

    pub(crate) fn pair_at(
        &self,
        candidate: &str,
        addr: IpAddr,
        user_agent: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<(String, Authed), PairError> {
        // A locked-out address is refused before its guess is looked at.
        if let Some(wait) = self.lock().limiter.wait(addr, now) {
            return Err(PairError::RateLimited(wait));
        }
        let token = self
            .token()
            .map_err(|e| PairError::Internal(e.to_string()))?;
        if !secrets_match(candidate, &token) {
            self.lock().limiter.fail(addr, now);
            tracing::warn!(%addr, "web access: wrong pairing token");
            return Err(PairError::Invalid);
        }
        let secret = new_secret().map_err(|e| PairError::Internal(e.to_string()))?;
        let hash = sha256(secret.as_bytes());
        let created_ms = now_ms();
        let mut session = Session {
            seq: 0,
            // Names the session in lists; derived from the hash, so it
            // can't be turned back into the secret.
            id: hex(&sha256(&hash)[..6]),
            label: device_label(user_agent),
            addr: addr.to_string(),
            created_ms,
            last_seen_ms: created_ms,
            expires_ms: created_ms
                .saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)),
        };
        let authed = Authed {
            id: session.id.clone(),
            expires_ms: session.expires_ms,
        };
        tracing::info!(%addr, session = %session.id, label = %session.label, "web access: browser paired");
        {
            let mut state = self.lock();
            state.sessions.retain(|_, s| s.expires_ms > created_ms);
            while state.sessions.len() >= MAX_SESSIONS {
                let Some(oldest) = state
                    .sessions
                    .iter()
                    .min_by_key(|(_, s)| (s.last_seen_ms, s.seq))
                    .map(|(h, s)| (*h, s.id.clone()))
                else {
                    break;
                };
                state.sessions.remove(&oldest.0);
                let _ = self.revoked.send(Revoked::One(oldest.1));
            }
            state.next_seq += 1;
            session.seq = state.next_seq;
            state.sessions.insert(hash, session);
        }
        self.save();
        Ok((secret, authed))
    }

    /// The session `secret` belongs to, if it's still good.
    pub(crate) fn authenticate(&self, secret: &str) -> Option<Authed> {
        self.authenticate_at(secret, now_ms())
    }

    /// [`Auth::authenticate`] as of `now` (ms since the epoch).
    pub(crate) fn authenticate_at(&self, secret: &str, now: i64) -> Option<Authed> {
        // Looked up by hash: timing says nothing about the secret itself.
        let hash = sha256(secret.as_bytes());
        let mut state = self.lock();
        let session = state.sessions.get_mut(&hash)?;
        if session.expires_ms <= now {
            let id = session.id.clone();
            state.sessions.remove(&hash);
            drop(state);
            let _ = self.revoked.send(Revoked::One(id));
            self.save();
            return None;
        }
        session.last_seen_ms = now;
        Some(Authed {
            id: session.id.clone(),
            expires_ms: session.expires_ms,
        })
    }

    /// Whether session `id` is still signed in.
    pub(crate) fn is_live(&self, id: &str) -> bool {
        let now = now_ms();
        self.lock()
            .sessions
            .values()
            .any(|s| s.id == id && s.expires_ms > now)
    }

    /// Sign one browser out; `false` if there's no such session.
    pub(crate) fn revoke(&self, id: &str) -> bool {
        let removed = {
            let mut state = self.lock();
            let before = state.sessions.len();
            state.sessions.retain(|_, s| s.id != id);
            state.sessions.len() != before
        };
        if removed {
            tracing::info!(session = %id, "web access: session revoked");
            let _ = self.revoked.send(Revoked::One(id.to_owned()));
            self.save();
        }
        removed
    }

    pub(crate) fn revoke_all(&self) {
        self.lock().sessions.clear();
        tracing::info!("web access: all sessions revoked");
        let _ = self.revoked.send(Revoked::All);
        self.save();
    }

    /// Hear about revocations (to close a revoked session's connections).
    pub(crate) fn revocations(&self) -> broadcast::Receiver<Revoked> {
        self.revoked.subscribe()
    }

    /// Count a connection against session `id` while the guard lives.
    pub(crate) fn connected(self: &Arc<Self>, id: &str) -> ConnectionGuard {
        *self.lock().connections.entry(id.to_owned()).or_default() += 1;
        ConnectionGuard {
            auth: self.clone(),
            id: id.to_owned(),
        }
    }

    /// Signed-in browsers, most recently seen first.
    pub(crate) fn sessions(&self) -> Vec<WebSession> {
        let now = now_ms();
        let state = self.lock();
        let mut out: Vec<WebSession> = state
            .sessions
            .values()
            .filter(|s| s.expires_ms > now)
            .map(|s| WebSession {
                id: s.id.clone(),
                label: s.label.clone(),
                addr: s.addr.clone(),
                created_ms: s.created_ms,
                last_seen_ms: s.last_seen_ms,
                expires_ms: s.expires_ms,
                connections: state.connections.get(&s.id).copied().unwrap_or(0),
            })
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse((s.last_seen_ms, s.created_ms)));
        out
    }

    /// Write the sessions (hashes only) to disk.
    pub(crate) fn save(&self) {
        let stored: Vec<StoredSession> = self
            .lock()
            .sessions
            .iter()
            .map(|(hash, s)| StoredSession {
                id: s.id.clone(),
                hash: hex(hash),
                label: s.label.clone(),
                addr: s.addr.clone(),
                created_ms: s.created_ms,
                last_seen_ms: s.last_seen_ms,
                expires_ms: s.expires_ms,
            })
            .collect();
        let result = serde_json::to_vec_pretty(&stored)
            .map_err(std::io::Error::other)
            .and_then(|json| write_private(&self.dir, &self.dir.join("sessions.json"), &json));
        if let Err(err) = result {
            tracing::warn!(%err, "web access: couldn't save sessions");
        }
    }
}

#[cfg(unix)]
fn private_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn private_mode(_: &Path, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Write `bytes` to `file` in the private directory `dir`: created 0600
/// under a temporary name, then renamed into place.
fn write_private(dir: &Path, file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    private_mode(dir, 0o700)?;
    let tmp = file.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, file)
}

/// "Chrome on macOS" from a User-Agent, for the sessions list. Only words
/// from this table are ever shown, never the header itself.
pub(crate) fn device_label(user_agent: &str) -> String {
    let ua = user_agent;
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("OPR/") {
        "Opera"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("Chrome/") || ua.contains("CriOS/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        "macOS"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("CrOS") {
        "ChromeOS"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() {
        browser.to_owned()
    } else {
        format!("{browser} on {os}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(3600);

    fn addr(last: u8) -> IpAddr {
        IpAddr::from([203, 0, 113, last])
    }

    fn auth() -> (tempfile::TempDir, Arc<Auth>) {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::new(dir.path().join("web"));
        (dir, auth)
    }

    #[test]
    fn token_is_long_random_and_private() {
        let (dir, auth) = auth();
        let token = auth.token().unwrap();
        // 32 bytes → 43 base64url characters.
        assert_eq!(token.len(), 43);
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(auth.token().unwrap(), token, "stable until rotated");
        // A second daemon on the same home reads the same token.
        assert_eq!(Auth::new(dir.path().join("web")).token().unwrap(), token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.path().join("web/token")), 0o600);
            assert_eq!(mode(&dir.path().join("web")), 0o700);
        }
        let (_d2, other) = self::auth();
        assert_ne!(other.token().unwrap(), token);
    }

    #[cfg(unix)]
    #[test]
    fn a_loosened_token_file_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, auth) = auth();
        auth.token().unwrap();
        let file = dir.path().join("web/token");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        Auth::new(dir.path().join("web")).token().unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn pairing_needs_the_right_token() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        for wrong in ["", "nope", &token[..42], &format!("{token}x")] {
            assert_eq!(
                auth.pair(wrong, addr(1), "", TTL).unwrap_err(),
                PairError::Invalid,
                "{wrong:?}"
            );
        }
        let (secret, authed) = auth.pair(&token, addr(2), "", TTL).unwrap();
        assert_eq!(secret.len(), 43);
        assert_ne!(secret, token);
        assert_eq!(auth.authenticate(&secret), Some(authed.clone()));
        assert!(
            auth.authenticate(&token).is_none(),
            "the token isn't a session"
        );
        assert!(auth.authenticate("").is_none());
        assert!(auth.is_live(&authed.id));
    }

    #[test]
    fn wrong_guesses_lock_the_address_out() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES {
            assert_eq!(
                auth.pair_at(
                    "wrong",
                    addr(1),
                    "",
                    TTL,
                    t0 + Duration::from_secs(i as u64)
                )
                .unwrap_err(),
                PairError::Invalid
            );
        }
        // Locked out: even the right token is refused, and the wait is told.
        let locked = t0 + Duration::from_secs(MAX_FAILURES as u64);
        match auth.pair_at(&token, addr(1), "", TTL, locked).unwrap_err() {
            PairError::RateLimited(wait) => assert!(wait <= LOCKOUT && wait > LOCKOUT / 2),
            other => panic!("{other:?}"),
        }
        // Another address isn't affected.
        assert!(auth.pair_at(&token, addr(2), "", TTL, locked).is_ok());
        // After the lockout the address may try again.
        let later = locked + LOCKOUT + Duration::from_secs(1);
        assert!(auth.pair_at(&token, addr(1), "", TTL, later).is_ok());
    }

    #[test]
    fn slow_guessing_stays_under_the_limit_but_is_counted_in_a_window() {
        let (_dir, auth) = auth();
        let t0 = Instant::now();
        // One wrong guess every 20 s: never five within a minute.
        for i in 0..20 {
            assert_eq!(
                auth.pair_at("wrong", addr(1), "", TTL, t0 + Duration::from_secs(i * 20))
                    .unwrap_err(),
                PairError::Invalid
            );
        }
    }

    #[test]
    fn many_addresses_trip_the_global_brake() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let t0 = Instant::now();
        for i in 0..MAX_FAILURES_GLOBAL {
            let from = IpAddr::from([198, 51, (i / 250) as u8, (i % 250) as u8]);
            let _ = auth.pair_at("wrong", from, "", TTL, t0);
        }
        assert!(matches!(
            auth.pair_at(&token, addr(9), "", TTL, t0).unwrap_err(),
            PairError::RateLimited(_)
        ));
    }

    #[test]
    fn ipv6_hosts_are_limited_by_their_slash_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(limiter_key(a), limiter_key(b));
        assert_ne!(limiter_key(a), limiter_key(c));
        let mapped: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        assert_eq!(limiter_key(mapped), addr(5));
    }

    #[test]
    fn sessions_expire() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let (secret, authed) = auth.pair(&token, addr(1), "", TTL).unwrap();
        let ttl_ms = i64::try_from(TTL.as_millis()).unwrap();
        // Good until the moment it expires…
        assert!(
            auth.authenticate_at(&secret, authed.expires_ms - 1)
                .is_some()
        );
        assert!((authed.expires_ms - now_ms() - ttl_ms).abs() < 5_000);
        let mut revoked = auth.revocations();
        // …then gone, for good.
        assert!(auth.authenticate_at(&secret, authed.expires_ms).is_none());
        assert!(auth.authenticate(&secret).is_none());
        assert!(!auth.is_live(&authed.id));
        assert!(auth.sessions().is_empty());
        // Its connections are told to close.
        assert_eq!(revoked.try_recv().unwrap(), Revoked::One(authed.id));
    }

    #[test]
    fn sessions_can_be_revoked_one_by_one_or_all() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let (s1, a1) = auth.pair(&token, addr(1), "", TTL).unwrap();
        let (s2, _) = auth.pair(&token, addr(2), "", TTL).unwrap();
        let (s3, _) = auth.pair(&token, addr(3), "", TTL).unwrap();
        let mut revoked = auth.revocations();

        assert!(auth.revoke(&a1.id));
        assert!(!auth.revoke(&a1.id), "already gone");
        assert!(auth.authenticate(&s1).is_none());
        assert!(auth.authenticate(&s2).is_some());
        assert_eq!(revoked.try_recv().unwrap(), Revoked::One(a1.id));

        auth.revoke_all();
        assert!(auth.authenticate(&s2).is_none());
        assert!(auth.authenticate(&s3).is_none());
        assert_eq!(revoked.try_recv().unwrap(), Revoked::All);
        // The pairing link still works.
        assert!(auth.pair(&token, addr(1), "", TTL).is_ok());
    }

    #[test]
    fn rotating_the_token_ends_the_old_one_and_every_session() {
        let (_dir, auth) = auth();
        let old = auth.token().unwrap();
        let (secret, _) = auth.pair(&old, addr(1), "", TTL).unwrap();
        auth.rotate_token().unwrap();
        let new = auth.token().unwrap();
        assert_ne!(old, new);
        assert!(auth.authenticate(&secret).is_none());
        assert_eq!(
            auth.pair(&old, addr(1), "", TTL).unwrap_err(),
            PairError::Invalid
        );
        assert!(auth.pair(&new, addr(1), "", TTL).is_ok());
    }

    #[test]
    fn sessions_survive_a_restart_as_hashes_only() {
        let (dir, auth) = auth();
        let token = auth.token().unwrap();
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/150.0 Safari/537.36";
        let (secret, authed) = auth.pair(&token, addr(1), ua, TTL).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("web/sessions.json")).unwrap();
        assert!(!on_disk.contains(&secret), "the secret is never written");
        assert!(on_disk.contains(&authed.id));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("web/sessions.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let reopened = Auth::new(dir.path().join("web"));
        assert_eq!(
            reopened.authenticate(&secret).map(|a| a.id),
            Some(authed.id)
        );
        let listed = reopened.sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].label, "Chrome on macOS");
        assert_eq!(listed[0].addr, "203.0.113.1");
    }

    #[test]
    fn connections_are_counted_per_session() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let (_, authed) = auth.pair(&token, addr(1), "", TTL).unwrap();
        let g1 = auth.connected(&authed.id);
        let g2 = auth.connected(&authed.id);
        assert_eq!(auth.sessions()[0].connections, 2);
        drop(g1);
        assert_eq!(auth.sessions()[0].connections, 1);
        drop(g2);
        assert_eq!(auth.sessions()[0].connections, 0);
    }

    #[test]
    fn the_oldest_session_makes_room() {
        let (_dir, auth) = auth();
        let token = auth.token().unwrap();
        let (first, _) = auth.pair(&token, addr(1), "", TTL).unwrap();
        for _ in 0..MAX_SESSIONS {
            auth.pair(&token, addr(2), "", TTL).unwrap();
        }
        assert_eq!(auth.sessions().len(), MAX_SESSIONS);
        assert!(auth.authenticate(&first).is_none());
    }

    #[test]
    fn labels_come_from_a_fixed_vocabulary() {
        assert_eq!(
            device_label(
                "Mozilla/5.0 (iPhone; CPU iPhone OS 19_0 like Mac OS X) AppleWebKit/605.1.15 Version/19.0 Mobile/15E148 Safari/604.1"
            ),
            "Safari on iPhone"
        );
        assert_eq!(
            device_label("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0"),
            "Firefox on Linux"
        );
        // Nothing from the header itself is echoed.
        assert_eq!(device_label("<script>alert(1)</script>"), "Browser");
    }

    #[test]
    fn secret_comparison_ignores_length_and_content_position() {
        assert!(secrets_match("abc", "abc"));
        assert!(!secrets_match("abc", "abd"));
        assert!(!secrets_match("abc", "abcd"));
        assert!(!secrets_match("", "abc"));
    }
}
