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
    let mut io = Io {
        lines: Box::new(stdin.lock().lines()),
        out: std::io::stdout(),
    };
    let mut held = Held::Nothing;
    while let Some(request) = io.next()? {
        let reply = match request {
            Ok(EngineRequest::StreamStart { language, words }) => match &mut held {
                Held::Speech(session, model) => {
                    stream(session, model, language, words, &mut io)?;
                    continue;
                }
                Held::FakeSpeech => {
                    fake_stream(&mut io)?;
                    continue;
                }
                _ => Err(anyhow::anyhow!("no speech model is loaded")),
            },
            Ok(request) => handle(&mut held, request),
            Err(err) => Err(err),
        };
        io.reply(reply)?;
    }
    Ok(())
}

/// The engine's stdin and stdout.
struct Io<'a> {
    lines: Box<dyn Iterator<Item = std::io::Result<String>> + 'a>,
    out: std::io::Stdout,
}

impl Io<'_> {
    /// The next request; `None` once stdin closes.
    fn next(&mut self) -> Result<Option<Result<EngineRequest>>> {
        for line in self.lines.by_ref() {
            let line = line.context("read a request")?;
            if line.trim().is_empty() {
                continue;
            }
            return Ok(Some(
                serde_json::from_str(&line).map_err(|err| anyhow::anyhow!("bad request: {err}")),
            ));
        }
        Ok(None)
    }

    fn reply(&mut self, reply: Result<EngineReply>) -> Result<()> {
        let reply = reply.unwrap_or_else(|err| EngineReply::Error {
            message: format!("{err:#}"),
        });
        let mut out = MARK.to_owned();
        out.push_str(&serde_json::to_string(&reply)?);
        out.push('\n');
        self.out.write_all(out.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

fn floats(pcm: &loft_protocol::Bytes) -> Vec<f32> {
    pcm.0
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
        .collect()
}

/// A live transcription: answers each piece of audio with the text so far,
/// until the recording finishes or is cancelled. The stream borrows the
/// session, so it lives here, on the stack, for as long as it runs.
fn stream(
    session: &mut transcribe_cpp::Session,
    model: &transcribe_cpp::Model,
    language: Option<String>,
    words: Vec<String>,
    io: &mut Io<'_>,
) -> Result<()> {
    let caps = match model.capabilities() {
        Ok(caps) => caps,
        Err(err) => return io.reply(Err(anyhow::anyhow!("read the model's capabilities: {err}"))),
    };
    if !caps.supports_streaming {
        return io.reply(Err(anyhow::anyhow!("this model doesn't transcribe live")));
    }
    let options = run_options(model, &caps, language, words);
    let mut stream = match session.stream(&options, &transcribe_cpp::StreamOptions::default()) {
        Ok(stream) => stream,
        Err(err) => return io.reply(Err(anyhow::anyhow!("start transcribing: {err}"))),
    };
    io.reply(Ok(EngineReply::Live {
        committed: String::new(),
        tentative: String::new(),
    }))?;
    while let Some(request) = io.next()? {
        match request {
            Ok(EngineRequest::StreamFeed { pcm }) => {
                let reply = stream
                    .feed(&floats(&pcm))
                    .map(|_| {
                        let text = stream.text();
                        EngineReply::Live {
                            committed: text.committed,
                            tentative: text.tentative,
                        }
                    })
                    .map_err(|err| anyhow::anyhow!("transcribe: {err}"));
                let failed = reply.is_err();
                io.reply(reply)?;
                if failed {
                    return Ok(());
                }
            }
            Ok(EngineRequest::StreamFinish { pcm }) => {
                let reply = (|| {
                    let pcm = floats(&pcm);
                    if !pcm.is_empty() {
                        stream.feed(&pcm)?;
                    }
                    stream.finalize()?;
                    Ok::<_, transcribe_cpp::Error>(stream.text().display())
                })()
                .map(|text| EngineReply::Text {
                    text: text.trim().to_owned(),
                })
                .map_err(|err| anyhow::anyhow!("transcribe: {err}"));
                return io.reply(reply);
            }
            Ok(EngineRequest::StreamCancel) => {
                return io.reply(Ok(EngineReply::Text {
                    text: String::new(),
                }));
            }
            Ok(_) => io.reply(Err(anyhow::anyhow!(
                "a live transcription is running; finish or cancel it first"
            )))?,
            Err(err) => io.reply(Err(err))?,
        }
    }
    Ok(())
}

/// The test model's live transcription: a word per second of audio.
fn fake_stream(io: &mut Io<'_>) -> Result<()> {
    io.reply(Ok(EngineReply::Live {
        committed: String::new(),
        tentative: String::new(),
    }))?;
    let mut samples = 0usize;
    let words = |n: usize| -> String {
        (0..n / 16_000)
            .map(|i| format!("word{}", i + 1))
            .collect::<Vec<_>>()
            .join(" ")
    };
    while let Some(request) = io.next()? {
        match request {
            Ok(EngineRequest::StreamFeed { pcm }) => {
                samples += pcm.0.len() / 2;
                io.reply(Ok(EngineReply::Live {
                    committed: words(samples.saturating_sub(16_000)),
                    tentative: String::new(),
                }))?;
            }
            Ok(EngineRequest::StreamFinish { pcm }) => {
                samples += pcm.0.len() / 2;
                return io.reply(Ok(EngineReply::Text {
                    text: words(samples),
                }));
            }
            Ok(EngineRequest::StreamCancel) => {
                return io.reply(Ok(EngineReply::Text {
                    text: String::new(),
                }));
            }
            Ok(_) => io.reply(Err(anyhow::anyhow!("a live transcription is running")))?,
            Err(err) => io.reply(Err(err))?,
        }
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
            let pcm = floats(&pcm);
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
        EngineRequest::StreamFeed { .. }
        | EngineRequest::StreamFinish { .. }
        | EngineRequest::StreamCancel => bail!("no live transcription is running"),
        EngineRequest::StreamStart { .. } => unreachable!("handled by run"),
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
    use transcribe_cpp::Feature;
    let caps = model
        .capabilities()
        .context("read the model's capabilities")?;
    let options = run_options(model, &caps, language, words);
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

fn run_options(
    model: &transcribe_cpp::Model,
    caps: &transcribe_cpp::Capabilities,
    language: Option<String>,
    words: Vec<String>,
) -> transcribe_cpp::RunOptions {
    use transcribe_cpp::Feature;
    let language = language.filter(|l| {
        // A model that takes one language needs no hint; one it doesn't
        // know would be refused.
        caps.languages.len() > 1 && caps.languages.iter().any(|c| c == l)
    });
    transcribe_cpp::RunOptions {
        language,
        vocabulary: if model.supports(Feature::Vocabulary) {
            words
        } else {
            Vec::new()
        },
        ..transcribe_cpp::RunOptions::default()
    }
}
