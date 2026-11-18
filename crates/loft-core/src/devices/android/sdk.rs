//! Where the Android SDK and its virtual devices are, read from files only:
//! the SDK folder (`ANDROID_HOME`, else the platform's default), the AVDs'
//! `.ini` files, and the "discovery" files a running emulator writes about
//! itself (`pid_<pid>.ini`: its AVD, ports and gRPC access token).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The SDK's tools and folders on this computer.
#[derive(Debug, Clone)]
pub struct Sdk {
    /// The SDK folder, when one was found.
    pub root: Option<PathBuf>,
    pub emulator: Option<PathBuf>,
    pub adb: Option<PathBuf>,
    /// Where the AVDs' `<name>.ini` files are.
    pub avd_home: PathBuf,
    /// Where running emulators describe themselves.
    pub running_dirs: Vec<PathBuf>,
}

fn var<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

fn home(env: &HashMap<String, String>) -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    var(env, key)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(key).map(PathBuf::from))
        .unwrap_or_default()
}

pub fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

impl Sdk {
    /// Look for the SDK with the environment `env` (the login shell's).
    pub fn find(env: &HashMap<String, String>) -> Self {
        let home = home(env);
        let root = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
            .iter()
            .filter_map(|k| var(env, k))
            .map(PathBuf::from)
            .chain(default_root(env, &home))
            .find(|p| p.is_dir());
        let path = var(env, "PATH").unwrap_or_default();
        let tool = |sub: &str, name: &str| {
            root.as_ref()
                .map(|r| r.join(sub).join(exe_name(name)))
                .filter(|p| p.is_file())
                .or_else(|| loft_config::which(name, path))
        };
        let emulator = tool("emulator", "emulator");
        let adb = tool("platform-tools", "adb");
        let user_home = var(env, "ANDROID_USER_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".android"));
        let avd_home = var(env, "ANDROID_AVD_HOME")
            .map(PathBuf::from)
            .or_else(|| var(env, "ANDROID_EMULATOR_HOME").map(|p| Path::new(p).join("avd")))
            .unwrap_or_else(|| user_home.join("avd"));
        Self {
            root,
            emulator,
            adb,
            avd_home,
            running_dirs: running_dirs(env, &home),
        }
    }

    /// Every AVD, by the name `emulator -avd` takes, in name order.
    pub fn avds(&self) -> Vec<Avd> {
        let Ok(entries) = std::fs::read_dir(&self.avd_home) else {
            return Vec::new();
        };
        let mut avds: Vec<Avd> = entries
            .flatten()
            .filter_map(|entry| {
                let file = entry.path();
                if file.extension()? != "ini" {
                    return None;
                }
                let name = file.file_stem()?.to_str()?.to_owned();
                let pointer = parse_ini(&std::fs::read_to_string(&file).ok()?);
                let dir = pointer
                    .get("path")
                    .map(PathBuf::from)
                    .filter(|p| p.is_dir())
                    .or_else(|| {
                        let rel = pointer.get("path.rel")?;
                        Some(self.avd_home.parent()?.join(rel)).filter(|p| p.is_dir())
                    })
                    .unwrap_or_else(|| self.avd_home.join(format!("{name}.avd")));
                let config = std::fs::read_to_string(dir.join("config.ini"))
                    .map(|text| parse_ini(&text))
                    .unwrap_or_default();
                Some(Avd::from_config(
                    name,
                    &pointer,
                    &config,
                    self.root.as_deref(),
                ))
            })
            .collect();
        avds.sort_by(|a, b| a.name.cmp(&b.name));
        avds
    }

    pub fn avd(&self, name: &str) -> Option<Avd> {
        self.avds().into_iter().find(|a| a.name == name)
    }

    /// Emulators that say they are running (some may have died without
    /// removing their file: check before trusting one).
    pub fn running(&self) -> Vec<Running> {
        let mut found = Vec::new();
        for dir in &self.running_dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let file = entry.path();
                let Some(pid) = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix("pid_")?.strip_suffix(".ini"))
                    .and_then(|n| n.parse().ok())
                else {
                    continue;
                };
                if let Ok(text) = std::fs::read_to_string(&file)
                    && let Some(running) = Running::parse(pid, &text)
                {
                    found.push(running);
                }
            }
        }
        found
    }
}

/// Where Android Studio puts the SDK by default.
fn default_root(env: &HashMap<String, String>, home: &Path) -> Option<PathBuf> {
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

/// Where the emulator writes its discovery files: its temporary folder's
/// `avd/running`, which differs per platform.
fn running_dirs(env: &HashMap<String, String>, home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(home.join("Library/Caches/TemporaryItems/avd/running"));
    } else if cfg!(windows) {
        for key in ["TEMP", "TMP"] {
            if let Some(t) = var(env, key) {
                dirs.push(Path::new(t).join("avd").join("running"));
            }
        }
        if let Some(local) = var(env, "LOCALAPPDATA") {
            dirs.push(Path::new(local).join("Temp").join("avd").join("running"));
        }
    } else {
        if let Some(xdg) = var(env, "XDG_RUNTIME_DIR") {
            dirs.push(Path::new(xdg).join("avd/running"));
        }
        let user = var(env, "USER").unwrap_or_default();
        dirs.push(PathBuf::from(format!("/tmp/android-{user}/avd/running")));
    }
    dirs.dedup();
    dirs
}

/// `key=value` lines; later keys win, comments and blanks skipped.
pub fn parse_ini(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with(';'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// One virtual device, as its files describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct Avd {
    /// What `emulator -avd` takes ("Pixel_8_Play").
    pub name: String,
    /// What the user named it ("Pixel 8 Play").
    pub display: String,
    /// "Android 15", when its system image is known.
    pub os: String,
    pub width: u32,
    pub height: u32,
    /// Dots per inch (`hw.lcd.density`); 0 when unknown.
    pub density: u32,
    /// It has a hardware keyboard (`hw.keyboard`): without one the system
    /// has no device for the emulator's key events, and keys go through
    /// adb instead.
    pub keyboard: bool,
    /// How it folds, when it is a foldable.
    pub fold: Option<super::fold::FoldConfig>,
}

impl Avd {
    fn from_config(
        name: String,
        pointer: &HashMap<String, String>,
        config: &HashMap<String, String>,
        sdk: Option<&Path>,
    ) -> Self {
        let display = config
            .get("avd.ini.displayname")
            .filter(|d| !d.is_empty())
            .cloned()
            .unwrap_or_else(|| name.replace('_', " "));
        let num = |key: &str| config.get(key).and_then(|v| v.parse().ok()).unwrap_or(0);
        // The system image says which Android it is; the AVD's `target`
        // (android-35) when the image can't be read.
        let image = config
            .get("image.sysdir.1")
            .zip(sdk)
            .and_then(|(dir, sdk)| {
                std::fs::read_to_string(sdk.join(dir).join("source.properties")).ok()
            })
            .map(|text| parse_ini(&text));
        let api = image
            .as_ref()
            .and_then(|p| p.get("AndroidVersion.ApiLevel").cloned())
            .or_else(|| {
                config
                    .get("target")
                    .or_else(|| pointer.get("target"))
                    .and_then(|t| t.rsplit('-').next())
                    .map(str::to_owned)
            });
        let codename = image
            .as_ref()
            .and_then(|p| p.get("AndroidVersion.CodeName").cloned());
        Self {
            name,
            display,
            os: android_version(api.as_deref(), codename.as_deref()),
            width: num("hw.lcd.width"),
            height: num("hw.lcd.height"),
            density: num("hw.lcd.density"),
            keyboard: config
                .get("hw.keyboard")
                .is_some_and(|v| v == "yes" || v == "true"),
            fold: super::fold::FoldConfig::parse(config),
        }
    }
}

/// "Android 15" for API level 35, and so on.
pub fn android_version(api: Option<&str>, codename: Option<&str>) -> String {
    if let Some(code) = codename.filter(|c| !c.is_empty()) {
        return format!("Android {code}");
    }
    let Some(api) = api else {
        return "Android".into();
    };
    // `36.1`: a minor release of 36.
    let level: Option<u32> = api.split('.').next().and_then(|l| l.parse().ok());
    let name = match level {
        Some(n @ 33..) => (n - 20).to_string(),
        Some(32) => "12L".into(),
        Some(31) => "12".into(),
        Some(30) => "11".into(),
        Some(29) => "10".into(),
        Some(28) => "9".into(),
        Some(27) => "8.1".into(),
        Some(26) => "8.0".into(),
        Some(25) => "7.1".into(),
        Some(24) => "7.0".into(),
        Some(23) => "6.0".into(),
        _ => return format!("Android (API {api})"),
    };
    format!("Android {name}")
}

/// A running emulator, from its discovery file.
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub pid: u32,
    pub avd: String,
    /// Its gRPC control service, on 127.0.0.1.
    pub grpc_port: Option<u16>,
    /// Present when it was started with `-grpc-use-token` (as Agentloft
    /// does); emulators Android Studio starts use signed JWTs instead.
    pub token: Option<String>,
    /// Its console port: adb calls it `emulator-<port>`.
    pub console_port: Option<u16>,
}

impl Running {
    pub fn parse(pid: u32, text: &str) -> Option<Self> {
        let ini = parse_ini(text);
        let avd = ini
            .get("avd.name")
            .or_else(|| ini.get("avd.id"))
            .filter(|n| !n.is_empty())?
            .clone();
        Some(Self {
            pid,
            avd,
            grpc_port: ini.get("grpc.port").and_then(|p| p.parse().ok()),
            token: ini.get("grpc.token").filter(|t| !t.is_empty()).cloned(),
            console_port: ini.get("port.serial").and_then(|p| p.parse().ok()),
        })
    }

    /// Its name to adb.
    pub fn serial(&self) -> Option<String> {
        self.console_port.map(|p| format!("emulator-{p}"))
    }
}

/// The serials of `adb devices` that are emulators ready for commands.
pub fn adb_emulators(output: &str) -> Vec<String> {
    output
        .lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let serial = parts.next()?;
            (serial.starts_with("emulator-") && parts.next() == Some("device"))
                .then(|| serial.to_owned())
        })
        .collect()
}

/// Quote `arg` for the device's shell: `adb shell` joins its arguments into
/// one command line.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_avd_and_its_system_image() {
        let dir = tempfile::tempdir().unwrap();
        let sdk = dir.path().join("sdk");
        let image = sdk.join("system-images/android-35/google_apis_playstore/arm64-v8a");
        std::fs::create_dir_all(&image).unwrap();
        std::fs::write(
            image.join("source.properties"),
            "Pkg.Revision=9\nAndroidVersion.ApiLevel=35\n",
        )
        .unwrap();
        let avd_home = dir.path().join("home/.android/avd");
        let avd_dir = avd_home.join("Pixel_8_Play.avd");
        std::fs::create_dir_all(&avd_dir).unwrap();
        std::fs::write(
            avd_home.join("Pixel_8_Play.ini"),
            format!(
                "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/Pixel_8_Play.avd\ntarget=android-35\n",
                avd_dir.display()
            ),
        )
        .unwrap();
        std::fs::write(
            avd_dir.join("config.ini"),
            "hw.lcd.width=1080\nhw.lcd.height=2400\nhw.lcd.density=420\nhw.keyboard=yes\n\
             image.sysdir.1=system-images/android-35/google_apis_playstore/arm64-v8a/\n",
        )
        .unwrap();
        // A second one named by the user, whose image is missing.
        let other = avd_home.join("tablet.avd");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(avd_home.join("tablet.ini"), "path.rel=avd/tablet.avd\n").unwrap();
        std::fs::write(
            other.join("config.ini"),
            "avd.ini.displayname=My Tablet\ntarget=android-36\n",
        )
        .unwrap();
        // Not an AVD.
        std::fs::write(avd_home.join("notes.txt"), "x").unwrap();

        let env = HashMap::from([
            ("ANDROID_HOME".to_owned(), sdk.display().to_string()),
            (
                "ANDROID_AVD_HOME".to_owned(),
                avd_home.display().to_string(),
            ),
        ]);
        let found = Sdk::find(&env);
        assert_eq!(found.root.as_deref(), Some(sdk.as_path()));
        assert_eq!(
            found.avds(),
            vec![
                Avd {
                    name: "Pixel_8_Play".into(),
                    display: "Pixel 8 Play".into(),
                    os: "Android 15".into(),
                    width: 1080,
                    height: 2400,
                    density: 420,
                    keyboard: true,
                    fold: None,
                },
                Avd {
                    name: "tablet".into(),
                    display: "My Tablet".into(),
                    os: "Android 16".into(),
                    width: 0,
                    height: 0,
                    density: 0,
                    keyboard: false,
                    fold: None,
                },
            ]
        );
    }

    #[test]
    fn names_android_versions() {
        assert_eq!(android_version(Some("35"), None), "Android 15");
        assert_eq!(android_version(Some("36.1"), None), "Android 16");
        assert_eq!(android_version(Some("32"), None), "Android 12L");
        assert_eq!(android_version(Some("27"), None), "Android 8.1");
        assert_eq!(android_version(Some("19"), None), "Android (API 19)");
        assert_eq!(
            android_version(Some("36"), Some("Baklava")),
            "Android Baklava"
        );
        assert_eq!(android_version(None, None), "Android");
    }

    #[test]
    fn reads_a_running_emulators_discovery_file() {
        let text = "emulator.build=16740729\navd.id=Pixel_8_Play\nport.serial=5554\n\
                    port.adb=5555\navd.name=Pixel_8_Play\ngrpc.token=secret\n\
                    grpc.port=8554\ncmdline=\"/sdk/emulator/qemu\" \"-avd\" \"x=y\"\n";
        let running = Running::parse(90616, text).unwrap();
        assert_eq!(
            running,
            Running {
                pid: 90616,
                avd: "Pixel_8_Play".into(),
                grpc_port: Some(8554),
                token: Some("secret".into()),
                console_port: Some(5554),
            }
        );
        assert_eq!(running.serial().as_deref(), Some("emulator-5554"));
        // Android Studio's: a JWT key instead of a token.
        let studio = Running::parse(
            1,
            "avd.name=Pixel\ngrpc.port=8556\ngrpc.jwks=/tmp/jwks\nport.serial=5556\n",
        )
        .unwrap();
        assert_eq!(studio.token, None);
        assert!(Running::parse(2, "port.serial=5554\n").is_none());
    }

    #[test]
    fn finds_ready_emulators_in_adb_devices() {
        let out = "* daemon started successfully\nList of devices attached\n\
                   emulator-5554\tdevice\nemulator-5556\toffline\n\
                   R5CT1234\tdevice\n\n";
        assert_eq!(adb_emulators(out), vec!["emulator-5554".to_owned()]);
    }

    #[test]
    fn quotes_for_the_device_shell() {
        assert_eq!(shell_quote("com.example.app"), "com.example.app");
        assert_eq!(
            shell_quote("https://example.com/a?b=1&c=2"),
            "'https://example.com/a?b=1&c=2'"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
