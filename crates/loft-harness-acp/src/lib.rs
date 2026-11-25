//! ACP harnesses (PLAN §2.6, M6): agents that speak the Agent Client Protocol
//! (JSON-RPC 2.0 over newline-delimited stdio) — OpenCode (`opencode acp`),
//! Gemini CLI (`gemini --acp`), Antigravity (`agy acp`) and Kiro (`kiro-cli acp`).
//! One agent process per active thread, with the Agentloft MCP server attached
//! through the session's `mcpServers`.
//!
//! The client advertises no `fs/*` or `terminal/*` capabilities, so agents use
//! their own file and shell tools; Agentloft only renders and approves them.

pub mod translate;

use anyhow::{Context as _, Result};
use loft_harness::process::{StderrTail, configure_group, stop_child};
use loft_harness::{
    Capabilities, Harness, HarnessEvent, SessionCommand, SessionConfig, SessionEnds, SessionHandle,
};
use loft_protocol::{HarnessId, PermissionMode};
use std::collections::HashMap;
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
    auth_hint: Option<&'static str>,
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
            auth_hint: None,
            resolved_args: OnceLock::new(),
        }
    }

    /// OpenCode: `opencode acp` (verified against 2.3.0).
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

    /// Google Antigravity CLI 1.5 and newer.
    pub fn antigravity() -> Self {
        Self {
            auth_hint: Some("Run `agy acp --login` in a terminal on the machine running Agentloft"),
            ..Self::new(
                HarnessId::ANTIGRAVITY,
                "Antigravity",
                "agy",
                vec!["acp".into()],
            )
        }
    }

    /// Kiro CLI runs its configured agent engine.
    pub fn kiro() -> Self {
        Self {
            auth_hint: Some("Run `kiro-cli login` in a terminal on the machine running Agentloft"),
            ..Self::new(HarnessId::KIRO, "Kiro", "kiro-cli", vec!["acp".into()])
        }
    }

    fn resolve_binary(&self, cfg: &SessionConfig) -> Result<PathBuf> {
        match &cfg.binary {
            Some(bin) => Ok(bin.clone()),
            None => self.which(&cfg.env),
        }
    }

    fn which(&self, env: &HashMap<String, String>) -> Result<PathBuf> {
        let path = env.get("PATH").map(String::as_str).unwrap_or_default();
        loft_config::which(&self.binary_name, path).with_context(|| {
            format!(
                "`{}` not found on PATH — install {} or set its binary path",
                self.binary_name, self.display
            )
        })
    }

    /// `args`, or `legacy_args` when the installed CLI predates `args[0]`.
    /// Probed once per daemon run.
    fn agent_args(&self, binary: &Path, env: &HashMap<String, String>) -> Vec<String> {
        let Some(legacy) = &self.legacy_args else {
            return self.args.clone();
        };
        self.resolved_args
            .get_or_init(|| {
                let flag = self.args.first().cloned().unwrap_or_default();
                match help_text(binary, env) {
                    Some(help) if mentions_flag(&help, &flag) => self.args.clone(),
                    Some(_) => legacy.clone(),
                    // Can't tell: use the current spelling.
                    None => self.args.clone(),
                }
            })
            .clone()
    }

    pub fn build_args(&self, binary: &Path, cfg: &SessionConfig) -> Vec<String> {
        let mut args = self.agent_args(binary, &cfg.env);
        args.extend(cfg.extra_args.iter().cloned());
        if self.id == HarnessId::ANTIGRAVITY
            && cfg.permission_mode == Some(PermissionMode::FullAccess)
        {
            args.push("--dangerously-skip-permissions".into());
        }
        args
    }
}

/// `<binary> --help` output (stdout + stderr), bounded by a timeout.
fn help_text(binary: &Path, env: &HashMap<String, String>) -> Option<String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("--help")
        .env_clear()
        .envs(env.iter())
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

/// The models a short-lived session offers, each with its effort levels
/// (read by switching the session to each model in turn). The session is
/// deleted again if the agent can (`sessionCapabilities.delete`), so it
/// doesn't show in the agent's own history.
// Runs a CLI: never in a browser, so std's clock is fine here.
#[allow(clippy::disallowed_methods)]
fn probe_session(
    binary: &Path,
    args: &[String],
    env: &HashMap<String, String>,
    auth_hint: Option<&str>,
    wait_for_models: bool,
) -> Result<Vec<loft_protocol::ModelInfo>> {
    use std::io::{BufRead as _, Write as _};
    // An empty folder of the user's own, not the shared `/tmp` (on Linux)
    // whose project settings anyone could have planted.
    let cwd = loft_config::private_temp_dir("agentloft-ask")?;
    let mut cmd = std::process::Command::new(binary);
    cmd.args(args)
        .current_dir(&cwd)
        .env_clear()
        .envs(env.iter())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;
    let mut stdin = child.stdin.take().context("ACP agent stdin")?;
    let stdout = child.stdout.take().context("ACP agent stdout")?;
    // Read on a thread so a stuck agent can't hang us past the deadline.
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
    let initial_models = std::cell::Cell::new(None);
    let mut call = |id: u64,
                    method: &str,
                    params: serde_json::Value|
     -> Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let msg =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(stdin, "{msg}")?;
        stdin.flush()?;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let v = rx
                .recv_timeout(left)
                .with_context(|| format!("the agent didn't answer `{method}`"))?;
            if wait_for_models && method == "session/new" && model_update(&v).is_some() {
                initial_models.set(Some(v));
            } else if v.get("id") == Some(&serde_json::json!(id)) && v.get("method").is_none() {
                if let Some(err) = v.get("error") {
                    if err.get("code").and_then(serde_json::Value::as_i64) == Some(-32000)
                        && let Some(hint) = auth_hint
                    {
                        let msg = err
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Authentication required");
                        anyhow::bail!("{msg} — {hint}");
                    }
                    anyhow::bail!("{method}: {err}");
                }
                return Ok(v.get("result").cloned().unwrap_or_default());
            }
        }
    };
    let result = (|| {
        let init = call(0, "initialize", translate::initialize_params())?;
        let mut session = call(
            1,
            "session/new",
            serde_json::json!({ "cwd": cwd.to_string_lossy(), "mcpServers": [] }),
        )?;
        let id = session.get("sessionId").cloned().unwrap_or_default();
        if wait_for_models && translate::offered_models(&session).is_empty() {
            let deadline = std::time::Instant::now() + MODEL_OPTIONS_WAIT;
            let mut update = initial_models.take();
            loop {
                if let Some(message) = update
                    && message.pointer("/params/sessionId") == Some(&id)
                    && let Some(options) = model_update(&message)
                {
                    session["configOptions"] = options.clone();
                    break;
                }
                let Ok(message) =
                    rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                else {
                    break;
                };
                update = Some(message);
            }
        }
        let mut models = translate::offered_models(&session);
        let mut next = 2;
        // Kiro publishes each model's efforts and default in its model list,
        // even if the current model (e.g. Auto) has no effort selector.
        if models.iter().all(|m| m.efforts.is_empty())
            && translate::offered_efforts(&session).is_some()
        {
            let current = models.iter().position(|m| m.is_default);
            for m in &mut models {
                let reply = call(
                    next,
                    "session/set_config_option",
                    serde_json::json!({ "sessionId": id, "configId": "model", "value": m.id }),
                );
                next += 1;
                if let Some(efforts) = reply.ok().as_ref().and_then(translate::offered_efforts) {
                    m.efforts = efforts;
                }
            }
            // Leave nothing changed behind (some agents remember the last
            // model for new sessions).
            if let Some(m) = current.map(|i| models[i].id.clone()) {
                let _ = call(
                    next,
                    "session/set_config_option",
                    serde_json::json!({ "sessionId": id, "configId": "model", "value": m }),
                );
                next += 1;
            }
        }
        let deletes = init
            .pointer("/agentCapabilities/sessionCapabilities/delete")
            .is_some();
        if deletes {
            let _ = call(
                next,
                "session/delete",
                serde_json::json!({ "sessionId": id }),
            );
        }
        Ok(models)
    })();
    // The agent exits when its input ends; its helpers go with the group.
    drop(stdin);
    let pid = child.id();
    for _ in 0..20 {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    loft_harness::process::signal_group(pid, loft_harness::process::SIGKILL);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn model_update(message: &serde_json::Value) -> Option<&serde_json::Value> {
    let update = message.pointer("/params/update")?;
    (message["method"] == "session/update"
        && update["sessionUpdate"] == "config_option_update"
        && !translate::offered_models(update).is_empty())
    .then(|| &update["configOptions"])
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
            // Antigravity handles a second ACP prompt while one is running.
            steer: self.id == HarnessId::ANTIGRAVITY,
            native_fork: false,
            // Effort is a session config option (`thought_level`) where the
            // agent offers one, set on the live session.
            live_effort_switch: true,
            // Only some agents advertise model/mode selectors (OpenCode does,
            // via `configOptions`); the session applies them when offered and
            // posts a notice otherwise, but the UI can't rely on them.
            live_model_switch: false,
            live_mode_switch: false,
            compact: false,
        }
    }

    fn permission_mode_requires_restart(
        &self,
        previous: Option<PermissionMode>,
        next: Option<PermissionMode>,
    ) -> bool {
        let full = Some(PermissionMode::FullAccess);
        // Antigravity's flag is process-wide. Kiro's preset is set on session
        // new/load and is dropped when that process exits.
        matches!(self.id.as_str(), HarnessId::ANTIGRAVITY | HarnessId::KIRO)
            && (previous == full) != (next == full)
    }

    /// ACP has no call for this: open a session in the temp folder, read
    /// the models it offers, and delete it again (when the agent can), so
    /// the picker has them before the first thread. Sessions report their
    /// own list once they start.
    fn list_models(
        &self,
        env: &HashMap<String, String>,
        binary: Option<PathBuf>,
    ) -> Result<Vec<loft_protocol::ModelInfo>> {
        let binary = match binary {
            Some(b) => b,
            None => self.which(env)?,
        };
        probe_session(
            &binary,
            &self.agent_args(&binary, env),
            env,
            self.auth_hint,
            self.id == HarnessId::KIRO,
        )
    }

    fn plan_limits(
        &self,
        env: &HashMap<String, String>,
        binary: Option<PathBuf>,
    ) -> Result<Option<serde_json::Value>> {
        if self.id != HarnessId::ANTIGRAVITY {
            return Ok(None);
        }
        let binary = match binary {
            Some(b) => b,
            None => self.which(env)?,
        };
        probe_antigravity_usage(&binary, env, self.auth_hint).map(Some)
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
        translator.is_antigravity = self.id == HarnessId::ANTIGRAVITY;
        translator.is_kiro = self.id == HarnessId::KIRO;
        translator.auth_hint = self.auth_hint.map(str::to_owned);
        translator.effort = cfg.effort.clone();
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
        let usage_probe = (self.id == HarnessId::ANTIGRAVITY)
            .then(|| (binary.clone(), cfg.env.clone(), self.auth_hint));
        tokio::spawn(async move {
            let events = ends.events.clone();
            match run(ends, translator, stdin, stdout, usage_probe).await {
                RunEnd::Shutdown(stdin) => {
                    drop(stdin);
                    stop_child(&mut child, Duration::from_secs(2), Duration::from_secs(5)).await;
                    drop(events);
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

/// Ask Antigravity's CLI for its quota buckets via `agy --print /usage
/// --output-format json` (uses zero tokens and refreshes its own OAuth
/// sign-in). Sets `ANTIGRAVITY_REMOTE_OOB_AUTH=1` and aborts immediately if
/// the CLI asks to sign in, so it never opens a browser window on its own.
#[allow(clippy::disallowed_methods)]
fn probe_antigravity_usage(
    binary: &Path,
    env: &HashMap<String, String>,
    auth_hint: Option<&str>,
) -> Result<serde_json::Value> {
    use std::io::BufRead as _;
    let cwd = loft_config::private_temp_dir("agentloft-ask")?;
    let mut cmd = std::process::Command::new(binary);
    cmd.args(["--print", "/usage", "--output-format", "json"])
        .current_dir(&cwd)
        .env_clear()
        .envs(env.iter())
        .env("ANTIGRAVITY_REMOTE_OOB_AUTH", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", binary.display()))?;
    let stdout = child.stdout.take().context("agy stdout")?;
    let stderr = child.stderr.take().context("agy stderr")?;

    enum Line {
        Out(String),
        Err(String),
        Done,
    }
    let (tx, rx) = std::sync::mpsc::channel::<Line>();
    let spawn_reader =
        |stream: Box<dyn std::io::Read + Send>, is_err: bool, tx: std::sync::mpsc::Sender<Line>| {
            std::thread::spawn(move || {
                for line in std::io::BufReader::new(stream)
                    .lines()
                    .map_while(Result::ok)
                {
                    let msg = if is_err {
                        Line::Err(line)
                    } else {
                        Line::Out(line)
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
                let _ = tx.send(Line::Done);
            });
        };
    spawn_reader(Box::new(stdout), false, tx.clone());
    spawn_reader(Box::new(stderr), true, tx);

    let kill = |child: &mut std::process::Child| {
        let pid = child.id();
        loft_harness::process::signal_group(pid, loft_harness::process::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut out_lines = Vec::new();
    let mut err_lines = Vec::new();
    let mut closed = 0;
    while closed < 2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(Line::Done) => closed += 1,
            Ok(Line::Out(line)) => {
                if looks_like_auth_prompt(&line) {
                    kill(&mut child);
                    anyhow::bail!("{}", auth_error(auth_hint));
                }
                out_lines.push(line);
            }
            Ok(Line::Err(line)) => {
                if looks_like_auth_prompt(&line) {
                    kill(&mut child);
                    anyhow::bail!("{}", auth_error(auth_hint));
                }
                err_lines.push(line);
            }
            Err(_) => {
                kill(&mut child);
                anyhow::bail!("Antigravity didn't answer `/usage`");
            }
        }
    }
    let status = child.wait().ok();
    let joined = out_lines.join("\n");
    let parsed = serde_json::from_str::<serde_json::Value>(&joined)
        .ok()
        .or_else(|| {
            out_lines
                .iter()
                .rev()
                .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
        });
    if let Some(v) = parsed
        && v.pointer("/command/data/groups")
            .and_then(serde_json::Value::as_array)
            .is_some()
    {
        return Ok(v);
    }
    // `agy` exits with code 2 when connected with GEMINI_API_KEY (no plan quota).
    if status.and_then(|s| s.code()) == Some(2) {
        return Ok(serde_json::json!({
            "command": { "name": "usage", "data": { "groups": [] } }
        }));
    }
    let detail = err_lines
        .iter()
        .chain(out_lines.iter())
        .rev()
        .find(|l| !l.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "Antigravity returned no usage report".into());
    anyhow::bail!("{detail}");
}

fn looks_like_auth_prompt(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("authentication required") || lower.contains("waiting for authentication")
}

fn auth_error(hint: Option<&str>) -> String {
    match hint {
        Some(h) => format!("Authentication required — {h}"),
        None => "Authentication required".to_owned(),
    }
}

type AntigravityUsageProbe = (
    PathBuf,
    std::sync::Arc<HashMap<String, String>>,
    Option<&'static str>,
);

enum RunEnd {
    Shutdown(ChildStdin),
    Eof,
}

/// Quiet window after an unsolicited Antigravity message + `usage_update`
/// before closing the autonomous turn (tool calls from the same LLM response
/// arrive within ~10 ms).
const AUTONOMOUS_SETTLE: Duration = Duration::from_secs(2);

/// Kiro V3 can advertise models in a config update just after opening a session.
const MODEL_OPTIONS_WAIT: Duration = Duration::from_secs(3);

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn run(
    mut ends: SessionEnds,
    mut t: Translator,
    mut stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
    usage_probe: Option<AntigravityUsageProbe>,
) -> RunEnd {
    let events = ends.events.clone();
    let mut lines = BufReader::new(stdout).lines();
    let init = t.initialize();
    if write_json(&mut stdin, &init).await.is_err() {
        return RunEnd::Eof;
    }
    let mut settle_deadline: Option<tokio::time::Instant> = None;
    let mut model_deadline: Option<tokio::time::Instant> = None;
    loop {
        let outs = tokio::select! {
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => return RunEnd::Eof,
                };
                if line.trim().is_empty() {
                    continue;
                }
                tracing::trace!(target: "harness.acp", "<< {line}");
                t.on_line(&line)
            }
            cmd = ends.commands.recv() => {
                match cmd {
                    Some(SessionCommand::StartTurn(input)) => {
                        let images = read_images(&input.images).await;
                        t.start_turn_with(&input.text, images)
                    }
                    Some(SessionCommand::Steer(input)) => {
                        let images = read_images(&input.images).await;
                        t.steer_with(&input.text, images)
                    }
                    Some(SessionCommand::Interrupt) => t.interrupt(),
                    Some(SessionCommand::StopSubagent(_)) => vec![],
                    Some(SessionCommand::Query { kind, reply }) => {
                        if kind == loft_protocol::AgentInfoKind::Usage
                            && let Some((bin, env, hint)) = usage_probe.clone()
                        {
                            tokio::task::spawn_blocking(move || {
                                let res = probe_antigravity_usage(&bin, &env, hint)
                                    .map_err(|e| e.to_string());
                                let _ = reply.send(res);
                            });
                        }
                        vec![]
                    }
                    Some(SessionCommand::Respond { native_request_id, decision, .. }) => {
                        t.respond(&native_request_id, decision)
                    }
                    Some(SessionCommand::SetModel(model)) => t.set_model(model),
                    Some(SessionCommand::SetEffort(effort)) => t.set_effort(effort),
                    Some(SessionCommand::SetPermissionMode(mode)) => t.set_permission_mode(mode),
                    Some(SessionCommand::Shutdown) | None => return RunEnd::Shutdown(stdin),
                }
            }
            () = sleep_until_opt(settle_deadline) => {
                settle_deadline = None;
                t.finish_autonomous_turn()
            }
            () = sleep_until_opt(model_deadline) => t.finish_model_wait(),
        };
        settle_deadline = if t.needs_autonomous_settle() {
            Some(settle_deadline.unwrap_or_else(|| tokio::time::Instant::now() + AUTONOMOUS_SETTLE))
        } else {
            None
        };
        model_deadline = t.awaiting_models.then(|| {
            model_deadline.unwrap_or_else(|| tokio::time::Instant::now() + MODEL_OPTIONS_WAIT)
        });
        // Deliver a startup error to the first turn before dropping the
        // failed session. A retry may need a new process to pick up sign-in.
        let failed_start = t.startup_failed()
            && outs
                .iter()
                .any(|out| matches!(out, Out::Event(HarnessEvent::TurnCompleted { .. })));
        if dispatch(&events, &mut stdin, outs).await.is_err() {
            return RunEnd::Eof;
        }
        if failed_start {
            return RunEnd::Shutdown(stdin);
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

/// Attached images as `(mime, base64)` for ACP image content blocks.
async fn read_images(images: &[loft_harness::ImageInput]) -> Vec<(String, String)> {
    use base64::Engine as _;
    let mut out = Vec::new();
    for image in images {
        if let Ok(bytes) = tokio::fs::read(&image.path).await {
            out.push((
                image.mime.clone(),
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ));
        }
    }
    out
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
            effort: None,
            resume: None,
            new_session_id: None,
            fork_from: None,
            fork_at: None,
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
        let agy = AcpHarness::antigravity();
        assert_eq!(agy.id().as_str(), "antigravity");
        assert_eq!(agy.binary_name, "agy");
        assert!(agy.capabilities().steer);
        assert_eq!(
            agy.build_args(
                Path::new("/nonexistent/agy"),
                &cfg(vec!["--sandbox".into()])
            ),
            ["acp", "--sandbox"]
        );
        let mut full = cfg(vec![]);
        full.permission_mode = Some(PermissionMode::FullAccess);
        assert_eq!(
            agy.build_args(Path::new("/nonexistent/agy"), &full),
            ["acp", "--dangerously-skip-permissions"]
        );
        assert!(agy.permission_mode_requires_restart(None, full.permission_mode));
        assert!(agy.permission_mode_requires_restart(full.permission_mode, None));
        assert!(!agy.permission_mode_requires_restart(full.permission_mode, full.permission_mode));
        assert!(!agy.permission_mode_requires_restart(None, Some(PermissionMode::AcceptEdits)));
        let kiro = AcpHarness::kiro();
        assert_eq!(kiro.id().as_str(), "kiro");
        assert_eq!(kiro.binary_name, "kiro-cli");
        assert!(kiro.permission_mode_requires_restart(None, full.permission_mode));
        assert!(kiro.permission_mode_requires_restart(full.permission_mode, None));
        assert!(!kiro.permission_mode_requires_restart(full.permission_mode, full.permission_mode));
        assert_eq!(
            kiro.build_args(
                Path::new("/nonexistent/kiro-cli"),
                &cfg(vec!["--agent-engine=v3".into()])
            ),
            ["acp", "--agent-engine=v3"]
        );
    }

    #[test]
    fn signed_out_kiro_turn_suggests_cli_login() {
        let mut translator = Translator::new("Kiro", "/tmp/probe".into(), None, None, None, vec![]);
        translator.auth_hint = AcpHarness::kiro().auth_hint.map(str::to_owned);
        translator.initialize();
        translator.start_turn("Hello");
        let out: Vec<_> = include_str!("../fixtures/kiro-3.6.0-auth-required.jsonl")
            .lines()
            .flat_map(|line| translator.on_line(line))
            .collect();
        assert!(out.iter().any(|o| matches!(
            o,
            Out::Event(HarnessEvent::TurnCompleted {
                status: loft_protocol::TurnStatus::Failed,
                error: Some(error),
                ..
            }) if error.contains("Authentication required") && error.contains("kiro-cli login")
        )));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn antigravity_sign_in_error_ends_the_failed_session() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = loft_config::private_temp_dir(&format!(
            "agentloft-acp-auth-test-{}",
            std::process::id()
        ))
        .unwrap();
        let binary = dir.join("agy");
        let fixture = include_str!("../fixtures/antigravity-1.5.14-auth-required.jsonl");
        let mut script = String::from("#!/bin/sh\n[ \"$1\" = acp ] || exit 1\n");
        for response in fixture.lines() {
            script.push_str(&format!(
                "read -r request || exit 1\nprintf '%s\\n' '{response}'\n"
            ));
        }
        script.push_str("read -r request\n");
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = cfg(Vec::new());
        config.cwd = dir.clone();
        config.binary = Some(binary);
        let mut session = AcpHarness::antigravity().open_session(config).unwrap();
        session
            .commands
            .send(SessionCommand::StartTurn(loft_harness::TurnInput {
                text: "Hello".into(),
                images: Vec::new(),
            }))
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), async {
            let mut error = None;
            while let Some(event) = session.events.recv().await {
                if let HarnessEvent::TurnCompleted {
                    status: loft_protocol::TurnStatus::Failed,
                    error: failure,
                    ..
                } = event
                {
                    error = failure;
                }
            }
            error.unwrap()
        })
        .await
        .unwrap();
        assert!(error.contains("Authentication required"), "{error}");
        assert!(error.contains("agy acp --login"), "{error}");
        assert!(session.commands.is_closed());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A stand-in ACP agent: answers `initialize` and `session/new` with
    /// two models, writes what it gets next (the delete) to a file.
    #[cfg(unix)]
    #[test]
    fn lists_models_from_a_short_lived_session() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!(
            "agentloft-acp-models-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("after.txt");
        let agent = dir.join("agent");
        std::fs::write(
            &agent,
            format!(
                r#"#!/bin/sh
read l; echo '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":1,"agentCapabilities":{{"sessionCapabilities":{{"delete":{{}}}}}}}}}}'
read l; echo '{{"jsonrpc":"2.0","method":"session/update","params":{{}}}}'
echo '{{"jsonrpc":"2.0","id":1,"result":{{"sessionId":"s1","configOptions":[{{"id":"model","category":"model","currentValue":"p/b","options":[{{"value":"p/a","name":"A"}},{{"value":"p/b","name":"B"}}]}}]}}}}'
read l; echo "$l" > '{}'; echo '{{"jsonrpc":"2.0","id":2,"result":{{}}}}'
read l
"#,
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: HashMap<String, String> = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let models = AcpHarness::opencode()
            .list_models(&env, Some(agent))
            .unwrap();
        let ids: Vec<_> = models
            .iter()
            .map(|m| (m.id.as_str(), m.is_default))
            .collect();
        assert_eq!(ids, vec![("p/a", false), ("p/b", true)]);
        assert_eq!(models[0].display_name, "A");
        let after = std::fs::read_to_string(&log).unwrap();
        assert!(after.contains(r#""method":"session/delete""#), "{after}");
        assert!(after.contains(r#""sessionId":"s1""#), "{after}");
        let _ = std::fs::remove_dir_all(&dir);
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

    #[cfg(unix)]
    #[test]
    fn antigravity_plan_limits_parses_usage_json() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            loft_config::private_temp_dir(&format!("agentloft-agy-limits-{}", std::process::id()))
                .unwrap();
        let fake = dir.join("agy");
        let fixture = include_str!("../fixtures/antigravity-1.5.14-usage.json").trim();
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$ANTIGRAVITY_REMOTE_OOB_AUTH\" = 1 ] || exit 9\n[ \"$1\" = --print ] && [ \"$2\" = /usage ] || exit 1\nprintf '%s\\n' '{}'\n",
                fixture.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: HashMap<String, String> = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let info = AcpHarness::antigravity()
            .plan_limits(&env, Some(fake))
            .unwrap()
            .expect("antigravity reports limits");
        assert_eq!(
            info.pointer("/command/data/groups/0/buckets/0/id")
                .and_then(serde_json::Value::as_str),
            Some("gemini-weekly")
        );
        // Non-Antigravity ACP agents return None.
        assert!(
            AcpHarness::opencode()
                .plan_limits(&env, None)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_plan_limits_aborts_on_auth_prompt() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = loft_config::private_temp_dir(&format!(
            "agentloft-agy-auth-limits-{}",
            std::process::id()
        ))
        .unwrap();
        let fake = dir.join("agy");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'Authentication required. Please visit the URL to log in:'\nsleep 30\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: HashMap<String, String> = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let err = AcpHarness::antigravity()
            .plan_limits(&env, Some(fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Authentication required"), "{err}");
        assert!(err.contains("agy acp --login"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_plan_limits_api_key_exit_code_2_has_no_limits() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir =
            loft_config::private_temp_dir(&format!("agentloft-agy-apikey-{}", std::process::id()))
                .unwrap();
        let fake = dir.join("agy");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'No account quota when using GEMINI_API_KEY' >&2\nexit 2\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env: HashMap<String, String> = [("PATH".into(), "/usr/bin:/bin".into())].into();
        let info = AcpHarness::antigravity()
            .plan_limits(&env, Some(fake))
            .unwrap()
            .expect("empty groups");
        assert_eq!(
            info.pointer("/command/data/groups")
                .and_then(serde_json::Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn antigravity_autonomous_turn_completes_after_settle_timeout() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = loft_config::private_temp_dir(&format!(
            "agentloft-acp-autonomous-{}",
            std::process::id()
        ))
        .unwrap();
        let binary = dir.join("agy");
        let fixture = include_str!("../fixtures/antigravity-1.5.16-daemon-wakeup.jsonl");
        let lines: Vec<&str> = fixture.lines().collect();
        let mut script = String::from("#!/bin/sh\n[ \"$1\" = acp ] || exit 1\n");
        // initialize, session/new
        for response in &lines[..2] {
            script.push_str(&format!(
                "read -r request || exit 1\nprintf '%s\\n' '{response}'\n"
            ));
        }
        // session/prompt → turn 1 + unsolicited wakeup updates (kept open on stdin)
        script.push_str("read -r request || exit 1\n");
        for response in &lines[2..] {
            script.push_str(&format!("printf '%s\\n' '{response}'\n"));
        }
        script.push_str("read -r request\n");
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = cfg(Vec::new());
        config.cwd = dir.clone();
        config.binary = Some(binary);
        let mut session = AcpHarness::antigravity().open_session(config).unwrap();
        session
            .commands
            .send(SessionCommand::StartTurn(loft_harness::TurnInput {
                text: "Start daemon".into(),
                images: Vec::new(),
            }))
            .unwrap();
        let seen = tokio::time::timeout(Duration::from_secs(10), async {
            let mut starts = 0usize;
            let mut finishes = 0usize;
            while let Some(event) = session.events.recv().await {
                match event {
                    HarnessEvent::TurnStarted => starts += 1,
                    HarnessEvent::TurnCompleted {
                        status: loft_protocol::TurnStatus::Completed,
                        ..
                    } => {
                        finishes += 1;
                        if finishes == 2 {
                            return (starts, finishes);
                        }
                    }
                    _ => {}
                }
            }
            (starts, finishes)
        })
        .await
        .expect("autonomous turn settles without a second prompt response");
        assert_eq!(seen, (2, 2));
        let _ = session.commands.send(SessionCommand::Shutdown);
        let _ = std::fs::remove_dir_all(dir);
    }
}
