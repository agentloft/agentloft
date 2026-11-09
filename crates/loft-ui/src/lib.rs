//! Agentloft GPUI views. All state comes from `agentloftd` through
//! [`store::AppStore`]; views never spawn processes or touch storage.

pub mod assets;
mod attachments;
mod attention;
pub mod brand;
mod items;
mod palette;
pub mod panel;
pub mod settings;
pub mod store;
pub mod theme;
pub mod windows;
pub mod workspace;

pub use store::AppStore;
pub use workspace::{WorkspaceView, app_menus, bind_keys};
