//! ACP harnesses (PLAN §2.6, M6): agents that speak the Agent Client Protocol
//! (JSON-RPC 2.0 over newline-delimited stdio) — OpenCode (`opencode acp`),
//! Gemini CLI (`gemini --acp`) and the long tail. One agent process per active
//! thread. The Agentloft MCP server is attached through `session/new`'s
//! `mcpServers`.
//!
//! The client advertises no `fs/*` or `terminal/*` capabilities, so agents use
//! their own file and shell tools; Agentloft only renders and approves them.

pub mod translate;

use anyhow::{Context as _, Result};
use loft_harness::process::{StderrTail, configure_group, stop_child};
use loft_harness::{
    Capabilities, Harness, HarnessEvent, SessionCommand, SessionConfig, SessionEnds, SessionHandle,
};
use loft_protocol::HarnessId;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use translate::{Out, Translator};

/// One ACP agent preset: which binary to run with which arguments.
#[derive(Debug)]
pub struct AcpHarness {
    /// Harness id (`opencode`, `gemini`, …).
    pub id: String,
    /// Human name for messages ("OpenCode").
    pub display: String,
    /// Executable looked up on `PATH` unless the config overrides it.
    pub binary_name: String,
    /// Arguments that start the agent's ACP server on stdio.
    pub args: Vec<String>,
    /// Used instead of `args` when `<binary> --help` doesn't mention
    /// `args[0]` (older CLIs that only know an experimental flag).
    pub legacy_args: Option<Vec<String>>,
    resolved_args: OnceLock<Vec<String>>,
}

impl AcpHarness {
    pub fn new(
        id: impl Into<String>,
        display: impl Into<String>,
        binary_name: impl Into<String>,
        args: Vec<String>,
    ) -> Self {
        Self {
            id: id.into(),
            display: display.into(),
            binary_name: binary_name.into(),
            args,
            legacy_args: None,
            resolved_args: OnceLock::new(),
        }
    }

    /// OpenCode: `opencode acp` (verified against 2.2.5).
    pub fn opencode() -> Self {
        Self::new(
            HarnessId::OPENCODE,
            "OpenCode",
            "opencode",
            vec!["acp".into()],
        )
    }

    /// Gemini CLI: `gemini --acp`, or `--experimental-acp` on versions whose
    /// `--help` doesn't list `--acp` yet.
    pub fn gemini() -> Self {
        Self {
            legacy_args: Some(vec!["--experimental-acp".into()]),
            ..Self::new(HarnessId::GEMINI, "Gemini", "gemini", vec!["--acp".into()])
        }
    }

    fn resolve_binary(&self, cfg: &SessionConfig) -> Result<PathBuf> {
        if let Some(bin) = &cfg.binary {
            return Ok(bin.clone());
        }
        let path = cfg.env.get("PATH").map(String::as_str).unwrap_or_default();
        loft_config::which(&self.binary_name, path).with_context(|| {
            format!(
                "`{}` not found on PATH — install {} or set its binary path",
                self.binary_name, self.display
            )
        })
    }

    /// `args`, or `legacy_args` when the installed CLI predates `args[0]`.
    /// Probed once per daemon run.
    fn agent_args(&self, binary: &Path, cfg: &SessionConfig) -> Vec<String> {
        let Some(legacy) = &self.legacy_args else {
            return self.args.clone();
        };
        self.resolved_args
            .get_or_init(|| {
                let flag = self.args.first().cloned().unwrap_or_default();
                match help_text(binary, cfg) {
                    Some(help) if mentions_flag(&help, &flag) => self.args.clone(),
                    Some(_) => legacy.clone(),
                    // Can't tell: use the current spelling.
                    None => self.args.clone(),
                }
            })
            .clone()
    }

    pub fn build_args(&self, binary: &Path, cfg: &SessionConfig) -> Vec<String> {
        let mut args = self.agent_args(binary, cfg);
        args.extend(cfg.extra_args.iter().cloned());
        args
    }
}

/// `<binary> --help` output (stdout + stderr), bounded by a timeout.
fn help_text(binary: &Path, cfg: &SessionConfig) -> Option<String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--help")
        .env_clear()
        .envs(cfg.env.iter())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(cmd.output());
    });
    let output = rx.recv_timeout(Duration::from_secs(15)).ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some(text)
}

/// Whether help text lists `flag` as its own option (`--acp`, not
/// `--experimental-acp`).
fn mentions_flag(help: &str, flag: &str) -> bool {
    if flag.is_empty() {
        return false;
    }
    help.match_indices(flag).any(|(i, _)| {
        let before = help[..i].chars().next_back();
        let after = help[i + flag.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '-')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '-')
    })
}

impl Harness for AcpHarness {
    fn id(&self) -> HarnessId {
        HarnessId(self.id.clone())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            client_session_id: false,
            // ACP has no steering; `session/fork` is still unstable.
            steer: false,
            native_fork: false,
            // Only some agents advertise model/mode selectors (OpenCode does,
            // via `configOptions`); the session applies them when offered and
            // posts a notice otherwise, but the UI can't rely on them.
            live_model_switch: false,
            live_mode_switch: false,
        }
    }

    fn open_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        let binary = self.resolve_binary(&cfg)?;
        let mut cmd = Command::new(&binary);
        cmd.args(self.build_args(&binary, &cfg))
            .current_dir(&cfg.cwd)
            .env_clear()
            .envs(cfg.env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_group(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", binary.display()))?;
        tracing::info!(thread = %cfg.thread_label, agent = %self.id, pid = ?child.id(), "ACP agent started");
        let stdin = child.stdin.take().context("ACP agent stdin")?;
        let stdout = child.stdout.take().context("ACP agent stdout")?;
        let tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            tail.collect(stderr, cfg.thread_label.clone());
        }
        let mut translator = Translator::new(
            self.display.clone(),
            cfg.cwd.to_string_lossy().into_owned(),
            cfg.model.clone(),
            cfg.permission_mode,
            cfg.resume.clone(),
            cfg.mcp_servers.clone(),
        );
        // `mcp__agentloft` in allowed_tools → auto-approve that server's tools.
        translator.auto_allow_mcp = cfg
            .allowed_tools
            .iter()
            .filter_map(|t| t.strip_prefix("mcp__"))
            .map(|s| s.split("__").next().unwrap_or(s).to_owned())
            .collect();
        let (mut handle, ends) = SessionHandle::pair();
        handle.pid = child.id();
        let agent = self.id.clone();
        tokio::spawn(async move {
            let events = ends.events.clone();
            match run(ends, translator, stdin, stdout).await {
                RunEnd::Shutdown(stdin) => {
                    drop(stdin);
                    stop_child(&mut child, Duration::from_secs(2), Duration::from_secs(5)).await;
                    tracing::info!(%agent, "ACP agent stopped");
                }
                RunEnd::Eof => {
                    let status = child.wait().await.ok();
                    let _ = events.send(HarnessEvent::Exited {
                        code: status.and_then(|s| s.code()),
                        stderr_tail: tail.snapshot(),
                    });
                }
            }
        });
        Ok(handle)
    }
}

enum RunEnd {
    Shutdown(ChildStdin),
    Eof,
}

async fn run(
    mut ends: SessionEnds,
    mut t: Translator,
    mut stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> RunEnd {
    let events = ends.events.clone();
    let mut lines = BufReader::new(stdout).lines();
    let init = t.initialize();
    if write_json(&mut stdin, &init).await.is_err() {
        return RunEnd::Eof;
    }
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => return RunEnd::Eof,
                };
                if line.trim().is_empty() {
                    continue;
                }
                tracing::trace!(target: "harness.acp", "<< {line}");
                if dispatch(&events, &mut stdin, t.on_line(&line)).await.is_err() {
                    return RunEnd::Eof;
                }
            }
            cmd = ends.commands.recv() => {
                let outs = match cmd {
                    Some(SessionCommand::StartTurn(input)) => t.start_turn(&input.text),
                    Some(SessionCommand::Interrupt) => t.interrupt(),
                    Some(SessionCommand::Respond { native_request_id, decision }) => {
                        t.respond(&native_request_id, decision)
                    }
                    Some(SessionCommand::SetModel(model)) => t.set_model(model),
                    Some(SessionCommand::SetPermissionMode(mode)) => t.set_permission_mode(mode),
                    Some(SessionCommand::Shutdown) | None => return RunEnd::Shutdown(stdin),
                };
                if dispatch(&events, &mut stdin, outs).await.is_err() {
                    return RunEnd::Eof;
                }
            }
        }
    }
}

async fn dispatch(
    events: &tokio::sync::mpsc::UnboundedSender<HarnessEvent>,
    stdin: &mut ChildStdin,
    outs: Vec<Out>,
) -> std::io::Result<()> {
    for out in outs {
        match out {
            Out::Event(e) => {
                let _ = events.send(e);
            }
            Out::Send(v) => write_json(stdin, &v).await?,
        }
    }
    Ok(())
}

async fn write_json(stdin: &mut ChildStdin, v: &serde_json::Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(v).map_err(std::io::Error::other)?;
    tracing::trace!(target: "harness.acp", ">> {}", String::from_utf8_lossy(&line));
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn cfg(extra: Vec<String>) -> SessionConfig {
        SessionConfig {
            thread_label: "t".into(),
            cwd: ".".into(),
            model: None,
            permission_mode: None,
            resume: None,
            new_session_id: None,
            env: Arc::new(HashMap::new()),
            binary: None,
            extra_args: extra,
            mcp_servers: vec![],
            allowed_tools: vec![],
        }
    }

    #[test]
    fn presets() {
        let oc = AcpHarness::opencode();
        assert_eq!(oc.id().as_str(), "opencode");
        let args = oc.build_args(Path::new("/nonexistent/opencode"), &cfg(vec!["-x".into()]));
        assert_eq!(args, vec!["acp", "-x"]);
        let gem = AcpHarness::gemini();
        assert_eq!(gem.id().as_str(), "gemini");
        assert!(!gem.capabilities().steer);
        // No runnable binary to probe: the current flag spelling is used.
        let args = gem.build_args(Path::new("/nonexistent/gemini"), &cfg(vec![]));
        assert_eq!(args, vec!["--acp"]);
    }

    #[test]
    fn help_flag_detection() {
        let new = "Options:\n  --acp                 Starts the agent in ACP mode\n  --experimental-acp    Deprecated";
        let old = "Options:\n  --experimental-acp    Starts the agent in ACP mode";
        assert!(mentions_flag(new, "--acp"));
        assert!(!mentions_flag(old, "--acp"));
        assert!(mentions_flag(old, "--experimental-acp"));
        assert!(!mentions_flag("--acpx", "--acp"));
    }
}
