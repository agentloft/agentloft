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
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            accent: None,
            font_family: None,
            mono_font_family: None,
            font_size: 14.0,
            mono_font_size: 12.5,
            density: "comfortable".into(),
            content_width: 780.0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HarnessConfigs {
    pub claude: HarnessConfig,
    pub codex: HarnessConfig,
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
    pub idle_timeout_minutes: u64,
    pub extra_args: Vec<String>,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            path: "auto".into(),
            default_model: "auto".into(),
            default_permission_mode: "harness-default".into(),
            idle_timeout_minutes: 15,
            extra_args: Vec::new(),
        }
    }
}

impl Config {
    /// Load `config.toml`; a missing file yields defaults.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        match std::fs::read_to_string(&file) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parse {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", file.display())),
        }
    }
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
    env.insert("PATH".into(), with_fallback_dirs(&path));
    env
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
