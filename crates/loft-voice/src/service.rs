//! The daemon's side of voice input: the catalog with each model's state,
//! downloads, and the engine processes.
//!
//! There are two engines, one for the speech model and one for a local
//! cleanup model, each a process of its own started on first use. After
//! each use a timer starts; if the engine isn't used again before
//! `[voice] unload_after`, its process is ended, which frees everything
//! the model held. The next recording starts it again (a second or two).

use crate::store::Store;
use crate::wire::{EngineReply, EngineRequest, MARK};
use crate::{FAKE_MODEL, ModelFile, catalog};
use anyhow::{Context as _, Result, anyhow, bail};
use loft_protocol::{Bytes, VoiceModel, VoiceModelState, VoiceStatus};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// Loading a big model the first time (GPU kernels compiled) is slow.
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
/// Five minutes of speech on a CPU, with room to spare.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(600);
const GENERATE_TIMEOUT: Duration = Duration::from_secs(180);
/// Longest cleanup answer (a long transcript, tidied).
const MAX_CLEANUP_TOKENS: usize = 4096;

pub struct VoiceOptions {
    /// Where models are kept (`<home>/models`).
    pub models_dir: PathBuf,
    /// The program that runs an engine and its arguments
    /// (`agentloftd voice-engine`); `None`: voice input isn't available.
    pub engine: Option<(PathBuf, Vec<String>)>,
    /// Also offer [`FAKE_MODEL`] (`AGENTLOFT_FAKE_VOICE=1`), which needs no
    /// download and answers canned text.
    pub fake: bool,
}

#[derive(Clone)]
pub struct Voice(Arc<Inner>);

struct Inner {
    opts: VoiceOptions,
    store: Arc<Store>,
    speech: Arc<Slot>,
    text: Arc<Slot>,
}

/// One engine process, started on demand.
struct Slot {
    kind: &'static str,
    proc: tokio::sync::Mutex<Option<Engine>>,
    /// Bumped by each use; an unload timer only fires if nothing used the
    /// engine since it was set.
    uses: AtomicU64,
    /// The model the process holds, readable without waiting for the
    /// process lock (`VoiceStatus::loaded`).
    loaded: std::sync::Mutex<Option<String>>,
}

struct Engine {
    model: String,
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::io::Lines<BufReader<ChildStdout>>,
}

impl Voice {
    pub fn new(opts: VoiceOptions) -> Self {
        let store = Store::new(opts.models_dir.clone());
        let slot = |kind| {
            Arc::new(Slot {
                kind,
                proc: tokio::sync::Mutex::new(None),
                uses: AtomicU64::new(0),
                loaded: std::sync::Mutex::new(None),
            })
        };
        Self(Arc::new(Inner {
            opts,
            store,
            speech: slot("speech"),
            text: slot("cleanup"),
        }))
    }

    pub fn status(&self) -> VoiceStatus {
        let store = &self.0.store;
        let mut models: Vec<VoiceModel> = catalog::MODELS
            .iter()
            .map(|m| VoiceModel {
                id: m.id.into(),
                name: m.name.into(),
                description: m.description.into(),
                size: m.file.size,
                languages: m.languages.iter().map(|l| (*l).to_owned()).collect(),
                recommended: m.recommended,
                english_error: m.english_error,
                seconds_per_30s: m.seconds_per_30s,
                state: store.state(m.id, std::slice::from_ref(&m.file)),
                error: store.error(m.id),
            })
            .collect();
        let mut cleanup_models: Vec<VoiceModel> = crate::cleanup::models()
            .map(|m| VoiceModel {
                id: m.id.into(),
                name: m.name.into(),
                description: m.description.into(),
                size: m.files.iter().map(|f| f.size).sum(),
                recommended: m.recommended,
                state: store.state(m.id, m.files),
                error: store.error(m.id),
                ..VoiceModel::default()
            })
            .collect();
        if self.0.opts.fake {
            let fake = VoiceModel {
                id: FAKE_MODEL.into(),
                name: "Test model".into(),
                description: "Answers made-up text, for testing".into(),
                state: VoiceModelState::Installed,
                ..VoiceModel::default()
            };
            models.push(VoiceModel {
                languages: vec!["en".into()],
                ..fake.clone()
            });
            cleanup_models.push(fake);
        }
        let loaded = [&self.0.speech, &self.0.text]
            .iter()
            .filter_map(|s| s.loaded.lock().unwrap().clone())
            .collect();
        VoiceStatus {
            unsupported: self
                .0
                .opts
                .engine
                .is_none()
                .then(|| "Voice input isn't available in this build.".to_owned()),
            models,
            cleanup_models,
            loaded,
        }
    }

    pub fn download(&self, id: &str) -> Result<()> {
        let files = files_of(id).ok_or_else(|| anyhow!("no model is called `{id}`"))?;
        self.0.store.download(id, files);
        Ok(())
    }

    pub fn cancel_download(&self, id: &str) {
        self.0.store.cancel(id);
    }

    /// Delete a model's files, unloading it first if it is in memory.
    pub async fn remove(&self, id: &str) -> Result<()> {
        if id == FAKE_MODEL {
            return Ok(());
        }
        for slot in [&self.0.speech, &self.0.text] {
            let mut proc = slot.proc.lock().await;
            if proc.as_ref().is_some_and(|e| e.model == id) {
                slot.end(&mut proc).await;
            }
        }
        let store = self.0.store.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.remove(&id)).await?
    }

    /// End both engines now.
    pub async fn unload(&self) {
        for slot in [&self.0.speech, &self.0.text] {
            let mut proc = slot.proc.lock().await;
            slot.end(&mut proc).await;
        }
    }

    /// `[voice] unload_after` changed: an idle engine goes by the new time.
    pub fn unload_after_changed(&self, after: Option<Duration>) {
        for slot in [&self.0.speech, &self.0.text] {
            if slot.loaded.lock().unwrap().is_some() {
                schedule_unload(slot.clone(), after);
            }
        }
    }

    /// The text of a recording (16 kHz mono i16 little-endian).
    pub async fn transcribe(
        &self,
        model: &str,
        pcm: Vec<u8>,
        language: Option<String>,
        words: Vec<String>,
        unload_after: Option<Duration>,
    ) -> Result<String> {
        let load = if model == FAKE_MODEL && self.0.opts.fake {
            EngineRequest::LoadSpeech {
                path: PathBuf::new(),
                fake: true,
            }
        } else {
            let m = catalog::model(model)
                .ok_or_else(|| anyhow!("no speech model is called `{model}`"))?;
            let files = std::slice::from_ref(&m.file);
            if !self.0.store.installed(m.id, files) {
                bail!("{} isn't downloaded yet: Settings ▸ Voice", m.name);
            }
            EngineRequest::LoadSpeech {
                path: self.0.store.model_dir(m.id).join(m.file.name),
                fake: false,
            }
        };
        let language = language.filter(|l| !l.is_empty() && l != "auto");
        let request = EngineRequest::Transcribe {
            pcm: Bytes(pcm),
            language,
            words,
        };
        let text = self
            .run(
                &self.0.speech,
                model,
                load,
                request,
                TRANSCRIBE_TIMEOUT,
                unload_after,
            )
            .await?;
        Ok(text.trim().to_owned())
    }

    /// An answer from a local cleanup model.
    pub async fn generate(
        &self,
        model: &str,
        system: String,
        user: String,
        unload_after: Option<Duration>,
    ) -> Result<String> {
        let fake = model == FAKE_MODEL && self.0.opts.fake;
        if !fake {
            let files = files_of(model)
                .filter(|_| crate::cleanup::models().any(|m| m.id == model))
                .ok_or_else(|| anyhow!("no cleanup model is called `{model}`"))?;
            if !self.0.store.installed(model, files) {
                bail!("The cleanup model isn't downloaded yet: Settings ▸ Voice");
            }
        }
        let load = EngineRequest::LoadText {
            model: model.to_owned(),
            dir: self.0.store.model_dir(model),
            fake,
        };
        let request = EngineRequest::Generate {
            system,
            user,
            max_tokens: MAX_CLEANUP_TOKENS,
        };
        self.run(
            &self.0.text,
            model,
            load,
            request,
            GENERATE_TIMEOUT,
            unload_after,
        )
        .await
    }

    async fn run(
        &self,
        slot: &Arc<Slot>,
        model: &str,
        load: EngineRequest,
        request: EngineRequest,
        timeout: Duration,
        unload_after: Option<Duration>,
    ) -> Result<String> {
        let (program, args) = self
            .0
            .opts
            .engine
            .clone()
            .ok_or_else(|| anyhow!("Voice input isn't available in this build."))?;
        slot.uses.fetch_add(1, Ordering::SeqCst);
        let result = {
            let mut proc = slot.proc.lock().await;
            if proc.as_ref().is_some_and(|e| e.model != model) {
                // Another model: a fresh process, so the old one's memory
                // is all given back.
                slot.end(&mut proc).await;
            }
            let result = async {
                if proc.is_none() {
                    let started = std::time::Instant::now();
                    let mut engine = Engine::start(&program, &args, model)?;
                    match tokio::time::timeout(LOAD_TIMEOUT, engine.ask(&load)).await {
                        Ok(Ok(EngineReply::Loaded { backend })) => tracing::info!(
                            "voice {} model {model} loaded on {backend} in {:.1}s",
                            slot.kind,
                            started.elapsed().as_secs_f32()
                        ),
                        Ok(Ok(EngineReply::Error { message })) => bail!("{message}"),
                        Ok(Ok(other)) => bail!("unexpected answer {other:?}"),
                        Ok(Err(err)) => return Err(err),
                        Err(_) => bail!("loading the model took too long"),
                    }
                    *slot.loaded.lock().unwrap() = Some(model.to_owned());
                    *proc = Some(engine);
                }
                let engine = proc.as_mut().expect("started above");
                match tokio::time::timeout(timeout, engine.ask(&request)).await {
                    Ok(Ok(EngineReply::Text { text })) => Ok(text),
                    Ok(Ok(EngineReply::Error { message })) => Err(anyhow!("{message}")),
                    Ok(Ok(other)) => Err(anyhow!("unexpected answer {other:?}")),
                    Ok(Err(err)) => Err(err),
                    Err(_) => Err(anyhow!("the {} model took too long", slot.kind)),
                }
            }
            .await;
            // A process that failed may be in any state: start afresh.
            if result.is_err() {
                slot.end(&mut proc).await;
            }
            result
        };
        schedule_unload(slot.clone(), unload_after);
        result
    }
}

/// End `slot`'s process once it has gone unused for `after` (`None`: never).
fn schedule_unload(slot: Arc<Slot>, after: Option<Duration>) {
    let mark = slot.uses.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(after) = after else {
        return;
    };
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        if slot.uses.load(Ordering::SeqCst) != mark {
            return;
        }
        // In use right now: that use sets a timer of its own when it ends.
        let Ok(mut proc) = slot.proc.try_lock() else {
            return;
        };
        if let Some(model) = proc.as_ref().map(|e| e.model.clone()) {
            slot.end(&mut proc).await;
            tracing::info!(
                "voice {} model {model} unloaded after {after:?} unused",
                slot.kind
            );
        }
    });
}

impl Slot {
    async fn end(&self, proc: &mut Option<Engine>) {
        if let Some(mut engine) = proc.take() {
            let _ = engine.child.start_kill();
            let _ = engine.child.wait().await;
        }
        *self.loaded.lock().unwrap() = None;
    }
}

impl Engine {
    fn start(program: &PathBuf, args: &[String], model: &str) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("start {}", program.display()))?;
        let stdin = child.stdin.take().context("the engine's stdin")?;
        let stdout = child.stdout.take().context("the engine's stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "loft_voice::engine", "{line}");
                }
            });
        }
        Ok(Self {
            model: model.to_owned(),
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
        })
    }

    async fn ask(&mut self, request: &EngineRequest) -> Result<EngineReply> {
        let mut line = serde_json::to_string(request)?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("the voice engine stopped")?;
        self.stdin.flush().await?;
        loop {
            let Some(line) = self.stdout.next_line().await? else {
                bail!("the voice engine stopped");
            };
            if let Some(reply) = line.strip_prefix(MARK) {
                return serde_json::from_str(reply).context("the voice engine's answer");
            }
            tracing::debug!(target: "loft_voice::engine", "{line}");
        }
    }
}

/// The files of a speech or cleanup model.
fn files_of(id: &str) -> Option<&'static [ModelFile]> {
    if let Some(m) = catalog::model(id) {
        return Some(std::slice::from_ref(&m.file));
    }
    crate::cleanup::models()
        .find(|m| m.id == id)
        .map(|m| m.files)
}
