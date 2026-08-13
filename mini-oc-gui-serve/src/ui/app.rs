//! TUI app state and render loop.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::serve::{ServeStatus, ServeSupervisor};
use crate::ui::events::InputEvent;
use crate::ui::menu::{MenuAction, MenuItem};
use crate::upgrade::{UpgradeResult, upgrade_opencode, upgrade_omo};

/// Main TUI application state.
pub struct TuiApp {
    /// Process supervisor (opencode + rathole).
    pub supervisor: ServeSupervisor,
    /// Currently focused menu entry.
    pub menu_state: ListState,
    /// All menu items.
    pub items: Vec<MenuItem>,
    /// Latest status message.
    pub status_message: String,
    /// Shared cache of the latest supervisor status snapshot.
    pub cached_status: std::sync::Arc<tokio::sync::Mutex<ServeStatus>>,
    /// Set by input handlers; the loop exits when `true`.
    pub should_quit: bool,
}

impl TuiApp {
    /// Construct a new TUI app bound to a supervisor.
    #[must_use]
    pub fn new(supervisor: ServeSupervisor) -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));
        Self {
            supervisor,
            menu_state,
            items: MenuItem::all(),
            status_message: "就绪".to_string(),
            cached_status: std::sync::Arc::new(tokio::sync::Mutex::new(ServeStatus::default())),
            should_quit: false,
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
                    if let crossterm::event::Event::Key(k) = ev {
                        self.handle_key(InputEvent::from(k));
                    }
                }
            }
        }
        refresh_handle.abort();
        Ok(())
    }

    fn handle_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Up => self.select_prev(),
            InputEvent::Down => self.select_next(),
            InputEvent::Select => {
                let Some(item) = self.current_item() else { return };
                let action = MenuAction::from(item);
                let supervisor = self.supervisor.clone();
                let mut status = std::mem::take(&mut self.status_message);
                // Synchronous action dispatcher; async upgrades are spawned.
                match action {
                    MenuAction::Launch(with_rathole) => {
                        status = format!("🚀 正在启动服务（rathole={with_rathole}）…");
                        let port: u16 = std::env::var("DEFAULT_PORT")
                            .ok()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(9464);
                        let supervisor_for_launch = supervisor.clone();
                        tokio::spawn(async move {
                            match ServeSupervisor::check_port(port).await {
                                Ok(()) => match supervisor_for_launch.launch_opencode(port).await {
                                    Ok(pid) => tracing::info!("opencode serve PID={pid}"),
                                    Err(e) => tracing::error!("launch failed: {e}"),
                                },
                                Err(e) => tracing::error!("port check failed: {e}"),
                            }
                        });
                    }
                    MenuAction::Upgrade => {
                        status = "⬆️  正在升级…".to_string();
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
                        let _ = supervisor.shutdown();
                    }
                }
                self.status_message = status;
            }
            InputEvent::Quit => {
                self.should_quit = true;
            }
            InputEvent::Other => {}
        }
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(5),
            ])
            .split(frame.area());

        // Header
        let header = Paragraph::new(Line::from(vec![Span::styled(
            " opencode TUI 启动器 (Rust + Axum + ratatui) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
        .block(Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
        frame.render_widget(header, chunks[0]);

        // Menu
        let items: Vec<ListItem<'_>> = self
            .items
            .iter()
            .map(|i| ListItem::new(Line::from(i.label())))
            .collect();
        let list = List::new(items)
            .block(Block::default().title("主菜单").borders(Borders::ALL))
            .highlight_style(Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        frame.render_stateful_widget(list, chunks[1], &mut self.menu_state);

        // Status panel — uses the cache refreshed by the background task.
        // `try_lock` avoids blocking the render thread on a contended mutex.
        let status_snapshot = self
            .cached_status
            .try_lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let status_text = render_status(&status_snapshot, &self.status_message);
        let status_para = Paragraph::new(status_text)
            .block(Block::default().title("状态").borders(Borders::ALL));
        frame.render_widget(status_para, chunks[2]);
    }
}

fn render_status(s: &ServeStatus, msg: &str) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("状态：", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(msg.to_string()),
        ]),
        Line::from(format!(
            "opencode PID: {:?}    rathole PID: {:?}    端口: {:?}",
            s.opencode_pid, s.rathole_pid, s.port
        )),
    ];
    if let Some(ts) = s.started_at {
        lines.push(Line::from(format!("启动时间：{ts}")));
    }
    lines.push(Line::from(Span::styled(
        "↑/↓ 选择   回车 确认   Esc / q 退出",
        Style::default().fg(Color::DarkGray),
    )));
    lines
}
