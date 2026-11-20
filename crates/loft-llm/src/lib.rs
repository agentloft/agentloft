//! A small language model run on this computer, to clean up dictated text:
//! punctuation, numbers, filler words, and corrections spoken along the way
//! ("make it 42, sorry, 24" becomes "Make it 24").
//!
//! Models are quantized GGUF files, run by candle: on the GPU through Metal
//! on a Mac, on the CPU elsewhere. Nothing here bundles ggml, which another
//! library in the same binary already links.
//!
//! Everything is synchronous and kept in [`Loaded`]: dropping it frees the
//! model's memory. (Measured with Gemma 3 4B on Metal: 2.9 GB loaded, 0.5 GB
//! a moment after dropping it, which the system allocator and the Metal
//! driver keep for reuse: loading again reuses it rather than adding to it,
//! and it goes back when the process exits. On the CPU of a Mac the
//! allocator also keeps the freed weights themselves.)
//!
//! `AGENTLOFT_LLM_DEVICE=cpu` in the environment keeps a Mac on the CPU, as
//! other systems are (to compare, or to rule out the GPU).

mod catalog;
mod chat;
mod decoder;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, anyhow, bail};
use candle_core::Device;
use tokenizers::Tokenizer;

pub use catalog::MODELS;

/// One file of a model, downloaded and checked by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    /// The file's name in the model's folder.
    pub name: &'static str,
    pub url: &'static str,
    /// Lowercase hex.
    pub sha256: &'static str,
    /// In bytes.
    pub size: u64,
}

/// A model's family: how its file is laid out and how it is prompted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    Qwen3,
    Gemma3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LlmModel {
    /// Stable, e.g. "qwen3-0.6b".
    pub id: &'static str,
    /// For people, e.g. "Qwen3 0.6B".
    pub name: &'static str,
    /// One short line for a settings list.
    pub description: &'static str,
    pub arch: Arch,
    /// The `.gguf` and its tokenizer.
    pub files: &'static [ModelFile],
    pub recommended: bool,
}

impl LlmModel {
    /// All its files together, in bytes.
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

/// The model with this id.
pub fn model(id: &str) -> Option<&'static LlmModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// What [`Loaded::generate`] fails with when it was cancelled
/// (`error.is::<Cancelled>()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// A model in memory, ready to answer.
pub struct Loaded {
    arch: Arch,
    /// Always there until dropped (see `Drop`).
    decoder: Option<decoder::Decoder>,
    tokenizer: Tokenizer,
    /// The tokens that end its turn.
    stops: Vec<u32>,
    device: Device,
}

impl Loaded {
    /// `dir` holds every file of `model.files` under its `name`.
    pub fn load(model: &LlmModel, dir: &Path) -> anyhow::Result<Loaded> {
        let file = |suffix: &str| {
            model
                .files
                .iter()
                .find(|f| f.name.ends_with(suffix))
                .map(|f| dir.join(f.name))
                .with_context(|| format!("{} has no {suffix} file", model.id))
        };
        let weights = file(".gguf")?;
        let tokenizer = file(".json")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer)
            .map_err(|e| anyhow!("couldn't read {}: {e}", tokenizer.display()))?;
        // Text in a prompt is text, even where it spells a control token.
        tokenizer.set_encode_special_tokens(true);
        let id = |name: &str| {
            tokenizer
                .token_to_id(name)
                .with_context(|| format!("the tokenizer has no {name}"))
        };
        let stops = chat::stops(model.arch)
            .iter()
            .map(|name| id(name))
            .collect::<anyhow::Result<Vec<_>>>()?;
        // Every control token a prompt uses must exist.
        for piece in chat::prompt(model.arch, "s", "u") {
            if let chat::Piece::Special(name) = piece {
                id(name)?;
            }
        }

        let device = device();
        let mut reader = std::io::BufReader::new(
            std::fs::File::open(&weights)
                .with_context(|| format!("couldn't open {}", weights.display()))?,
        );
        let decoder = decoder::Decoder::load(model.arch, &mut reader, &device)
            .with_context(|| format!("couldn't load {}", weights.display()))?;
        Ok(Loaded {
            arch: model.arch,
            decoder: Some(decoder),
            tokenizer,
            stops,
            device,
        })
    }

    /// Greedy (deterministic) chat completion with the model's chat template,
    /// thinking turned off for Qwen3. Stops at end-of-turn, at `max_tokens`,
    /// or when `cancel` becomes true (returns [`Cancelled`] then).
    pub fn generate(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: usize,
        cancel: &AtomicBool,
    ) -> anyhow::Result<String> {
        let prompt = self.encode(&chat::prompt(self.arch, system, user))?;
        if prompt.len() + max_tokens > decoder::CONTEXT {
            bail!(
                "the text is too long for the model ({} tokens and {max_tokens} more to write; at most {})",
                prompt.len(),
                decoder::CONTEXT
            );
        }
        let decoder = self.decoder.as_mut().context("the model was unloaded")?;
        decoder.clear();
        let result = run(decoder, &self.stops, prompt, max_tokens, cancel);
        // The cache is only for this answer: give its memory back.
        decoder.clear();
        let answer = result?;
        let text = self
            .tokenizer
            .decode(&answer, true)
            .map_err(|e| anyhow!("couldn't decode the answer: {e}"))?;
        Ok(chat::clean(&text))
    }

    fn encode(&self, pieces: &[chat::Piece]) -> anyhow::Result<Vec<u32>> {
        let mut ids = Vec::new();
        for piece in pieces {
            match piece {
                chat::Piece::Special(name) => ids.push(
                    self.tokenizer
                        .token_to_id(name)
                        .with_context(|| format!("the tokenizer has no {name}"))?,
                ),
                chat::Piece::Text(text) => ids.extend_from_slice(
                    self.tokenizer
                        .encode(text.as_str(), false)
                        .map_err(|e| anyhow!("couldn't tokenize the prompt: {e}"))?
                        .get_ids(),
                ),
            }
        }
        Ok(ids)
    }

    /// Where the model runs: "metal" or "cpu".
    pub fn backend(&self) -> &'static str {
        if self.device.is_metal() {
            "metal"
        } else {
            "cpu"
        }
    }
}

/// Greedy decoding: the likeliest token each time, until one of `stops`.
fn run(
    decoder: &mut decoder::Decoder,
    stops: &[u32],
    prompt: Vec<u32>,
    max_tokens: usize,
    cancel: &AtomicBool,
) -> anyhow::Result<Vec<u32>> {
    let mut answer = Vec::new();
    let mut input = prompt;
    let mut position = 0;
    while answer.len() < max_tokens {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let logits = decoder.forward(&input, position)?;
        position += input.len();
        let next = logits.argmax(0)?.to_scalar::<u32>()?;
        if stops.contains(&next) {
            break;
        }
        answer.push(next);
        input = vec![next];
    }
    Ok(answer)
}

impl Drop for Loaded {
    fn drop(&mut self) {
        drop(self.decoder.take());
        // candle's Metal device keeps freed buffers for reuse, and leaves
        // them in the GPU's residency set, until it next waits for the GPU:
        // without this they outlive the device.
        let _ = self.device.synchronize();
    }
}

/// The GPU where there is one we can use, else the CPU.
fn device() -> Device {
    #[cfg(target_os = "macos")]
    if std::env::var_os("AGENTLOFT_LLM_DEVICE").is_none_or(|d| d != "cpu")
        && let Ok(device) = Device::new_metal(0)
    {
        return device;
    }
    Device::Cpu
}

// The daemon's helper loads on one thread and answers on another.
const _: () = {
    const fn send<T: Send>() {}
    send::<Loaded>();
};
