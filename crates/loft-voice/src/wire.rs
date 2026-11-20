//! Between the daemon and `agentloftd voice-engine`: one JSON request per
//! line on the engine's stdin, one reply per line on its stdout. Native
//! libraries may print on stdout too, so replies carry [`MARK`] and any
//! other line is ignored.

use loft_protocol::Bytes;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const MARK: &str = "@@agentloft-voice ";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum EngineRequest {
    /// Load a speech model (the only one this process will hold).
    LoadSpeech {
        path: PathBuf,
        #[serde(default)]
        fake: bool,
    },
    /// 16 kHz mono i16 little-endian.
    Transcribe {
        pcm: Bytes,
        #[serde(default)]
        language: Option<String>,
        #[serde(default)]
        words: Vec<String>,
    },
    /// Load a cleanup model from the catalog, its files in `dir`.
    LoadText {
        model: String,
        dir: PathBuf,
        #[serde(default)]
        fake: bool,
    },
    Generate {
        system: String,
        user: String,
        max_tokens: usize,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub(crate) enum EngineReply {
    Loaded { backend: String },
    Text { text: String },
    Error { message: String },
}
