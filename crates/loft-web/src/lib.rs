//! Agentloft in the browser (ADR-0016): the same `loft-ui` views as the
//! desktop app, compiled to WebAssembly and drawn by GPUI's web platform.
//! Everything here is wasm-only; on native targets the crate is empty so
//! `cargo check --workspace` stays cheap.
//!
//! The page (`static/boot.js`) signs the browser in first, then loads this
//! module. It connects back to the daemon that served it over a WebSocket.

#[cfg(target_arch = "wasm32")]
mod app;
