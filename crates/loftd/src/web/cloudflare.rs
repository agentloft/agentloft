//! Public with Cloudflare Tunnel (ADR-0019 §4): `cloudflared` carries this
//! computer's own address on the internet (`[web] tunnel`, e.g.
//! `agentloft-desk.example.com`) to the listener on 127.0.0.1. A phone
//! needs nothing installed, and Cloudflare's network is fast enough for the
//! app's first load and for video.
//!
//! - **Set up once**, with the Cloudflare login `[web] cloudflare_cert`
//!   names: a named tunnel (`agentloft-<the name, dotted to dashes>`), its
//!   credentials in `<home>/web/cloudflare-tunnel.json` (0600), and a DNS
//!   record for the name. A record that points somewhere else is never
//!   overwritten. What was set up is kept in `<home>/web/cloudflare.json`.
//! - **Run**: the daemon runs `cloudflared tunnel run` for that tunnel and
//!   watches it; one that stops is started again at the next sync (each
//!   config change, and every minute). Turning it off, or the daemon
//!   stopping, stops it; the tunnel and record stay for next time.
//! - On the tunnel's name only a ticket from the website signs a browser
//!   in (`Tunnel::public_hosts`, `http.rs`).
//! - **Quick tunnel** (`tunnel = "quick"`): no Cloudflare account and no
//!   domain. `cloudflared tunnel --url …` gets a random
//!   `*.trycloudflare.com` name from Cloudflare, a new one each start,
//!   read from its log; the website is told each new one. It runs with a
//!   home of its own, so a login or config in `~/.cloudflared` is never
//!   used. It runs only while this computer is linked to the website (the
//!   only way in there), which `web/mod.rs` decides.

use super::cloudflared_install;
use loft_protocol::TunnelStatus;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// A `cloudflared` setup command that takes longer than this is given up on.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

struct Ran {
    ok: bool,
    /// stdout and stderr together.
    text: String,
    timed_out: bool,
}

async fn run(cli: &Path, args: &[&str], env: &HashMap<String, String>) -> std::io::Result<Ran> {
    let mut command = tokio::process::Command::new(cli);
    command
        .args(args)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = command.spawn()?;
    match tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => {
            let output = output?;
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok(Ran {
                ok: output.status.success(),
                text,
                timed_out: false,
            })
        }
        // Dropping the future kills the child (`kill_on_drop`).
        Err(_) => Ok(Ran {
            ok: false,
            text: String::new(),
            timed_out: true,
        }),
    }
}

fn first_line(text: &str, fallback: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(300).collect())
        .unwrap_or_else(|| fallback.to_owned())
}

/// What setting the tunnel up left, to run it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SetUp {
    hostname: String,
    tunnel_id: String,
    /// The `cloudflared` that runs it, to stop an orphan it left behind.
    #[serde(default)]
    pid: Option<u32>,
}

/// What the tunnel should look like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Want {
    /// The tunnel's name; [`QUICK`] for a quick tunnel.
    pub hostname: String,
    /// Where it forwards to: `http://127.0.0.1:4780`.
    pub target: String,
}

/// `Want::hostname` (and `Running::tunnel_id`) of a quick tunnel.
pub(crate) const QUICK: &str = loft_config::WebConfig::QUICK_TUNNEL;

struct Running {
    child: tokio::process::Child,
    target: String,
    tunnel_id: String,
}

#[derive(Default)]
struct State {
    set_up: Option<SetUp>,
    running: Option<Running>,
    /// The name being served (or set up), for the `Host` check.
    hostname: Option<String>,
    status: Option<TunnelStatus>,
    /// When `cloudflared` was last started, and how many times in a row it
    /// stopped soon after: one that keeps stopping waits for the minute.
    started: Option<std::time::Instant>,
    quick_stops: u32,
    /// When fetching cloudflared last failed, and why: not tried again
    /// for [`INSTALL_RETRY`].
    install_failed: Option<std::time::Instant>,
    install_error: Option<String>,
}

/// After a failed download of cloudflared, wait this long before the next.
const INSTALL_RETRY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A `cloudflared` that stops sooner than this after starting, twice in a
/// row, is left for the next minute's sync rather than started again at
/// once.
const QUICK_STOP: std::time::Duration = std::time::Duration::from_secs(10);

/// What `cloudflared` said: shared with the task reading its log.
#[derive(Default)]
struct Heard {
    /// Which run this is about: a stopped run's last words don't count
    /// for the next.
    run: u64,
    connected: bool,
    /// A quick tunnel's name, once Cloudflare has given it one.
    quick_host: Option<String>,
    /// Its output ended: it stopped.
    exited: bool,
    last_error: Option<String>,
}

pub(crate) struct Tunnel {
    dir: PathBuf,
    state: Mutex<State>,
    heard: Arc<Mutex<Heard>>,
    /// Raised when `cloudflared` connects: the website hears the address
    /// at once, not at the next minute's check.
    pub(crate) connected: Arc<tokio::sync::Notify>,
    /// Raised when `cloudflared` stops by itself: it is started again at
    /// once, not at the next minute's check.
    pub(crate) stopped: Arc<tokio::sync::Notify>,
}

/// The tunnel's name in the Cloudflare account.
fn tunnel_name(hostname: &str) -> String {
    format!("agentloft-{}", hostname.replace('.', "-"))
}

fn find_cli(configured: Option<&str>, env: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(cli) = configured.map(str::trim).filter(|c| !c.is_empty()) {
        let path = loft_config::expand_tilde(cli);
        return path.is_file().then_some(path);
    }
    let path = env.get("PATH").map(String::as_str).unwrap_or("");
    loft_config::which("cloudflared", path).or_else(|| {
        let mut known: Vec<PathBuf> = [
            "/opt/homebrew/bin/cloudflared",
            "/usr/local/bin/cloudflared",
            "/usr/bin/cloudflared",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        // Where Cloudflare's Windows installer (and winget) puts it.
        for var in ["ProgramFiles(x86)", "ProgramFiles"] {
            if let Some(dir) = std::env::var_os(var) {
                known.push(
                    PathBuf::from(dir)
                        .join("cloudflared")
                        .join("cloudflared.exe"),
                );
            }
        }
        known.into_iter().find(|p| p.is_file())
    })
}

fn status(state: &str, url: Option<String>, error: Option<String>) -> TunnelStatus {
    TunnelStatus {
        state: state.into(),
        url,
        error,
    }
}

/// The id of a tunnel `cloudflared tunnel create --output json` made, or
/// `list --output json` found.
fn tunnel_id(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Found {
        id: String,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Output {
        One(Found),
        Many(Vec<Found>),
    }
    // The first JSON value in the output: cloudflared adds log lines
    // ("your version is outdated") before or after it.
    let start = json.find(['{', '['])?;
    let first = serde_json::Deserializer::from_str(&json[start..])
        .into_iter::<Output>()
        .next()?
        .ok()?;
    match first {
        Output::One(found) => Some(found),
        Output::Many(all) => all.into_iter().next(),
    }
    .map(|f| f.id)
    .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
}

/// The name Cloudflare gave a quick tunnel, from the line of `cloudflared`'s
/// log that shows it (`|  https://<words>.trycloudflare.com  |`).
fn quick_host(line: &str) -> Option<String> {
    let start = line.find("https://")? + "https://".len();
    let host: String = line[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect::<String>()
        .to_ascii_lowercase();
    let label = host.strip_suffix(".trycloudflare.com")?;
    (!label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
    .then_some(host)
}

/// Whether a running `cloudflared` is one this daemon started (its
/// credentials file is ours), so an orphan can be stopped safely.
fn is_ours(pid: u32, credentials: &Path) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .is_some_and(|command| {
            command.contains("cloudflared") && command.contains(&*credentials.to_string_lossy())
        })
}

impl Tunnel {
    /// `dir`: `<home>/web`.
    pub(crate) fn new(dir: &Path) -> Self {
        let set_up: Option<SetUp> = std::fs::read_to_string(dir.join("cloudflare.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        let tunnel = Self {
            dir: dir.to_path_buf(),
            state: Mutex::new(State {
                set_up: set_up.clone(),
                ..State::default()
            }),
            heard: Arc::default(),
            connected: Arc::default(),
            stopped: Arc::default(),
        };
        // A `cloudflared` a previous daemon left running (it crashed, or
        // was killed): this one starts its own.
        if let Some(pid) = set_up.and_then(|s| s.pid)
            && is_ours(pid, &tunnel.credentials())
        {
            tracing::info!(pid, "cloudflare: stopping a cloudflared left from before");
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        // …and a quick tunnel's (its pid is kept next to its config).
        let quick_config = tunnel.quick_home().join("config.yml");
        if let Some(pid) = std::fs::read_to_string(tunnel.quick_home().join("pid"))
            .ok()
            .and_then(|p| p.trim().parse::<u32>().ok())
            && is_ours(pid, &quick_config)
        {
            tracing::info!(pid, "cloudflare: stopping a quick tunnel left from before");
            let _ = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status();
        }
        tunnel
    }

    /// Asked for, but not to run now (`why` says what is missing): stopped,
    /// and its status says why (`waiting`: a next step, not a failure).
    pub(crate) fn hold(&self, why: &str) {
        self.stop();
        let mut state = self.lock();
        state.hostname = None;
        state.status = Some(status("waiting", None, Some(why.to_owned())));
    }

    fn credentials(&self) -> PathBuf {
        self.dir.join("cloudflare-tunnel.json")
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, set_up: Option<&SetUp>) {
        let file = self.dir.join("cloudflare.json");
        let result = match set_up {
            Some(set_up) => serde_json::to_vec_pretty(set_up)
                .map_err(std::io::Error::other)
                .and_then(|json| super::auth::write_private(&self.dir, &file, &json)),
            None => match std::fs::remove_file(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        if let Err(err) = result {
            tracing::warn!(%err, "cloudflare: couldn't save the tunnel's setup");
        }
    }

    /// How the tunnel stands; `None` while it isn't asked for.
    pub(crate) fn status(&self) -> Option<TunnelStatus> {
        let mut state = self.lock();
        let running = state.running.is_some();
        let quick = state.hostname.as_deref() == Some(QUICK);
        let (connected, exited, quick_host) = self
            .heard
            .lock()
            .map(|h| (h.connected, h.exited, h.quick_host.clone()))
            .unwrap_or_default();
        if let Some(status) = state.status.as_mut()
            && running
        {
            if quick {
                // Its address is whatever Cloudflare gave this run.
                status.url = quick_host
                    .filter(|_| !exited)
                    .map(|h| format!("https://{h}"));
            }
            if exited && status.state == "on" {
                // Stopped since the last sync: not "on" until it is back.
                status.state = "starting".into();
                status.error = Some("cloudflared stopped; starting it again.".into());
            } else if connected && !exited && status.state == "starting" {
                // Connected since the last sync: say so at once.
                status.state = "on".into();
                status.error = None;
            }
        }
        state.status.clone()
    }

    /// `https://<name>` while the tunnel is connected.
    pub(crate) fn url(&self) -> Option<String> {
        self.status()
            .filter(|s| s.state == "on")
            .and_then(|s| s.url)
    }

    /// The name browsers reach this computer by through the tunnel, while
    /// it is asked for (for the `Host` check). Everything addressed to it
    /// is from the internet: only a ticket from the website signs a browser
    /// in there.
    pub(crate) fn public_hosts(&self) -> Vec<String> {
        match self.lock().hostname.clone() {
            // Only the name of the run going on now: an old one may be
            // handed to someone else by Cloudflare.
            Some(name) if name == QUICK => self
                .heard
                .lock()
                .ok()
                .and_then(|h| h.quick_host.clone().filter(|_| !h.exited))
                .into_iter()
                .collect(),
            name => name.into_iter().collect(),
        }
    }

    /// Stop `cloudflared` (the daemon is stopping, or the tunnel was turned
    /// off).
    pub(crate) fn stop(&self) {
        let running = self.lock().running.take();
        if let Some(mut running) = running {
            let _ = running.child.start_kill();
            tracing::info!("cloudflare: stopped the tunnel");
        }
        let mut state = self.lock();
        if let Some(set_up) = state.set_up.as_mut().filter(|s| s.pid.is_some()) {
            set_up.pid = None;
            let set_up = set_up.clone();
            drop(state);
            self.save(Some(&set_up));
        }
    }

    /// Make the tunnel match `want`. `asked`: it is on in the config
    /// (`want` is `None` anyway while the listener is down). Returns
    /// whether the address browsers use changed.
    pub(crate) async fn sync(
        &self,
        want: Option<&Want>,
        asked: bool,
        cli: Option<&str>,
        cert: Option<&str>,
        env: &HashMap<String, String>,
    ) -> bool {
        let before = self.url();
        let status = match want {
            None => {
                self.stop();
                self.lock().hostname = None;
                asked.then(|| {
                    status(
                        "error",
                        None,
                        Some("Web access isn't running, so there is nothing to share.".into()),
                    )
                })
            }
            Some(want) => {
                self.lock().hostname = Some(want.hostname.clone());
                Some(self.bring_up(want, cli, cert, env).await)
            }
        };
        self.lock().status = status;
        before != self.url()
    }

    async fn bring_up(
        &self,
        want: &Want,
        cli: Option<&str>,
        cert: Option<&str>,
        env: &HashMap<String, String>,
    ) -> TunnelStatus {
        let quick = want.hostname == QUICK;
        let url = if quick {
            // Known once this run has one (`status` fills it in).
            None
        } else {
            Some(format!("https://{}", want.hostname))
        };
        let configured = cli.map(str::trim).is_some_and(|c| !c.is_empty());
        let found = find_cli(cli, env).or_else(|| {
            (!configured)
                .then(|| cloudflared_install::installed(&self.dir))
                .flatten()
        });
        let cli = match found {
            Some(cli) => cli,
            // None here: fetch Cloudflare's, so nobody has to install it.
            // A cloudflared set in the config is never replaced.
            None if !configured && cloudflared_install::available() => {
                let failed_recently = self
                    .lock()
                    .install_failed
                    .is_some_and(|at| at.elapsed() < INSTALL_RETRY);
                if failed_recently {
                    return status("missing", None, self.lock().install_error.clone());
                }
                self.stop();
                self.lock().status = Some(status(
                    "starting",
                    url.clone(),
                    Some("Getting cloudflared from Cloudflare (the first time only)…".into()),
                ));
                let dir = self.dir.clone();
                let installed =
                    tokio::task::spawn_blocking(move || cloudflared_install::install(&dir))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r);
                match installed {
                    Ok(path) => {
                        let mut state = self.lock();
                        state.install_failed = None;
                        state.install_error = None;
                        path
                    }
                    Err(err) => {
                        tracing::warn!(%err, "cloudflare: couldn't install cloudflared");
                        let why = format!(
                            "{err} Trying again in ten minutes, or install cloudflared yourself."
                        );
                        let mut state = self.lock();
                        state.install_failed = Some(std::time::Instant::now());
                        state.install_error = Some(why.clone());
                        return status("missing", None, Some(why));
                    }
                }
            }
            None => {
                self.stop();
                return status(
                    "missing",
                    None,
                    Some(
                        if configured {
                            "The cloudflared set in the config (cloudflared_cli) isn't there."
                        } else if cfg!(windows) {
                            "cloudflared isn't installed here. Install it (winget install Cloudflare.cloudflared), then try again."
                        } else if cfg!(target_os = "macos") {
                            "cloudflared isn't installed here. Install it (brew install cloudflared), then try again."
                        } else {
                            "cloudflared isn't installed here. Install it (see developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads), then try again."
                        }
                        .into(),
                    ),
                );
            }
        };
        let set_up = self.lock().set_up.clone();
        let tunnel_id = match set_up {
            _ if quick => QUICK.to_owned(),
            Some(set_up) if set_up.hostname == want.hostname && self.credentials().is_file() => {
                set_up.tunnel_id
            }
            _ => {
                self.stop();
                match self.set_up(&cli, &want.hostname, cert, env).await {
                    Ok(id) => id,
                    Err(err) => return status("error", None, Some(err)),
                }
            }
        };

        // Running already, for this tunnel and listener?
        {
            let mut state = self.lock();
            if let Some(running) = state.running.as_mut() {
                let alive = matches!(running.child.try_wait(), Ok(None));
                if alive && running.target == want.target && running.tunnel_id == tunnel_id {
                    let heard = self.heard.lock().ok();
                    let connected = heard.as_ref().is_some_and(|h| h.connected && !h.exited);
                    let error = heard.and_then(|h| h.last_error.clone());
                    return if connected {
                        status("on", url, None)
                    } else {
                        status("starting", url, error)
                    };
                }
            }
        }
        // Stopped, or for something else: (re)start it.
        let last_error = self.heard.lock().ok().and_then(|h| h.last_error.clone());
        let was_running = self.lock().running.is_some();
        let keeps_stopping = {
            let mut state = self.lock();
            if was_running && state.started.is_some_and(|at| at.elapsed() < QUICK_STOP) {
                state.quick_stops += 1;
            } else {
                state.quick_stops = 0;
            }
            state.quick_stops >= 2
        };
        if keeps_stopping {
            // It keeps stopping: wait for the next sync rather than loop.
            self.stop();
            let why = last_error.unwrap_or_else(|| "it stopped".into());
            return status(
                "error",
                None,
                Some(format!(
                    "cloudflared keeps stopping ({why}). Trying again in a minute."
                )),
            );
        }
        self.stop();
        match self.start(&cli, &tunnel_id, &want.target, env) {
            Ok(()) => status(
                "starting",
                url,
                was_running
                    .then(|| {
                        last_error.map(|e| format!("cloudflared stopped ({e}); starting it again."))
                    })
                    .flatten(),
            ),
            Err(err) => status(
                "error",
                None,
                Some(format!("couldn't run cloudflared: {err}")),
            ),
        }
    }

    /// Create the tunnel (or find the one made before), fetch its
    /// credentials, and point the name at it. Returns the tunnel's id.
    async fn set_up(
        &self,
        cli: &Path,
        hostname: &str,
        cert: Option<&str>,
        env: &HashMap<String, String>,
    ) -> Result<String, String> {
        let name = tunnel_name(hostname);
        let credentials = self.credentials();
        let credentials_arg = credentials.to_string_lossy().into_owned();
        let cert = cert
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| loft_config::expand_tilde(c).to_string_lossy().into_owned());
        let tunnel = |args: &[&str]| {
            let mut all = vec!["tunnel"];
            if let Some(cert) = cert.as_deref() {
                all.extend(["--origincert", cert]);
            }
            all.extend_from_slice(args);
            all.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        let cloudflared = |args: Vec<String>| async move {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let ran = run(cli, &args, env)
                .await
                .map_err(|e| format!("couldn't run cloudflared: {e}"))?;
            if ran.timed_out {
                return Err("cloudflared didn't answer (is this computer online?).".to_owned());
            }
            Ok(ran)
        };
        let _ = std::fs::create_dir_all(&self.dir);
        // Left from a setup that didn't finish (cloudflared writes it
        // read-only, and won't write over it): fetched again below.
        let _ = std::fs::remove_file(&credentials);

        let created = cloudflared(tunnel(&[
            "create",
            "--output",
            "json",
            "--credentials-file",
            &credentials_arg,
            &name,
        ]))
        .await?;
        let id = if created.ok {
            tunnel_id(&created.text)
                .ok_or_else(|| first_line(&created.text, "cloudflared made no tunnel"))?
        } else if created.text.contains("already exists") {
            // Made before (by an earlier setup, or another home): use it.
            let listed =
                cloudflared(tunnel(&["list", "--output", "json", "--name", &name])).await?;
            let id = tunnel_id(&listed.text)
                .ok_or_else(|| first_line(&listed.text, "couldn't find the tunnel"))?;
            let token =
                cloudflared(tunnel(&["token", "--cred-file", &credentials_arg, &id])).await?;
            if !token.ok {
                return Err(first_line(
                    &token.text,
                    "couldn't fetch the tunnel's credentials",
                ));
            }
            id
        } else if created.text.contains("cert.pem")
            || created.text.contains("origincert")
            || created.text.contains("origin cert")
        {
            return Err("cloudflared isn't logged in to Cloudflare. Run `cloudflared tunnel login`, or set the login to use (cloudflare_cert), then try again.".into());
        } else {
            return Err(first_line(
                &created.text,
                "cloudflared couldn't create the tunnel",
            ));
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o600));
        }

        // The name: added, or already this tunnel's. Never overwritten.
        let routed = cloudflared(tunnel(&["route", "dns", &id, hostname])).await?;
        if !routed.ok
            && !routed
                .text
                .contains("already configured to route to your tunnel")
        {
            let why = if routed.text.contains("already exists") {
                format!(
                    "{hostname} is already in use in Cloudflare's DNS. Choose another name, or remove that record, then try again."
                )
            } else {
                first_line(&routed.text, "cloudflared couldn't add the DNS record")
            };
            return Err(why);
        }
        let set_up = SetUp {
            hostname: hostname.to_owned(),
            tunnel_id: id.clone(),
            pid: None,
        };
        tracing::info!(%hostname, tunnel = %id, "cloudflare: tunnel set up");
        self.lock().set_up = Some(set_up.clone());
        self.save(Some(&set_up));
        Ok(id)
    }

    /// Where a quick tunnel's `cloudflared` keeps its home: empty, so no
    /// login or config of the user's applies to it.
    fn quick_home(&self) -> PathBuf {
        self.dir.join("cloudflared-quick")
    }

    fn start(
        &self,
        cli: &Path,
        tunnel_id: &str,
        target: &str,
        env: &HashMap<String, String>,
    ) -> std::io::Result<()> {
        let mut command = tokio::process::Command::new(cli);
        command
            .args(["tunnel", "--no-autoupdate", "--metrics", "127.0.0.1:0"])
            .env_clear()
            .envs(env);
        if tunnel_id == QUICK {
            let home = self.quick_home();
            std::fs::create_dir_all(&home)?;
            let config = home.join("config.yml");
            // A file of its own (or a system-wide /etc/cloudflared one would
            // apply), with something in it: cloudflared logs an error for
            // an empty one.
            std::fs::write(
                &config,
                "# Agentloft's quick tunnel (web/cloudflare.rs)\nno-autoupdate: true\n",
            )?;
            command
                .arg("--config")
                .arg(&config)
                .args(["--url", target])
                .env("HOME", &home)
                .env("USERPROFILE", &home)
                .env_remove("TUNNEL_ORIGIN_CERT")
                .env_remove("TUNNEL_TOKEN");
        } else {
            command
                .args(["run", "--credentials-file"])
                .arg(self.credentials())
                .args(["--url", target, tunnel_id]);
        }
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let run = {
            let mut heard = self.heard.lock().unwrap_or_else(|e| e.into_inner());
            let run = heard.run + 1;
            *heard = Heard {
                run,
                ..Heard::default()
            };
            run
        };
        if let Some(stderr) = child.stderr.take() {
            let heard = self.heard.clone();
            let connected = self.connected.clone();
            let stopped = self.stopped.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt as _;
                let mut lines = tokio::io::BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut heard = heard.lock().unwrap_or_else(|e| e.into_inner());
                    if heard.run != run {
                        return;
                    }
                    if let Some(host) = quick_host(&line) {
                        heard.quick_host = Some(host);
                        if heard.connected {
                            connected.notify_one();
                        }
                    } else if line.contains("Registered tunnel connection") {
                        if !heard.connected {
                            connected.notify_one();
                        }
                        heard.connected = true;
                        heard.last_error = None;
                    } else if line.contains(" ERR ") {
                        let text = line.split(" ERR ").nth(1).unwrap_or(&line).trim();
                        heard.last_error = Some(text.chars().take(300).collect());
                    }
                }
                // Its output ended: it stopped (killed by us for a newer
                // run, which then counts instead, or by itself).
                let mut heard = heard.lock().unwrap_or_else(|e| e.into_inner());
                if heard.run == run {
                    heard.exited = true;
                    heard.connected = false;
                    drop(heard);
                    stopped.notify_one();
                }
            });
        }
        let pid = child.id();
        tracing::info!(tunnel = %tunnel_id, %target, ?pid, "cloudflare: running the tunnel");
        if tunnel_id == QUICK
            && let Some(pid) = pid
        {
            let _ = std::fs::write(self.quick_home().join("pid"), pid.to_string());
        }
        let mut state = self.lock();
        state.started = Some(std::time::Instant::now());
        state.running = Some(Running {
            child,
            target: target.to_owned(),
            tunnel_id: tunnel_id.to_owned(),
        });
        if let Some(set_up) = state.set_up.as_mut() {
            set_up.pid = pid;
            let set_up = set_up.clone();
            drop(state);
            self.save(Some(&set_up));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_tunnel_names() {
        let line = "2026-11-17T01:00:00Z INF |  https://shiny-river-otter-lamp.trycloudflare.com                    |";
        assert_eq!(
            quick_host(line).as_deref(),
            Some("shiny-river-otter-lamp.trycloudflare.com")
        );
        for not in [
            "2026-11-17T01:00:00Z INF Requesting new quick Tunnel on trycloudflare.com...",
            "INF |  https://example.com  |",
            "INF https://evil.trycloudflare.com.example.com/",
            "INF https://a.b.trycloudflare.com",
            "INF https://.trycloudflare.com",
            "INF See https://developers.cloudflare.com/ for more",
        ] {
            assert_eq!(quick_host(not), None, "{not}");
        }
    }

    #[test]
    fn names_and_ids() {
        assert_eq!(
            tunnel_name("agentloft-desk.example.com"),
            "agentloft-agentloft-desk-example-com"
        );
        let id = "6ff42ae2-765d-4adf-8112-31c55c1551ef";
        assert_eq!(
            tunnel_id(&format!(
                r#"{{"id":"{id}","name":"x","created_at":"2026-11-15T00:00:00Z"}}"#
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(
            tunnel_id(&format!(r#"[{{"id":"{id}","name":"x"}}]"#)).as_deref(),
            Some(id)
        );
        // Log lines before the JSON.
        assert_eq!(
            tunnel_id(&format!(
                "2026-11-15T00:00:00Z INF Created\n{{\"id\":\"{id}\"}}"
            ))
            .as_deref(),
            Some(id)
        );
        // …and after it (the real cloudflared, 2026.10.3).
        assert_eq!(
            tunnel_id(&format!(
                "{{\n  \"id\": \"{id}\",\n  \"name\": \"x\"\n}}\n2026-11-16T03:40:14Z WRN Your version 2026.10.3 is outdated. We recommend upgrading it to 2026.11.0\n"
            ))
            .as_deref(),
            Some(id)
        );
        assert_eq!(tunnel_id("[]"), None);
        assert_eq!(tunnel_id(r#"{"id":"../../x"}"#), None);
        assert_eq!(tunnel_id("error"), None);
    }
}
