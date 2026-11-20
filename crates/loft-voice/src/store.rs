//! Model files on disk: `<home>/models/<model id>/<file>`.
//!
//! A download goes to `<file>.part` (resumed with a range request if one is
//! there), is checked against the catalog's SHA-256, and only then renamed
//! into place: a file under its own name is always a whole, checked one.
//! Before downloading, a copy another program already has is looked for
//! (the Hugging Face cache, the other Agentloft home) and, once its
//! checksum matches, linked in instead.

use crate::ModelFile;
use anyhow::{Context as _, Result, bail};
use loft_protocol::VoiceModelState;
use sha2::Digest as _;
use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Store {
    dir: PathBuf,
    downloads: Mutex<HashMap<String, Arc<Download>>>,
    /// Why a model's last download failed.
    errors: Mutex<HashMap<String, String>>,
}

struct Download {
    done: AtomicU64,
    total: u64,
    verifying: AtomicBool,
    cancel: AtomicBool,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            dir: dir.into(),
            downloads: Mutex::default(),
            errors: Mutex::default(),
        })
    }

    /// Where a model's files are.
    pub fn model_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// Every file is there (a file under its own name was checked when it
    /// arrived; the size catches one cut short by something else).
    pub fn installed(&self, id: &str, files: &[ModelFile]) -> bool {
        let dir = self.model_dir(id);
        files.iter().all(|f| {
            std::fs::metadata(dir.join(f.name)).is_ok_and(|m| m.is_file() && m.len() == f.size)
        })
    }

    pub fn state(&self, id: &str, files: &[ModelFile]) -> VoiceModelState {
        if let Some(d) = self.downloads.lock().unwrap().get(id) {
            return if d.verifying.load(Ordering::Relaxed) {
                VoiceModelState::Verifying
            } else {
                VoiceModelState::Downloading {
                    done: d.done.load(Ordering::Relaxed),
                    total: d.total,
                }
            };
        }
        if self.installed(id, files) {
            VoiceModelState::Installed
        } else {
            VoiceModelState::NotDownloaded
        }
    }

    pub fn error(&self, id: &str) -> Option<String> {
        self.errors.lock().unwrap().get(id).cloned()
    }

    /// Start fetching a model's files on a thread of its own; nothing if
    /// it is already being fetched or is there.
    pub fn download(self: &Arc<Self>, id: &str, files: &'static [ModelFile]) {
        if self.installed(id, files) {
            return;
        }
        let download = {
            let mut downloads = self.downloads.lock().unwrap();
            if downloads.contains_key(id) {
                return;
            }
            let d = Arc::new(Download {
                done: AtomicU64::new(0),
                total: files.iter().map(|f| f.size).sum(),
                verifying: AtomicBool::new(false),
                cancel: AtomicBool::new(false),
            });
            downloads.insert(id.to_owned(), d.clone());
            d
        };
        self.errors.lock().unwrap().remove(id);
        let store = self.clone();
        let id = id.to_owned();
        std::thread::Builder::new()
            .name(format!("download {id}"))
            .spawn(move || {
                let result = store.fetch_all(&id, files, &download);
                store.downloads.lock().unwrap().remove(&id);
                match result {
                    Ok(()) => tracing::info!("voice model {id} downloaded"),
                    Err(_) if download.cancel.load(Ordering::Relaxed) => {}
                    Err(err) => {
                        tracing::warn!("voice model {id}: {err:#}");
                        store.errors.lock().unwrap().insert(id, format!("{err:#}"));
                    }
                }
            })
            .expect("spawn a download thread");
    }

    pub fn cancel(&self, id: &str) {
        if let Some(d) = self.downloads.lock().unwrap().get(id) {
            d.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Delete a model's files (a download of it is stopped first).
    pub fn remove(&self, id: &str) -> Result<()> {
        self.cancel(id);
        // The download thread notices within a read; give it a moment so
        // it doesn't write a `.part` back after the folder is gone.
        for _ in 0..50 {
            if !self.downloads.lock().unwrap().contains_key(id) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.errors.lock().unwrap().remove(id);
        let dir = self.model_dir(id);
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("delete {}", dir.display()))
            }
            _ => Ok(()),
        }
    }

    fn fetch_all(&self, id: &str, files: &[ModelFile], d: &Download) -> Result<()> {
        let dir = self.model_dir(id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let mut before = 0;
        for file in files {
            let dest = dir.join(file.name);
            if !std::fs::metadata(&dest).is_ok_and(|m| m.len() == file.size) {
                d.verifying.store(false, Ordering::Relaxed);
                if !self.reuse(id, file, &dest) {
                    fetch(file, &dest, before, d)?;
                }
            }
            before += file.size;
            d.done.store(before, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Link in a copy of `file` that is already on this computer, if one
    /// with the right checksum is found.
    fn reuse(&self, id: &str, file: &ModelFile, dest: &Path) -> bool {
        for found in self.copies_elsewhere(id, file) {
            let Ok(found) = found.canonicalize() else {
                continue;
            };
            if !std::fs::metadata(&found).is_ok_and(|m| m.len() == file.size)
                || !sha256_of(&found, None).is_ok_and(|sum| sum == file.sha256)
            {
                continue;
            }
            // A hard link takes no space and outlives the other copy;
            // across volumes, a copy.
            let linked = std::fs::hard_link(&found, dest)
                .or_else(|_| std::fs::copy(&found, dest).map(|_| ()));
            if linked.is_ok() {
                tracing::info!(
                    "voice model file {} reused from {}",
                    file.name,
                    found.display()
                );
                return true;
            }
        }
        false
    }

    fn copies_elsewhere(&self, id: &str, file: &ModelFile) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let home = std::env::home_dir();
        // The Hugging Face cache: `models--<org>--<repo>/snapshots/<rev>/<file>`.
        if let Some(rest) = file.url.strip_prefix("https://huggingface.co/")
            && let [org, repo, "resolve", rev, ..] = rest.split('/').collect::<Vec<_>>()[..]
        {
            let hub = std::env::var_os("HF_HUB_CACHE")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HF_HOME").map(|h| PathBuf::from(h).join("hub")))
                .or_else(|| home.as_ref().map(|h| h.join(".cache/huggingface/hub")));
            if let Some(hub) = hub {
                found.push(
                    hub.join(format!("models--{org}--{repo}"))
                        .join("snapshots")
                        .join(rev)
                        .join(file.name),
                );
            }
        }
        // The installed app's and dev builds' homes keep the same layout.
        if let Some(home) = home {
            for other in [".agentloft", ".agentloft-dev"] {
                let path = home.join(other).join("models").join(id).join(file.name);
                if !path.starts_with(&self.dir) {
                    found.push(path);
                }
            }
        }
        found.into_iter().filter(|p| p.is_file()).collect()
    }
}

/// Fetch one file into `dest` (through `<dest>.part`), checking it.
fn fetch(file: &ModelFile, dest: &Path, before: u64, d: &Download) -> Result<()> {
    let part = dest.with_file_name(format!("{}.part", file.name));
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let have = if have > file.size { 0 } else { have };
    let agent = http_agent();
    let mut request = agent.get(file.url);
    if have > 0 {
        request = request.header("Range", format!("bytes={have}-"));
    }
    let mut response = request
        .call()
        .with_context(|| format!("download {}", file.url))?;
    let resumed = have > 0 && response.status() == 206;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(resumed)
        .truncate(!resumed)
        .open(&part)
        .with_context(|| format!("write {}", part.display()))?;
    let mut done = if resumed { have } else { 0 };
    d.done.store(before + done, Ordering::Relaxed);
    let mut body = response
        .body_mut()
        .with_config()
        .limit(file.size + 1)
        .reader();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        if d.cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        let n = body.read(&mut buf).context("download")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("write the download")?;
        done += n as u64;
        d.done.store(before + done, Ordering::Relaxed);
    }
    out.flush()?;
    drop(out);
    if done != file.size {
        bail!(
            "the download stopped at {done} of {} bytes; try again",
            file.size
        );
    }
    d.verifying.store(true, Ordering::Relaxed);
    let sum = sha256_of(&part, Some(&d.cancel))?;
    if sum != file.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!(
            "{} arrived damaged (its checksum doesn't match); try again",
            file.name
        );
    }
    std::fs::rename(&part, dest).with_context(|| format!("move {} into place", file.name))?;
    Ok(())
}

pub(crate) fn sha256_of(path: &Path, cancel: Option<&AtomicBool>) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            bail!("cancelled");
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn http_agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    // Cargo.toml picks the TLS stack per platform; ureq has to be told which
    // (and, with the OS's stack, to trust the OS's certificates).
    let (provider, roots) = if cfg!(any(target_os = "macos", windows)) {
        (TlsProvider::NativeTls, RootCerts::PlatformVerifier)
    } else {
        (TlsProvider::Rustls, RootCerts::WebPki)
    };
    ureq::Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(provider)
                .root_certs(roots)
                .build(),
        )
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        // Gigabytes take a while; a stalled one can be cancelled.
        .timeout_global(None)
        .user_agent("agentloft")
        .build()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_is_not_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(tmp.path());
        let files = [ModelFile {
            name: "m.gguf",
            url: "https://example.invalid/m.gguf",
            sha256: "00",
            size: 4,
        }];
        assert_eq!(store.state("m", &files), VoiceModelState::NotDownloaded);
        std::fs::create_dir_all(store.model_dir("m")).unwrap();
        std::fs::write(store.model_dir("m").join("m.gguf"), b"abc").unwrap();
        assert!(!store.installed("m", &files));
        std::fs::write(store.model_dir("m").join("m.gguf"), b"abcd").unwrap();
        assert_eq!(store.state("m", &files), VoiceModelState::Installed);
        store.remove("m").unwrap();
        assert!(!store.model_dir("m").exists());
    }

    #[test]
    fn checksums_are_lowercase_hex() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_of(&path, None).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
