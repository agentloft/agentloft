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
    /// Data home (defaults to $AGENTLOFT_HOME or ~/.agentloft).
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
    let paths = match cli.home {
        Some(home) => Paths::new(home),
        None => Paths::from_env()?,
    };
    paths.ensure_dirs()?;
    init_tracing(&paths, cli.foreground)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("agentloftd")
        .build()?;
    runtime.block_on(run(paths))
}

async fn run(paths: Paths) -> Result<()> {
    let harnesses: Vec<Arc<dyn Harness>> = vec![
        Arc::new(loft_harness_claude::ClaudeHarness),
        Arc::new(loft_harness::fake::FakeHarness),
    ];
    let daemon = match Daemon::start(DaemonOptions {
        paths,
        harnesses,
        capture_login_env: true,
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

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
        _ = sigterm.recv() => tracing::info!("SIGTERM"),
        _ = daemon.wait_for_shutdown_request() => {}
    }
    daemon.shutdown().await;
    Ok(())
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
