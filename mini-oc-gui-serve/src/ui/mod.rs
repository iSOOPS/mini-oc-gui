//! ratatui-based terminal UI that replaces the original `gum` shell menus.

pub mod app;
pub mod events;
pub mod menu;

pub use app::{LogLine, TuiApp, TuiLogSink, TuiLogSinkFactory};
pub use menu::{MenuAction, MenuItem};
