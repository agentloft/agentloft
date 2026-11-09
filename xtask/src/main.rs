//! Developer tasks: `cargo xtask <task>`.
//!
//! - `dev`: build both binaries and run the app against `./.dev-home`
//!   (the app starts its own isolated `agentloftd`).
//! - `stop-dev`: ask the dev daemon to shut down.
//! - `live-claude`: run the live Claude end-to-end test (uses real tokens).
//! - `ci`: fmt check, clippy, tests, cargo-deny — what CI runs.

use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "dev" => dev(),
        "stop-dev" => stop_dev(),
        "live-claude" => run(cargo().args([
            "test",
            "-p",
            "loftd",
            "--test",
            "live_claude",
            "--",
            "--ignored",
            "--nocapture",
        ])),
        "ci" => {
            run(cargo().args(["fmt", "--all", "--check"]))?;
            run(cargo().args([
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ]))?;
            run(cargo().args(["test", "--workspace"]))?;
            run(cargo().args(["deny", "check"]))
        }
        _ => {
            eprintln!("usage: cargo xtask <dev|stop-dev|live-claude|ci>");
            Ok(())
        }
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn cargo() -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root());
    c
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

fn dev_home() -> PathBuf {
    root().join(".dev-home")
}

fn dev() -> Result<()> {
    run(cargo().args(["build", "-p", "loft-app", "-p", "loftd"]))?;
    let bin = root().join("target/debug");
    // A daemon from an older build would keep serving stale code.
    stop_dev().ok();
    run(Command::new(bin.join("agentloft"))
        .env("AGENTLOFT_HOME", dev_home())
        .env("AGENTLOFT_DAEMON_BIN", bin.join("agentloftd")))
}

fn stop_dev() -> Result<()> {
    let socket = dev_home().join("run/agentloftd.sock");
    if !socket.exists() {
        return Ok(());
    }
    // Minimal protocol client: hello + shutdown request.
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(&socket)?;
    let send = |s: &mut std::os::unix::net::UnixStream, json: &str| -> Result<()> {
        s.write_all(&(json.len() as u32).to_be_bytes())?;
        s.write_all(json.as_bytes())?;
        Ok(())
    };
    send(
        &mut s,
        r#"{"type":"hello","protocol_version":1,"client_kind":"cli","client_build":"xtask"}"#,
    )?;
    send(
        &mut s,
        r#"{"type":"request","id":1,"body":{"op":"shutdown","reason":"upgrade"}}"#,
    )?;
    let mut buf = [0u8; 256];
    let _ = s.read(&mut buf);
    std::thread::sleep(std::time::Duration::from_millis(500));
    Ok(())
}
