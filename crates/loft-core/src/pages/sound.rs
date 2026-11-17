//! The sound of the daemon's browser (ADR-0017 §6). Chrome plays a page's
//! sound on the speakers of the computer the daemon runs on, so a tab is
//! muted unless someone there is listening: an Agentloft window on this
//! computer is showing it ([`loft_protocol::SOUND`]), and nobody elsewhere
//! (a phone, another computer) used it since.
//!
//! The DevTools protocol can't mute one tab, only the whole browser
//! (`--mute-audio`). Chrome's own tab muting is reached through a small
//! extension of ours (`sound/`), loaded over the pipe with
//! `Extensions.loadUnpacked`. It mutes every new tab, and the daemon tells
//! it which tabs may be heard. A browser that won't load it (an older
//! Chrome, another Chromium) is started muted, as before.

use super::cdp::Cdp;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MANIFEST: &str = include_str!("sound/manifest.json");
const WORKER: &str = include_str!("sound/worker.js");

/// The extension that mutes and unmutes tabs, once loaded.
pub(super) struct Sound {
    /// The extension's id (the start of its worker's address).
    extension: String,
    /// Held while the tabs to be heard are worked out and told, so an older
    /// list never lands after a newer one.
    told: tokio::sync::Mutex<Told>,
}

#[derive(Default)]
struct Told {
    /// The DevTools session attached to the extension's worker. Staying
    /// attached keeps the worker alive.
    worker: Option<String>,
    /// The targets it was last told may be heard.
    loud: Vec<String>,
}

/// Where the extension's files are written: beside the profile.
fn folder(profile: &Path) -> PathBuf {
    profile.with_file_name("sound")
}

impl Sound {
    /// Load the extension into a browser started with
    /// [`super::chrome::args`]`(.., true)`.
    pub(super) async fn load(cdp: &Cdp, profile: &Path) -> Result<Self, String> {
        let dir = folder(profile);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("manifest.json"), MANIFEST).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("worker.js"), WORKER).map_err(|e| e.to_string())?;
        let loaded = cdp
            .send(
                "Extensions.loadUnpacked",
                json!({ "path": dir.to_string_lossy() }),
                None,
            )
            .await?;
        let extension = loaded["id"].as_str().ok_or("no extension id")?.to_owned();
        let sound = Self {
            extension,
            told: tokio::sync::Mutex::default(),
        };
        let worker = sound.find_worker(cdp).await;
        sound.told.lock().await.worker = worker;
        Ok(sound)
    }

    /// Attach to the extension's worker, waiting a little for it to start.
    async fn find_worker(&self, cdp: &Cdp) -> Option<String> {
        let prefix = format!("chrome-extension://{}/", self.extension);
        for _ in 0..30 {
            let targets = cdp.send("Target.getTargets", json!({}), None).await.ok()?;
            let worker = targets["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|t| {
                    t["type"] == "service_worker"
                        && t["url"].as_str().is_some_and(|u| u.starts_with(&prefix))
                })
                .and_then(|t| t["targetId"].as_str());
            if let Some(target) = worker {
                let attached = cdp
                    .send(
                        "Target.attachToTarget",
                        json!({ "targetId": target, "flatten": true }),
                        None,
                    )
                    .await
                    .ok()?;
                return attached["sessionId"].as_str().map(str::to_owned);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::warn!("the browser's sound extension didn't start");
        None
    }

    /// Run `expression` in the extension's worker (finding it again if it
    /// was lost) and return its value.
    async fn call(&self, cdp: &Cdp, told: &mut Told, expression: &str) -> Result<Value, String> {
        for _ in 0..2 {
            if told.worker.is_none() {
                told.worker = self.find_worker(cdp).await;
            }
            let Some(session) = &told.worker else {
                break;
            };
            let answer = cdp
                .send(
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "awaitPromise": true,
                        "returnByValue": true,
                    }),
                    Some(session),
                )
                .await;
            match answer {
                Ok(answer) if answer.get("exceptionDetails").is_none() => {
                    return Ok(answer["result"]["value"].clone());
                }
                Ok(answer) => return Err(answer["exceptionDetails"]["text"].to_string()),
                // The worker is gone: find it again, once.
                Err(_) => told.worker = None,
            }
        }
        Err("the sound extension isn't running".into())
    }

    /// Let the targets `loud` gives be heard, and mute every other tab.
    /// `loud` is asked in turn, so the newest answer is the one told.
    pub(super) async fn tell(&self, cdp: &Cdp, loud: impl FnOnce() -> Vec<String>) {
        let mut told = self.told.lock().await;
        let mut loud = loud();
        loud.sort();
        if told.loud == loud {
            return;
        }
        let expression = format!("hear({})", Value::from(loud.clone()));
        match self.call(cdp, &mut told, &expression).await {
            Ok(_) => told.loud = loud,
            Err(err) => tracing::warn!(%err, "couldn't mute or unmute the browser's tabs"),
        }
    }

    /// Whether `target`'s tab is muted (`None`: no such tab).
    #[cfg(test)]
    pub(super) async fn muted(&self, cdp: &Cdp, target: &str) -> Option<bool> {
        let mut told = self.told.lock().await;
        let expression = format!("muted({})", Value::from(target));
        self.call(cdp, &mut told, &expression).await.ok()?.as_bool()
    }
}
