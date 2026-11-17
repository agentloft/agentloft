//! Newer releases of the agents' CLIs, and installing them.
//!
//! Where a CLI was installed from decides both what "newer" means and how it
//! is updated. A Homebrew cask can trail npm by days, and Claude Code's own
//! installer follows the channel set in its settings (`latest` or `stable`).
//! Comparing against another channel than the install's own is how a CLI
//! that is already current gets an "update available", so an install that
//! can't be placed gets no check at all.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fetches a URL's body (the daemon's HTTP client; `None` in tests and
/// offline builds, which then never report updates).
pub type Fetcher = Arc<dyn Fn(&str) -> anyhow::Result<String> + Send + Sync>;

/// How long an answer about the latest release is kept.
const FRESH: Duration = Duration::from_secs(6 * 60 * 60);
/// How long a failed ask is kept before asking again.
const FAILED_FRESH: Duration = Duration::from_secs(30 * 60);

/// Where a CLI was installed from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    /// A global npm, pnpm or bun package.
    Npm { package: String },
    /// A Homebrew formula or cask.
    Brew { name: String, cask: bool },
    /// Claude Code's own installer (`~/.local/share/claude/versions/…`),
    /// following its `autoUpdatesChannel`.
    ClaudeNative { channel: String },
}

impl Source {
    /// Where the latest release on this channel is published.
    fn url(&self) -> String {
        match self {
            Source::Npm { package } => {
                format!("https://registry.npmjs.org/-/package/{package}/dist-tags")
            }
            Source::ClaudeNative { .. } => {
                "https://registry.npmjs.org/-/package/@anthropic-ai/claude-code/dist-tags".into()
            }
            Source::Brew { name, cask: true } => {
                format!("https://formulae.brew.sh/api/cask/{name}.json")
            }
            Source::Brew { name, cask: false } => {
                format!("https://formulae.brew.sh/api/formula/{name}.json")
            }
        }
    }

    /// The version in an answer from [`Source::url`].
    fn parse(&self, body: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(body).ok()?;
        let v = match self {
            Source::Npm { .. } => json.get("latest"),
            Source::ClaudeNative { channel } => json.get(channel),
            // Casks may add a build after a comma (`1.2.3,abc`).
            Source::Brew { cask: true, .. } => json.get("version"),
            Source::Brew { cask: false, .. } => json.pointer("/versions/stable"),
        }?
        .as_str()?;
        let v = v.split(',').next().unwrap_or(v).trim();
        (!v.is_empty()).then(|| v.to_owned())
    }
}

/// An installed CLI: where it came from and what updates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Install {
    pub source: Source,
    /// Program and arguments that install the latest release.
    pub command: Option<(PathBuf, Vec<String>)>,
}

impl Install {
    /// `npm install -g @openai/codex@latest`, for the user to read.
    pub fn command_line(&self) -> Option<String> {
        let (program, args) = self.command.as_ref()?;
        let name = program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Some(
            std::iter::once(name)
                .chain(args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

/// The npm package each harness's CLI is published as (to place a Windows
/// npm install, whose `codex.cmd` sits beside `node_modules` rather than
/// linking into it).
fn known_packages(harness: &str) -> &'static [&'static str] {
    match harness {
        "claude" => &["@anthropic-ai/claude-code"],
        "codex" => &["@openai/codex"],
        "gemini" => &["@google/gemini-cli"],
        "opencode" => &["opencode-ai", "@opencode/cli"],
        _ => &[],
    }
}

/// Where `bin` (the CLI the harness runs) was installed from.
pub(crate) fn locate(harness: &str, bin: &Path, env: &HashMap<String, String>) -> Option<Install> {
    let path_env = env.get("PATH").map(String::as_str).unwrap_or("");
    let home = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .map(PathBuf::from);
    let real = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let parts: Vec<String> = real
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let up_to = |i: usize| -> PathBuf { real.components().take(i).collect() };

    // Claude Code's installer: versions under `~/.local/share/claude`,
    // linked from `~/.local/bin/claude` (copied on Windows).
    let native_link = home.as_ref().is_some_and(|h| {
        let dir = h.join(".local").join("bin");
        bin.parent() == Some(dir.as_path())
            && bin.file_stem().is_some_and(|s| s == "claude")
            && h.join(".local/share/claude").is_dir()
    });
    if harness == "claude"
        && (parts
            .windows(3)
            .any(|w| w[0] == "share" && w[1] == "claude" && w[2] == "versions")
            || native_link)
    {
        let config_dir = env
            .get("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".claude")));
        let channel = config_dir
            .and_then(|d| std::fs::read_to_string(d.join("settings.json")).ok())
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|j| j.get("autoUpdatesChannel")?.as_str().map(str::to_owned))
            .filter(|c| c == "stable" || c == "latest")
            .unwrap_or_else(|| "latest".into());
        return Some(Install {
            source: Source::ClaudeNative { channel },
            command: Some((bin.to_path_buf(), vec!["update".into()])),
        });
    }

    // Homebrew: `<prefix>/Cellar/<name>/…` or `<prefix>/Caskroom/<name>/…`.
    if let Some(i) = parts
        .iter()
        .position(|p| p == "Cellar" || p == "Caskroom")
        .filter(|i| i + 1 < parts.len())
    {
        let cask = parts[i] == "Caskroom";
        let name = parts[i + 1].clone();
        let brew = Some(up_to(i).join("bin").join("brew"))
            .filter(|b| b.is_file())
            .or_else(|| loft_config::which("brew", path_env));
        let mut args = vec!["upgrade".to_owned()];
        if cask {
            args.push("--cask".into());
        }
        args.push(name.clone());
        return Some(Install {
            source: Source::Brew { name, cask },
            command: brew.map(|b| (b, args)),
        });
    }

    // A global package: `…/node_modules/<package>/…`.
    let (package, modules) = match parts.iter().rposition(|p| p == "node_modules") {
        Some(i) if i + 1 < parts.len() => {
            let package = if parts[i + 1].starts_with('@') && i + 2 < parts.len() {
                format!("{}/{}", parts[i + 1], parts[i + 2])
            } else {
                parts[i + 1].clone()
            };
            (package, up_to(i + 1))
        }
        _ => {
            let dir = bin.parent()?;
            let package = known_packages(harness)
                .iter()
                .find(|p| dir.join("node_modules").join(p).is_dir())?;
            ((*package).to_owned(), dir.join("node_modules"))
        }
    };
    let latest = format!("{package}@latest");
    let command = if parts.iter().any(|p| p == "pnpm") {
        loft_config::which("pnpm", path_env).map(|p| (p, vec!["add".into(), "-g".into(), latest]))
    } else if parts.iter().any(|p| p == ".bun") {
        loft_config::which("bun", path_env).map(|p| (p, vec!["add".into(), "-g".into(), latest]))
    } else {
        // The npm of the Node that owns this `node_modules`: `<prefix>/lib/
        // node_modules` with `<prefix>/bin/npm`, or (Windows) `<prefix>/
        // node_modules` with `<prefix>/npm.cmd`.
        let prefix = modules.parent();
        let own = prefix.and_then(|p| {
            let unix = p.parent().map(|pp| pp.join("bin").join("npm"));
            let windows = p.join("npm.cmd");
            [unix.filter(|_| p.ends_with("lib")), Some(windows)]
                .into_iter()
                .flatten()
                .find(|c| c.is_file())
        });
        own.or_else(|| loft_config::which("npm", path_env))
            .map(|p| (p, vec!["install".into(), "-g".into(), latest]))
    };
    Some(Install {
        source: Source::Npm { package },
        command,
    })
}

/// `codex-cli 0.167.0` → `[0, 167, 0]`; `2.1.319 (Claude Code)` →
/// `[2, 1, 319]`. A pre-release suffix is dropped.
fn numbers(v: &str) -> Option<Vec<u64>> {
    let word = v
        .split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let core = word.split(['-', '+', ',']).next()?;
    core.split('.').map(|n| n.parse().ok()).collect()
}

/// `latest` is a later release than `installed` (both as their CLIs or
/// registries write them).
pub(crate) fn is_newer(latest: &str, installed: &str) -> bool {
    let (Some(a), Some(b)) = (numbers(latest), numbers(installed)) else {
        return false;
    };
    let len = a.len().max(b.len());
    let pad = |v: &[u64]| {
        (0..len)
            .map(|i| v.get(i).copied().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    pad(&a) > pad(&b)
}

/// Per release URL: when it was asked, and the answer (`None`: failed).
type Answers = HashMap<String, (Instant, Option<String>)>;

/// The latest release on each channel, asked for at most every few hours.
#[derive(Default, Clone)]
pub(crate) struct Latest {
    answers: Arc<Mutex<Answers>>,
}

impl Latest {
    /// The latest release `source` publishes (blocking: it may fetch).
    pub fn get(&self, source: &Source, fetch: &Fetcher) -> Option<String> {
        let url = source.url();
        let cached = crate::lock(&self.answers).get(&url).cloned();
        if let Some((at, answer)) = cached {
            let fresh = if answer.is_some() {
                FRESH
            } else {
                FAILED_FRESH
            };
            if at.elapsed() < fresh {
                return answer;
            }
        }
        let answer = match fetch(&url) {
            Ok(body) => source.parse(&body),
            Err(err) => {
                tracing::debug!(%err, %url, "couldn't ask for the latest release");
                None
            }
        };
        crate::lock(&self.answers).insert(url, (Instant::now(), answer.clone()));
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_as_the_clis_write_them() {
        assert!(is_newer("0.170.0", "codex-cli 0.169.0"));
        assert!(is_newer("2.1.327", "2.1.326 (Claude Code)"));
        assert!(is_newer("2.3.0", "opencode v2.2.5"));
        assert!(!is_newer("2.1.326", "2.1.326 (Claude Code)"));
        assert!(!is_newer("2.1.311", "2.1.326 (Claude Code)"));
        assert!(!is_newer("0.68.0", "0.69.0-preview.0"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("garbage", "1.0.0"));
    }

    #[test]
    fn reads_each_channel() {
        let tags = r#"{"stable":"2.1.311","latest":"2.1.326"}"#;
        let npm = Source::Npm {
            package: "@anthropic-ai/claude-code".into(),
        };
        assert_eq!(npm.parse(tags).as_deref(), Some("2.1.326"));
        let stable = Source::ClaudeNative {
            channel: "stable".into(),
        };
        assert_eq!(stable.parse(tags).as_deref(), Some("2.1.311"));
        let cask = Source::Brew {
            name: "codex".into(),
            cask: true,
        };
        assert_eq!(
            cask.parse(r#"{"version":"0.169.0,abc"}"#).as_deref(),
            Some("0.169.0")
        );
        assert_eq!(
            cask.url(),
            "https://formulae.brew.sh/api/cask/codex.json".to_owned()
        );
        let formula = Source::Brew {
            name: "gemini-cli".into(),
            cask: false,
        };
        assert_eq!(
            formula
                .parse(r#"{"versions":{"stable":"0.68.0"}}"#)
                .as_deref(),
            Some("0.68.0")
        );
    }

    #[cfg(unix)]
    #[test]
    fn places_installs_by_their_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let link = |target: &Path, at: &Path| {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, at).unwrap();
        };
        let file = |at: &Path| {
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, "").unwrap();
        };
        let env = |home: &Path| {
            HashMap::from([
                ("HOME".to_owned(), home.to_string_lossy().into_owned()),
                ("PATH".to_owned(), String::new()),
            ])
        };

        // npm, with the prefix's own npm.
        let node = root.join("node");
        let js = node.join("lib/node_modules/@openai/codex/bin/codex.js");
        file(&js);
        file(&node.join("bin/npm"));
        link(&js, &node.join("bin/codex"));
        let found = locate("codex", &node.join("bin/codex"), &env(&root)).unwrap();
        assert_eq!(
            found.source,
            Source::Npm {
                package: "@openai/codex".into()
            }
        );
        assert_eq!(found.command.as_ref().unwrap().0, node.join("bin/npm"));
        assert_eq!(
            found.command_line().as_deref(),
            Some("npm install -g @openai/codex@latest")
        );

        // A Homebrew cask.
        let brew = root.join("homebrew");
        let exe = brew.join("Caskroom/codex/0.169.0/codex-aarch64-apple-darwin");
        file(&exe);
        file(&brew.join("bin/brew"));
        link(&exe, &brew.join("bin/codex"));
        let found = locate("codex", &brew.join("bin/codex"), &env(&root)).unwrap();
        assert_eq!(
            found.source,
            Source::Brew {
                name: "codex".into(),
                cask: true
            }
        );
        assert_eq!(
            found.command_line().as_deref(),
            Some("brew upgrade --cask codex")
        );

        // Claude Code's installer, on its stable channel.
        let home = root.join("home");
        let version = home.join(".local/share/claude/versions/2.1.326");
        file(&version);
        link(&version, &home.join(".local/bin/claude"));
        file(&home.join(".claude/settings.json"));
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"autoUpdatesChannel":"stable"}"#,
        )
        .unwrap();
        let bin = home.join(".local/bin/claude");
        let found = locate("claude", &bin, &env(&home)).unwrap();
        assert_eq!(
            found.source,
            Source::ClaudeNative {
                channel: "stable".into()
            }
        );
        assert_eq!(found.command, Some((bin, vec!["update".to_owned()])));

        // Anywhere else: no check.
        let other = root.join("opt/bin/gemini");
        file(&other);
        assert_eq!(locate("gemini", &other, &env(&root)), None);
    }

    #[test]
    fn asks_once_per_channel() {
        let calls = Arc::new(Mutex::new(0));
        let c = calls.clone();
        let fetch: Fetcher = Arc::new(move |_| {
            *c.lock().unwrap() += 1;
            Ok(r#"{"latest":"1.2.3"}"#.into())
        });
        let latest = Latest::default();
        let source = Source::Npm {
            package: "x".into(),
        };
        assert_eq!(latest.get(&source, &fetch).as_deref(), Some("1.2.3"));
        assert_eq!(latest.get(&source, &fetch).as_deref(), Some("1.2.3"));
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}
