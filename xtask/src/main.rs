//! Developer tasks: `cargo xtask <task>`.
//!
//! - `dev`: build both binaries and run the app against `./.dev-home`
//!   (the app starts its own isolated `agentloftd`).
//! - `stop-dev`: ask the dev daemon to shut down.
//! - `live-claude`: run the live Claude end-to-end test (uses real tokens).
//! - `web [--release]`: build the browser client into `target/web`
//!   (wasm + JS glue + static shell); `agentloftd` serves that folder.
//! - `ci`: fmt check, clippy, tests, cargo-deny — what CI runs.
//! - `bundle [--debug] [--open]`: build `target/bundle/Agentloft.app`
//!   (macOS; both binaries, `AppIcon.icns`, Info.plist, ad-hoc signature).

use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "dev" => dev(),
        "bundle" => bundle(),
        "stop-dev" => stop_dev(),
        "web" => web(),
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
            eprintln!("usage: cargo xtask <dev|stop-dev|live-claude|ci|bundle|web>");
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

/// Build the browser client into `target/web`: the wasm module, its JS glue
/// and the static shell (`crates/loft-web/static`). `--release` optimizes.
fn web() -> Result<()> {
    let release = std::env::args().skip(2).any(|a| a == "--release");
    let mut build = cargo();
    build
        .args(["build", "-p", "loft-web", "--target", WASM_TARGET])
        // gpui-pre-web always compiles `wasm_thread`, which uses one unstable
        // feature gate. Allow it for that crate only (ADR-0016).
        .env("RUSTC_BOOTSTRAP", "wasm_thread");
    let profile = if release {
        build.args(["--profile", "web-release"]);
        "web-release"
    } else {
        "debug"
    };
    run(&mut build)?;

    let out = root().join("target/web");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let wasm = root()
        .join("target")
        .join(WASM_TARGET)
        .join(profile)
        .join("loft_web.wasm");
    wasm_bindgen_cli_support::Bindgen::new()
        .input_path(&wasm)
        .web(true)?
        .typescript(false)
        .remove_name_section(release)
        .remove_producers_section(release)
        .generate(&out)
        .context("wasm-bindgen")?;
    copy_dir(&root().join("crates/loft-web/static"), &out)?;
    println!("{}", out.display());
    Ok(())
}

const WASM_TARGET: &str = "wasm32-unknown-unknown";

fn copy_dir(from: &std::path::Path, to: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
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

const BUNDLE_ID: &str = "dev.agentloft.Agentloft";

fn bundle() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let debug = args.iter().any(|a| a == "--debug");
    let profile = if debug { "debug" } else { "release" };
    let mut build = cargo();
    build.args(["build", "-p", "loft-app", "-p", "loftd"]);
    if !debug {
        build.arg("--release");
    }
    run(&mut build)?;

    let target = root().join("target");
    let app = target.join("bundle/Agentloft.app");
    let contents = app.join("Contents");
    let _ = std::fs::remove_dir_all(&app);
    std::fs::create_dir_all(contents.join("MacOS"))?;
    std::fs::create_dir_all(contents.join("Resources"))?;
    for bin in ["agentloft", "agentloftd"] {
        std::fs::copy(
            target.join(profile).join(bin),
            contents.join("MacOS").join(bin),
        )
        .with_context(|| format!("copy {bin}"))?;
    }
    std::fs::copy(
        root().join("assets/icon/AppIcon.icns"),
        contents.join("Resources/AppIcon.icns"),
    )
    .context("copy AppIcon.icns")?;
    std::fs::write(
        contents.join("Info.plist"),
        info_plist(env!("CARGO_PKG_VERSION")),
    )?;
    std::fs::write(contents.join("PkgInfo"), "APPL????")?;

    // Ad-hoc signature so Gatekeeper treats the local build consistently.
    let signed = Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&app)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !signed {
        eprintln!("warning: ad-hoc codesign failed; the bundle is unsigned");
    }
    println!("{}", app.display());
    if args.iter().any(|a| a == "--open") {
        run(Command::new("open").arg(&app))?;
    }
    Ok(())
}

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Agentloft</string>
  <key>CFBundleDisplayName</key><string>Agentloft</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>agentloft</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
  <key>NSHumanReadableCopyright</key><string>Apache-2.0 — github.com/agentloft/agentloft</string>
</dict>
</plist>
"#
    )
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
