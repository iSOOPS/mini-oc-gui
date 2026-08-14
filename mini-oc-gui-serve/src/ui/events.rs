//! Input event normalization (keyboard + mouse).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// A high-level input event consumed by the TUI loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// Move selection up.
    Up,
    /// Move selection down.
    Down,
    /// Confirm the current selection (Enter).
    Select,
    /// Cancel the current modal dialog (Esc / q). In the main menu this
    /// is a no-op so users cannot accidentally quit by reflex; in the
    /// port prompt it dismisses the dialog and returns to the menu.
    Cancel,
    /// Force-quit the TUI (Ctrl-C).
    Quit,
    /// A backspace key for the port prompt.
    Backspace,
    /// A typed character for the port prompt (digits only).
    Char(char),
    /// Left mouse button pressed at a terminal cell (0-based).
    ///
    /// Used both for menu selection (a click activates the item under the
    /// cursor) and for starting a log-text selection.
    MouseDown {
        /// 0-based terminal row.
        row: u16,
        /// 0-based terminal column.
        col: u16,
    },
    /// Left mouse button dragged to a terminal cell (0-based).
    ///
    /// Extends the current log-text selection.
    MouseDrag {
        /// 0-based terminal row.
        row: u16,
        /// 0-based terminal column.
        col: u16,
    },
    /// Left mouse button released (finishes a selection).
    MouseUp,
    /// Mouse wheel scrolled up (show older log lines).
    ScrollUp,
    /// Mouse wheel scrolled down (show newer log lines).
    ScrollDown,
    /// Any other, unhandled event.
    Other,
}

impl From<KeyEvent> for InputEvent {
    fn from(k: KeyEvent) -> Self {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => Self::Up,
            KeyCode::Down | KeyCode::Char('j') => Self::Down,
            KeyCode::Enter => Self::Select,
            KeyCode::Esc => Self::Cancel,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Self::Quit,
            KeyCode::Char('q') => Self::Cancel,
            KeyCode::Char(c) => Self::Char(c),
            _ => Self::Other,
        }
    }
}

impl From<MouseEvent> for InputEvent {
    fn from(m: MouseEvent) -> Self {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => Self::MouseDown {
                row: m.row,
                col: m.column,
            },
            MouseEventKind::Drag(MouseButton::Left) => Self::MouseDrag {
                row: m.row,
                col: m.column,
            },
            MouseEventKind::Up(MouseButton::Left) => Self::MouseUp,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            _ => Self::Other,
        }
    }
}
