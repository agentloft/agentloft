//! adb on this computer: finding it, running it, and reading what it says
//! about the phones plugged in.

use anyhow::{Context as _, anyhow, bail};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

/// adb, run with the login shell's environment.
#[derive(Clone)]
pub struct Adb {
    pub path: Option<PathBuf>,
    env: Arc<HashMap<String, String>>,
}

fn var<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// Where Android Studio puts the SDK by default.
fn default_sdk(env: &HashMap<String, String>) -> Option<PathBuf> {
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = var(env, home_key)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(home_key).map(PathBuf::from))?;
    if cfg!(target_os = "macos") {
        Some(home.join("Library/Android/sdk"))
    } else if cfg!(windows) {
        var(env, "LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
            .map(|p| p.join("Android").join("Sdk"))
    } else {
        Some(home.join("Android/Sdk"))
    }
}

impl Adb {
    /// The SDK's adb (`ANDROID_HOME`, else Android Studio's default SDK),
    /// else one on `PATH` (Homebrew's `android-platform-tools`, say).
    pub fn find(env: &HashMap<String, String>) -> Self {
        let in_sdk = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .filter_map(|k| var(env, k))
            .map(PathBuf::from)
            .chain(default_sdk(env))
            .map(|sdk| sdk.join("platform-tools").join(exe("adb")))
            .find(|p| p.is_file());
        let path = in_sdk.or_else(|| loft_config::which("adb", var(env, "PATH").unwrap_or("")));
        Self {
            path,
            env: Arc::new(env.clone()),
        }
    }

    pub fn env(&self) -> &HashMap<String, String> {
        &self.env
    }

    /// A command running adb (on `serial` when given), its output piped,
    /// killed when dropped.
    pub fn command(&self, serial: Option<&str>) -> anyhow::Result<tokio::process::Command> {
        let adb = self
            .path
            .as_ref()
            .ok_or_else(|| anyhow!("adb wasn't found"))?;
        let mut cmd = tokio::process::Command::new(adb);
        cmd.envs(self.env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        no_window(&mut cmd);
        if let Some(serial) = serial {
            cmd.args(["-s", serial]);
        }
        Ok(cmd)
    }

    /// Run adb and return what it printed, as bytes.
    pub async fn bytes(
        &self,
        serial: Option<&str>,
        args: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<Vec<u8>> {
        let mut cmd = self.command(serial)?;
        cmd.args(args);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| {
                anyhow!(
                    "adb {} took longer than {}s",
                    args.join(" "),
                    timeout.as_secs()
                )
            })?
            .context("running adb")?;
        succeeded(out, args.first().copied().unwrap_or_default())
    }

    pub async fn run(
        &self,
        serial: Option<&str>,
        args: &[&str],
        timeout: Duration,
    ) -> anyhow::Result<String> {
        let out = self.bytes(serial, args, timeout).await?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// A command on the phone; adb joins the words with spaces, so each is
    /// quoted for the phone's shell.
    pub async fn shell(&self, serial: &str, words: &[&str]) -> anyhow::Result<String> {
        let line = words
            .iter()
            .map(|w| shell_quote(w))
            .collect::<Vec<_>>()
            .join(" ");
        self.script(serial, &line).await
    }

    /// A line for the phone's shell, as is.
    pub async fn script(&self, serial: &str, line: &str) -> anyhow::Result<String> {
        self.run(Some(serial), &["shell", line], Duration::from_secs(60))
            .await
    }

    /// Lines for the phone's shell, given to adb on its stdin, never on its
    /// command line ([`crate::devices::adb_shell_script`]): for what an
    /// agent types.
    pub async fn script_on_stdin(&self, serial: &str, script: &str) -> anyhow::Result<String> {
        let out = crate::devices::adb_shell_script(
            self.command(Some(serial))?,
            script,
            Duration::from_secs(60),
        )
        .await?;
        let out = succeeded(out, "shell")?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// `adb devices -l`.
    pub async fn devices(&self) -> anyhow::Result<Vec<Listed>> {
        let out = self
            .run(None, &["devices", "-l"], Duration::from_secs(10))
            .await?;
        Ok(parse_devices(&out))
    }
}

/// What adb printed, if it succeeded; else why it failed (`what`: adb's
/// command, for the message).
fn succeeded(out: std::process::Output, what: &str) -> anyhow::Result<Vec<u8>> {
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let why = [stderr.trim(), stdout.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("it failed");
        bail!("adb {what}: {why}");
    }
    Ok(out.stdout)
}

/// No console window for adb on Windows.
pub fn no_window(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub serial: String,
    /// `device` (ready), `unauthorized`, `offline`, `no permissions`…
    pub state: String,
    /// adb's own name for the model (`Pixel_8`), when it says.
    pub model: Option<String>,
}

impl Listed {
    pub fn is_emulator(&self) -> bool {
        self.serial.starts_with("emulator-")
    }

    pub fn ready(&self) -> bool {
        self.state == "device"
    }

    /// Connected over Wi-Fi (adb over TCP, or wireless debugging), not USB.
    pub fn wireless(&self) -> bool {
        self.serial.contains("._adb-tls-connect.")
            || self
                .serial
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
    }
}

/// The entries of `adb devices -l`.
pub fn parse_devices(output: &str) -> Vec<Listed> {
    output
        .lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|line| {
            let (serial, rest) = line.trim().split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            // "no permissions (…udev rules…); see [url] usb:…": one state of
            // several words.
            let (state, props) = if let Some(after) = rest.strip_prefix("no permissions") {
                let props = after.rsplit_once(']').map_or(after, |(_, p)| p);
                ("no permissions".to_owned(), props)
            } else {
                let (state, props) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                (state.to_owned(), props)
            };
            let model = props
                .split_whitespace()
                .find_map(|p| p.strip_prefix("model:"))
                .map(str::to_owned);
            Some(Listed {
                serial: serial.to_owned(),
                state,
                model,
            })
        })
        .collect()
}

/// Quote `arg` for the phone's shell.
pub fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// What `getprop`/`wm` say about a phone, read in one go (see
/// [`ABOUT_SCRIPT`]).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct About {
    pub name: String,
    pub release: String,
    pub width: u32,
    pub height: u32,
    pub density: u32,
    /// `arm64-v8a`, …
    pub abi: String,
}

/// One shell line that prints what [`parse_about`] reads.
pub const ABOUT_SCRIPT: &str = "echo market=$(getprop ro.product.marketname); \
     echo vmarket=$(getprop ro.product.vendor.marketname); \
     echo model=$(getprop ro.product.model); \
     echo release=$(getprop ro.build.version.release); \
     echo abi=$(getprop ro.product.cpu.abi); \
     wm size; wm density";

pub fn parse_about(text: &str) -> About {
    let mut about = About::default();
    let (mut market, mut model) = (String::new(), String::new());
    let mut size = None;
    let mut density = None;
    for line in text.lines().map(str::trim) {
        let Some((key, value)) = line.split_once(['=', ':']) else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "market" | "vmarket" if market.is_empty() => market = value.to_owned(),
            "model" => model = value.to_owned(),
            "release" => about.release = value.to_owned(),
            "abi" => about.abi = value.to_owned(),
            // An override (`wm size 720x1600`) is what the screen shows.
            "Physical size" if size.is_none() => size = parse_size(value),
            "Override size" => size = parse_size(value).or(size),
            "Physical density" if density.is_none() => density = value.parse().ok(),
            "Override density" => density = value.parse().ok().or(density),
            _ => {}
        }
    }
    about.name = if market.is_empty() { model } else { market };
    if let Some((w, h)) = size {
        (about.width, about.height) = (w, h);
    }
    about.density = density.unwrap_or(0);
    about
}

/// `1080x2400`.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.trim().split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// ffmpeg on this computer, if there is one.
pub fn find_ffmpeg(env: &HashMap<String, String>) -> Option<PathBuf> {
    loft_config::which("ffmpeg", var(env, "PATH").unwrap_or("")).or_else(|| {
        ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.is_file())
            .map(Path::to_path_buf)
    })
}
