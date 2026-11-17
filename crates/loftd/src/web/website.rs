//! This computer and the website (ADR-0019, `cloud/` in the repository):
//! linking it to an account there, telling the website where browsers can
//! reach it, and checking the tickets the website sends browsers here with.
//!
//! - **Linking** works like `gh auth login`: the daemon asks the website
//!   for a link, the desktop app opens its page, and the person signs in
//!   with Google there and approves (both sides show the same code). The
//!   daemon, polling, then gets this computer's id and its own key, kept in
//!   `<home>/web/website.json` (0600).
//! - **Reporting**: while linked, the daemon tells the website its name and
//!   the address browsers open it at, and whether that
//!   address is public (Cloudflare Tunnel), on every change and every five minutes.
//!   Agentloft's traffic never goes through the website.
//! - **Framing**: the website shows a public computer in a frame, so the
//!   browser's address stays the website's. Only the linked website's
//!   origin may frame the public address (`frame_ancestor`).
//! - **Tickets**: the website sends a signed-in browser here with
//!   `#ticket=…`, a two-minute note signed (HMAC-SHA256) with this
//!   computer's key, naming this computer, the account it is linked to, and
//!   the origin it is for. A good one is traded once for an ordinary
//!   session, as the pairing token is.

use base64::Engine as _;
use hmac::{Hmac, Mac as _};
use loft_protocol::{WebsiteLinkPending, WebsiteStatus};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// How often a linked computer tells the website it is still there.
const REPORT_EVERY_MS: i64 = 5 * 60 * 1000;
/// Longest a ticket may live (the website makes them for two minutes).
const MAX_TICKET_SECS: i64 = 600;
/// Clocks may disagree by this much.
const SKEW_SECS: i64 = 60;
const MAX_TICKET_LEN: usize = 4096;
/// Largest answer read from the website.
const MAX_REPLY: u64 = 64 * 1024;

/// What linking left this computer with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Linked {
    website: String,
    machine_id: String,
    /// This computer's key: it signs tickets for it and proves its reports.
    secret: String,
    user_id: String,
    email: String,
    linked_ms: i64,
}

#[derive(Debug, Clone)]
struct Pending {
    website: String,
    device_code: String,
    shown: WebsiteLinkPending,
    /// Bumped by every start and cancel: a poller that sees another number
    /// stops.
    generation: u64,
}

#[derive(Default)]
struct State {
    linked: Option<Linked>,
    pending: Option<Pending>,
    generation: u64,
    error: Option<String>,
    /// What the website was last told, and when.
    reported: Option<(Option<Address>, i64)>,
    /// Tickets used, until they expire (seconds).
    used: HashMap<String, i64>,
}

pub(crate) struct Website {
    file: PathBuf,
    state: Mutex<State>,
    /// Report at once (linked, or the address changed).
    pub(crate) wake: tokio::sync::Notify,
    /// Linked or unlinked: a quick tunnel starts or stops with it.
    pub(crate) link_changed: tokio::sync::Notify,
}

/// Where browsers reach this computer, as the website is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Address {
    pub url: String,
    /// Reachable from the internet (Cloudflare Tunnel): the website opens it in a
    /// frame, under its own address.
    pub public: bool,
}

/// A ticket that checked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ticket {
    pub email: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TicketError {
    /// Not linked: no key to check it with.
    NotLinked,
    /// Forged, for another computer or account or origin, expired, or used.
    Invalid,
}

#[derive(Deserialize)]
struct Claims {
    v: u32,
    m: String,
    u: String,
    #[serde(default)]
    e: String,
    aud: String,
    iat: i64,
    exp: i64,
    n: String,
}

fn now_ms() -> i64 {
    super::now_ms()
}

fn b64url(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()
}

/// An origin as browsers send it: lowercase, no trailing slash, no default
/// port.
fn normal_origin(origin: &str) -> String {
    let lower = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("https://") {
        format!("https://{}", rest.strip_suffix(":443").unwrap_or(rest))
    } else if let Some(rest) = lower.strip_prefix("http://") {
        format!("http://{}", rest.strip_suffix(":80").unwrap_or(rest))
    } else {
        lower
    }
}

/// Check `ticket` with `secret`: signed by it, for this computer and
/// account, for `origin`, current as of `now` (seconds). Returns the claims;
/// whether it was used before is the caller's to check.
fn read_ticket(
    ticket: &str,
    linked: &Linked,
    origin: &str,
    now: i64,
) -> Result<Claims, TicketError> {
    if ticket.len() > MAX_TICKET_LEN {
        return Err(TicketError::Invalid);
    }
    let (payload, mac) = ticket.split_once('.').ok_or(TicketError::Invalid)?;
    let mac = b64url(mac).ok_or(TicketError::Invalid)?;
    let mut check = Hmac::<Sha256>::new_from_slice(linked.secret.as_bytes())
        .map_err(|_| TicketError::Invalid)?;
    check.update(payload.as_bytes());
    // Constant time.
    check.verify_slice(&mac).map_err(|_| TicketError::Invalid)?;
    let claims: Claims = b64url(payload)
        .and_then(|json| serde_json::from_slice(&json).ok())
        .ok_or(TicketError::Invalid)?;
    let good = claims.v == 1
        && claims.m == linked.machine_id
        && claims.u == linked.user_id
        && normal_origin(&claims.aud) == normal_origin(origin)
        && claims.exp >= now
        && claims.iat <= now + SKEW_SECS
        && claims.exp - claims.iat <= MAX_TICKET_SECS
        && !claims.n.is_empty()
        && claims.n.len() <= 64;
    if good {
        Ok(claims)
    } else {
        Err(TicketError::Invalid)
    }
}

// -- talking to the website -------------------------------------------------

struct Reply {
    status: u16,
    json: serde_json::Value,
}

/// POST `body` as JSON to `url` (blocking: run it on a blocking thread).
fn post(url: &str, body: &serde_json::Value, bearer: Option<&str>) -> Result<Reply, String> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // Cargo.toml picks the TLS stack per platform; ureq has to be told which.
    // With the OS stack, trust the OS's certificates: ureq's default (its
    // own bundled roots) turns the system's off, and Windows then rejects
    // every chain ("unable to find any user-specified roots").
    let (provider, roots) = if cfg!(any(target_os = "macos", windows)) {
        (TlsProvider::NativeTls, RootCerts::PlatformVerifier)
    } else {
        (TlsProvider::Rustls, RootCerts::WebPki)
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(provider)
                .root_certs(roots)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(concat!("agentloft/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut request = agent.post(url).header("Content-Type", "application/json");
    if let Some(bearer) = bearer {
        request = request.header("Authorization", &format!("Bearer {bearer}"));
    }
    let mut response = request
        .send(body.to_string())
        .map_err(|e| format!("couldn't reach the website: {e}"))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .with_config()
        .limit(MAX_REPLY)
        .read_to_string()
        .unwrap_or_default();
    Ok(Reply {
        status,
        json: serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    })
}

async fn post_async(
    url: String,
    body: serde_json::Value,
    bearer: Option<String>,
) -> Result<Reply, String> {
    tokio::task::spawn_blocking(move || post(&url, &body, bearer.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

/// What this computer is called, for the website's list: the name set in
/// System Settings on a Mac, else the host name.
pub(crate) fn computer_name() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
        .args(["--get", "ComputerName"])
        .output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    #[cfg(windows)]
    if let Some(name) = std::env::var("COMPUTERNAME")
        .ok()
        .map(|n| n.trim().to_owned())
        .filter(|n| !n.is_empty())
    {
        return name;
    }
    #[cfg(unix)]
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    #[cfg(unix)]
    if ok {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).trim().to_owned();
        if !name.is_empty() {
            return name.trim_end_matches(".local").to_owned();
        }
    }
    "A computer".into()
}

fn str_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

impl Website {
    /// `dir`: `<home>/web`.
    pub(crate) fn new(dir: &std::path::Path) -> Arc<Self> {
        let file = dir.join("website.json");
        let linked = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        Arc::new(Self {
            file,
            state: Mutex::new(State {
                linked,
                ..State::default()
            }),
            wake: tokio::sync::Notify::new(),
            link_changed: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn status(&self, website: &str) -> WebsiteStatus {
        let state = self.lock();
        let linked = state.linked.as_ref();
        WebsiteStatus {
            url: linked.map_or_else(|| website.to_owned(), |l| l.website.clone()),
            account: linked.map(|l| l.email.clone()),
            reported_url: state
                .reported
                .as_ref()
                .and_then(|(at, _)| at.as_ref().map(|a| a.url.clone())),
            pending: state.pending.as_ref().map(|p| p.shown.clone()),
            error: state.error.clone(),
        }
    }

    pub(crate) fn is_linked(&self) -> bool {
        self.lock().linked.is_some()
    }

    fn save(&self, linked: Option<&Linked>) {
        let result = match linked {
            Some(linked) => serde_json::to_vec_pretty(linked)
                .map_err(std::io::Error::other)
                .and_then(|json| {
                    let dir = self.file.parent().unwrap_or(std::path::Path::new("."));
                    super::auth::write_private(dir, &self.file, &json)
                }),
            None => match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        if let Err(err) = result {
            tracing::warn!(%err, "website: couldn't save the link");
        }
    }

    /// Ask `website` for a link, then wait (in the background) for someone
    /// to approve it there.
    pub(crate) async fn start_link(self: &Arc<Self>, website: String) -> Result<(), String> {
        let name = tokio::task::spawn_blocking(computer_name)
            .await
            .unwrap_or_else(|_| "A computer".into());
        let reply = post_async(
            format!("{website}/api/link/start"),
            serde_json::json!({
                "name": name,
                "platform": std::env::consts::OS,
                "version": env!("CARGO_PKG_VERSION"),
            }),
            None,
        )
        .await;
        let reply = match reply {
            Ok(reply) if reply.status == 200 => reply,
            Ok(reply) if reply.status == 429 => {
                return Err("Too many links asked for. Try again in an hour.".into());
            }
            Ok(reply) => return Err(format!("The website said no ({}).", reply.status)),
            Err(err) => return Err(err),
        };
        let (Some(device_code), Some(verify_url), Some(user_code)) = (
            str_field(&reply.json, "device_code"),
            str_field(&reply.json, "verify_url"),
            str_field(&reply.json, "user_code"),
        ) else {
            return Err("The website's answer made no sense.".into());
        };
        // Only a page of that website is ever opened.
        if !verify_url.starts_with(&format!("{website}/")) {
            return Err("The website's answer made no sense.".into());
        }
        let interval = reply
            .json
            .get("interval")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .clamp(1, 30);
        let expires_in = reply
            .json
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(600)
            .clamp(10, 3600);
        let generation = {
            let mut state = self.lock();
            state.generation += 1;
            state.error = None;
            state.pending = Some(Pending {
                website: website.clone(),
                device_code,
                shown: WebsiteLinkPending {
                    verify_url,
                    user_code,
                    expires_ms: now_ms() + expires_in * 1000,
                },
                generation: state.generation,
            });
            state.generation
        };
        tracing::info!(%website, "website: waiting for the link to be approved");
        let me = self.clone();
        tokio::spawn(async move {
            me.poll(generation, Duration::from_secs(interval)).await;
        });
        Ok(())
    }

    /// Report at the next chance, whatever was said last.
    pub(crate) fn report_soon(&self) {
        self.lock().reported = None;
        self.wake.notify_one();
    }

    pub(crate) fn set_error(&self, error: String) {
        self.lock().error = Some(error);
    }

    pub(crate) fn cancel_link(&self) {
        let mut state = self.lock();
        state.generation += 1;
        state.pending = None;
    }

    /// The pending link, while it is still the one `generation` started.
    fn pending(&self, generation: u64) -> Option<Pending> {
        self.lock()
            .pending
            .clone()
            .filter(|p| p.generation == generation)
    }

    fn end_pending(&self, generation: u64, error: Option<String>) {
        let mut state = self.lock();
        if state.pending.as_ref().map(|p| p.generation) == Some(generation) {
            state.pending = None;
            state.error = error;
        }
    }

    async fn poll(&self, generation: u64, interval: Duration) {
        loop {
            tokio::time::sleep(interval).await;
            let Some(pending) = self.pending(generation) else {
                return;
            };
            if now_ms() > pending.shown.expires_ms {
                self.end_pending(
                    generation,
                    Some("The link expired. Choose Link this computer again.".into()),
                );
                return;
            }
            let reply = post_async(
                format!("{}/api/link/poll", pending.website),
                serde_json::json!({ "device_code": pending.device_code }),
                None,
            )
            .await;
            let reply = match reply {
                Ok(reply) => reply,
                // Offline for a moment: keep trying until it expires.
                Err(err) => {
                    tracing::debug!(%err, "website: poll failed");
                    continue;
                }
            };
            match reply.status {
                202 => continue,
                200 => {
                    let fields = (
                        str_field(&reply.json, "machine_id"),
                        str_field(&reply.json, "secret"),
                        str_field(&reply.json, "user_id"),
                        str_field(&reply.json, "email"),
                    );
                    let (Some(machine_id), Some(secret), Some(user_id), Some(email)) = fields
                    else {
                        self.end_pending(
                            generation,
                            Some("The website's answer made no sense.".into()),
                        );
                        return;
                    };
                    let linked = Linked {
                        website: pending.website.clone(),
                        machine_id,
                        secret,
                        user_id,
                        email,
                        linked_ms: now_ms(),
                    };
                    // Cancelled while the answer was on its way: the link
                    // stands on the website, so keep it here too.
                    self.save(Some(&linked));
                    tracing::info!(website = %linked.website, "website: linked");
                    {
                        let mut state = self.lock();
                        state.linked = Some(linked);
                        state.reported = None;
                        if state.pending.as_ref().map(|p| p.generation) == Some(generation) {
                            state.pending = None;
                        }
                        state.error = None;
                    }
                    self.wake.notify_one();
                    self.link_changed.notify_one();
                    return;
                }
                410 if reply.json.get("error").and_then(|e| e.as_str()) == Some("denied") => {
                    self.end_pending(
                        generation,
                        Some("Linking was turned down on the website.".into()),
                    );
                    return;
                }
                410 => {
                    self.end_pending(
                        generation,
                        Some("The link expired. Choose Link this computer again.".into()),
                    );
                    return;
                }
                // The website is having a moment: keep trying.
                _ => continue,
            }
        }
    }

    /// Unlink: tell the website (if it's reachable), forget the key here.
    pub(crate) async fn unlink(&self) {
        let linked = {
            let mut state = self.lock();
            state.generation += 1;
            state.pending = None;
            state.error = None;
            state.reported = None;
            state.linked.take()
        };
        let Some(linked) = linked else { return };
        self.save(None);
        self.link_changed.notify_one();
        tracing::info!(website = %linked.website, "website: unlinked");
        let bearer = format!("{}.{}", linked.machine_id, linked.secret);
        if let Err(err) = post_async(
            format!("{}/api/machine/unlink", linked.website),
            serde_json::json!({}),
            Some(bearer),
        )
        .await
        {
            tracing::warn!(%err, "website: couldn't say we unlinked");
        }
    }

    /// The origin that may show this computer's public address in a frame:
    /// the website it is linked to, if any. Checked to be a bare origin, so
    /// it can go into a header as it is.
    pub(crate) fn frame_ancestor(&self) -> Option<String> {
        let state = self.lock();
        let origin = normal_origin(&state.linked.as_ref()?.website);
        let rest = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))?;
        let plain = !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.:[]".contains(&b));
        plain.then_some(origin)
    }

    /// Tell the website where browsers reach this computer, if it hasn't
    /// heard lately or the address changed.
    pub(crate) async fn report(&self, at: Option<Address>) {
        let (linked, due) = {
            let state = self.lock();
            let Some(linked) = state.linked.clone() else {
                return;
            };
            let due = match &state.reported {
                Some((said, when)) => *said != at || now_ms() - when >= REPORT_EVERY_MS,
                None => true,
            };
            (linked, due)
        };
        if !due {
            return;
        }
        let name = tokio::task::spawn_blocking(computer_name)
            .await
            .unwrap_or_else(|_| "A computer".into());
        let bearer = format!("{}.{}", linked.machine_id, linked.secret);
        let reply = post_async(
            format!("{}/api/machine/report", linked.website),
            serde_json::json!({
                "name": name,
                "url": at.as_ref().map(|a| a.url.as_str()),
                "public": at.as_ref().is_some_and(|a| a.public),
                "platform": std::env::consts::OS,
                "version": env!("CARGO_PKG_VERSION"),
            }),
            Some(bearer),
        )
        .await;
        let mut state = self.lock();
        // Unlinked (or relinked) meanwhile: this answer is about another.
        if state.linked.as_ref() != Some(&linked) {
            return;
        }
        match reply {
            Ok(reply) if reply.status == 200 => {
                state.reported = Some((at, now_ms()));
                state.error = None;
                if let Some(email) = str_field(&reply.json, "email")
                    && email != linked.email
                    && let Some(linked) = state.linked.as_mut()
                {
                    linked.email = email;
                    let linked = linked.clone();
                    drop(state);
                    self.save(Some(&linked));
                }
            }
            Ok(reply) if reply.status == 401 => {
                tracing::warn!("website: this computer was removed there; unlinking");
                state.linked = None;
                state.reported = None;
                state.error = Some(format!(
                    "This computer was removed from {}'s computers on the website.",
                    linked.email
                ));
                drop(state);
                self.save(None);
            }
            Ok(reply) => {
                // Try again at the next round rather than in a tight loop.
                state.reported = Some((None, now_ms() - REPORT_EVERY_MS + 60_000));
                state.error = Some(format!(
                    "The website didn't take the update ({}).",
                    reply.status
                ));
            }
            Err(err) => {
                state.reported = Some((None, now_ms() - REPORT_EVERY_MS + 60_000));
                state.error = Some(err);
            }
        }
    }

    /// Check a ticket a browser brought from the website, for `origin` (the
    /// page's own). Each is good once.
    pub(crate) fn check_ticket(&self, ticket: &str, origin: &str) -> Result<Ticket, TicketError> {
        self.check_ticket_at(ticket, origin, now_ms() / 1000)
    }

    pub(crate) fn check_ticket_at(
        &self,
        ticket: &str,
        origin: &str,
        now: i64,
    ) -> Result<Ticket, TicketError> {
        let mut state = self.lock();
        let linked = state.linked.clone().ok_or(TicketError::NotLinked)?;
        let claims = read_ticket(ticket, &linked, origin, now)?;
        state.used.retain(|_, exp| *exp >= now);
        if state.used.contains_key(&claims.n) {
            return Err(TicketError::Invalid);
        }
        state.used.insert(claims.n, claims.exp);
        Ok(Ticket { email: claims.e })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A ticket as the website makes it (`cloud/src/crypto.js`).
    pub(crate) fn ticket(secret: &str, claims: serde_json::Value) -> String {
        let payload = b64(claims.to_string().as_bytes());
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload.as_bytes());
        format!("{payload}.{}", b64(&mac.finalize().into_bytes()))
    }

    fn linked() -> Linked {
        Linked {
            website: "https://site.example".into(),
            machine_id: "m_1".into(),
            secret: "k".repeat(43),
            user_id: "g_2".into(),
            email: "a@example.com".into(),
            linked_ms: 0,
        }
    }

    fn claims(now: i64) -> serde_json::Value {
        serde_json::json!({
            "v": 1, "m": "m_1", "u": "g_2", "e": "a@example.com",
            "aud": "https://desk.example.com", "iat": now, "exp": now + 120, "n": "nonce1"
        })
    }

    /// The website's tests check the same vector (cloud/test/crypto.test.js).
    #[test]
    fn hmac_matches_rfc_4231_case_2() {
        let mut mac = Hmac::<Sha256>::new_from_slice(b"Jefe").unwrap();
        mac.update(b"what do ya want for nothing?");
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_good_ticket_is_good_once() {
        let dir = tempfile::tempdir().unwrap();
        let site = Website::new(dir.path());
        let now = 1_800_000_000;
        let t = ticket(&linked().secret, claims(now));
        assert_eq!(
            site.check_ticket_at(&t, "https://desk.example.com", now),
            Err(TicketError::NotLinked)
        );
        site.lock().linked = Some(linked());
        let ok = site
            .check_ticket_at(&t, "https://desk.example.com", now)
            .unwrap();
        assert_eq!(ok.email, "a@example.com");
        assert_eq!(
            site.check_ticket_at(&t, "https://desk.example.com", now),
            Err(TicketError::Invalid),
            "used"
        );
    }

    #[test]
    fn bad_tickets_are_refused() {
        let l = linked();
        let now = 1_800_000_000;
        let origin = "https://desk.example.com";
        let with = |patch: serde_json::Value| {
            let mut c = claims(now);
            for (k, v) in patch.as_object().unwrap() {
                c[k] = v.clone();
            }
            ticket(&l.secret, c)
        };
        assert!(read_ticket(&with(serde_json::json!({})), &l, origin, now).is_ok());
        // The default port, a trailing slash and case don't matter.
        assert!(
            read_ticket(
                &with(serde_json::json!({})),
                &l,
                "https://DESK.example.com:443/",
                now
            )
            .is_ok()
        );
        for (why, t, at) in [
            ("another key", ticket("other", claims(now)), origin),
            (
                "another computer",
                with(serde_json::json!({"m": "m_9"})),
                origin,
            ),
            (
                "another account",
                with(serde_json::json!({"u": "g_9"})),
                origin,
            ),
            (
                "another origin",
                with(serde_json::json!({})),
                "https://evil.example",
            ),
            (
                "http, not https",
                with(serde_json::json!({})),
                "http://desk.example.com",
            ),
            ("expired", with(serde_json::json!({"exp": now - 1})), origin),
            (
                "from the future",
                with(serde_json::json!({"iat": now + 300, "exp": now + 400})),
                origin,
            ),
            (
                "too long-lived",
                with(serde_json::json!({"exp": now + 3600})),
                origin,
            ),
            ("another version", with(serde_json::json!({"v": 2})), origin),
            ("no nonce", with(serde_json::json!({"n": ""})), origin),
            ("garbage", "not.a-ticket".to_owned(), origin),
            ("no signature", "abc".to_owned(), origin),
        ] {
            assert!(read_ticket(&t, &l, at, now).is_err(), "{why}");
        }
        // A forged payload under a real signature.
        let good = with(serde_json::json!({}));
        let (_, mac) = good.split_once('.').unwrap();
        let mut forged = claims(now);
        forged["u"] = "g_9".into();
        let forged = format!("{}.{mac}", b64(forged.to_string().as_bytes()));
        assert!(read_ticket(&forged, &l, origin, now).is_err());
    }

    #[test]
    fn the_link_survives_a_restart_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let site = Website::new(dir.path());
        site.save(Some(&linked()));
        let again = Website::new(dir.path());
        assert!(again.is_linked());
        assert_eq!(
            again.status("https://x").account.as_deref(),
            Some("a@example.com")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join("website.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        again.save(None);
        assert!(!Website::new(dir.path()).is_linked());
    }

    #[test]
    fn only_the_linked_website_may_frame_this_computer() {
        let dir = tempfile::tempdir().unwrap();
        let site = Website::new(dir.path());
        assert_eq!(site.frame_ancestor(), None, "not linked: nobody");
        for (website, framer) in [
            ("https://site.example", Some("https://site.example")),
            ("https://Site.Example/", Some("https://site.example")),
            ("https://site.example:443", Some("https://site.example")),
            ("http://127.0.0.1:8787", Some("http://127.0.0.1:8787")),
            // Anything that would add to the header: refused.
            ("https://site.example; script-src *", None),
            ("https://site.example 'unsafe-inline'", None),
            ("https://", None),
            ("javascript:alert(1)", None),
        ] {
            site.lock().linked = Some(Linked {
                website: website.into(),
                ..linked()
            });
            assert_eq!(site.frame_ancestor().as_deref(), framer, "{website}");
        }
    }
}
