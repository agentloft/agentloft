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
            steer: true,
            live_model_switch: true,
            live_mode_switch: true,
            // `apply_flag_settings` can't express `max`; restart instead.
            live_effort_switch: false,
            native_fork: true,
            compact: true,
        }
    }

    fn list_models(
        &self,
        env: &std::collections::HashMap<String, String>,
        binary: Option<PathBuf>,
    ) -> Result<Vec<loft_protocol::ModelInfo>> {
        let binary = match binary {
            Some(b) => b,
            None => loft_config::which("claude", env.get("PATH").map(String::as_str).unwrap_or(""))
                .context("`claude` not found on PATH")?,
        };
        probe_models(&binary, env).or_else(|err| {
            tracing::warn!("claude model probe failed, using aliases: {err:#}");
            Ok(alias_models())
        })
    }

    fn list_commands(
        &self,
        env: &std::collections::HashMap<String, String>,
        binary: Option<PathBuf>,
        cwd: &str,
    ) -> Result<Vec<loft_protocol::SlashCommandInfo>> {
        let binary = match binary {
            Some(b) => b,
            None => loft_config::which("claude", env.get("PATH").map(String::as_str).unwrap_or(""))
                .context("`claude` not found on PATH")?,
        };
        let v = probe_initialize(&binary, env, std::path::Path::new(cwd))?;
        Ok(translate::commands(&v))
    }

    fn plan_limits(
        &self,
        env: &std::collections::HashMap<String, String>,
        binary: Option<PathBuf>,
    ) -> Result<Option<serde_json::Value>> {
        let binary = match binary {
            Some(b) => b,
            None => loft_config::which("claude", env.get("PATH").map(String::as_str).unwrap_or(""))
                .context("`claude` not found on PATH")?,
        };
        probe_plan_limits(&binary, env).map(Some)
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
    let mut waiting: std::collections::HashMap<
        String,
        tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
    > = std::collections::HashMap::new();
    let mut lines = BufReader::new(stdout).lines();

    let init = t.initialize();
    let cost = t.ask_cost_so_far();
    if write_json(&mut stdin, &init).await.is_err() || write_json(&mut stdin, &cost).await.is_err()
    {
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
                        let images = read_images(&input.images).await;
                        t.start_turn_with(&input.text, &images)
                    }
                    Some(SessionCommand::Steer(input)) => {
                        let images = read_images(&input.images).await;
                        t.steer_with(&input.text, &images)
                    }
                    Some(SessionCommand::Interrupt) => t.interrupt(),
                    Some(SessionCommand::StopSubagent(native)) => t.stop_subagent(&native),
                    Some(SessionCommand::Query { kind, reply }) => {
                        let (id, v) = t.query(kind);
                        waiting.insert(id, reply);
                        vec![translate::Out::Send(v)]
                    }
                    Some(SessionCommand::Respond { native_request_id, decision, reply }) => {
                        t.respond(&native_request_id, decision, reply)
                    }
                    Some(SessionCommand::SetModel(model)) => t.set_model(model.as_deref()),
                    // The core restarts the session with `--effort` instead.
                    Some(SessionCommand::SetEffort(_)) => vec![],
                    Some(SessionCommand::SetPermissionMode(mode)) => t.set_permission_mode(mode),
                    Some(SessionCommand::Shutdown) | None => return RunEnd::Shutdown(stdin),
                };
                if dispatch(&events, &mut stdin, &mut waiting, outs).await.is_err() {
                    return RunEnd::Eof;
                }
            }
            // A turn's end is waiting for an answer Claude never gave.
            _ = tokio::time::sleep(std::time::Duration::from_secs(3)), if t.holding_result() => {
                if dispatch(&events, &mut stdin, &mut waiting, t.release_result()).await.is_err() {
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
        String,
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
            Out::Reply { request_id, result } => {
                if let Some(reply) = waiting.remove(&request_id) {
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

/// Attached images as `(mime, base64)` for stream-json image blocks.
async fn read_images(images: &[loft_harness::ImageInput]) -> Vec<(String, String)> {
    use base64::Engine as _;
    let mut out = Vec::new();
    for image in images {
        match tokio::fs::read(&image.path).await {
            Ok(bytes) => out.push((
                image.mime.clone(),
                base64::engine::general_purpose::STANDARD.encode(bytes),
            )),
            Err(err) => {
                tracing::warn!(path = %image.path.display(), %err, "attachment unreadable")
            }
        }
    }
    out
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
        // Echo our messages so a mid-turn one can be told apart from the
        // end of the turn.
        "--replay-user-messages",
        "--permission-prompt-tool",
        "stdio",
        // Full access can be picked mid-session (`set_permission_mode`
        // to bypassPermissions is refused otherwise). It only makes the
        // mode available; the session starts in `--permission-mode`.
        "--allow-dangerously-skip-permissions",
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
            if let Some(at) = &cfg.fork_at {
                args.extend(["--resume-session-at".into(), at.clone()]);
            }
        }
        if let Some(id) = &cfg.new_session_id {
            args.extend(["--session-id".into(), id.clone()]);
        }
    }
    if let Some(model) = &cfg.model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(effort) = &cfg.effort {
        args.extend(["--effort".into(), effort.clone()]);
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

/// The family aliases, for when the CLI can't be asked.
fn alias_models() -> Vec<loft_protocol::ModelInfo> {
    [
        ("opus", "Opus", "Most capable, for complex work"),
        ("sonnet", "Sonnet", "Fast and capable, for everyday work"),
        ("haiku", "Haiku", "Fastest, for simple tasks"),
    ]
    .into_iter()
    .map(|(id, name, description)| loft_protocol::ModelInfo {
        id: id.into(),
        display_name: name.into(),
        description: Some(description.into()),
        ..Default::default()
    })
    .collect()
}

/// Ask a short-lived `claude` for its model catalog: the `initialize`
/// control response lists each model with its effort levels. No prompt is
/// sent, so this costs no tokens; hooks are off so the user's session hooks
/// don't fire for it.
fn probe_models(
    binary: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<Vec<loft_protocol::ModelInfo>> {
    // An empty folder of the user's own, not the shared `/tmp` (on Linux).
    let v = probe_initialize(
        binary,
        env,
        &loft_config::private_temp_dir("agentloft-ask")?,
    )?;
    let models = translate::models(&v);
    anyhow::ensure!(!models.is_empty(), "no models in the initialize response");
    Ok(models)
}

/// Whether an administrator's `managed-mcp.json` exists: Claude exits before
/// initializing when `--strict-mcp-config` is passed alongside it.
fn has_managed_mcp_config(env: &std::collections::HashMap<String, String>) -> bool {
    #[cfg(target_os = "macos")]
    {
        let _ = env;
        std::path::Path::new("/Library/Application Support/ClaudeCode/managed-mcp.json").exists()
    }
    #[cfg(target_os = "linux")]
    {
        let _ = env;
        std::path::Path::new("/etc/claude-code/managed-mcp.json").exists()
    }
    #[cfg(windows)]
    {
        env.get("ProgramData")
            .or_else(|| env.get("PROGRAMDATA"))
            .is_some_and(|dir| {
                std::path::Path::new(dir)
                    .join("ClaudeCode")
                    .join("managed-mcp.json")
                    .exists()
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = env;
        false
    }
}

/// Spawn a lightweight, short-lived `claude` in `cwd` with hooks, session
/// persistence, IDE auto-connect and MCP servers turned off.
fn spawn_probe(
    binary: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    cwd: &std::path::Path,
    extra_args: &[&str],
) -> Result<std::process::Child> {
    let mut cmd = std::process::Command::new(binary);
    cmd.args([
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--no-session-persistence",
        "--settings",
        r#"{"disableAllHooks":true}"#,
    ])
    .args(extra_args);
    if !has_managed_mcp_config(env) {
        cmd.args([
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
        ]);
    }
    cmd.current_dir(cwd)
        .env_clear()
        .envs(
            env.iter()
                .filter(|(k, _)| !SCRUBBED_ENV.contains(&k.as_str())),
        )
        .env("ENABLE_CLAUDEAI_MCP_SERVERS", "false")
        .env("CLAUDE_CODE_AUTO_CONNECT_IDE", "0")
        .env("CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn()
        .with_context(|| format!("spawn {}", binary.display()))
}

fn kill_probe(mut child: std::process::Child) {
    let pid = child.id();
    loft_harness::process::signal_group(pid, loft_harness::process::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
}

/// Ask a short-lived `claude` for its account plan limits (`get_usage` with
/// `skip_behaviors: true`, so it skips scanning local transcripts, never
/// starts MCP servers or hooks, and never resumes a session).
fn probe_plan_limits(
    binary: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
) -> Result<serde_json::Value> {
    use std::io::{BufRead as _, Write as _};
    let cwd = loft_config::private_temp_dir("agentloft-ask")?;
    let mut child = spawn_probe(binary, env, &cwd, &["--disable-slash-commands"])?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
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
    let result = (|| {
        let mut t = Translator::new();
        let init = t.initialize();
        let usage_id = "loft_usage";
        let usage = serde_json::json!({
            "type": "control_request",
            "request_id": usage_id,
            "request": {"subtype": "get_usage", "skip_behaviors": true},
        });
        writeln!(stdin, "{init}")?;
        writeln!(stdin, "{usage}")?;
        stdin.flush()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let v = rx.recv_timeout(left).context("claude didn't answer")?;
            if v["type"] == "control_response"
                && v.pointer("/response/request_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(usage_id)
            {
                if v.pointer("/response/subtype")
                    .and_then(serde_json::Value::as_str)
                    == Some("error")
                {
                    let msg = v
                        .pointer("/response/error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Claude couldn't report plan limits");
                    anyhow::bail!("{msg}");
                }
                return Ok(v
                    .pointer("/response/response")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        }
    })();
    drop(stdin);
    kill_probe(child);
    result
}

/// Claude's answer to `initialize` (models, commands, …), from a
/// short-lived CLI started in `cwd`.
fn probe_initialize(
    binary: &std::path::Path,
    env: &std::collections::HashMap<String, String>,
    cwd: &std::path::Path,
) -> Result<serde_json::Value> {
    use std::io::{BufRead as _, Write as _};
    let mut child = spawn_probe(binary, env, cwd, &[])?;
    let mut stdin = child.stdin.take().context("stdin")?;
    let stdout = child.stdout.take().context("stdout")?;
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
    let result = (|| {
        let init = Translator::new().initialize();
        writeln!(stdin, "{init}")?;
        stdin.flush()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let v = rx.recv_timeout(left).context("claude didn't answer")?;
            if v["type"] == "control_response" {
                return Ok(v);
            }
        }
    })();
    drop(stdin);
    kill_probe(child);
    result
}

#[cfg(test)]
mod tests {
    /// Talks to the real `claude` (run with `--ignored`).
    #[test]
    #[ignore]
    fn lists_commands_without_a_session() {
        use loft_harness::Harness as _;
        let env: HashMap<String, String> = std::env::vars().collect();
        let cmds = ClaudeHarness
            .list_commands(&env, None, &std::env::temp_dir().to_string_lossy())
            .unwrap();
        assert!(
            cmds.iter().any(|c| c.name == "compact"),
            "{} commands",
            cmds.len()
        );
    }

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
            effort: None,
            resume: None,
            new_session_id: Some("abc".into()),
            fork_from: None,
            fork_at: None,
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
        // Full access stays available to switch to later.
        assert!(args.contains(&"--allow-dangerously-skip-permissions".to_owned()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(args.contains(&"--session-id".to_owned()));
        assert!(!args.contains(&"--permission-mode".to_owned()));
        assert!(!args.contains(&"--model".to_owned()));
        assert!(!args.contains(&"--effort".to_owned()));
    }

    #[test]
    fn fork_from_an_earlier_turn() {
        let mut c = cfg();
        c.new_session_id = None;
        c.fork_from = Some("parent".into());
        c.fork_at = Some("msg-uuid".into());
        let joined = build_args(&c).join(" ");
        assert!(
            joined.contains("--resume parent --fork-session --resume-session-at msg-uuid"),
            "{joined}"
        );
    }

    #[test]
    fn effort_is_passed_at_launch() {
        let mut c = cfg();
        c.effort = Some("max".into());
        assert!(build_args(&c).windows(2).any(|w| w == ["--effort", "max"]));
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

    #[cfg(unix)]
    #[test]
    fn plan_limits_probe_skips_behaviors_and_reads_rate_limits() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = loft_config::private_temp_dir(&format!(
            "agentloft-claude-limits-{}",
            std::process::id()
        ))
        .unwrap();
        let req_log = dir.join("requests.jsonl");
        let fake = dir.join("claude");
        std::fs::write(
            &fake,
            format!(
                r#"#!/bin/sh
read -r init
printf '%s\n' "$init" >> '{}'
printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"loft_1","response":{{}}}}}}'
read -r usage
printf '%s\n' "$usage" >> '{}'
printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"loft_usage","response":{{"subscription_type":"max","rate_limits":{{"limits":[{{"kind":"five_hour","group":"five_hour","percent":42.0}}]}}}}}}}}'
"#,
                req_log.display(),
                req_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: HashMap<String, String> = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let info = ClaudeHarness
            .plan_limits(&env, Some(fake))
            .unwrap()
            .expect("claude reports limits");
        assert_eq!(info["subscription_type"], "max");
        assert_eq!(info["rate_limits"]["limits"][0]["percent"], 42.0);
        let sent = std::fs::read_to_string(&req_log).unwrap();
        assert!(sent.contains(r#""skip_behaviors":true"#), "{sent}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
