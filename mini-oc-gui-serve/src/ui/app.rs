//! TUI app state and render loop.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tracing::Level;

use crate::serve::{ServeStatus, ServeSupervisor};
use crate::ui::events::InputEvent;
use crate::ui::menu::{MenuAction, MenuItem};
use crate::upgrade::{UpgradeResult, upgrade_opencode, upgrade_omo};

/// Maximum number of log lines preserved in the in-memory ring buffer.
const LOG_RING_CAPACITY: usize = 512;

/// A single log entry captured by [`TuiLogSink`].
#[derive(Debug, Clone)]
pub struct LogLine {
    /// Tracing level.
    pub level: Level,
    /// Pre-rendered text (without ANSI).
    pub text: String,
}

impl LogLine {
    fn new(level: Level, text: String) -> Self {
        Self { level, text }
    }
}

/// Shared, thread-safe log ring buffer that the TUI status panel reads.
///
/// Cloning this handle is cheap (it's just an `Arc`). The tracing
/// subscriber writes here via `tracing_subscriber::fmt().with_writer(...)`;
/// the render loop reads here.
#[derive(Clone, Default)]
pub struct TuiLogSink {
    inner: Arc<StdMutex<VecDeque<LogLine>>>,
}

impl TuiLogSink {
    /// Create a new empty log sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StdMutex::new(VecDeque::with_capacity(LOG_RING_CAPACITY))),
        }
    }

    /// Push a formatted log line, evicting the oldest entry when the
    /// ring buffer is full.
    pub fn push(&self, line: LogLine) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.len() == LOG_RING_CAPACITY {
                guard.pop_front();
            }
            guard.push_back(line);
        }
    }

    /// Snapshot all current log lines (oldest first).
    #[must_use]
    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl std::io::Write for TuiLogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf).into_owned();
        // `tracing_subscriber::fmt` writes one record at a time and flushes
        // on newline. We split on newline so multiple lines flushed in a
        // single write are captured individually.
        for line in text.split('\n') {
            if line.is_empty() {
                continue;
            }
            // tracing-subscriber's default fmt produces a level tag like
            // "2026-08-13T13:44:35.295985Z ERROR ...". We surface the level
            // by sniffing the first token after the timestamp.
            let level = if line.contains(" ERROR ") {
                Level::ERROR
            } else if line.contains(" WARN ") {
                Level::WARN
            } else if line.contains(" INFO ") {
                Level::INFO
            } else if line.contains(" DEBUG ") {
                Level::DEBUG
            } else if line.contains(" TRACE ") {
                Level::TRACE
            } else {
                Level::INFO
            };
            self.push(LogLine::new(level, line.to_string()));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `tracing_subscriber::fmt` requires a `MakeWriter` that hands out owned
/// `Write` references. We always hand out a clone of the sink handle.
pub struct TuiLogSinkFactory {
    sink: TuiLogSink,
}

impl TuiLogSinkFactory {
    /// Build a factory that produces writers backed by `sink`.
    #[must_use]
    pub fn new(sink: TuiLogSink) -> Self {
        Self { sink }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TuiLogSinkFactory {
    type Writer = TuiLogSink;

    fn make_writer(&'a self) -> Self::Writer {
        self.sink.clone()
    }
}

/// Top-level TUI mode. The app is either browsing the main menu
/// or asking the user to confirm a port.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Main menu (default).
    Menu,
    /// Modal port prompt. Holds the input buffer, the suggested default,
    /// and an optional error message from the previous attempt.
    PortPrompt {
        /// Current input buffer (digits only).
        input: String,
        /// Suggested default port (from `DEFAULT_PORT` env or 9464).
        default: u16,
        /// Last error message (e.g. port already in use).
        error: Option<String>,
    },
}

/// A log-text selection spanning a contiguous run of log lines.
///
/// Indices are into the *oldest-first* `TuiLogSink::snapshot()` vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogSelection {
    /// First selected line (the anchor where the drag started).
    anchor: usize,
    /// Current drag position.
    cursor: usize,
}

impl LogSelection {
    /// The inclusive `(lo, hi)` line range, normalized.
    fn range(self) -> (usize, usize) {
        (self.anchor.min(self.cursor), self.anchor.max(self.cursor))
    }
}

/// Main TUI application state.
pub struct TuiApp {
    /// Process supervisor (opencode + rathole).
    pub supervisor: ServeSupervisor,
    /// Currently focused menu entry.
    pub menu_state: ListState,
    /// All menu items.
    pub items: Vec<MenuItem>,
    /// Latest user-facing status message.
    pub status_message: String,
    /// Shared cache of the latest supervisor status snapshot.
    pub cached_status: std::sync::Arc<tokio::sync::Mutex<ServeStatus>>,
    /// Shared log sink (so the render loop can read what tracing wrote).
    pub log_sink: TuiLogSink,
    /// Current TUI mode.
    mode: Mode,
    /// Set by input handlers; the loop exits when `true`.
    pub should_quit: bool,
    /// Top row of the menu list in the rendered frame (used to map a
    /// mouse click `row` to a list index).
    pub menu_area_top: u16,
    /// Top row of the log panel's *content* area (inside its border).
    log_content_top: u16,
    /// Height (in rows) of the log panel's content area.
    log_content_height: u16,
    /// Log scroll offset (0 = follow newest; N = scroll up N lines).
    log_scroll: usize,
    /// Active log-text selection, if any.
    selection: Option<LogSelection>,
}

impl TuiApp {
    /// Construct a new TUI app bound to a supervisor.
    #[must_use]
    pub fn new(supervisor: ServeSupervisor) -> Self {
        Self::with_log_sink(supervisor, TuiLogSink::new())
    }

    /// Construct a TUI app and share a log sink with the caller (typically
    /// `main.rs`, which registers it with the tracing subscriber).
    #[must_use]
    pub fn with_log_sink(supervisor: ServeSupervisor, log_sink: TuiLogSink) -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));
        Self {
            supervisor,
            menu_state,
            items: MenuItem::all(),
            status_message: "就绪".to_string(),
            cached_status: std::sync::Arc::new(tokio::sync::Mutex::new(ServeStatus::default())),
            log_sink,
            mode: Mode::Menu,
            should_quit: false,
            menu_area_top: 0,
            log_content_top: 0,
            log_content_height: 0,
            log_scroll: 0,
            selection: None,
        }
    }

    /// Run the TUI loop until the user quits.
    ///
    /// # Errors
    /// Returns [`anyhow::Error`] on terminal draw / event read failures.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        let period = Duration::from_millis(33); // ~30 FPS
        let mut interval = tokio::time::interval(period);
        let mut events = EventStream::new();

        // Background task: refresh cached supervisor status at 4Hz.
        let supervisor_for_refresh = self.supervisor.clone();
        let cache_for_refresh = self.cached_status.clone();
        let refresh_handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            loop {
                tick.tick().await;
                let status = supervisor_for_refresh.status().await;
                *cache_for_refresh.lock().await = status;
            }
        });

        while !self.should_quit {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = terminal.draw(|f| self.render(f)) {
                        tracing::error!("draw error: {e}");
                    }
                }
                Some(Ok(ev)) = events.next() => {
                    match ev {
                        crossterm::event::Event::Key(k) => {
                            self.handle_event(InputEvent::from(k));
                        }
                        crossterm::event::Event::Mouse(m) => {
                            self.handle_event(InputEvent::from(m));
                        }
                        _ => {}
                    }
                }
            }
        }
        refresh_handle.abort();
        Ok(())
    }

    fn handle_event(&mut self, event: InputEvent) {
        match &self.mode {
            Mode::Menu => self.handle_menu_event(event),
            Mode::PortPrompt { .. } => self.handle_port_prompt_event(event),
        }
    }

    fn handle_menu_event(&mut self, event: InputEvent) {
        match event {
            InputEvent::Up => self.select_prev(),
            InputEvent::Down => self.select_next(),
            InputEvent::Select => self.activate_selected(),
            InputEvent::Cancel => {
                // Esc / q: clear any log selection, otherwise no-op. The
                // user must pick "🚪 退出" to quit, so a reflex Esc press
                // can't kill the whole program.
                self.selection = None;
            }
            InputEvent::Quit => {
                // Ctrl-C: copy the selection if one is active, otherwise
                // force-quit. This gives tmux-style "copy first, quit when
                // nothing selected".
                if self.selection.is_some() {
                    self.copy_selection();
                    self.selection = None;
                } else {
                    self.should_quit = true;
                }
            }
            InputEvent::MouseDown { row, col: _ } => self.handle_mouse_down(row),
            InputEvent::MouseDrag { row, col: _ } => self.handle_mouse_drag(row),
            InputEvent::MouseUp => {
                // Keep the selection so the user can copy it with Ctrl-C.
            }
            InputEvent::ScrollUp => self.scroll_log(3),
            InputEvent::ScrollDown => self.scroll_log(-3),
            InputEvent::Backspace | InputEvent::Char(_) | InputEvent::Other => {}
        }
    }

    fn handle_port_prompt_event(&mut self, event: InputEvent) {
        match event {
            InputEvent::Char(c) => {
                if let Mode::PortPrompt { input, .. } = &mut self.mode {
                    if c.is_ascii_digit() && input.len() < 5 {
                        input.push(c);
                    }
                }
            }
            InputEvent::Backspace => {
                if let Mode::PortPrompt { input, .. } = &mut self.mode {
                    input.pop();
                }
            }
            InputEvent::Select => self.submit_port_prompt(),
            InputEvent::Cancel => {
                // Esc / q dismisses the dialog (NOT the whole program) and
                // returns to the main menu.
                self.mode = Mode::Menu;
                self.status_message = "已取消端口选择".to_string();
            }
            InputEvent::Quit => {
                // Ctrl-C still force-quits even while the prompt is open.
                self.should_quit = true;
            }
            // Modal swallows all mouse interaction; the user must type or
            // hit Enter.
            InputEvent::Up
            | InputEvent::Down
            | InputEvent::MouseDown { .. }
            | InputEvent::MouseDrag { .. }
            | InputEvent::MouseUp
            | InputEvent::ScrollUp
            | InputEvent::ScrollDown
            | InputEvent::Other => {}
        }
    }

    fn handle_mouse_down(&mut self, row: u16) {
        // Log panel takes precedence: a press inside the log content area
        // starts a text selection there.
        if self.is_in_log_area(row) {
            let logs = self.log_sink.snapshot();
            if let Some(idx) = self.log_index_at_row(row, &logs) {
                self.selection = Some(LogSelection { anchor: idx, cursor: idx });
            }
            return;
        }

        // Otherwise, treat it as a menu click (select + activate).
        let list_first = self.menu_area_top.saturating_add(1);
        if row < list_first {
            return;
        }
        let idx = (row - list_first) as usize;
        if idx < self.items.len() {
            self.menu_state.select(Some(idx));
            self.activate_selected();
        }
    }

    fn handle_mouse_drag(&mut self, row: u16) {
        if self.selection.is_none() {
            return;
        }
        let logs = self.log_sink.snapshot();
        let idx = self.log_index_at_row(row, &logs);
        if let (Some(sel), Some(idx)) = (self.selection.as_mut(), idx) {
            sel.cursor = idx;
        }
    }

    fn scroll_log(&mut self, delta: i32) {
        let logs = self.log_sink.snapshot();
        let total = logs.len();
        let visible = self.log_content_height as usize;
        if total <= visible {
            self.log_scroll = 0;
            return;
        }
        let max_scroll = total - visible;
        let new = (self.log_scroll as i32 + delta).clamp(0, max_scroll as i32);
        self.log_scroll = new as usize;
    }

    fn is_in_log_area(&self, row: u16) -> bool {
        if self.log_content_height == 0 {
            return false;
        }
        let top = self.log_content_top;
        let bottom = top.saturating_add(self.log_content_height);
        row >= top && row < bottom
    }

    /// Map a terminal `row` to an absolute log-line index (into the
    /// oldest-first snapshot), honoring the current scroll offset.
    fn log_index_at_row(&self, row: u16, logs: &[LogLine]) -> Option<usize> {
        if !self.is_in_log_area(row) {
            return None;
        }
        let offset = (row - self.log_content_top) as usize;
        let visible = self.log_content_height as usize;
        let total = logs.len();
        let start = total.saturating_sub(visible + self.log_scroll);
        let idx = start + offset;
        (idx < total).then_some(idx)
    }

    /// Copy the current selection to the system clipboard.
    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else {
            self.status_message = "未选择任何日志".to_string();
            return;
        };
        let logs = self.log_sink.snapshot();
        let (lo, hi) = sel.range();
        let mut text = String::new();
        for log in logs.iter().skip(lo).take(hi - lo + 1) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&log.text);
        }
        if text.is_empty() {
            self.status_message = "选中的日志为空".to_string();
            return;
        }

        match arboard::Clipboard::new() {
            Ok(mut cb) => match cb.set_text(text) {
                Ok(()) => {
                    self.status_message = format!("✓ 已复制 {} 行日志到剪贴板", hi - lo + 1);
                }
                Err(e) => {
                    self.status_message = format!("⚠️  复制到剪贴板失败: {e}");
                }
            },
            Err(e) => {
                self.status_message = format!("⚠️  无法访问剪贴板: {e}");
            }
        }
    }

    fn activate_selected(&mut self) {
        let Some(item) = self.current_item() else { return };
        match MenuAction::from(item) {
            MenuAction::Launch(_with_rathole) => {
                let default: u16 = std::env::var("DEFAULT_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(9464);
                self.mode = Mode::PortPrompt {
                    input: default.to_string(),
                    default,
                    error: None,
                };
                self.status_message = format!("请输入 opencode serve 端口（默认 {default}）");
            }
            MenuAction::Upgrade => {
                self.status_message = "⬆️  正在升级…".to_string();
                let oc_config_dir = dirs::config_dir()
                    .map(|p| p.join("opencode"))
                    .unwrap_or_else(|| PathBuf::from(".config/opencode"));
                let oc_cache_dir = dirs::cache_dir()
                    .map(|p| p.join("opencode"))
                    .unwrap_or_else(|| PathBuf::from(".cache/opencode"));
                tokio::spawn(async move {
                    match upgrade_opencode().await {
                        Ok((UpgradeResult::Upgraded, before, after)) => {
                            tracing::info!("opencode: {before} → {after}");
                        }
                        Ok((UpgradeResult::AlreadyLatest, v, _)) => {
                            tracing::info!("opencode already latest: {v}");
                        }
                        Ok((UpgradeResult::Failed(msg), _, _)) => {
                            tracing::warn!("opencode upgrade: {msg}");
                        }
                        Err(e) => tracing::error!("opencode upgrade failed: {e}"),
                    }
                    match upgrade_omo(&oc_config_dir, &oc_cache_dir).await {
                        Ok(UpgradeResult::Upgraded) => tracing::info!("omo upgraded"),
                        Ok(UpgradeResult::AlreadyLatest) => tracing::info!("omo already latest"),
                        Ok(UpgradeResult::Failed(msg)) => tracing::warn!("omo upgrade: {msg}"),
                        Err(e) => tracing::error!("omo upgrade failed: {e}"),
                    }
                });
            }
            MenuAction::Exit => {
                self.should_quit = true;
                let supervisor = self.supervisor.clone();
                tokio::spawn(async move {
                    let _ = supervisor.shutdown().await;
                });
            }
        }
    }

    fn submit_port_prompt(&mut self) {
        let (input, default) = match &self.mode {
            Mode::PortPrompt { input, default, .. } => (input.clone(), *default),
            Mode::Menu => return,
        };
        // Empty input → use the default.
        let port: u16 = if input.is_empty() {
            default
        } else {
            match input.parse::<u16>() {
                Ok(p) if p > 0 => p,
                Ok(_) => {
                    self.set_port_error("端口必须大于 0".to_string());
                    return;
                }
                Err(_) => {
                    self.set_port_error(format!("'{}' 不是有效的端口号", input));
                    return;
                }
            }
        };

        // Spawn the launch in the background; the log sink will surface
        // the result in the status panel.
        let supervisor = self.supervisor.clone();
        self.status_message = format!("🚀 正在启动服务（port={port}）…");
        self.mode = Mode::Menu;
        tokio::spawn(async move {
            match ServeSupervisor::check_port(port).await {
                Ok(()) => match supervisor.launch_opencode(port).await {
                    Ok(pid) => {
                        tracing::info!("opencode serve PID={pid}");
                    }
                    Err(e) => tracing::error!("launch failed: {e}"),
                },
                Err(e) => tracing::error!("port check failed: {e}"),
            }
        });
    }

    fn set_port_error(&mut self, msg: String) {
        // Re-open the prompt with the same input but a fresh error. We
        // snapshot the existing fields first so we don't fight the borrow
        // checker while rewriting `self.mode`.
        let (input, default) = match &self.mode {
            Mode::PortPrompt { input, default, .. } => (input.clone(), *default),
            Mode::Menu => return,
        };
        self.mode = Mode::PortPrompt {
            input,
            default,
            error: Some(msg.clone()),
        };
        self.status_message = format!("⚠️  {msg}");
    }

    fn select_next(&mut self) {
        let i = self.menu_state.selected().unwrap_or(0);
        let next = (i + 1) % self.items.len();
        self.menu_state.select(Some(next));
    }

    fn select_prev(&mut self) {
        let i = self.menu_state.selected().unwrap_or(0);
        let prev = if i == 0 { self.items.len() - 1 } else { i - 1 };
        self.menu_state.select(Some(prev));
    }

    fn current_item(&self) -> Option<MenuItem> {
        self.menu_state.selected().and_then(|i| self.items.get(i).copied())
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // header
                Constraint::Length(3),  // status summary
                Constraint::Min(6),     // menu
                Constraint::Min(8),     // log panel
            ])
            .split(area);

        // Header
        let header = Paragraph::new(Line::from(vec![Span::styled(
            " opencode TUI 启动器 (Rust + Axum + ratatui) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(header, chunks[0]);

        // Status summary (PID / port / latest message).
        let status_snapshot = self
            .cached_status
            .try_lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let status_text = render_status(&status_snapshot, &self.status_message);
        let status_para = Paragraph::new(status_text)
            .block(Block::default().title("状态").borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        frame.render_widget(status_para, chunks[1]);

        // Menu
        let items: Vec<ListItem<'_>> = self
            .items
            .iter()
            .map(|i| ListItem::new(Line::from(i.label())))
            .collect();
        let list = List::new(items)
            .block(Block::default().title("主菜单").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        // Remember the list's top so mouse clicks can be mapped back.
        self.menu_area_top = chunks[2].y;
        frame.render_stateful_widget(list, chunks[2], &mut self.menu_state);

        // Log panel — scrollable, selectable, copyable.
        self.render_log_panel(frame, chunks[3]);

        // Overlay the port prompt if active.
        if let Mode::PortPrompt { input, default, error } = &self.mode {
            render_port_prompt(frame, area, input, *default, error.as_deref());
        }
    }

    fn render_log_panel(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .title("日志（滚轮滚动 · 拖动选择 · Ctrl+C 复制）")
            .borders(Borders::ALL);
        let inner = block.inner(area);
        // Record the content geometry for mouse hit-testing.
        self.log_content_top = inner.y;
        self.log_content_height = inner.height;

        let logs = self.log_sink.snapshot();
        let visible = inner.height as usize;
        let total = logs.len();

        // Clamp the scroll offset in case the log shrank.
        if total <= visible {
            self.log_scroll = 0;
        } else {
            let max_scroll = total - visible;
            if self.log_scroll > max_scroll {
                self.log_scroll = max_scroll;
            }
        }

        let start = total.saturating_sub(visible + self.log_scroll);
        let end = total.saturating_sub(self.log_scroll);
        let sel_range = self.selection.map(LogSelection::range);

        let mut lines: Vec<Line<'static>> = Vec::new();
        for i in start..end {
            let log = &logs[i];
            let color = level_color(log.level);
            let style = match sel_range {
                Some((lo, hi)) if i >= lo && i <= hi => Style::default()
                    .fg(color)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
                _ => Style::default().fg(color),
            };
            lines.push(Line::from(Span::styled(log.text.clone(), style)));
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "（暂无日志）",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let para = Paragraph::new(lines).block(block);
        frame.render_widget(para, area);
    }
}

fn level_color(level: Level) -> Color {
    match level {
        Level::ERROR => Color::Red,
        Level::WARN => Color::Yellow,
        Level::INFO => Color::Green,
        Level::DEBUG => Color::Blue,
        Level::TRACE => Color::Magenta,
    }
}

fn render_status(s: &ServeStatus, msg: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "状态：",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(msg.to_string()),
        ]),
        Line::from(vec![
            Span::styled(
                "opencode PID: ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!("{:?}  ", s.opencode_pid)),
            Span::styled(
                "rathole PID: ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!("{:?}  ", s.rathole_pid)),
            Span::styled("端口: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{:?}", s.port)),
        ]),
        Line::from(Span::styled(
            "↑/↓ 选择  回车 确认  Esc 清除选择/取消  Ctrl+C 复制(选中时)/退出",
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

fn render_port_prompt(
    frame: &mut Frame<'_>,
    area: Rect,
    input: &str,
    default: u16,
    error: Option<&str>,
) {
    let popup = centered_rect(60, 30, area);
    frame.render_widget(Clear, popup);

    let title = format!(" 启动 opencode serve（默认 {default}） ");
    let mut block = Block::default().title(title).borders(Borders::ALL);
    if error.is_some() {
        block = block.border_style(Style::default().fg(Color::Red));
    } else {
        block = block.border_style(Style::default().fg(Color::Cyan));
    }

    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "请输入端口号（1-65535），回车确认，Esc 取消:",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(
                if input.is_empty() { format!("{default}") } else { input.to_string() },
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            ),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]),
    ];
    if let Some(err) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("⚠  {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, popup);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_w = area.width.saturating_mul(percent_x) / 100;
    let popup_h = area.height.saturating_mul(percent_y) / 100;
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    Rect::new(x, y, popup_w, popup_h)
}
