//! `agentloftd voice-engine`: the process that holds a model. It reads
//! requests on stdin (see `wire`) and answers on stdout until stdin closes:
//! when the daemon ends it, or the daemon itself goes away.

use crate::chunk;
use crate::wire::{EngineReply, EngineRequest, MARK};
use anyhow::{Context as _, Result, bail};
use std::io::{BufRead as _, Write as _};

/// What this process holds.
enum Held {
    Nothing,
    Speech(transcribe_cpp::Session, transcribe_cpp::Model),
    FakeSpeech,
    Text(Box<crate::cleanup::Local>),
    FakeText,
}

pub fn run() -> Result<()> {
    transcribe_cpp::disable_logging();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut held = Held::Nothing;
    for line in stdin.lock().lines() {
        let line = line.context("read a request")?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<EngineRequest>(&line) {
            Ok(request) => handle(&mut held, request),
            Err(err) => Err(anyhow::anyhow!("bad request: {err}")),
        }
        .unwrap_or_else(|err| EngineReply::Error {
            message: format!("{err:#}"),
        });
        let mut out = MARK.to_owned();
        out.push_str(&serde_json::to_string(&reply)?);
        out.push('\n');
        stdout.write_all(out.as_bytes())?;
        stdout.flush()?;
    }
    Ok(())
}

fn handle(held: &mut Held, request: EngineRequest) -> Result<EngineReply> {
    match request {
        EngineRequest::LoadSpeech { path, fake } => {
            *held = Held::Nothing;
            if fake {
                *held = Held::FakeSpeech;
                return Ok(EngineReply::Loaded {
                    backend: "fake".into(),
                });
            }
            let model = transcribe_cpp::Model::load(&path)
                .with_context(|| format!("load {}", path.display()))?;
            let session = model.session().context("start the model")?;
            let backend = model.backend().to_lowercase();
            *held = Held::Speech(session, model);
            Ok(EngineReply::Loaded { backend })
        }
        EngineRequest::Transcribe {
            pcm,
            language,
            words,
        } => {
            let pcm: Vec<f32> = pcm
                .0
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
                .collect();
            let text = match held {
                Held::Speech(session, model) => transcribe(session, model, &pcm, language, words)?,
                Held::FakeSpeech => format!(
                    "This is what you said in {:.1} seconds.",
                    pcm.len() as f32 / 16_000.0
                ),
                _ => bail!("no speech model is loaded"),
            };
            Ok(EngineReply::Text { text })
        }
        EngineRequest::LoadText { model, dir, fake } => {
            *held = Held::Nothing;
            if fake {
                *held = Held::FakeText;
                return Ok(EngineReply::Loaded {
                    backend: "fake".into(),
                });
            }
            let local = crate::cleanup::Local::load(&model, &dir)?;
            let backend = local.backend().to_owned();
            *held = Held::Text(Box::new(local));
            Ok(EngineReply::Loaded { backend })
        }
        EngineRequest::Generate {
            system,
            user,
            max_tokens,
        } => {
            let text = match held {
                Held::Text(local) => local.generate(&system, &user, max_tokens)?,
                // The transcript, tidied the way a test can check.
                Held::FakeText => crate::cleanup::transcript_of(&user)
                    .map(|t| format!("{}.", t.trim().trim_end_matches('.')))
                    .unwrap_or_default(),
                _ => bail!("no cleanup model is loaded"),
            };
            Ok(EngineReply::Text { text })
        }
    }
}

fn transcribe(
    session: &mut transcribe_cpp::Session,
    model: &transcribe_cpp::Model,
    pcm: &[f32],
    language: Option<String>,
    words: Vec<String>,
) -> Result<String> {
    use transcribe_cpp::{Feature, RunOptions};
    let caps = model
        .capabilities()
        .context("read the model's capabilities")?;
    let language = language.filter(|l| {
        // A model that takes one language needs no hint; one it doesn't
        // know would be refused.
        caps.languages.len() > 1 && caps.languages.iter().any(|c| c == l)
    });
    let options = RunOptions {
        language,
        vocabulary: if model.supports(Feature::Vocabulary) {
            words
        } else {
            Vec::new()
        },
        ..RunOptions::default()
    };
    let max = if caps.max_audio_ms > 0 && !model.supports(Feature::LongForm) {
        // A little under the limit, at 16 samples a millisecond.
        (caps.max_audio_ms as usize * 16).saturating_sub(16_000)
    } else {
        usize::MAX
    };
    let mut text = String::new();
    for piece in chunk::pieces(pcm, max) {
        let part = session
            .run(&pcm[piece], &options)
            .context("transcribe")?
            .text;
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(part);
    }
    Ok(text)
}
