//! Paths under `~/.agentloft`, the user's `config.toml`, and the login-shell
//! environment used to spawn agent CLIs (PLAN §1.7, §4.1, §4.5).

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable that relocates the whole data home (tests, dev).
pub const HOME_ENV: &str = "AGENTLOFT_HOME";

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `$AGENTLOFT_HOME`, else `~/.agentloft`.
    pub fn from_env() -> Result<Self> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            return Ok(Self::new(PathBuf::from(home)));
        }
        let user_home = std::env::home_dir().context("cannot determine home directory")?;
        Ok(Self::new(user_home.join(".agentloft")))
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn db_file(&self) -> PathBuf {
        self.home.join("agentloft.db")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.home.join("backups")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }
    /// The daemon socket. Unix socket paths are limited to ~104 bytes on
    /// macOS, so deep data homes fall back to a short per-user directory
    /// (`/tmp/agentloft-<user>/<hash>.sock`) keyed by the home path.
    pub fn socket_file(&self) -> PathBuf {
        let preferred = self.run_dir().join("agentloftd.sock");
        if preferred.as_os_str().len() < MAX_SOCKET_PATH {
            return preferred;
        }
        short_socket_dir().join(format!(
            "{:016x}.sock",
            fnv1a(self.home.as_os_str().as_encoded_bytes())
        ))
    }

    /// Directory holding the socket (created by [`Paths::ensure_dirs`]).
    pub fn socket_dir(&self) -> PathBuf {
        self.socket_file()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.run_dir())
    }
    pub fn lock_file(&self) -> PathBuf {
        self.run_dir().join("agentloftd.lock")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    pub fn worktrees_dir(&self) -> PathBuf {
        self.home.join("worktrees")
    }

    /// Chats (threads without a project) live here, each in a folder of its
    /// own; the folder itself is the built-in "Chats" project.
    pub fn chats_dir(&self) -> PathBuf {
        self.home.join("chats")
    }

    /// New projects (made from a name) live here, a folder each.
    pub fn projects_dir(&self) -> PathBuf {
        self.home.join("projects")
    }

    /// Images uploaded with messages.
    pub fn attachments_dir(&self) -> PathBuf {
        self.home.join("attachments")
    }

    /// Project icons the user chose.
    pub fn icons_dir(&self) -> PathBuf {
        self.home.join("icons")
    }

    /// Colour themes the user (or an agent) added.
    pub fn themes_dir(&self) -> PathBuf {
        self.home.join("themes")
    }

    /// Web access secrets: the pairing token and signed-in sessions
    /// (private, 0700; see ADR-0016).
    pub fn web_dir(&self) -> PathBuf {
        self.home.join("web")
    }

    /// Create the directory layout. `run/` is private (0700).
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.home, &self.logs_dir(), &self.backups_dir()] {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        for dir in [self.run_dir(), self.socket_dir()] {
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            set_mode(&dir, 0o700)?;
        }
        Ok(())
    }
}

/// macOS `sun_path` is 104 bytes including the NUL; keep a margin.
const MAX_SOCKET_PATH: usize = 100;

fn short_socket_dir() -> PathBuf {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| {
            !u.is_empty()
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
        .unwrap_or_else(|| "user".into());
    PathBuf::from("/tmp").join(format!("agentloft-{user}"))
}

/// Stable, dependency-free 64-bit FNV-1a hash.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("chmod {}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// config.toml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub harness: HarnessConfigs,
    pub worktrees: WorktreeConfig,
    pub projects: ProjectsConfig,
    pub web: WebConfig,
}

/// `[web]` in config.toml: Agentloft in a browser, served by the daemon
/// (ADR-0016). Off by default, and local-only unless told otherwise.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct WebConfig {
    pub enabled: bool,
    /// The address to listen on: an IP (`127.0.0.1`, `::1`, a LAN or
    /// tailnet address, `0.0.0.0`) or `localhost`.
    pub bind: String,
    /// `0` picks a free port.
    pub port: u16,
    /// PEM certificate chain and private key; both or neither (`~` allowed).
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    /// Serve plain HTTP on a non-loopback address. Anyone on that network
    /// can read the traffic; meant only for a link that is already
    /// encrypted (a VPN or tailnet interface).
    pub allow_insecure_remote: bool,
    /// Extra names browsers may reach the listener by (the `Host` header),
    /// e.g. a reverse proxy's or tunnel's hostname.
    pub allowed_hosts: Vec<String>,
    /// How long a paired browser stays signed in.
    pub session_days: u32,
    /// Let browser sessions open terminals (a shell on this machine).
    pub allow_terminal: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1".into(),
            port: WebConfig::DEFAULT_PORT,
            tls_cert: None,
            tls_key: None,
            allow_insecure_remote: false,
            allowed_hosts: Vec::new(),
            session_days: 30,
            allow_terminal: false,
        }
    }
}

/// A `[web]` section that passed the safety rules: where to listen and how.
#[derive(Debug, Clone, PartialEq)]
pub struct WebPlan {
    pub addr: std::net::SocketAddr,
    /// Certificate chain and key files when serving HTTPS.
    pub tls: Option<(PathBuf, PathBuf)>,
    /// Plain HTTP on a non-loopback address, explicitly allowed.
    pub insecure_remote: bool,
    pub allowed_hosts: Vec<String>,
    pub session_ttl: Duration,
    pub allow_terminal: bool,
}

impl WebConfig {
    pub const DEFAULT_PORT: u16 = 4780;

    /// Check the section against the safety rules. A non-loopback address
    /// is refused unless TLS is configured or `allow_insecure_remote` is set.
    pub fn plan(&self) -> Result<WebPlan, String> {
        let bind = self.bind.trim();
        let ip: std::net::IpAddr = if bind.eq_ignore_ascii_case("localhost") {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            bind.trim_start_matches('[')
                .trim_end_matches(']')
                .parse()
                .map_err(|_| format!("`{bind}` isn't an IP address"))?
        };
        let file = |v: &Option<String>| {
            v.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(expand_tilde)
        };
        let tls = match (file(&self.tls_cert), file(&self.tls_key)) {
            (Some(cert), Some(key)) => Some((cert, key)),
            (None, None) => None,
            _ => return Err("TLS needs both a certificate and a key".into()),
        };
        let insecure_remote = !ip.is_loopback() && tls.is_none();
        if insecure_remote && !self.allow_insecure_remote {
            return Err(format!(
                "{ip} is reachable from other machines: add a TLS certificate, \
                 or set allow_insecure_remote"
            ));
        }
        let mut allowed_hosts = Vec::new();
        for host in &self.allowed_hosts {
            let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
            let valid = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'));
            if !valid {
                return Err(format!("`{host}` isn't a host name"));
            }
            allowed_hosts.push(host);
        }
        Ok(WebPlan {
            addr: std::net::SocketAddr::new(ip, self.port),
            tls,
            insecure_remote,
            allowed_hosts,
            session_ttl: Duration::from_secs(u64::from(self.session_days.clamp(1, 365)) * 86_400),
            allow_terminal: self.allow_terminal,
        })
    }
}

/// `~` and `~/…` resolved against the user's home folder.
pub fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some("") => std::env::home_dir().unwrap_or_else(|| PathBuf::from(path)),
        Some(rest) if rest.starts_with('/') => std::env::home_dir()
            .map(|home| home.join(rest.trim_start_matches('/')))
            .unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

/// `[projects]` in config.toml.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectsConfig {
    /// Where Add Project starts browsing and where clones go (`~`
    /// allowed); unset = the home folder.
    pub base_dir: Option<String>,
    /// Where new projects (made from a name) go; unset =
    /// `~/.agentloft/projects`.
    pub new_dir: Option<String>,
}

/// `[worktrees]` in config.toml (PLAN §5.2).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct WorktreeConfig {
    /// New threads: `ask` (pick per thread, local preselected) | `local` |
    /// `worktree`.
    pub mode: String,
    /// Branch prefix for worktree branches.
    pub branch_prefix: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            mode: "ask".into(),
            branch_prefix: "agentloft".into(),
        }
    }
}

/// Appearance settings (`[ui]` in config.toml). Reloaded live by the app.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// `system` | `light` | `dark`
    pub theme: String,
    /// Colour theme: a built-in id (`loft`, `fjord`, …) or `custom:<file>`
    /// for one in `~/.agentloft/themes/`.
    pub palette: String,
    /// `soft` | `normal` | `high`: how strongly borders and secondary text
    /// stand out.
    pub contrast: String,
    /// Accent color as `#rrggbb`; `None` = theme default.
    pub accent: Option<String>,
    /// UI font family; `None` = system UI font.
    pub font_family: Option<String>,
    /// Monospace font family for code and tool output.
    pub mono_font_family: Option<String>,
    /// Base UI font size in px.
    pub font_size: f32,
    /// Monospace font size in px.
    pub mono_font_size: f32,
    /// `comfortable` | `compact`
    pub density: String,
    /// Max width of the conversation column in px.
    pub content_width: f32,
    /// Sidebar: `threads` (one list, most recent first, each with its
    /// project) | `projects` (threads grouped under their project).
    pub sidebar: String,
    /// Shortcuts changed from their defaults: shortcut id → keys
    /// (`"cmd-shift-t"`; `""` turns it off).
    pub keys: std::collections::BTreeMap<String, String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            palette: "loft".into(),
            contrast: "normal".into(),
            accent: None,
            font_family: None,
            mono_font_family: None,
            font_size: 14.0,
            mono_font_size: 12.5,
            density: "comfortable".into(),
            content_width: 780.0,
            sidebar: "threads".into(),
            keys: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessConfigs {
    pub claude: HarnessConfig,
    pub codex: HarnessConfig,
    pub opencode: HarnessConfig,
    pub gemini: HarnessConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessConfig {
    /// `"auto"` = resolve from the login-shell `PATH`.
    pub path: String,
    /// `"auto"` = the harness's own default model.
    pub default_model: String,
    /// `"harness-default"` or one of the canonical modes.
    pub default_permission_mode: String,
    /// Reasoning effort for new threads; `"auto"` = the model's default.
    pub default_effort: String,
    pub idle_timeout_minutes: u64,
    pub extra_args: Vec<String>,
    /// Off: the agent is hidden from pickers (its threads keep working).
    pub enabled: bool,
    /// Extra environment variables for the agent's processes.
    pub env: std::collections::BTreeMap<String, String>,
    /// Models left out of the pickers.
    pub hidden_models: Vec<String>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            path: "auto".into(),
            default_model: "auto".into(),
            default_permission_mode: "harness-default".into(),
            default_effort: "auto".into(),
            idle_timeout_minutes: 15,
            extra_args: Vec::new(),
            enabled: true,
            env: Default::default(),
            hidden_models: Vec::new(),
        }
    }
}

impl Config {
    /// Load `config.toml`; a missing file yields defaults.
    pub fn load(paths: &Paths) -> Result<Self> {
        Self::load_file(&paths.config_file())
    }

    /// Load a config file; a missing file yields defaults.
    pub fn load_file(file: &Path) -> Result<Self> {
        match std::fs::read_to_string(file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }
}

/// Per-project settings in `<repo>/.agentloft/config.toml` (PLAN §5.2),
/// meant to be committed so the whole team gets them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectConfig {
    pub worktree: ProjectWorktreeConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProjectWorktreeConfig {
    /// Shell command run in a new worktree before the agent starts
    /// (e.g. `npm install`).
    pub setup: Option<String>,
    /// Untracked files to copy into new worktrees: paths relative to the
    /// repo, `*`/`?` wildcards in the last component (`.env*`,
    /// `config/*.local.json`).
    pub copy: Vec<String>,
}

impl Default for ProjectWorktreeConfig {
    fn default() -> Self {
        Self {
            setup: None,
            copy: vec![".env*".into()],
        }
    }
}

impl ProjectConfig {
    /// `<root>/.agentloft/config.toml`; missing or invalid → defaults.
    pub fn load(root: &Path) -> Self {
        let file = root.join(".agentloft").join("config.toml");
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!(file = %file.display(), %err, "invalid project config");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

/// A value written by the settings UI.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Str(String),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
    /// A table of strings (e.g. environment variables).
    Map(Vec<(String, String)>),
    /// Remove the key (back to the default).
    Unset,
}

/// Set one key in `config.toml` (e.g. `["ui", "theme"]`), keeping the rest
/// of the file — comments and formatting included — as the user wrote it.
pub fn write_setting(paths: &Paths, key: &[&str], value: SettingValue) -> Result<()> {
    use toml_edit::{DocumentMut, Item, Table, value as v};
    let file = paths.config_file();
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("read {}", file.display())),
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", file.display()))?;
    let (last, tables) = key.split_last().context("empty key")?;
    let mut table = doc.as_table_mut();
    for name in tables {
        let entry = table.entry(name).or_insert(Item::Table(Table::new()));
        table = entry
            .as_table_mut()
            .with_context(|| format!("`{name}` is not a table"))?;
    }
    match value {
        SettingValue::Str(s) => {
            table.insert(last, v(s));
        }
        SettingValue::Float(f) => {
            // Whole numbers stay floats (`14.0`) so serde reads them as f32.
            table.insert(last, v(f));
        }
        SettingValue::Bool(b) => {
            table.insert(last, v(b));
        }
        SettingValue::List(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(item);
            }
            table.insert(last, v(array));
        }
        SettingValue::Map(pairs) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, val) in pairs {
                inline.insert(&k, val.into());
            }
            table.insert(last, v(inline));
        }
        SettingValue::Unset => {
            table.remove(last);
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write-then-rename so a crash never leaves a truncated config.
    let tmp = file.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Login-shell environment (macOS GUI apps don't inherit the shell PATH)
// ---------------------------------------------------------------------------

/// Captures `$SHELL -l -i -c 'env -0'` with a timeout, falling back to the
/// current environment plus well-known binary directories.
pub fn capture_login_env(timeout: Duration) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    match run_login_shell_env(timeout) {
        Ok(captured) if captured.contains_key("PATH") => {
            env.extend(captured);
        }
        Ok(_) => tracing::warn!("login shell env had no PATH; using fallback"),
        Err(err) => tracing::warn!(%err, "login shell env capture failed; using fallback"),
    }
    let path = env.get("PATH").cloned().unwrap_or_default();
    env.insert("PATH".into(), with_fallback_dirs(&stable_path_var(&path)));
    env
}

/// `PATH` with fnm's per-session directories resolved (see [`stable_path`]),
/// so Node-based CLIs keep finding `node` after that shell is gone.
fn stable_path_var(path: &str) -> String {
    path.split(':')
        .map(|dir| {
            if dir.contains("fnm_multishells") {
                std::fs::canonicalize(dir)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| dir.to_owned())
            } else {
                dir.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn run_login_shell_env(timeout: Duration) -> Result<HashMap<String, String>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(&shell)
        .args(["-l", "-i", "-c", "env -0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {shell}"))?;

    let mut stdout = child.stdout.take().context("no stdout")?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("login shell timed out after {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let buf = reader.join().unwrap_or_default();
    Ok(parse_env_nul(&buf))
}

fn parse_env_nul(buf: &[u8]) -> HashMap<String, String> {
    buf.split(|b| *b == 0)
        .filter_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            let (k, v) = entry.split_once('=')?;
            (!k.is_empty() && !k.contains(char::is_whitespace))
                .then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

fn with_fallback_dirs(path: &str) -> String {
    let mut parts: Vec<String> = path
        .split(':')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    let home = std::env::home_dir().unwrap_or_default();
    let extras = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
        home.join(".volta/bin"),
    ];
    for extra in extras {
        let s = extra.to_string_lossy().into_owned();
        if !parts.contains(&s) {
            parts.push(s);
        }
    }
    parts.join(":")
}

/// Resolve a binary name against a `PATH` string (`which`).
pub fn which(bin: &str, path: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.is_file().then_some(p);
    }
    path.split(':')
        .map(|dir| Path::new(dir).join(bin))
        .find(|candidate| is_executable(candidate))
        .map(|found| stable_path(&found))
}

/// fnm puts each shell session's Node on a temporary `fnm_multishells/<id>`
/// symlink farm that disappears with that shell; resolve through it so a
/// path captured once keeps working. (Other shims — Volta, asdf — must stay
/// as found: they dispatch on the name they were invoked by.)
fn stable_path(found: &Path) -> PathBuf {
    if found
        .components()
        .any(|c| c.as_os_str() == "fnm_multishells")
        && let Ok(real) = std::fs::canonicalize(found)
    {
        return real;
    }
    found.to_path_buf()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {

    #[test]
    fn web_is_off_and_local_by_default() {
        let web = WebConfig::default();
        assert!(!web.enabled);
        let plan = web.plan().unwrap();
        assert!(plan.addr.ip().is_loopback());
        assert_eq!(plan.addr.port(), WebConfig::DEFAULT_PORT);
        assert!(plan.tls.is_none() && !plan.insecure_remote);
    }

    #[test]
    fn web_refuses_a_remote_bind_without_tls() {
        let remote = |bind: &str| WebConfig {
            bind: bind.into(),
            ..WebConfig::default()
        };
        for bind in ["0.0.0.0", "192.168.1.20", "100.64.0.7", "::", "fd7a::1"] {
            let err = remote(bind).plan().unwrap_err();
            assert!(err.contains("TLS"), "{bind}: {err}");
        }
        for bind in ["127.0.0.1", "localhost", "::1", "[::1]", "127.0.0.2"] {
            assert!(remote(bind).plan().is_ok(), "{bind}");
        }
        assert!(
            remote("example.com").plan().is_err(),
            "names other than localhost"
        );

        // TLS, or the explicit override, allows it.
        let tls = WebConfig {
            tls_cert: Some("/etc/tls/cert.pem".into()),
            tls_key: Some("/etc/tls/key.pem".into()),
            ..remote("0.0.0.0")
        };
        let plan = tls.plan().unwrap();
        assert!(plan.tls.is_some() && !plan.insecure_remote);
        let insecure = WebConfig {
            allow_insecure_remote: true,
            ..remote("0.0.0.0")
        };
        assert!(insecure.plan().unwrap().insecure_remote);
        // The override doesn't mark a loopback listener as insecure.
        let local = WebConfig {
            allow_insecure_remote: true,
            ..WebConfig::default()
        };
        assert!(!local.plan().unwrap().insecure_remote);
    }

    #[test]
    fn web_tls_needs_both_files_and_clean_host_names() {
        let half = WebConfig {
            tls_cert: Some("cert.pem".into()),
            ..WebConfig::default()
        };
        assert!(half.plan().is_err());
        let hosts = WebConfig {
            allowed_hosts: vec![" Desk.Tailnet.ts.net. ".into()],
            ..WebConfig::default()
        };
        assert_eq!(hosts.plan().unwrap().allowed_hosts, ["desk.tailnet.ts.net"]);
        let bad = WebConfig {
            allowed_hosts: vec!["evil.example/path".into()],
            ..WebConfig::default()
        };
        assert!(bad.plan().is_err());
    }

    #[test]
    fn web_section_parses() {
        let cfg: Config =
            toml::from_str("[web]\nenabled = true\nport = 0\nallowed_hosts = [\"desk.example\"]\n")
                .unwrap();
        assert!(cfg.web.enabled);
        assert_eq!(cfg.web.port, 0);
        assert_eq!(cfg.web.bind, "127.0.0.1");
        assert_eq!(cfg.web.session_days, 30);
    }

    #[test]
    fn project_config_defaults_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ProjectConfig::load(dir.path()).worktree.copy,
            vec![".env*".to_owned()]
        );
        std::fs::create_dir_all(dir.path().join(".agentloft")).unwrap();
        std::fs::write(
            dir.path().join(".agentloft/config.toml"),
            "[worktree]\nsetup = \"npm ci\"\ncopy = [\".env*\", \"config/*.local.json\"]\n",
        )
        .unwrap();
        let c = ProjectConfig::load(dir.path());
        assert_eq!(c.worktree.setup.as_deref(), Some("npm ci"));
        assert_eq!(c.worktree.copy.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn which_resolves_fnm_session_links_only() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("node-v24/bin");
        std::fs::create_dir_all(&real).unwrap();
        let tool = real.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = dir.path().join("fnm_multishells/123_456/bin");
        std::fs::create_dir_all(&session).unwrap();
        std::os::unix::fs::symlink(&tool, session.join("tool")).unwrap();
        let shim = dir.path().join("shims");
        std::fs::create_dir_all(&shim).unwrap();
        std::os::unix::fs::symlink(&tool, shim.join("tool")).unwrap();

        let found = which("tool", &session.to_string_lossy()).unwrap();
        assert_eq!(found, std::fs::canonicalize(&tool).unwrap());
        let found = which("tool", &shim.to_string_lossy()).unwrap();
        assert_eq!(found, shim.join("tool"), "other shims keep their name");
    }

    #[test]
    fn write_setting_keeps_comments_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        std::fs::write(
            paths.config_file(),
            "# my settings\n[ui]\ntheme = \"dark\" # night owl\n",
        )
        .unwrap();
        write_setting(&paths, &["ui", "font_size"], SettingValue::Float(15.0)).unwrap();
        write_setting(
            &paths,
            &["ui", "accent"],
            SettingValue::Str("#3b82f6".into()),
        )
        .unwrap();
        write_setting(
            &paths,
            &["harness", "codex", "default_model"],
            SettingValue::Str("gpt-5".into()),
        )
        .unwrap();
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(
            text.contains("# my settings") && text.contains("# night owl"),
            "{text}"
        );
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.ui.theme, "dark");
        assert_eq!(cfg.ui.font_size, 15.0);
        assert_eq!(cfg.ui.accent.as_deref(), Some("#3b82f6"));
        assert_eq!(cfg.harness.codex.default_model, "gpt-5");
        write_setting(&paths, &["ui", "accent"], SettingValue::Unset).unwrap();
        write_setting(
            &paths,
            &["harness", "codex", "enabled"],
            SettingValue::Bool(false),
        )
        .unwrap();
        write_setting(
            &paths,
            &["harness", "codex", "extra_args"],
            SettingValue::List(vec!["--foo".into(), "bar".into()]),
        )
        .unwrap();
        write_setting(
            &paths,
            &["harness", "codex", "env"],
            SettingValue::Map(vec![("OPENAI_BASE_URL".into(), "http://x".into())]),
        )
        .unwrap();
        let c = Config::load(&paths).unwrap();
        assert!(!c.harness.codex.enabled);
        assert_eq!(c.harness.codex.extra_args, ["--foo", "bar"]);
        assert_eq!(c.harness.codex.env["OPENAI_BASE_URL"], "http://x");
        assert_eq!(Config::load(&paths).unwrap().ui.accent, None);
    }

    use super::*;

    #[test]
    fn parses_nul_separated_env() {
        let env = parse_env_nul(b"PATH=/a:/b\0HOME=/h\0junk\0MULTI=a=b\0");
        assert_eq!(env["PATH"], "/a:/b");
        assert_eq!(env["MULTI"], "a=b");
        assert!(!env.contains_key("junk"));
    }

    #[test]
    fn fallback_dirs_are_appended_once() {
        let p = with_fallback_dirs("/usr/bin:/opt/homebrew/bin");
        assert_eq!(p.matches("/opt/homebrew/bin").count(), 1);
        assert!(p.starts_with("/usr/bin:"));
    }

    #[test]
    fn long_homes_get_a_short_socket_path() {
        let short = Paths::new("/tmp/h");
        assert_eq!(
            short.socket_file(),
            PathBuf::from("/tmp/h/run/agentloftd.sock")
        );
        let long = Paths::new(format!("/tmp/{}", "x".repeat(120)));
        let sock = long.socket_file();
        assert!(
            sock.as_os_str().len() < MAX_SOCKET_PATH,
            "{}",
            sock.display()
        );
        assert!(sock.to_string_lossy().starts_with("/tmp/agentloft-"));
        // Stable per home, different across homes.
        assert_eq!(sock, long.socket_file());
        assert_ne!(
            sock,
            Paths::new(format!("/tmp/{}", "y".repeat(120))).socket_file()
        );
    }

    #[test]
    fn missing_config_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(&Paths::new(dir.path())).unwrap();
        assert_eq!(cfg.harness.claude.path, "auto");
    }

    #[test]
    fn ui_config_defaults_and_overrides() {
        let cfg: Config = toml::from_str("[ui]\ntheme = \"dark\"\naccent = \"#7c5cff\"\n").unwrap();
        assert_eq!(cfg.ui.theme, "dark");
        assert_eq!(cfg.ui.accent.as_deref(), Some("#7c5cff"));
        assert_eq!(cfg.ui.font_size, 14.0);
    }

    #[test]
    fn config_parses_partial() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[harness.claude]\npath = \"/x/claude\"\n",
        )
        .unwrap();
        let cfg = Config::load(&Paths::new(dir.path())).unwrap();
        assert_eq!(cfg.harness.claude.path, "/x/claude");
        assert_eq!(cfg.harness.claude.idle_timeout_minutes, 15);
    }
}
