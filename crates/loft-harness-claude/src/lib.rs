//! Claude Code harness (ADR-0003): one `claude -p` process per active thread,
//! speaking stream-json with the bidirectional control protocol.

pub mod translate;

use anyhow::{Context as _, Result};
use loft_harness::process::{StderrTail, configure_group, stop_child};
use loft_harness::{
    Capabilities, Harness, HarnessEvent, SessionCommand, SessionConfig, SessionEnds, SessionHandle,
};
use loft_protocol::HarnessId;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use translate::{Out, Translator, permission_mode_arg};

/// Environment variables that make a nested `claude` think it runs inside
/// another Claude Code session.
const SCRUBBED_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SSE_PORT",
];

#[derive(Debug, Default, Clone)]
pub struct ClaudeHarness;

impl Harness for ClaudeHarness {
    fn id(&self) -> HarnessId {
        HarnessId::claude()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            client_session_id: true,
            steer: false,
            live_model_switch: true,
            live_mode_switch: true,
            native_fork: true,
        }
    }

    fn open_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        let binary = resolve_binary(&cfg)?;
        let mut cmd = Command::new(&binary);
        cmd.args(build_args(&cfg))
            .current_dir(&cfg.cwd)
            .env_clear()
            .envs(
                cfg.env
                    .iter()
                    .filter(|(k, _)| !SCRUBBED_ENV.contains(&k.as_str())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_group(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", binary.display()))?;
        tracing::info!(thread = %cfg.thread_label, pid = ?child.id(), "claude session started");

        let stdin = child.stdin.take().context("claude stdin")?;
        let stdout = child.stdout.take().context("claude stdout")?;
        let tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            tail.collect(stderr, cfg.thread_label.clone());
        }

        let (mut handle, ends) = SessionHandle::pair();
        handle.pid = child.id();
        tokio::spawn(async move {
            let events = ends.events.clone();
            match run(ends, stdin, stdout).await {
                RunEnd::Shutdown(stdin) => {
                    drop(stdin);
                    stop_child(&mut child, Duration::from_secs(2), Duration::from_secs(5)).await;
                    tracing::info!("claude session stopped");
                }
                RunEnd::Eof => {
                    let status = child.wait().await.ok();
                    tracing::info!(?status, "claude session exited");
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
    /// Core asked to stop; stdin is handed back so it can be closed first.
    Shutdown(ChildStdin),
    /// The CLI closed stdout (exited or crashed).
    Eof,
}

async fn run(
    mut ends: SessionEnds,
    mut stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
) -> RunEnd {
    let events = ends.events.clone();
    let mut t = Translator::new();
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
                    Some(SessionCommand::SetModel(model)) => t.set_model(model.as_deref()),
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
    line.push(b'\n');
    stdin.write_all(&line).await?;
    stdin.flush().await
}

pub fn build_args(cfg: &SessionConfig) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--input-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(id) = &cfg.resume {
        args.extend(["--resume".into(), id.clone()]);
    } else {
        // A fork resumes the parent into a new session (with our id).
        if let Some(parent) = &cfg.fork_from {
            args.extend(["--resume".into(), parent.clone(), "--fork-session".into()]);
        }
        if let Some(id) = &cfg.new_session_id {
            args.extend(["--session-id".into(), id.clone()]);
        }
    }
    if let Some(model) = &cfg.model {
        args.extend(["--model".into(), model.clone()]);
    }
    // Only when the user chose one; otherwise Claude's own default applies.
    if let Some(mode) = cfg.permission_mode {
        args.extend(["--permission-mode".into(), permission_mode_arg(mode).into()]);
    }
    if !cfg.mcp_servers.is_empty() {
        let servers: serde_json::Map<String, serde_json::Value> = cfg
            .mcp_servers
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    serde_json::json!({"type": "stdio", "command": s.command, "args": s.args}),
                )
            })
            .collect();
        let config = serde_json::json!({ "mcpServers": servers });
        args.extend(["--mcp-config".into(), config.to_string()]);
    }
    if !cfg.allowed_tools.is_empty() {
        args.extend(["--allowedTools".into(), cfg.allowed_tools.join(",")]);
    }
    args.extend(cfg.extra_args.iter().cloned());
    args
}

fn resolve_binary(cfg: &SessionConfig) -> Result<PathBuf> {
    if let Some(bin) = &cfg.binary {
        return Ok(bin.clone());
    }
    let path = cfg.env.get("PATH").map(String::as_str).unwrap_or_default();
    loft_config::which("claude", path)
        .context("`claude` not found on PATH — install Claude Code or set harness.claude.path")
}

/// Generate a fresh session id for a new thread (Claude requires a UUID).
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use loft_protocol::PermissionMode;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn cfg() -> SessionConfig {
        SessionConfig {
            thread_label: "t".into(),
            cwd: ".".into(),
            model: None,
            permission_mode: None,
            resume: None,
            new_session_id: Some("abc".into()),
            fork_from: None,
            env: Arc::new(HashMap::new()),
            binary: None,
            extra_args: vec![],
            mcp_servers: vec![],
            allowed_tools: vec![],
        }
    }

    #[test]
    fn mcp_servers_become_inline_config() {
        let mut c = cfg();
        c.mcp_servers.push(loft_harness::McpServer {
            name: "agentloft".into(),
            command: "/bin/agentloftd".into(),
            args: vec!["mcp".into()],
        });
        c.allowed_tools.push("mcp__agentloft".into());
        let args = build_args(&c);
        let ix = args.iter().position(|a| a == "--mcp-config").unwrap();
        let v: serde_json::Value = serde_json::from_str(&args[ix + 1]).unwrap();
        assert_eq!(v["mcpServers"]["agentloft"]["command"], "/bin/agentloftd");
        assert!(
            args.windows(2)
                .any(|w| w == ["--allowedTools", "mcp__agentloft"])
        );
    }

    #[test]
    fn default_args_omit_mode_and_model() {
        let args = build_args(&cfg());
        assert!(args.contains(&"--session-id".to_owned()));
        assert!(!args.contains(&"--permission-mode".to_owned()));
        assert!(!args.contains(&"--model".to_owned()));
    }

    #[test]
    fn fork_resumes_parent_into_new_session() {
        let mut c = cfg();
        c.fork_from = Some("parent".into());
        let args = build_args(&c);
        let joined = args.join(" ");
        assert!(
            joined.contains("--resume parent --fork-session --session-id abc"),
            "{joined}"
        );
    }

    #[test]
    fn resume_wins_over_new_id() {
        let mut c = cfg();
        c.resume = Some("old".into());
        c.permission_mode = Some(PermissionMode::AcceptEdits);
        let args = build_args(&c);
        assert!(args.windows(2).any(|w| w == ["--resume", "old"]));
        assert!(!args.contains(&"--session-id".to_owned()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "acceptEdits"])
        );
    }
}
