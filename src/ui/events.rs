//! Keyboard event normalization.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A high-level input event consumed by the TUI loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Move selection up.
    Up,
    /// Move selection down.
    Down,
    /// Move focus left.
    Left,
    /// Move focus right.
    Right,
    /// Page up — used to scroll a scrollable popup (e.g. settings) up by one page.
    PageUp,
    /// Page down — used to scroll a scrollable popup (e.g. settings) down by one page.
    PageDown,
    /// Confirm the current selection / field.
    Select,
    /// Quit the TUI (Esc in menus).
    Quit,
    /// Delete the character before the cursor.
    Backspace,
    /// Switch to the next field (form mode).
    Tab,
    /// A printable character.
    Char(char),
    /// A pasted text payload from `Ctrl+V` / bracketed paste / terminal paste.
    ///
    /// `String` 是粘到当前编辑焦点的整段内容 —— 由调用方负责
    /// 拼到对应 buffer 的末尾(端口字段仅追加 ASCII 数字)。
    Paste(String),
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
        // Ctrl+V 显式触发：crossterm 在 Windows / macOS / Linux 上对
        // Ctrl+V 一律产生 `Char('v')` + Ctrl 修饰，肉眼无差别，所以这里
        // 统一拦截并转成空 Paste —— 调用方在 `handle_settings_key` /
        // `handle_sub_page_key` 里会用 arboard 拉真实剪贴板内容。
        //
        // 注：crossterm 0.28 还没把"bracketed paste"的开始/结束 marker
        // 暴露成 KeyCode 变体 —— 那些 ESC[200~/ESC[201~ 在 raw mode
        // 下会被解码成普通 Char / Esc 事件并落到 Other 分支,所以我们
        // 这里无需额外处理。
        if let KeyCode::Char('v') = k.code {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                return Self::Paste(String::new());
            }
        }
        match k.code {
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn arrow_keys_map_to_direction() {
        assert_eq!(InputEvent::from(press(KeyCode::Up)), InputEvent::Up);
        assert_eq!(InputEvent::from(press(KeyCode::Down)), InputEvent::Down);
        assert_eq!(InputEvent::from(press(KeyCode::Left)), InputEvent::Left);
        assert_eq!(InputEvent::from(press(KeyCode::Right)), InputEvent::Right);
    }

    #[test]
    fn enter_maps_to_select_and_esc_to_quit() {
        assert_eq!(InputEvent::from(press(KeyCode::Enter)), InputEvent::Select);
        assert_eq!(InputEvent::from(press(KeyCode::Esc)), InputEvent::Quit);
    }

    #[test]
    fn backspace_and_tab_map_to_expected_events() {
        assert_eq!(
            InputEvent::from(press(KeyCode::Backspace)),
            InputEvent::Backspace
        );
        assert_eq!(InputEvent::from(press(KeyCode::Tab)), InputEvent::Tab);
    }

    #[test]
    fn printable_chars_are_preserved() {
        assert_eq!(
            InputEvent::from(press(KeyCode::Char('a'))),
            InputEvent::Char('a')
        );
        assert_eq!(
            InputEvent::from(press(KeyCode::Char('中'))),
            InputEvent::Char('中')
        );
    }

    #[test]
    fn ctrl_c_maps_to_quit() {
        let ev = press_with(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(InputEvent::from(ev), InputEvent::Quit);
    }

    #[test]
    fn ctrl_v_maps_to_paste_with_empty_payload() {
        // Ctrl+V 在跨平台都产生 Char('v') + Ctrl，
        // 我们把它统一转成空 Paste —— handler 再去拉剪贴板内容。
        let ev = press_with(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(InputEvent::from(ev), InputEvent::Paste(String::new()));
    }

    #[test]
    fn ctrl_x_does_not_become_paste() {
        // Ctrl+V → Paste,但其他 Ctrl+字母(没特殊处理)应当被当作
        // 普通字符 —— 因为 macOS/Win Terminal 把 Ctrl+字母仍以
        // Char(x) + Ctrl 形式上报,我们没理由把它们从 Char 退回 Other。
        // 这里只验证 Ctrl+x 不会被误判成 Paste,免得新增其他快捷键时被破坏。
        let ev = press_with(KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert_eq!(InputEvent::from(ev), InputEvent::Char('x'));
        assert!(!matches!(InputEvent::from(ev), InputEvent::Paste(_)));
    }

    #[test]
    fn release_events_are_ignored() {
        let mut k = press(KeyCode::Enter);
        k.kind = KeyEventKind::Release;
        assert_eq!(InputEvent::from(k), InputEvent::Other);

        let mut k = press(KeyCode::Char('a'));
        k.kind = KeyEventKind::Repeat;
        assert_eq!(InputEvent::from(k), InputEvent::Other);
    }

    #[test]
    fn function_keys_become_other() {
        assert_eq!(
            InputEvent::from(press(KeyCode::F(1))),
            InputEvent::Other
        );
    }
}