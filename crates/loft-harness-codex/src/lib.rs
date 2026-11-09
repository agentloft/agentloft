//! Codex harness (ADR-0004): one `codex app-server` child per active thread,
//! JSON-RPC over stdio. The Agentloft MCP server is attached through
//! `-c mcp_servers.agentloft.*` overrides.

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
use translate::{Out, Translator};

#[derive(Debug, Default, Clone)]
pub struct CodexHarness;

impl Harness for CodexHarness {
    fn id(&self) -> HarnessId {
        HarnessId(HarnessId::CODEX.into())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            client_session_id: false,
            steer: true,
            live_model_switch: true,
            live_mode_switch: true,
            live_effort_switch: true,
            native_fork: true,
        }
    }

    fn list_models(
        &self,
        env: &std::collections::HashMap<String, String>,
        binary: Option<PathBuf>,
    ) -> Result<Vec<loft_protocol::ModelInfo>> {
        let binary = match binary {
            Some(b) => b,
            None => loft_config::which("codex", env.get("PATH").map(String::as_str).unwrap_or(""))
                .context("`codex` not found on PATH")?,
        };
        probe_models(&binary, env)
    }

    fn open_session(&self, cfg: SessionConfig) -> Result<SessionHandle> {
        let binary = resolve_binary(&cfg)?;
        let mut cmd = Command::new(&binary);
        cmd.args(build_args(&cfg))
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
        tracing::info!(thread = %cfg.thread_label, pid = ?child.id(), "codex app-server started");
        let stdin = child.stdin.take().context("codex stdin")?;
        let stdout = child.stdout.take().context("codex stdout")?;
        let tail = StderrTail::default();
        if let Some(stderr) = child.stderr.take() {
            tail.collect(stderr, cfg.thread_label.clone());
        }
        let mut translator = Translator::new(
            cfg.cwd.to_string_lossy().into_owned(),
            cfg.model.clone(),
            cfg.permission_mode,
            cfg.resume.clone(),
        );
        translator.effort = cfg.effort.clone();
        // `mcp__agentloft` in allowed_tools → auto-approve that server's tools.
        translator.fork_from = cfg.fork_from.clone();
        translator.fork_at = cfg.fork_at.clone();
        translator.auto_allow_mcp = cfg
            .allowed_tools
            .iter()
            .filter_map(|t| t.strip_prefix("mcp__"))
            .map(|s| s.split("__").next().unwrap_or(s).to_owned())
            .collect();
        let (mut handle, ends) = SessionHandle::pair();
        handle.pid = child.id();
        tokio::spawn(async move {
            let events = ends.events.clone();
            match run(ends, translator, stdin, stdout).await {
                RunEnd::Shutdown(stdin) => {
                    drop(stdin);
                    stop_child(&mut child, Duration::from_secs(2), Duration::from_secs(5)).await;
                    tracing::info!("codex app-server stopped");
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
    let mut waiting: std::collections::HashMap<
        u64,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    > = std::collections::HashMap::new();
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
                if dispatch(&events, &mut stdin, &mut waiting, t.on_line(&line)).await.is_err() {
                    return RunEnd::Eof;
                }
            }
            cmd = ends.commands.recv() => {
                let outs = match cmd {
                    Some(SessionCommand::StartTurn(input)) => {
                        let images: Vec<String> = input
                            .images
                            .iter()
                            .map(|i| i.path.to_string_lossy().into_owned())
                            .collect();
                        t.start_turn_with(&input.text, &images)
                    }
                    Some(SessionCommand::Interrupt) => t.interrupt(),
                    Some(SessionCommand::StopSubagent(_)) => vec![],
                    Some(SessionCommand::Query { reply, .. }) => {
                        let (query, outs) = t.query();
                        waiting.insert(query, reply);
                        outs
                    }
                    Some(SessionCommand::Respond { native_request_id, decision }) => {
                        t.respond(&native_request_id, decision)
                    }
                    Some(SessionCommand::SetModel(model)) => t.set_model(model),
                    Some(SessionCommand::SetEffort(effort)) => {
                        t.effort = effort; // applied on the next turn/start
                        vec![]
                    }
                    Some(SessionCommand::SetPermissionMode(mode)) => t.set_permission_mode(mode),
                    Some(SessionCommand::Shutdown) | None => return RunEnd::Shutdown(stdin),
                };
                if dispatch(&events, &mut stdin, &mut waiting, outs).await.is_err() {
                    return RunEnd::Eof;
                }
            }
        }
    }
}

async fn dispatch(
    events: &tokio::sync::mpsc::UnboundedSender<HarnessEvent>,
    stdin: &mut ChildStdin,
    waiting: &mut std::collections::HashMap<
        u64,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    >,
    outs: Vec<Out>,
) -> std::io::Result<()> {
    for out in outs {
        match out {
            Out::Event(e) => {
                let _ = events.send(e);
            }
            Out::Send(v) => write_json(stdin, &v).await?,
            Out::Reply { query, result } => {
                if let Some(reply) = waiting.remove(&query) {
                    let _ = reply.send(result);
                }
            }
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

/// Ask a short-lived `codex app-server` for `model/list`.
fn probe_models(
    binary: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<Vec<loft_protocol::ModelInfo>> {
    use std::io::{BufRead as _, Write as _};
    let mut child = std::process::Command::new(binary)
        .arg("app-server")
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
    // Read on a thread so a stuck CLI can't hang us past the deadline.
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if let Ok(v) = serde_json::from_str(&line)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let wait_for = |id: u64| -> Result<serde_json::Value> {
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let v = rx
                .recv_timeout(left)
                .context("codex app-server didn't answer")?;
            if v["id"] == id {
                return Ok(v);
            }
        }
    };
    let mut send = |v: serde_json::Value| -> Result<()> {
        writeln!(stdin, "{v}")?;
        Ok(stdin.flush()?)
    };
    let result = (|| {
        send(
            serde_json::json!({"id": 1, "method": "initialize", "params": {
                "clientInfo": {"name": "agentloft", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }}),
        )?;
        wait_for(1)?;
        send(serde_json::json!({"method": "initialized"}))?;
        send(serde_json::json!({"id": 2, "method": "model/list", "params": {}}))?;
        let reply = wait_for(2)?;
        if let Some(err) = reply.get("error") {
            anyhow::bail!("model/list: {err}");
        }
        Ok(translate::models(&reply["result"]))
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// `codex app-server` plus config overrides attaching MCP servers.
pub fn build_args(cfg: &SessionConfig) -> Vec<String> {
    let mut args = vec!["app-server".to_owned()];
    for server in &cfg.mcp_servers {
        // Values are parsed as TOML; JSON strings/arrays are valid TOML.
        let key = format!("mcp_servers.{}", server.name);
        args.push("-c".into());
        args.push(format!(
            "{key}.command={}",
            serde_json::to_string(&server.command).unwrap_or_default()
        ));
        args.push("-c".into());
        args.push(format!(
            "{key}.args={}",
            serde_json::to_string(&server.args).unwrap_or_default()
        ));
    }
    args.extend(cfg.extra_args.iter().cloned());
    args
}

fn resolve_binary(cfg: &SessionConfig) -> Result<PathBuf> {
    if let Some(bin) = &cfg.binary {
        return Ok(bin.clone());
    }
    let path = cfg.env.get("PATH").map(String::as_str).unwrap_or_default();
    loft_config::which("codex", path)
        .context("`codex` not found on PATH — install the Codex CLI or set harness.codex.path")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn mcp_servers_become_config_overrides() {
        let cfg = SessionConfig {
            thread_label: "t".into(),
            cwd: ".".into(),
            model: None,
            permission_mode: None,
            effort: None,
            resume: None,
            new_session_id: None,
            fork_from: None,
            fork_at: None,
            env: Arc::new(HashMap::new()),
            binary: None,
            extra_args: vec![],
            mcp_servers: vec![loft_harness::McpServer {
                name: "agentloft".into(),
                command: "/Apps/Agent loft/agentloftd".into(),
                args: vec!["mcp".into(), "--thread".into(), "T".into()],
            }],
            allowed_tools: vec![],
        };
        let args = build_args(&cfg);
        assert_eq!(args[0], "app-server");
        assert!(args.contains(
            &r#"mcp_servers.agentloft.command="/Apps/Agent loft/agentloftd""#.to_owned()
        ));
        assert!(args.contains(&r#"mcp_servers.agentloft.args=["mcp","--thread","T"]"#.to_owned()));
    }
}
