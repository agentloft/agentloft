//! Getting `cloudflared` when this computer has none (ADR-0019 §4a): a
//! pinned release, downloaded from Cloudflare's GitHub releases the first
//! time the public tunnel is wanted, checked against the SHA-256 sums
//! below, and kept in `<home>/web/cloudflared-bin/<version>/`. One
//! already installed (on PATH, or set as `[web] cloudflared_cli`) is
//! always preferred: this is only for people who have none.
//!
//! Updating the pin: set [`VERSION`], then for each asset take the sum of
//! the file as downloaded (`shasum -a 256`) and, for the macOS archives,
//! of the `cloudflared` inside, which is the sum Cloudflare's release notes
//! list for them (they list the files' own sums for the others).

use sha2::Digest as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The cloudflared release this build installs.
pub(crate) const VERSION: &str = "2026.11.0";

/// Largest download accepted (the Windows binary is 55 MB).
const MAX_DOWNLOAD: u64 = 120 * 1024 * 1024;

struct Pinned {
    asset: &'static str,
    /// SHA-256 of the file as downloaded.
    sha256: &'static str,
    /// A `.tgz`: SHA-256 of the `cloudflared` inside.
    inner_sha256: Option<&'static str>,
}

fn pinned() -> Option<Pinned> {
    let (asset, sha256, inner_sha256) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => (
            "cloudflared-darwin-arm64.tgz",
            "8c2f90277ef06a6080bb40b964d33f09f1188102f8798f2e344174b6cf41accf",
            Some("97e24268d9704a570fab276293028f3580ae2c7d5d2678621a4809e90f776a5e"),
        ),
        ("macos", "x86_64") => (
            "cloudflared-darwin-amd64.tgz",
            "8a59d51120cebbe902f0a8d6e1b7a7fd4f9a5bb4dcb3e0f05872483e75e7db6b",
            Some("be6be8d4aa0c3ff66917e265002fc9f702d42eae877edf495143f5b874c424af"),
        ),
        ("linux", "x86_64") => (
            "cloudflared-linux-amd64",
            "51f7c68b57ed7269164075d7e789efce5b59355982f06392deeb037d982a51fd",
            None,
        ),
        ("linux", "aarch64") => (
            "cloudflared-linux-arm64",
            "abe54a006acf9faf2d19ee9d953cb0d0804e92f59c5382a162e57b84d086f56f",
            None,
        ),
        ("windows", "x86_64") => (
            "cloudflared-windows-amd64.exe",
            "d5c1d82031da6253dcdcff64c944cd0fe063016edeb2e4fd088075fc9bd52d0a",
            None,
        ),
        _ => return None,
    };
    Some(Pinned {
        asset,
        sha256,
        inner_sha256,
    })
}

fn file_name() -> &'static str {
    if cfg!(windows) {
        "cloudflared.exe"
    } else {
        "cloudflared"
    }
}

/// Where the pinned `cloudflared` is (or would be) installed under `dir`
/// (`<home>/web`).
pub(crate) fn installed_path(dir: &Path) -> PathBuf {
    dir.join("cloudflared-bin").join(VERSION).join(file_name())
}

/// The installed copy, if there is one.
pub(crate) fn installed(dir: &Path) -> Option<PathBuf> {
    let path = installed_path(dir);
    path.is_file().then_some(path)
}

/// Whether this platform has a pinned download.
pub(crate) fn available() -> bool {
    pinned().is_some()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex(&sha2::Sha256::digest(bytes))
}

/// Download, check and install the pinned `cloudflared` under `dir`.
/// Blocking: run it on a blocking thread.
pub(crate) fn install(dir: &Path) -> Result<PathBuf, String> {
    let pinned = pinned().ok_or(
        "Agentloft can't fetch cloudflared for this computer: install it yourself, then try again.",
    )?;
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{VERSION}/{}",
        pinned.asset
    );
    let bytes = download(&url)?;
    if sha256(&bytes) != pinned.sha256 {
        return Err(
            "The cloudflared download didn't match its checksum; nothing was installed.".into(),
        );
    }
    let target = installed_path(dir);
    let folder = target.parent().ok_or("no folder")?.to_path_buf();
    // Built beside the final folder, then renamed into place: a copy cut
    // short is never taken for a whole one.
    let staging = folder.with_extension("partial");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("couldn't make {}: {e}", staging.display()))?;
    let result = (|| {
        let binary = match pinned.inner_sha256 {
            None => bytes,
            Some(inner) => {
                let archive = staging.join(pinned.asset);
                std::fs::write(&archive, &bytes).map_err(|e| e.to_string())?;
                let out = std::process::Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&staging)
                    .arg("cloudflared")
                    .output()
                    .map_err(|e| format!("couldn't unpack cloudflared: {e}"))?;
                let _ = std::fs::remove_file(&archive);
                if !out.status.success() {
                    return Err("couldn't unpack cloudflared".to_owned());
                }
                let unpacked = staging.join("cloudflared");
                let binary = std::fs::read(&unpacked).map_err(|e| e.to_string())?;
                let _ = std::fs::remove_file(&unpacked);
                if sha256(&binary) != inner {
                    return Err("The unpacked cloudflared didn't match its checksum; nothing was installed.".to_owned());
                }
                binary
            }
        };
        let file = staging.join(file_name());
        std::fs::write(&file, &binary).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::rename(&staging, &folder).map_err(|e| e.to_string())?;
        Ok(target.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let installed = result?;
    tracing::info!(path = %installed.display(), version = VERSION, "cloudflare: installed cloudflared");
    Ok(installed)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // As in website.rs: the OS's TLS stack and its certificates on macOS
    // and Windows, rustls elsewhere.
    let (provider, roots) = if cfg!(any(target_os = "macos", windows)) {
        (TlsProvider::NativeTls, RootCerts::PlatformVerifier)
    } else {
        (TlsProvider::Rustls, RootCerts::WebPki)
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(provider)
                .root_certs(roots)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(300)))
        .user_agent(concat!("agentloft/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| format!("couldn't download cloudflared: {e}"))?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
        .map_err(|e| format!("couldn't download cloudflared: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real download, checked and run (`--ignored`: it fetches about
    /// 20–55 MB from GitHub).
    #[test]
    #[ignore]
    fn installs_the_pinned_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = install(dir.path()).unwrap();
        assert_eq!(installed(dir.path()), Some(path.clone()));
        let out = std::process::Command::new(&path)
            .arg("--version")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(said.contains(VERSION), "{said}");
        assert!(
            !dir.path()
                .join("cloudflared-bin")
                .join(format!("{VERSION}.partial"))
                .exists()
        );
    }

    #[test]
    fn sums_are_sha256_hex() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        if let Some(p) = pinned() {
            for sum in std::iter::once(p.sha256).chain(p.inner_sha256) {
                assert_eq!(sum.len(), 64);
                assert!(sum.chars().all(|c| c.is_ascii_hexdigit()));
            }
        }
    }
}
