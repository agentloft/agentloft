//! Agentloft GPUI views. All state comes from `agentloftd` through
//! [`store::AppStore`]; views never spawn processes or touch storage.

pub mod assets;
pub mod brand;
mod items;
pub mod panel;
pub mod store;
pub mod theme;
pub mod workspace;

pub use store::AppStore;
pub use workspace::{WorkspaceView, app_menus, bind_keys};
