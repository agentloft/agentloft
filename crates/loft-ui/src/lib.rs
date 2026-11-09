//! Agentloft GPUI views. All state comes from `agentloftd` through
//! [`store::AppStore`]; views never spawn processes or touch storage.

pub mod assets;
mod items;
pub mod store;
pub mod theme;
pub mod workspace;

pub use store::AppStore;
pub use workspace::{WorkspaceView, bind_keys};
