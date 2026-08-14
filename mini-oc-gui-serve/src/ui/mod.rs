//! ratatui-based terminal UI that replaces the original `gum` shell menus.

pub mod app;
pub mod events;
pub mod log;
pub mod menu;

pub use app::TuiApp;
pub use log::LogBuffer;
pub use menu::{MenuAction, MenuItem};
