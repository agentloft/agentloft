//! Agentloft's browser entry point. Everything here is wasm-only; on native
//! targets the crate is empty so `cargo check --workspace` stays cheap.

#[cfg(target_arch = "wasm32")]
mod spike;
