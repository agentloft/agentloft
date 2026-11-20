//! Voice input: speech to text for the composer, on the daemon's computer.
//!
//! A client records the microphone and sends the recording
//! (`Request::VoiceTranscribe`); the daemon runs a downloaded speech model
//! and answers the text. Optionally the text is tidied
//! (`Request::VoiceCleanup`) by a small model on the same computer or an
//! agent the user picked (`[voice] cleanup`). Models are loaded on first use
//! and unloaded after `[voice] unload_after` without use.

use serde::{Deserialize, Serialize};

/// Sample rate of `Request::VoiceTranscribe`'s audio (mono, 16-bit).
pub const VOICE_SAMPLE_RATE: u32 = 16_000;
/// The longest recording the daemon takes, in seconds.
pub const VOICE_MAX_SECONDS: u32 = 300;

/// What the daemon has for voice input (`Response::Voice`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VoiceStatus {
    /// Why voice input can't run on the daemon's computer, if it can't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<String>,
    /// Speech models, in the catalog's order.
    pub models: Vec<VoiceModel>,
    /// Small models that tidy transcripts on this computer.
    pub cleanup_models: Vec<VoiceModel>,
    /// Ids of the models in memory right now.
    #[serde(default)]
    pub loaded: Vec<String>,
}

/// A model in the daemon's catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VoiceModel {
    pub id: String,
    pub name: String,
    /// One line: what it is good at.
    pub description: String,
    /// Bytes on disk once downloaded.
    pub size: u64,
    /// ISO codes of the languages it takes; empty = not about language
    /// (cleanup models).
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub recommended: bool,
    /// Transcribes while the person speaks (the text shows as they go).
    #[serde(default, skip_serializing_if = "crate::is_false")]
    pub live: bool,
    /// Measured share of English words it gets wrong (percent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub english_error: Option<f32>,
    /// Seconds it takes for 30 seconds of speech, on a GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds_per_30s: Option<f32>,
    pub state: VoiceModelState,
    /// Why its last download failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VoiceModelState {
    #[default]
    NotDownloaded,
    Downloading {
        done: u64,
        total: u64,
    },
    /// Downloaded; its checksum is being checked.
    Verifying,
    Installed,
}

impl VoiceModelState {
    pub fn is_installed(&self) -> bool {
        matches!(self, Self::Installed)
    }

    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Downloading { .. } | Self::Verifying)
    }
}
