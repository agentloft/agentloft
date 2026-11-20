//! Voice input on the daemon's computer: the speech models it offers
//! ([`catalog`]), their files on disk ([`store`]), the process that runs
//! them ([`engine`], `agentloftd voice-engine`), and the daemon's side of
//! it ([`Voice`]), which starts that process when a recording comes in and
//! ends it once the model has gone unused for `[voice] unload_after`.
//!
//! Models run in a process of their own so that unloading one gives every
//! byte back to the system (GPU buffers and allocator caches included), and
//! so that a native call that hangs or crashes takes nothing else down.

pub mod catalog;
mod chunk;
pub mod cleanup;
pub mod engine;
mod service;
pub mod store;
mod wire;

pub use service::{Voice, VoiceOptions};

/// One file of a model, fetched from `url` and checked by its SHA-256
/// (the same for speech and cleanup models).
pub use loft_llm::ModelFile;

/// The model id that needs no download and answers canned text, for tests
/// (offered only with `AGENTLOFT_FAKE_VOICE=1`).
pub const FAKE_MODEL: &str = "fake";
