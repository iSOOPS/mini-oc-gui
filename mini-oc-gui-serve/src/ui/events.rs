//! Keyboard event normalization.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A high-level input event consumed by the TUI loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Move selection up.
    Up,
    /// Move selection down.
    Down,
    /// Move focus left.
    Left,
    /// Move focus right.
    Right,
    /// Confirm the current selection / field.
    Select,
    /// Quit the TUI.
    Quit,
    /// Delete the character before the cursor.
    Backspace,
    /// Switch to the next field (form mode).
    Tab,
    /// A printable character.
    Char(char),
    /// Any other, unhandled key.
    Other,
}

impl From<KeyEvent> for InputEvent {
    fn from(k: KeyEvent) -> Self {
        match k.code {
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Enter => Self::Select,
            KeyCode::Esc => Self::Quit,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Self::Quit,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Tab => Self::Tab,
            KeyCode::Char(c) => Self::Char(c),
            _ => Self::Other,
        }
    }
}
