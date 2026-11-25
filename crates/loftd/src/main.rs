use anyhow::Result;
use clap::{Parser, Subcommand};
use loft_config::Paths;
use loft_harness::Harness;
use loftd::{Daemon, DaemonOptions};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "agentloftd", version, about = "Agentloft core daemon")]
struct Cli {
    /// Data home (defaults to $AGENTLOFT_HOME or ~/.agentloft; ~/.agentloft-dev for a dev build).
    #[arg(long, global = true)]
    home: Option<PathBuf>,
    /// Also log to stderr.
    #[arg(long, global = true)]
    foreground: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (default).
    Run,
    /// Print build info as JSON.
    Version,
    /// Run the Agentloft MCP server on stdio for one thread (spawned by agents).
    Mcp {
        #[arg(long)]
        thread: String,
    },
    /// Carry the daemon's connection over stdin and stdout, starting the
    /// daemon if it isn't running. Another computer's Agentloft runs this
    /// over SSH to reach this one: SSH has already said who it is.
    Bridge,
    /// Show one running iOS simulator's screen and take its input, over
    /// stdin and stdout (spawned by the daemon for its device panel).
    #[command(hide = true)]
    Simulator {
        udid: String,
        /// The Xcode whose SimulatorKit to use.
        #[arg(long)]
        developer_dir: PathBuf,
        /// A device set other than the user's.
        #[arg(long)]
        set: Option<PathBuf>,
        /// A built-in screen to follow besides the main one (a foldable's
        /// inner screen); may be given more than once.
        #[arg(long = "screen")]
        screens: Vec<u32>,
    },
    /// `agent-device`: agents' device command line (ADR-0020). The daemon's
    /// `<home>/bin/agent-device` runs this; `agentloftd device help` says
    /// what it takes.
    #[command(hide = true, disable_help_flag = true)]
    Device {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Hold a voice model and answer the daemon's requests for it, over
    /// stdin and stdout (spawned by the daemon; ends when stdin closes).
    #[command(hide = true)]
    VoiceEngine,
    /// Show web access as the running daemon has it: where it listens, the
    /// pairing link, and the browsers signed in.
    Web {
        /// Replace the pairing link and sign every browser out.
        #[arg(long)]
        new_link: bool,
        /// Sign every browser out.
        #[arg(long)]
        sign_out_all: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Version) = cli.cmd {
        println!(
            "{}",
            serde_json::json!({ "build": loftd::BUILD, "protocol": loft_protocol::PROTOCOL_VERSION })
        );
        return Ok(());
    }
    if let Some(Cmd::Simulator {
        udid,
        developer_dir,
        set,
        screens,
    }) = cli.cmd
    {
        // stdout carries pictures; nothing else may write to it.
        return simulator(udid, developer_dir, set, screens);
    }
    if let Some(Cmd::VoiceEngine) = cli.cmd {
        // stdout carries the answers.
        return loft_voice::engine::run();
    }
    let paths = match cli.home {
        Some(home) => Paths::new(home),
        None => Paths::from_env()?,
    };
    if let Some(Cmd::Mcp { thread }) = cli.cmd {
        // stdout carries the MCP protocol: log to a file only.
        init_file_tracing(&paths, "mcp.log");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return runtime.block_on(loftd::mcp::run(paths, loft_protocol::ThreadId(thread)));
    }
    if let Some(Cmd::Device { args }) = cli.cmd {
        // Its answers go to stdout and stderr: no logging there.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let status = runtime.block_on(loftd::device_cli::run(paths, args));
        std::process::exit(status);
    }
    if let Some(Cmd::Bridge) = cli.cmd {
        // stdout carries the connection: log to a file only.
        paths.ensure_dirs()?;
        init_file_tracing(&paths, "bridge.log");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return runtime.block_on(loftd::bridge::run(paths));
    }
    if let Some(Cmd::Web {
        new_link,
        sign_out_all,
    }) = cli.cmd
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return runtime.block_on(web_status(paths, new_link, sign_out_all));
    }
    paths.ensure_dirs()?;
    init_tracing(&paths, cli.foreground)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("agentloftd")
        .build()?;
    runtime.block_on(run(paths))
}

#[cfg(target_os = "macos")]
fn simulator(
    udid: String,
    developer_dir: PathBuf,
    set: Option<PathBuf>,
    screens: Vec<u32>,
) -> Result<()> {
    loft_sim::run(loft_sim::Options {
        udid,
        set,
        developer_dir,
        screens,
    })
}

#[cfg(not(target_os = "macos"))]
fn simulator(
    _udid: String,
    _developer_dir: PathBuf,
    _set: Option<PathBuf>,
    _screens: Vec<u32>,
) -> Result<()> {
    anyhow::bail!("iOS simulators need a Mac")
}

async fn run(paths: Paths) -> Result<()> {
    let harnesses: Vec<Arc<dyn Harness>> = vec![
        Arc::new(loft_harness_claude::ClaudeHarness),
        Arc::new(loft_harness_codex::CodexHarness),
        Arc::new(loft_harness_acp::AcpHarness::opencode()),
        Arc::new(loft_harness_acp::AcpHarness::gemini()),
        Arc::new(loft_harness_acp::AcpHarness::antigravity()),
        Arc::new(loft_harness_acp::AcpHarness::kiro()),
        Arc::new(loft_harness::fake::FakeHarness),
    ];
    let mcp_bridge = std::env::current_exe()
        .ok()
        .map(|exe| loft_core::McpBridge {
            exe,
            home: paths.home.clone(),
        });
    // An app updating us may start this daemon while the old one is still
    // shutting down; give it a moment to let go of the home.
    loftd::wait_for_lock(&paths, std::time::Duration::from_secs(15)).await;
    let daemon = match Daemon::start(DaemonOptions {
        paths,
        harnesses,
        capture_login_env: true,
        check_agent_updates: true,
        mcp_bridge,
        web_bundle: None,
        device_backends: None,
    })
    .await
    {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("{err:#}");
            eprintln!("agentloftd: {err:#}");
            std::process::exit(2);
        }
    };

    let terminated = terminated()?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
        what = terminated => tracing::info!("{what}"),
        _ = daemon.wait_for_shutdown_request() => {}
    }
    daemon.shutdown().await;
    Ok(())
}

/// Resolves when the system asks the daemon to stop: SIGTERM, or on
/// Windows the console closing or the user signing out / shutting down.
#[cfg(unix)]
fn terminated() -> std::io::Result<impl std::future::Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        sigterm.recv().await;
        "SIGTERM"
    })
}

#[cfg(windows)]
fn terminated() -> std::io::Result<impl std::future::Future<Output = &'static str>> {
    use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
    let (mut close, mut logoff, mut shutdown) = (ctrl_close()?, ctrl_logoff()?, ctrl_shutdown()?);
    Ok(async move {
        tokio::select! {
            _ = close.recv() => "console closed",
            _ = logoff.recv() => "signing out",
            _ = shutdown.recv() => "shutting down",
        }
    })
}

/// `agentloftd web`: ask the running daemon (over its Unix socket, so only
/// this user can) and print what it says.
async fn web_status(paths: Paths, new_link: bool, sign_out_all: bool) -> Result<()> {
    use loft_protocol::{ClientKind, Request, Response};
    let (client, _events) =
        loft_client::Client::connect(&paths.socket_file(), ClientKind::Cli, loftd::BUILD)
            .await
            .map_err(|err| anyhow::anyhow!("agentloftd isn't running for this home ({err:#})"))?;
    let request = if new_link {
        Request::WebRotateToken
    } else if sign_out_all {
        Request::WebRevokeAllSessions
    } else {
        Request::WebStatus
    };
    let Response::WebStatus { status } = client
        .request(request)
        .await
        .map_err(|e| anyhow::anyhow!(e))?
    else {
        anyhow::bail!("unexpected answer from the daemon");
    };
    if !status.enabled {
        println!("Web access is off. Turn it on with `[web] enabled = true` in config.toml.");
        return Ok(());
    }
    match (&status.url, &status.error) {
        (Some(url), _) => println!("Listening at {url}"),
        (None, Some(error)) => println!("Not running: {error}"),
        (None, None) => println!("Not running."),
    }
    if status.insecure_remote {
        println!("Warning: reachable from other devices over plain HTTP.");
    } else if status.remote {
        println!("Reachable from other devices.");
    }
    if !status.bundle && status.running {
        println!("The web client's files weren't found (build them with `cargo xtask web`).");
    }
    if let Some(tunnel) = &status.tunnel {
        match (tunnel.state.as_str(), &tunnel.url, &tunnel.error) {
            ("on", Some(url), _) => println!("On the internet at {url} (Cloudflare Tunnel)"),
            (_, _, Some(error)) => println!("Cloudflare Tunnel: {error}"),
            (state, _, None) => println!("Cloudflare Tunnel: {state}"),
        }
    }
    match &status.website.account {
        Some(account) => println!(
            "Linked to {account} on {}{}",
            status.website.url,
            status
                .website
                .reported_url
                .as_deref()
                .map(|url| format!(" (opens {url})"))
                .unwrap_or_default()
        ),
        None => println!("Not linked to {}", status.website.url),
    }
    if let Some(link) = &status.pairing_url {
        println!("Pairing link (keep it secret): {link}");
    }
    println!("Signed-in browsers: {}", status.sessions.len());
    for session in &status.sessions {
        println!(
            "  {}  {}  {}{}",
            session.id,
            session.label,
            session.addr,
            if session.connections > 0 {
                "  (connected)"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn init_file_tracing(paths: &Paths, file: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter =
        EnvFilter::try_from_env("AGENTLOFT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = std::fs::create_dir_all(paths.logs_dir());
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join(file))
    {
        fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .init();
    }
}

fn init_tracing(paths: &Paths, foreground: bool) -> Result<()> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter =
        EnvFilter::try_from_env("AGENTLOFT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.logs_dir().join("agentloftd.log"))?;
    let file_layer = fmt::layer()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(file));
    let stderr_layer = foreground.then(|| fmt::layer().with_writer(std::io::stderr));
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();
    Ok(())
}
