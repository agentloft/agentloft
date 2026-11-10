//! Agentloft GPUI views. All state comes from `agentloftd` through
//! [`store::AppStore`]; views never spawn processes or touch storage.

mod agent_choice;
mod app_icons;
pub mod assets;
mod attachments;
mod attention;
mod branch_picker;
pub mod brand;
mod completion;
mod details;
mod file_icons;
mod folder_picker;
pub mod host;
mod host_ui;
mod image_preview;
mod info_cards;
mod items;
pub mod keymap;
mod model_picker;
mod new_in;
mod palette;
pub mod panel;
mod project_icon;
pub mod settings;
pub mod store;
pub mod theme;
pub mod themes;
pub mod windows;
pub mod workspace;

pub use store::AppStore;
pub use workspace::{WorkspaceView, app_menus, bind_keys};
