//! Keyboard event normalization.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A high-level input event consumed by the TUI loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Move selection up.
    Up,
    /// Move selection down.
    Down,
    /// Confirm the current selection.
    Select,
    /// Quit the TUI.
    Quit,
    /// Any other, unhandled key.
    Other,
}

impl From<KeyEvent> for InputEvent {
    fn from(k: KeyEvent) -> Self {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => Self::Up,
            KeyCode::Down | KeyCode::Char('j') => Self::Down,
            KeyCode::Enter => Self::Select,
            KeyCode::Esc => Self::Quit,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Self::Quit,
            KeyCode::Char('q') => Self::Quit,
            _ => Self::Other,
        }
    }
}
