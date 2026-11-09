//! Agentloft GPUI views. All state comes from `agentloftd` through
//! [`store::AppStore`]; views never spawn processes or touch storage.

mod items;
pub mod store;
pub mod workspace;

pub use store::AppStore;
pub use workspace::WorkspaceView;
