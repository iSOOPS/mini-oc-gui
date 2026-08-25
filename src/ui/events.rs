//! Keyboard event normalization.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
        // Windows 控制台会为一次按键产生 Press 与 Release 两个事件（macOS 的
        // termios 只报 Press）。若不过滤，Esc 会被处理两次：第一次关闭弹窗、
        // 第二次（Release）又被当成菜单退出键，表现为「弹窗按 Esc 直接退出程序」。
        // 这里只保留 Press，忽略 Release / Repeat。
        if k.kind != KeyEventKind::Press {
            return Self::Other;
        }
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
