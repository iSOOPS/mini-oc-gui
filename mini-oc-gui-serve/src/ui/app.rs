//! TUI app state and render loop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::EventStream;
use futures::StreamExt;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, ListState, Padding, Paragraph},
};

use crate::attach::{AttachedSession, OcSession, OpencodeClient, choose_folder, spawn_in_new_terminal};
use crate::auth::AuthConfig;
use crate::config::{SbConfig, SB_ENV_FILE};
use crate::domain::{PathEntry, PathValidator};
use crate::serve::{
    ServeStatus, ServeSupervisor, rathole_default_bin, rathole_default_config,
};
use crate::storage::PathListStore;
use crate::storage::remote::RemoteClient;
use crate::ui::events::InputEvent;
use crate::ui::log::LogBuffer;
use crate::ui::menu::{MenuAction, MenuItem};
use crate::upgrade::{UpgradeResult, upgrade_opencode, upgrade_omo};

const MAIN_ITEMS: [MenuItem; 4] = [
    MenuItem::OcServe,
    MenuItem::Rathole,
    MenuItem::UpgradeOpenCodeAndOmo,
    MenuItem::Exit,
];
const PROJECTS_ITEMS: [MenuItem; 1] = [MenuItem::OcProjects];

/// TUI 交互模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    /// 主菜单导航。
    Menu,
    /// 首次配置：编辑用户名。
    ConfigUsername,
    /// 首次配置：编辑密码。
    ConfigPassword,
    /// 启动 serve 端口输入。
    ConfigPort,
    /// 验证 serve 端口（OC 项目入口）。
    ServePort,
    /// 设置：远程 SilverBullet 路径。
    SettingsUrl,
    /// 设置：SilverBullet 用户名。
    SettingsUser,
    /// 设置：SilverBullet 密码。
    SettingsPassword,
}

/// 主菜单模式下的焦点位置（三个栏）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// 合并栏（服务 + 系统）.
    Main,
    /// OC 项目栏。
    Projects,
    /// 当前服务面板。
    ServicePanel,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Main => Self::Projects,
            Self::Projects => Self::ServicePanel,
            Self::ServicePanel => Self::Main,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Main => Self::ServicePanel,
            Self::Projects => Self::Main,
            Self::ServicePanel => Self::Projects,
        }
    }
}

/// 「OC 项目」子页面（复刻 `oc-serve-tui-actuator.sh` 的选择流程）。
#[derive(Debug)]
enum SubPage {
    /// 项目列表。
    Projects { list_state: ListState, projects: Vec<PathEntry> },
    /// 某项目的会话列表。
    Sessions { project: String, list_state: ListState, sessions: Vec<OcSession> },
    /// 新建 path 方式选择（系统路径 / 手动输入）。
    NewPathChoice { list_state: ListState },
    /// 手动输入路径。
    ManualPath { input: String, error: Option<String> },
}

/// 子页面「选择」动作（先提取数据再执行，避免借用冲突）。
enum SelectAction {
    None,
    EnterNewPathChoice,
    ChooseFolder,
    EnterManualPath,
    EnterSessions(String),
    CreateSession(String),
    Attach(String, String),
    DeleteProject(String),
    ConfirmPath(String),
}

/// 二次确认动作。
#[derive(Debug, Clone, Copy)]
enum ConfirmAction {
    /// 杀死/关闭「当前服务」栏第 i 个服务。
    ExitService(usize),
}

/// 确认弹框的按钮选中态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmChoice {
    Confirm,
    Cancel,
}

impl ConfirmChoice {
    fn toggle(self) -> Self {
        match self {
            Self::Confirm => Self::Cancel,
            Self::Cancel => Self::Confirm,
        }
    }
}

/// 鼠标点击目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickTarget {
    /// 主菜单栏（服务与系统）第 i 项。
    MainColumn(usize),
    /// OC 项目栏第 i 项。
    ProjectsColumn(usize),
    /// 当前服务栏第 i 项。
    ServicePanel(usize),
    /// OC 项目子页面第 i 项。
    SubPage(usize),
    /// Header 设置按钮。
    Settings,
    /// 确认弹框的确认按钮。
    ConfirmButton,
    /// 确认弹框的取消按钮。
    CancelButton,
}

/// 可点击区域（每帧渲染时记录）。
#[derive(Debug, Clone, Copy)]
struct ClickRegion {
    rect: Rect,
    target: ClickTarget,
}

/// 主菜单栏的列类型（用于记录点击目标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Main,
    Projects,
}

/// Main TUI application state.
pub struct TuiApp {
    /// Process supervisor (opencode + rathole).
    pub supervisor: ServeSupervisor,
    /// 运行时共享的认证配置，首次配置填写后热更新。
    pub auth: Arc<RwLock<AuthConfig>>,
    /// 日志缓冲区（tracing 写入，日志面板渲染）.
    pub log_buffer: LogBuffer,
    /// path-list 存储（项目列表 + sections 记账）.
    pub store: Arc<PathListStore>,
    /// 两个主菜单栏的选中状态。
    main_state: ListState,
    projects_state: ListState,
    /// 「当前服务」栏的选中状态。
    service_state: ListState,
    /// Latest status message（共享，供异步任务回写结果）.
    pub status_message: Arc<Mutex<String>>,
    /// Shared cache of the latest supervisor status snapshot.
    pub cached_status: Arc<Mutex<ServeStatus>>,
    /// Set by input handlers; the loop exits when `true`.
    pub should_quit: bool,
    /// 当前交互模式。
    input_mode: InputMode,
    /// 主菜单模式下的焦点。
    focus: Focus,
    /// 「OC 项目」子页面（None = 主菜单）。
    sub_page: Option<SubPage>,
    /// 已在新终端启动的 attach 会话（「当前服务」栏展示）.
    attached_sessions: Arc<Mutex<Vec<AttachedSession>>>,
    /// 当前 attach 目标 URL（进入 OC 项目时根据 serve 状态确定）。
    attach_url: String,
    /// 首次配置：用户名输入缓冲。
    username_input: String,
    /// 首次配置：密码输入缓冲。
    password_input: String,
    /// 首次配置：表单校验错误提示。
    config_error: Option<String>,
    /// 端口输入缓冲（启动 / 验证共用）。
    port_input: String,
    /// 日志全屏模式。
    show_full_log: bool,
    /// 全屏日志滚动偏移（向上滚动的行数）。
    log_scroll: usize,
    /// 待二次确认的动作（杀死/关闭服务）。
    confirm: Option<ConfirmAction>,
    /// 确认弹框的按钮选中态。
    confirm_choice: ConfirmChoice,
    /// 远程 SilverBullet 配置（设置弹框热更新）.
    sb_config: Arc<RwLock<SbConfig>>,
    /// 远端服务验证状态（状态框第二行展示）.
    remote_status: Arc<Mutex<String>>,
    /// 程序启动时刻（状态框展示运行时长）.
    program_started_at: chrono::DateTime<chrono::Local>,
    /// 设置：SB URL 输入缓冲。
    sb_url_input: String,
    /// 设置：SB 用户名输入缓冲。
    sb_user_input: String,
    /// 设置：SB 密码输入缓冲。
    sb_password_input: String,
    /// 当前帧的可点击区域（渲染时填充，鼠标事件查询）。
    click_regions: Vec<ClickRegion>,
}

impl TuiApp {
    /// Construct a new TUI app bound to a supervisor + shared auth + log buffer + store + sb config.
    #[must_use]
    pub fn new(
        supervisor: ServeSupervisor,
        auth: Arc<RwLock<AuthConfig>>,
        log_buffer: LogBuffer,
        store: Arc<PathListStore>,
        sb_config: Arc<RwLock<SbConfig>>,
    ) -> Self {
        let mut main_state = ListState::default();
        main_state.select(Some(0));
        let mut projects_state = ListState::default();
        projects_state.select(Some(0));
        let mut service_state = ListState::default();
        service_state.select(Some(0));

        let configured = auth.read().map(|a| a.is_configured()).unwrap_or(false);
        let (input_mode, username_input, status) = if configured {
            (InputMode::Menu, String::new(), "就绪".to_string())
        } else {
            let existing = auth
                .read()
                .map(|a| a.basic_user.clone())
                .unwrap_or_default();
            let username = if existing.is_empty() {
                "opencode".to_string()
            } else {
                existing
            };
            (
                InputMode::ConfigUsername,
                username,
                "首次启动：请配置用户名和密码".to_string(),
            )
        };

        Self {
            supervisor,
            auth,
            log_buffer,
            store,
            main_state,
            projects_state,
            service_state,
            status_message: Arc::new(Mutex::new(status)),
            cached_status: Arc::new(Mutex::new(ServeStatus::default())),
            should_quit: false,
            input_mode,
            focus: Focus::Main,
            sub_page: None,
            attached_sessions: Arc::new(Mutex::new(Vec::new())),
            attach_url: std::env::var("ATTACH_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:9464".to_string()),
            username_input,
            password_input: String::new(),
            config_error: None,
            port_input: String::new(),
            show_full_log: false,
            log_scroll: 0,
            confirm: None,
            confirm_choice: ConfirmChoice::Confirm,
            sb_config,
            remote_status: Arc::new(Mutex::new(String::new())),
            program_started_at: chrono::Local::now(),
            sb_url_input: String::new(),
            sb_user_input: String::new(),
            sb_password_input: String::new(),
            click_regions: Vec::new(),
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
                *cache_for_refresh.lock().unwrap() = status;
            }
        });

        // 启动后异步验证远端服务可用性。
        self.verify_remote();

        // 启用鼠标捕获。
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::EnableMouseCapture
        );

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
                            self.handle_key(InputEvent::from(k)).await;
                        }
                        crossterm::event::Event::Mouse(m) => {
                            self.handle_mouse(m).await;
                        }
                        _ => {}
                    }
                }
            }
        }
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::event::DisableMouseCapture
        );
        refresh_handle.abort();
        Ok(())
    }

    fn verify_remote(&self) {
        let cfg = self.sb_config.read().unwrap_or_else(|e| e.into_inner()).clone();
        let remote_status = self.remote_status.clone();
        tokio::spawn(async move {
            if !cfg.is_configured() {
                *remote_status.lock().unwrap() =
                    "未配置远程存储地址，请在[设置]中配置".to_string();
                return;
            }
            let mut remote =
                RemoteClient::with_credentials(cfg.url.clone(), cfg.user.clone(), cfg.password.clone());
            match remote.get("/serv/opencode/path-list.md").await {
                Ok((0, _)) => {
                    *remote_status.lock().unwrap() = format!("远程存储不可用：{}", cfg.url);
                }
                Ok((status, _)) if status >= 400 => {
                    *remote_status.lock().unwrap() = format!("远程存储不可用：HTTP {status}");
                }
                Ok(_) => {
                    *remote_status.lock().unwrap() = format!("远程存储已连接：{}", cfg.url);
                }
                Err(e) => {
                    *remote_status.lock().unwrap() = format!("远程存储不可用：{e}");
                }
            }
        });
    }

    async fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        match mouse.kind {
            crossterm::event::MouseEventKind::Moved | crossterm::event::MouseEventKind::Drag(_) => {
                self.hover_at(mouse.column, mouse.row);
            }
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.click_at(mouse.column, mouse.row).await;
            }
            _ => {}
        }
    }

    fn find_target(&self, col: u16, row: u16) -> Option<ClickTarget> {
        self.click_regions
            .iter()
            .find(|r| {
                r.rect.x <= col
                    && col < r.rect.x + r.rect.width
                    && r.rect.y <= row
                    && row < r.rect.y + r.rect.height
            })
            .map(|r| r.target)
    }

    fn hover_at(&mut self, col: u16, row: u16) {
        let Some(target) = self.find_target(col, row) else { return };
        match target {
            ClickTarget::MainColumn(i) => {
                self.main_state.select(Some(i));
                self.focus = Focus::Main;
            }
            ClickTarget::ProjectsColumn(i) => {
                self.projects_state.select(Some(i));
                self.focus = Focus::Projects;
            }
            ClickTarget::ServicePanel(i) => {
                self.service_state.select(Some(i));
                self.focus = Focus::ServicePanel;
            }
            ClickTarget::SubPage(i) => self.set_sub_page_selected(i),
            _ => {}
        }
    }

    async fn click_at(&mut self, col: u16, row: u16) {
        let Some(target) = self.find_target(col, row) else { return };
        match target {
            ClickTarget::MainColumn(i) => {
                self.main_state.select(Some(i));
                self.focus = Focus::Main;
                self.focus_select().await;
            }
            ClickTarget::ProjectsColumn(i) => {
                self.projects_state.select(Some(i));
                self.focus = Focus::Projects;
                self.focus_select().await;
            }
            ClickTarget::ServicePanel(i) => {
                self.service_state.select(Some(i));
                self.focus = Focus::ServicePanel;
                self.focus_select().await;
            }
            ClickTarget::SubPage(i) => {
                self.set_sub_page_selected(i);
                self.sub_page_select().await;
            }
            ClickTarget::Settings => self.open_settings(),
            ClickTarget::ConfirmButton => {
                let action = self.confirm.take();
                if let Some(ConfirmAction::ExitService(i)) = action {
                    self.exit_service(i);
                }
            }
            ClickTarget::CancelButton => {
                self.confirm = None;
            }
        }
    }

    fn set_sub_page_selected(&mut self, i: usize) {
        if let Some(sub) = &mut self.sub_page {
            match sub {
                SubPage::Projects { list_state, .. } => list_state.select(Some(i)),
                SubPage::Sessions { list_state, .. } => list_state.select(Some(i)),
                SubPage::NewPathChoice { list_state } => list_state.select(Some(i)),
                SubPage::ManualPath { .. } => {}
            }
        }
    }

    fn open_settings(&mut self) {
        let cfg = self.sb_config.read().unwrap_or_else(|e| e.into_inner()).clone();
        self.sb_url_input = cfg.url;
        self.sb_user_input = cfg.user;
        self.sb_password_input = cfg.password;
        self.input_mode = InputMode::SettingsUrl;
    }

    async fn handle_settings_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Tab => self.switch_settings_field(),
            InputEvent::Backspace => match self.input_mode {
                InputMode::SettingsUrl => {
                    self.sb_url_input.pop();
                }
                InputMode::SettingsUser => {
                    self.sb_user_input.pop();
                }
                InputMode::SettingsPassword => {
                    self.sb_password_input.pop();
                }
                _ => {}
            },
            InputEvent::Char(c) => match self.input_mode {
                InputMode::SettingsUrl => self.sb_url_input.push(c),
                InputMode::SettingsUser => self.sb_user_input.push(c),
                InputMode::SettingsPassword => self.sb_password_input.push(c),
                _ => {}
            },
            InputEvent::Select => self.submit_settings().await,
            InputEvent::Quit => {
                self.input_mode = InputMode::Menu;
            }
            _ => {}
        }
    }

    fn switch_settings_field(&mut self) {
        self.input_mode = match self.input_mode {
            InputMode::SettingsUrl => InputMode::SettingsUser,
            InputMode::SettingsUser => InputMode::SettingsPassword,
            InputMode::SettingsPassword => InputMode::SettingsUrl,
            other => other,
        };
    }

    async fn submit_settings(&mut self) {
        let cfg = SbConfig {
            url: self.sb_url_input.trim().to_string(),
            user: self.sb_user_input.trim().to_string(),
            password: self.sb_password_input.clone(),
        };

        // 连接测试：成功才写文件 + 更新内存。
        let mut remote = RemoteClient::with_credentials(
            cfg.url.clone(),
            cfg.user.clone(),
            cfg.password.clone(),
        );
        let test_result = remote.get("/serv/opencode/path-list.md").await;
        let connected = matches!(&test_result, Ok((status, _)) if (200..400).contains(status));
        if !connected {
            let msg = match &test_result {
                Ok((0, _)) => "❌ 连接测试失败：无法访问远端（网络/超时），未保存".to_string(),
                Ok((status, _)) => format!("❌ 连接测试失败：HTTP {status}，未保存"),
                Err(e) => format!("❌ 连接测试失败：{e}，未保存"),
            };
            *self.status_message.lock().unwrap() = msg;
            return;
        }

        let write_result = cfg.write_env_file(Path::new(SB_ENV_FILE));
        {
            let mut guard = self.sb_config.write().unwrap_or_else(|e| e.into_inner());
            guard.url = cfg.url.clone();
            guard.user = cfg.user.clone();
            guard.password = cfg.password.clone();
        }
        let store = self.store.clone();
        let store_cfg = cfg.clone();
        tokio::spawn(async move {
            let remote = RemoteClient::with_credentials(
                store_cfg.url,
                store_cfg.user,
                store_cfg.password,
            );
            store.with_remote(remote).await;
            if let Err(e) = store.refresh().await {
                tracing::warn!("settings refresh failed: {e}");
            }
        });
        self.input_mode = InputMode::Menu;
        let msg = match write_result {
            Ok(()) => "✅ 设置已保存".to_string(),
            Err(e) => format!("⚠️ 连接成功但写文件失败：{e}"),
        };
        *self.status_message.lock().unwrap() = msg;
        self.verify_remote();
    }

    async fn handle_key(&mut self, event: InputEvent) {
        if self.confirm.is_some() {
            match event {
                InputEvent::Left | InputEvent::Right | InputEvent::Tab => {
                    self.confirm_choice = self.confirm_choice.toggle();
                }
                InputEvent::Select => {
                    let action = self.confirm.take();
                    if let Some(ConfirmAction::ExitService(i)) = action {
                        if self.confirm_choice == ConfirmChoice::Confirm {
                            self.exit_service(i);
                        }
                    }
                }
                InputEvent::Quit | InputEvent::Char('q') => {
                    self.confirm = None;
                }
                _ => {}
            }
            return;
        }
        if self.show_full_log {
            match event {
                InputEvent::Quit | InputEvent::Char('q') | InputEvent::Char('l') => {
                    self.show_full_log = false;
                }
                InputEvent::Up | InputEvent::Char('k') => {
                    self.log_scroll = self.log_scroll.saturating_add(1);
                }
                InputEvent::Down | InputEvent::Char('j') => {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
                _ => {}
            }
            return;
        }
        if self.sub_page.is_some() {
            self.handle_sub_page_key(event).await;
            return;
        }
        match self.input_mode {
            InputMode::Menu => self.handle_menu_key(event).await,
            InputMode::ConfigUsername | InputMode::ConfigPassword => {
                self.handle_config_key(event)
            }
            InputMode::ConfigPort => self.handle_port_key(event),
            InputMode::ServePort => self.handle_serve_port_key(event).await,
            InputMode::SettingsUrl | InputMode::SettingsUser | InputMode::SettingsPassword => {
                self.handle_settings_key(event).await
            }
        }
    }

    fn status_snapshot(&self) -> ServeStatus {
        self.cached_status.lock().unwrap().clone()
    }

    fn build_oc_client(&self) -> OpencodeClient {
        let auth = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone();
        OpencodeClient::new(self.attach_url.clone(), auth.basic_user, auth.basic_password)
    }

    // --- 主菜单键盘处理 ---

    async fn handle_menu_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Tab | InputEvent::Right => self.focus = self.focus.next(),
            InputEvent::Left => self.focus = self.focus.prev(),
            InputEvent::Up | InputEvent::Char('k') => self.focus_move(-1),
            InputEvent::Down | InputEvent::Char('j') => self.focus_move(1),
            InputEvent::Select => self.focus_select().await,
            InputEvent::Char('l') | InputEvent::Char('L') => {
                self.show_full_log = true;
                self.log_scroll = 0;
            }
            InputEvent::Char('s') | InputEvent::Char('S') => self.open_settings(),
            InputEvent::Quit | InputEvent::Char('q') => self.should_quit = true,
            _ => {}
        }
    }

    fn focus_move(&mut self, delta: i32) {
        match self.focus {
            Focus::Main => {
                let i = self.main_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(MAIN_ITEMS.len() as i32) as usize;
                self.main_state.select(Some(next));
            }
            Focus::Projects => {
                let i = self.projects_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(PROJECTS_ITEMS.len() as i32) as usize;
                self.projects_state.select(Some(next));
            }
            Focus::ServicePanel => {
                let len = self.service_item_count();
                if len == 0 {
                    return;
                }
                let i = self.service_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(len as i32) as usize;
                self.service_state.select(Some(next));
            }
        }
    }

    async fn focus_select(&mut self) {
        match self.focus {
            Focus::Main => {
                let i = self.main_state.selected().unwrap_or(0);
                if let Some(item) = MAIN_ITEMS.get(i) {
                    self.activate_item(*item).await;
                }
            }
            Focus::Projects => {
                if let Some(item) = PROJECTS_ITEMS.first() {
                    self.activate_item(*item).await;
                }
            }
            Focus::ServicePanel => {
                let i = self.service_state.selected().unwrap_or(0);
                self.confirm = Some(ConfirmAction::ExitService(i));
            }
        }
    }

    fn service_item_count(&self) -> usize {
        let status = self.status_snapshot();
        let mut count = 0;
        if status.opencode_pid.is_some() {
            count += 1;
        }
        if status.rathole_pid.is_some() {
            count += 1;
        }
        count + self.attached_sessions.lock().unwrap().len()
    }

    fn exit_service(&mut self, i: usize) {
        let status = self.status_snapshot();
        let mut offset = 0;
        if status.opencode_pid.is_some() {
            if i == offset {
                self.stop_opencode();
                return;
            }
            offset += 1;
        }
        if status.rathole_pid.is_some() {
            if i == offset {
                self.stop_rathole();
                return;
            }
            offset += 1;
        }
        self.kill_session(i - offset);
    }

    fn kill_session(&mut self, idx: usize) {
        let removed = {
            let mut sessions = self.attached_sessions.lock().unwrap();
            if idx >= sessions.len() {
                return;
            }
            sessions.remove(idx)
        };
        if let Ok(pid_str) = std::fs::read_to_string(&removed.pid_file) {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let _ = std::process::Command::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .status();
            }
        }
        let _ = std::fs::remove_file(&removed.pid_file);
        *self.status_message.lock().unwrap() = format!("✅ 已杀死会话：{}", removed.session);
    }

    async fn activate_item(&mut self, item: MenuItem) {
        match MenuAction::from(item) {
            MenuAction::ToggleOcServe => {
                if self.status_snapshot().opencode_pid.is_some() {
                    self.stop_opencode();
                } else {
                    self.port_input = std::env::var("DEFAULT_PORT")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "9464".to_string());
                    self.input_mode = InputMode::ConfigPort;
                }
            }
            MenuAction::ToggleRathole => {
                if self.status_snapshot().rathole_pid.is_some() {
                    self.stop_rathole();
                } else {
                    self.launch_rathole();
                }
            }
            MenuAction::EnterProjects => self.enter_projects_flow().await,
            MenuAction::Upgrade => self.start_upgrade(),
            MenuAction::Exit => {
                self.should_quit = true;
            }
        }
    }

    // --- OC 项目 serve 端口逻辑 ---

    async fn enter_projects_flow(&mut self) {
        let status = self.status_snapshot();
        if let (Some(_pid), Some(port)) = (status.opencode_pid, status.port) {
            self.attach_url = format!("http://127.0.0.1:{port}");
            self.enter_projects().await;
        } else {
            self.port_input = "9464".to_string();
            self.input_mode = InputMode::ServePort;
        }
    }

    async fn handle_serve_port_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Backspace => {
                self.port_input.pop();
            }
            InputEvent::Char(c) if c.is_ascii_digit() => {
                if self.port_input.len() < 5 {
                    self.port_input.push(c);
                }
            }
            InputEvent::Select => self.submit_serve_port().await,
            InputEvent::Quit => {
                self.input_mode = InputMode::Menu;
            }
            _ => {}
        }
    }

    async fn submit_serve_port(&mut self) {
        let port: u16 = match self.port_input.parse() {
            Ok(p) if p > 0 => p,
            _ => {
                *self.status_message.lock().unwrap() =
                    "❌ 端口无效，请输入 1-65535".to_string();
                self.port_input.clear();
                return;
            }
        };
        let url = format!("http://127.0.0.1:{port}");
        let auth = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone();
        let client = OpencodeClient::new(url.clone(), auth.basic_user, auth.basic_password);
        match client.health_check().await {
            Ok(()) => {
                self.attach_url = url;
                self.input_mode = InputMode::Menu;
                self.enter_projects().await;
            }
            Err(e) => {
                *self.status_message.lock().unwrap() =
                    format!("❌ opencode 验证失败：{e}，请重新输入端口");
                self.port_input.clear();
            }
        }
    }

    // --- 服务操作 ---

    fn stop_opencode(&mut self) {
        let status = self.status_message.clone();
        *status.lock().unwrap() = "⏹ 正在停止 OpenCode Serve…".to_string();
        let supervisor = self.supervisor.clone();
        tokio::spawn(async move {
            let msg = match supervisor.stop_opencode().await {
                Ok(()) => "✅ OpenCode Serve 已停止".to_string(),
                Err(e) => format!("❌ 停止失败：{e}"),
            };
            *status.lock().unwrap() = msg;
        });
    }

    fn stop_rathole(&mut self) {
        let status = self.status_message.clone();
        *status.lock().unwrap() = "⏹ 正在停止 Rathole…".to_string();
        let supervisor = self.supervisor.clone();
        tokio::spawn(async move {
            let msg = match supervisor.stop_rathole().await {
                Ok(()) => "✅ Rathole 已停止".to_string(),
                Err(e) => format!("❌ 停止失败：{e}"),
            };
            *status.lock().unwrap() = msg;
        });
    }

    fn launch_rathole(&mut self) {
        let status = self.status_message.clone();
        *status.lock().unwrap() = "🚀 正在启动 rathole…".to_string();
        let bin = rathole_default_bin();
        let config = rathole_default_config();
        let supervisor = self.supervisor.clone();
        tokio::spawn(async move {
            let msg = match supervisor.launch_rathole(&bin, &config).await {
                Ok(pid) => format!("✅ rathole 已启动，PID={pid}"),
                Err(e) => format!("❌ rathole 启动失败：{e}"),
            };
            *status.lock().unwrap() = msg;
        });
    }

    fn start_upgrade(&mut self) {
        let status = self.status_message.clone();
        *status.lock().unwrap() = "⬆️ 正在升级…".to_string();
        let oc_config_dir = dirs::config_dir()
            .map(|p| p.join("opencode"))
            .unwrap_or_else(|| PathBuf::from(".config/opencode"));
        let oc_cache_dir = dirs::cache_dir()
            .map(|p| p.join("opencode"))
            .unwrap_or_else(|| PathBuf::from(".cache/opencode"));
        tokio::spawn(async move {
            let oc_msg = match upgrade_opencode().await {
                Ok((UpgradeResult::Upgraded, before, after)) => {
                    format!("opencode: {before} → {after}")
                }
                Ok((UpgradeResult::AlreadyLatest, v, _)) => {
                    format!("opencode 已是最新: {v}")
                }
                Ok((UpgradeResult::Failed(msg), _, _)) => {
                    format!("opencode 升级失败: {msg}")
                }
                Err(e) => format!("opencode 升级错误: {e}"),
            };
            let omo_msg = match upgrade_omo(&oc_config_dir, &oc_cache_dir).await {
                Ok(UpgradeResult::Upgraded) => "omo 已升级".to_string(),
                Ok(UpgradeResult::AlreadyLatest) => "omo 已是最新".to_string(),
                Ok(UpgradeResult::Failed(msg)) => format!("omo 升级失败: {msg}"),
                Err(e) => format!("omo 升级错误: {e}"),
            };
            *status.lock().unwrap() = format!("{oc_msg} | {omo_msg}");
        });
    }

    // --- 启动端口输入（ConfigPort） ---

    fn handle_port_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Backspace => {
                self.port_input.pop();
            }
            InputEvent::Char(c) if c.is_ascii_digit() => {
                if self.port_input.len() < 5 {
                    self.port_input.push(c);
                }
            }
            InputEvent::Select => self.submit_port(),
            InputEvent::Quit => {
                self.input_mode = InputMode::Menu;
            }
            _ => {}
        }
    }

    fn submit_port(&mut self) {
        let port: u16 = match self.port_input.parse() {
            Ok(p) if p > 0 => p,
            _ => {
                *self.status_message.lock().unwrap() =
                    "❌ 端口无效，请输入 1-65535".to_string();
                return;
            }
        };
        self.input_mode = InputMode::Menu;

        let status = self.status_message.clone();
        *status.lock().unwrap() = format!("🚀 正在启动服务（port={port}）…");
        let supervisor_for_launch = self.supervisor.clone();
        tokio::spawn(async move {
            let result = match ServeSupervisor::check_port(port).await {
                Ok(()) => supervisor_for_launch.launch_opencode(port).await,
                Err(e) => Err(e),
            };
            let msg = match result {
                Ok(pid) => format!("✅ 服务已启动，端口 {port}，PID={pid}"),
                Err(e) => format!("❌ 启动失败：{e}"),
            };
            *status.lock().unwrap() = msg;
        });
    }

    // --- 首次配置键盘处理 ---

    fn handle_config_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Tab => self.switch_config_field(),
            InputEvent::Backspace => {
                match self.input_mode {
                    InputMode::ConfigUsername => {
                        self.username_input.pop();
                    }
                    InputMode::ConfigPassword => {
                        self.password_input.pop();
                    }
                    _ => {}
                }
                self.config_error = None;
            }
            InputEvent::Char(c) => {
                match self.input_mode {
                    InputMode::ConfigUsername => self.username_input.push(c),
                    InputMode::ConfigPassword => self.password_input.push(c),
                    _ => {}
                }
                self.config_error = None;
            }
            InputEvent::Select => self.submit_config(),
            InputEvent::Quit => {
                self.should_quit = true;
            }
            _ => {}
        }
    }

    fn switch_config_field(&mut self) {
        self.input_mode = match self.input_mode {
            InputMode::ConfigUsername => InputMode::ConfigPassword,
            InputMode::ConfigPassword => InputMode::ConfigUsername,
            other => other,
        };
        self.config_error = None;
    }

    fn submit_config(&mut self) {
        let username = if self.username_input.is_empty() {
            "opencode".to_string()
        } else {
            self.username_input.clone()
        };
        if self.password_input.is_empty() {
            self.config_error = Some("密码不能为空".to_string());
            return;
        }
        let password = self.password_input.clone();

        let env_path = std::env::var("OC_SERVE_AUTH_ENV")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".oc-serve-auth.env"));

        let write_result = {
            let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
            guard.write_env_file(&env_path, &username, &password)
        };

        let mut guard = self.auth.write().unwrap_or_else(|e| e.into_inner());
        guard.basic_user = username;
        guard.basic_password = password;
        drop(guard);

        self.input_mode = InputMode::Menu;
        self.config_error = None;
        let msg = match write_result {
            Ok(()) => format!("✅ 凭据已保存到 {}", env_path.display()),
            Err(e) => format!("⚠️ 写文件失败（内存已生效）：{e}"),
        };
        *self.status_message.lock().unwrap() = msg;
    }

    // --- 「OC 项目」子页面处理 ---

    async fn enter_projects(&mut self) {
        let _ = self.store.refresh().await;
        let projects = self.store.list().await.unwrap_or_default();
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        self.sub_page = Some(SubPage::Projects { list_state, projects });
    }

    async fn enter_sessions(&mut self, project: String) {
        let client = self.build_oc_client();
        let sessions = match client.list_sessions(&project).await {
            Ok(s) => s,
            Err(e) => {
                *self.status_message.lock().unwrap() = format!("⚠️ 拉取会话失败：{e}");
                Vec::new()
            }
        };
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        self.sub_page = Some(SubPage::Sessions { project, list_state, sessions });
    }

    async fn create_and_attach(&mut self, project: String) {
        let client = self.build_oc_client();
        *self.status_message.lock().unwrap() = "🚀 正在创建会话…".to_string();
        match client.create_session(&project).await {
            Ok(sid) => {
                let _ = self.store.append_session(&project, &sid).await;
                let _ = self.store.touch_path(&project).await;
                self.trigger_attach(project, sid);
            }
            Err(e) => {
                *self.status_message.lock().unwrap() = format!("❌ 创建会话失败：{e}");
            }
        }
    }

    async fn delete_project(&mut self, project: String) {
        let _ = self.store.remove_path(&project).await;
        *self.status_message.lock().unwrap() = format!("✅ 已删除项目记录：{project}");
        self.enter_projects().await;
    }

    async fn confirm_manual_path(&mut self, input: String) {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            let default_dir = std::env::var("OC_DEFAULT_DIR")
                .unwrap_or_else(|_| "/Users/samuel/.config/opencode".to_string());
            self.enter_sessions(default_dir).await;
            return;
        }
        match PathValidator::validate(trimmed) {
            Ok(path) => {
                let _ = self.store.upsert_path(&path).await;
                self.enter_sessions(path).await;
            }
            Err(e) => {
                if let Some(SubPage::ManualPath { error, .. }) = &mut self.sub_page {
                    *error = Some(e.to_string());
                }
            }
        }
    }

    fn trigger_attach(&mut self, directory: String, session: String) {
        let auth = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone();
        let pid_file = format!("/tmp/oc-attach-{}.pid", session);
        let attach_cmd = format!(
            "opencode attach \"{}\" --dir \"{}\" --session \"{}\" -u \"{}\" -p \"{}\"",
            self.attach_url, directory, session, auth.basic_user, auth.basic_password
        );
        let wrapped = format!("bash -c 'echo $$ > {pid_file}; exec {attach_cmd}'");
        let sessions = self.attached_sessions.clone();
        let dir_clone = directory.clone();
        let sess_clone = session.clone();
        let status = self.status_message.clone();
        let started_at = chrono::Utc::now().timestamp();
        tokio::spawn(async move {
            match spawn_in_new_terminal(&wrapped) {
                Ok(()) => {
                    sessions.lock().unwrap().push(AttachedSession {
                        directory: dir_clone,
                        session: sess_clone.clone(),
                        pid_file,
                        started_at,
                    });
                    *status.lock().unwrap() = format!("🚀 已在新终端启动会话 {}", sess_clone);
                }
                Err(e) => {
                    *status.lock().unwrap() = format!("❌ 新终端启动失败：{e}");
                }
            }
        });
        self.sub_page = None;
    }

    async fn pop_sub_page(&mut self) {
        match &self.sub_page {
            Some(SubPage::Projects { .. }) => self.sub_page = None,
            Some(SubPage::Sessions { .. })
            | Some(SubPage::ManualPath { .. })
            | Some(SubPage::NewPathChoice { .. }) => {
                self.enter_projects().await;
            }
            None => {}
        }
    }

    async fn handle_sub_page_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Quit | InputEvent::Left => self.pop_sub_page().await,
            InputEvent::Up | InputEvent::Char('k') => self.sub_page_move(-1),
            InputEvent::Down | InputEvent::Char('j') => self.sub_page_move(1),
            InputEvent::Select | InputEvent::Right => self.sub_page_select().await,
            InputEvent::Char(c) => self.sub_page_input(c),
            InputEvent::Backspace => self.sub_page_backspace(),
            _ => {}
        }
    }

    fn sub_page_move(&mut self, delta: i32) {
        match &mut self.sub_page {
            Some(SubPage::Projects { list_state, projects }) => {
                let len = projects.len() + 1;
                let i = list_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(len as i32) as usize;
                list_state.select(Some(next));
            }
            Some(SubPage::Sessions { list_state, sessions, .. }) => {
                let len = sessions.len() + 2;
                let i = list_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(len as i32) as usize;
                list_state.select(Some(next));
            }
            Some(SubPage::NewPathChoice { list_state }) => {
                let i = list_state.selected().unwrap_or(0) as i32;
                let next = (i + delta).rem_euclid(2) as usize;
                list_state.select(Some(next));
            }
            _ => {}
        }
    }

    fn sub_page_input(&mut self, c: char) {
        if let Some(SubPage::ManualPath { input, error }) = &mut self.sub_page {
            input.push(c);
            *error = None;
        }
    }

    fn sub_page_backspace(&mut self) {
        if let Some(SubPage::ManualPath { input, .. }) = &mut self.sub_page {
            input.pop();
        }
    }

    async fn sub_page_select(&mut self) {
        let action = match &self.sub_page {
            Some(SubPage::Projects { list_state, projects }) => {
                let i = list_state.selected().unwrap_or(0);
                if i == 0 {
                    SelectAction::EnterNewPathChoice
                } else {
                    SelectAction::EnterSessions(projects[i - 1].path.clone())
                }
            }
            Some(SubPage::Sessions { list_state, sessions, project }) => {
                let i = list_state.selected().unwrap_or(0);
                if i == 0 {
                    SelectAction::CreateSession(project.clone())
                } else if i <= sessions.len() {
                    SelectAction::Attach(project.clone(), sessions[i - 1].id.clone())
                } else {
                    SelectAction::DeleteProject(project.clone())
                }
            }
            Some(SubPage::NewPathChoice { list_state }) => {
                let i = list_state.selected().unwrap_or(0);
                if i == 0 {
                    SelectAction::ChooseFolder
                } else {
                    SelectAction::EnterManualPath
                }
            }
            Some(SubPage::ManualPath { input, .. }) => SelectAction::ConfirmPath(input.clone()),
            None => SelectAction::None,
        };

        match action {
            SelectAction::EnterNewPathChoice => {
                let mut list_state = ListState::default();
                list_state.select(Some(0));
                self.sub_page = Some(SubPage::NewPathChoice { list_state });
            }
            SelectAction::ChooseFolder => self.choose_folder_flow().await,
            SelectAction::EnterManualPath => {
                self.sub_page = Some(SubPage::ManualPath { input: String::new(), error: None });
            }
            SelectAction::EnterSessions(path) => self.enter_sessions(path).await,
            SelectAction::CreateSession(project) => self.create_and_attach(project).await,
            SelectAction::Attach(project, sid) => self.trigger_attach(project, sid),
            SelectAction::DeleteProject(project) => self.delete_project(project).await,
            SelectAction::ConfirmPath(input) => self.confirm_manual_path(input).await,
            SelectAction::None => {}
        }
    }

    async fn choose_folder_flow(&mut self) {
        match choose_folder().await {
            Ok(path) => {
                let _ = self.store.upsert_path(&path).await;
                self.enter_sessions(path).await;
            }
            Err(e) => {
                *self.status_message.lock().unwrap() = format!("⚠️ {e}");
            }
        }
    }

    // --- 卡片渲染辅助 ---

    fn item_card(item: MenuItem, status: &ServeStatus) -> Vec<Line<'static>> {
        let title_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
        let desc_style = Style::default().fg(Color::DarkGray);
        let status_style = Style::default().fg(Color::Yellow);
        match item {
            MenuItem::OcServe => {
                let (title, status_line) = match status.opencode_pid {
                    Some(_) => (
                        "⏹ 停止 OpenCode Serve".to_string(),
                        format!(
                            "当前：运行中 端口 {}",
                            status.port.map(|p| p.to_string()).unwrap_or_default()
                        ),
                    ),
                    None => (
                        "🚀 启动 OpenCode Serve".to_string(),
                        "当前：未运行".to_string(),
                    ),
                };
                vec![
                    Line::from(Span::styled(title, title_style)),
                    Line::from(Span::styled("启动 opencode serve 服务", desc_style)),
                    Line::from(Span::styled(status_line, status_style)),
                ]
            }
            MenuItem::Rathole => {
                let (title, status_line) = match status.rathole_pid {
                    Some(pid) => (
                        "⏹ 停止 Rathole 隧道".to_string(),
                        format!("当前：运行中 PID {pid}"),
                    ),
                    None => ("🚀 启动 Rathole 隧道".to_string(), "当前：未运行".to_string()),
                };
                vec![
                    Line::from(Span::styled(title, title_style)),
                    Line::from(Span::styled("启动 rathole 内网穿透", desc_style)),
                    Line::from(Span::styled(status_line, status_style)),
                ]
            }
            MenuItem::OcProjects => vec![
                Line::from(Span::styled("📂 OC 项目", title_style)),
                Line::from(Span::styled("选择项目并 attach 会话", desc_style)),
                Line::from(Span::styled("进入项目选择", status_style)),
            ],
            MenuItem::UpgradeOpenCodeAndOmo => vec![
                Line::from(Span::styled("⬆️ 升级 OpenCode + omo", title_style)),
                Line::from(Span::styled("升级 opencode 与 oh-my-openagent", desc_style)),
                Line::from(Span::styled("执行升级流程", status_style)),
            ],
            MenuItem::Exit => vec![
                Line::from(Span::styled("🚪 退出", title_style)),
                Line::from(Span::styled("退出程序", desc_style)),
                Line::from(Span::styled("安全退出并终止服务", status_style)),
            ],
        }
    }

    fn breadcrumb(&self) -> String {
        match &self.sub_page {
            None => "主菜单".to_string(),
            Some(SubPage::Projects { .. }) => "主菜单 -> OC项目".to_string(),
            Some(SubPage::Sessions { project, .. }) => {
                let name = std::path::Path::new(project)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| project.clone());
                format!("主菜单 -> OC项目 -> {name}")
            }
            Some(SubPage::NewPathChoice { .. }) | Some(SubPage::ManualPath { .. }) => {
                "主菜单 -> OC项目 -> 新建路径".to_string()
            }
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        if self.show_full_log {
            self.render_full_log(frame);
            return;
        }

        self.click_regions.clear();

        // 进入 OC 项目子页面时隐藏日志框，给列表腾出空间。
        let hide_logs = self.sub_page.is_some();
        let chunks = if hide_logs {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Header
                    Constraint::Min(8),    // 操作空间
                    Constraint::Length(6), // 状态
                ])
                .split(frame.area())
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),  // Header
                    Constraint::Min(8),     // 操作空间
                    Constraint::Length(10), // 日志记录
                    Constraint::Length(6),  // 状态
                ])
                .split(frame.area())
        };

        // Header
        let header_block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan));
        let header_inner = header_block.inner(chunks[0]);
        frame.render_widget(header_block, chunks[0]);
        let header_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(12)])
            .split(header_inner);
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                " opencode TUI 启动器 (Rust + Axum + ratatui) ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )])),
            header_cols[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "设置 [s]",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )])),
            header_cols[1],
        );
        self.click_regions.push(ClickRegion {
            rect: header_cols[1],
            target: ClickTarget::Settings,
        });

        // 操作空间
        if self.sub_page.is_some() {
            self.render_sub_page(frame, chunks[1]);
        } else {
            match self.input_mode {
                InputMode::Menu
                | InputMode::ConfigPort
                | InputMode::ServePort
                | InputMode::SettingsUrl
                | InputMode::SettingsUser
                | InputMode::SettingsPassword => {
                    self.render_menu_area(frame, chunks[1])
                }
                InputMode::ConfigUsername | InputMode::ConfigPassword => {
                    self.render_config_form(frame, chunks[1])
                }
            }
        }

        // 日志记录（子页面时隐藏）+ 状态面板
        if hide_logs {
            self.render_status_panel(frame, chunks[2]);
        } else {
            self.render_logs(frame, chunks[2]);
            self.render_status_panel(frame, chunks[3]);
        }

        if matches!(self.input_mode, InputMode::ConfigPort | InputMode::ServePort) {
            self.render_port_popup(frame);
        }

        if matches!(
            self.input_mode,
            InputMode::SettingsUrl | InputMode::SettingsUser | InputMode::SettingsPassword
        ) {
            self.render_settings_popup(frame);
        }

        if self.confirm.is_some() {
            self.render_confirm(frame);
        }
    }

    fn render_status_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let pid = std::process::id();
        let now = chrono::Local::now();
        let duration = (now - self.program_started_at).num_seconds().max(0);
        let remote = self.remote_status.lock().unwrap().clone();
        let help = self.help_text();
        let status_text = vec![
            Line::from(format!(
                "PID: {pid}    启动时间: {}    运行时长: {}",
                self.program_started_at.format("%H:%M:%S"),
                Self::format_duration(duration),
            )),
            Line::from(Span::styled(
                if remote.is_empty() {
                    "正在验证远端存储…".to_string()
                } else {
                    remote
                },
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(help, Style::default().fg(Color::DarkGray))),
        ];
        let status_para = Paragraph::new(status_text)
            .block(Block::default().title("状态").borders(Borders::ALL));
        frame.render_widget(status_para, area);
    }

    fn help_text(&self) -> String {
        if self.show_full_log {
            return "↑/↓ 滚动  Esc/q 退出".to_string();
        }
        if self.confirm.is_some() {
            return "←/→ 切换  回车确认  Esc 取消".to_string();
        }
        if self.sub_page.is_some() {
            return match &self.sub_page {
                Some(SubPage::Projects { .. }) => {
                    "↑/↓ 选择  →/回车 进入  ←/Esc 返回".to_string()
                }
                Some(SubPage::Sessions { .. }) => {
                    "↑/↓ 选择  →/回车 确认  ←/Esc 返回".to_string()
                }
                Some(SubPage::NewPathChoice { .. }) => {
                    "↑/↓ 选择  →/回车 确认  ←/Esc 返回".to_string()
                }
                Some(SubPage::ManualPath { .. }) => "输入路径  回车确认  Esc 返回".to_string(),
                None => String::new(),
            };
        }
        match self.input_mode {
            InputMode::ConfigUsername | InputMode::ConfigPassword => {
                "Tab 切换字段  回车确认  Esc 退出".to_string()
            }
            InputMode::ConfigPort | InputMode::ServePort => {
                "输入数字  回车确认  Esc 取消".to_string()
            }
            InputMode::SettingsUrl | InputMode::SettingsUser | InputMode::SettingsPassword => {
                "Tab 切换字段  回车保存  Esc 取消".to_string()
            }
            InputMode::Menu => {
                "↑/↓ 选择  Tab/←/→ 切换栏  回车确认  s 设置  l 日志  Esc/q 退出".to_string()
            }
        }
    }

    fn render_settings_popup(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let w = 60u16;
        let h = 9u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);

        let active = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let inactive = Style::default();
        let url_style = if self.input_mode == InputMode::SettingsUrl { active } else { inactive };
        let user_style = if self.input_mode == InputMode::SettingsUser { active } else { inactive };
        let pass_style = if self.input_mode == InputMode::SettingsPassword { active } else { inactive };

        let lines = vec![
            Line::from(Span::styled(
                "远程 SilverBullet 设置",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  远程路径: "),
                Span::styled(self.sb_url_input.clone(), url_style),
            ]),
            Line::from(vec![
                Span::raw("  用户名:   "),
                Span::styled(self.sb_user_input.clone(), user_style),
            ]),
            Line::from(vec![
                Span::raw("  密码:     "),
                Span::styled("*".repeat(self.sb_password_input.len()), pass_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Tab 切换字段  Enter 保存  Esc 取消",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let form = Paragraph::new(lines).block(
            Block::default()
                .title("设置")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(form, rect);
    }

    fn render_confirm(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let w = 44u16;
        let h = 5u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("确认")
            .border_style(Style::default().fg(Color::Yellow));

        let confirm_selected = self.confirm_choice == ConfirmChoice::Confirm;
        let cancel_selected = self.confirm_choice == ConfirmChoice::Cancel;
        let selected_style = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let confirm_btn = Span::styled(
            if confirm_selected { "▶ [ 确认 ]" } else { "  [ 确认 ]" },
            if confirm_selected { selected_style } else { Style::default() },
        );
        let cancel_btn = Span::styled(
            if cancel_selected { "▶ [ 取消 ]" } else { "  [ 取消 ]" },
            if cancel_selected { selected_style } else { Style::default() },
        );

        let lines = vec![
            Line::from("确认杀死/关闭该服务？"),
            Line::from(""),
            Line::from(vec![confirm_btn, Span::raw("   "), cancel_btn]),
        ];
        let para = Paragraph::new(lines).block(block);
        frame.render_widget(Clear, rect);
        frame.render_widget(para, rect);

        let btn_y = rect.y + 3;
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 1, btn_y, 11, 1),
            target: ClickTarget::ConfirmButton,
        });
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 13, btn_y, 11, 1),
            target: ClickTarget::CancelButton,
        });
    }

    fn render_menu_area(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(46),
                Constraint::Percentage(26),
                Constraint::Percentage(28),
            ])
            .split(area);

        let status = self.status_snapshot();
        let main_focused = self.focus == Focus::Main;
        let projects_focused = self.focus == Focus::Projects;
        let panel_focused = self.focus == Focus::ServicePanel;
        let sessions = self.attached_sessions.lock().unwrap().clone();

        Self::render_card_column(
            frame,
            cols[0],
            "服务与系统",
            &MAIN_ITEMS,
            &status,
            main_focused,
            &mut self.main_state,
            &mut self.click_regions,
            ColumnKind::Main,
        );
        Self::render_card_column(
            frame,
            cols[1],
            "OC 项目",
            &PROJECTS_ITEMS,
            &status,
            projects_focused,
            &mut self.projects_state,
            &mut self.click_regions,
            ColumnKind::Projects,
        );
        Self::render_service_panel(
            frame,
            cols[2],
            &status,
            &sessions,
            panel_focused,
            &mut self.service_state,
            &mut self.click_regions,
        );
    }

    fn render_card_column(
        frame: &mut Frame<'_>,
        area: Rect,
        title: &str,
        items: &[MenuItem],
        status: &ServeStatus,
        focused: bool,
        state: &mut ListState,
        regions: &mut Vec<ClickRegion>,
        kind: ColumnKind,
    ) {
        let outer = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let card_h = 5u16;
        let mut y = inner.y;
        for (i, item) in items.iter().enumerate() {
            if y + card_h > inner.y + inner.height {
                break;
            }
            let card_area = Rect::new(inner.x, y, inner.width, card_h);
            let target = match kind {
                ColumnKind::Main => ClickTarget::MainColumn(i),
                ColumnKind::Projects => ClickTarget::ProjectsColumn(i),
            };
            regions.push(ClickRegion { rect: card_area, target });
            let selected = focused && state.selected() == Some(i);
            let border_style = if selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            let para = Paragraph::new(Self::item_card(*item, status)).block(block);
            frame.render_widget(para, card_area);
            y += card_h;
        }
    }

    fn render_service_panel(
        frame: &mut Frame<'_>,
        area: Rect,
        status: &ServeStatus,
        sessions: &[AttachedSession],
        focused: bool,
        state: &mut ListState,
        regions: &mut Vec<ClickRegion>,
    ) {
        let now = chrono::Utc::now().timestamp();
        let mut cards: Vec<Vec<Line<'static>>> = Vec::new();

        if status.opencode_pid.is_some() {
            cards.push(Self::service_card(
                "opencode",
                status.port,
                status.opencode_pid,
                status.started_at.map(|t| t.timestamp()),
                now,
            ));
        }

        if status.rathole_pid.is_some() {
            cards.push(Self::service_card(
                "rathole",
                None,
                status.rathole_pid,
                None,
                now,
            ));
        }

        for s in sessions {
            let name = std::path::Path::new(&s.directory)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| s.directory.clone());
            let pid = std::fs::read_to_string(&s.pid_file)
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok());
            cards.push(Self::service_card(
                &format!("attach {name}"),
                None,
                pid,
                Some(s.started_at),
                now,
            ));
        }

        let outer = Block::default()
            .title("当前服务 [点击即可杀死/关闭选择的服务]")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let card_h = 7u16;
        let mut y = inner.y;
        for (i, card) in cards.iter().enumerate() {
            if y + card_h > inner.y + inner.height {
                break;
            }
            let card_area = Rect::new(inner.x, y, inner.width, card_h);
            regions.push(ClickRegion { rect: card_area, target: ClickTarget::ServicePanel(i) });
            let selected = focused && state.selected() == Some(i);
            let border_style = if selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            let para = Paragraph::new(card.clone()).block(block);
            frame.render_widget(para, card_area);
            y += card_h;
        }
    }

    fn service_card(
        name: &str,
        port: Option<u16>,
        pid: Option<u32>,
        started_at: Option<i64>,
        now: i64,
    ) -> Vec<Line<'static>> {
        let port_str = port.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
        let pid_str = pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
        let start_str = started_at
            .map(Self::format_time)
            .unwrap_or_else(|| "-".to_string());
        let dur_str = started_at
            .map(|t| Self::format_duration((now - t).max(0)))
            .unwrap_or_else(|| "-".to_string());
        vec![
            Line::from(Span::styled(
                format!("名称: {name}"),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("端口: {port_str}")),
            Line::from(format!("PID: {pid_str}")),
            Line::from(format!("启动时间: {start_str}")),
            Line::from(format!("运行时长: {dur_str}")),
        ]
    }

    fn format_time(ts: i64) -> String {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    fn format_duration(secs: i64) -> String {
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }

    fn render_sub_page(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let crumb = self.breadcrumb();
        match &mut self.sub_page {
            Some(SubPage::Projects { list_state, projects }) => {
                let mut cards: Vec<Vec<Line<'static>>> = vec![Self::new_path_card()];
                for p in projects.iter() {
                    cards.push(Self::project_card(p));
                }
                Self::render_card_stack(frame, area, &crumb, &cards, list_state, &mut self.click_regions);
            }
            Some(SubPage::Sessions { list_state, sessions, project }) => {
                let mut cards: Vec<Vec<Line<'static>>> = vec![Self::new_session_card()];
                for s in sessions.iter() {
                    cards.push(Self::session_card(s));
                }
                cards.push(Self::delete_card());
                let header = format!("{} -> {}", crumb, project);
                Self::render_card_stack(frame, area, &header, &cards, list_state, &mut self.click_regions);
            }
            Some(SubPage::NewPathChoice { list_state }) => {
                let cards: Vec<Vec<Line<'static>>> = vec![
                    vec![
                        Line::from(Span::styled(
                            "🖥 系统路径选择",
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            "打开 Finder 选择项目目录",
                            Style::default().fg(Color::DarkGray),
                        )),
                        Line::from(""),
                    ],
                    vec![
                        Line::from(Span::styled(
                            "⌨️ 手动输入路径",
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(
                            "粘贴路径回车确认",
                            Style::default().fg(Color::DarkGray),
                        )),
                        Line::from(""),
                    ],
                ];
                Self::render_card_stack(frame, area, &crumb, &cards, list_state, &mut self.click_regions);
            }
            Some(SubPage::ManualPath { input, error }) => {
                let mut lines: Vec<Line<'_>> = vec![
                    Line::from(vec![Span::styled(
                        "请输入项目路径（留空使用默认目录）",
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )]),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("  路径: "),
                        Span::styled(
                            input.clone(),
                            Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(""),
                ];
                if let Some(err) = error {
                    lines.push(Line::from(Span::styled(
                        format!("  ⚠ {err}"),
                        Style::default().fg(Color::Red),
                    )));
                } else {
                    lines.push(Line::from(Span::styled(
                        "  Enter 确认    Esc 返回",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                let form = Paragraph::new(lines).block(
                    Block::default()
                        .title(crumb)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Cyan)),
                );
                frame.render_widget(form, area);
            }
            None => {}
        }
    }

    fn render_card_stack(
        frame: &mut Frame<'_>,
        area: Rect,
        title: &str,
        cards: &[Vec<Line<'static>>],
        state: &mut ListState,
        regions: &mut Vec<ClickRegion>,
    ) {
        let title_area = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            title_area,
        );

        let card_h = 5u16;
        let visible_h = area.height.saturating_sub(1);
        let visible_count = (visible_h / card_h).max(1) as usize;
        let selected = state.selected().unwrap_or(0);

        // 滚动窗口：让选中项始终可见。
        let scroll = if selected >= visible_count {
            selected - visible_count + 1
        } else {
            0
        };

        let mut y = area.y + 1;
        let end = (scroll + visible_count).min(cards.len());
        for idx in scroll..end {
            let card_area = Rect::new(area.x, y, area.width, card_h);
            regions.push(ClickRegion { rect: card_area, target: ClickTarget::SubPage(idx) });
            let is_selected = state.selected() == Some(idx);
            let border_style = if is_selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            let para = Paragraph::new(cards[idx].clone()).block(block);
            frame.render_widget(para, card_area);
            y += card_h;
        }
    }

    fn new_path_card() -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "➕ 新建 path",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled("手动输入本地路径", Style::default().fg(Color::DarkGray))),
            Line::from(""),
        ]
    }

    fn project_card(p: &PathEntry) -> Vec<Line<'static>> {
        let name = std::path::Path::new(&p.path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| p.path.clone());
        let desc = format!("{} sessions  ·  {}", p.sections.len(), p.path);
        let status = p
            .last_opened_at
            .map(|t| format!("最后打开：{}", t.format("%Y-%m-%d %H:%M")))
            .unwrap_or_else(|| "从未打开".to_string());
        vec![
            Line::from(Span::styled(
                format!("📁 {name}"),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(desc, Style::default().fg(Color::DarkGray))),
            Line::from(Span::styled(status, Style::default().fg(Color::Yellow))),
        ]
    }

    fn new_session_card() -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "➕ 新建会话（attach）",
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled("创建新会话并 attach", Style::default().fg(Color::DarkGray))),
            Line::from(""),
        ]
    }

    fn session_card(s: &OcSession) -> Vec<Line<'static>> {
        let title = s.title.clone().unwrap_or_else(|| "(untitled)".to_string());
        vec![
            Line::from(Span::styled(
                s.id.clone(),
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(title, Style::default().fg(Color::DarkGray))),
            Line::from(Span::styled(
                "Enter attach 到此会话",
                Style::default().fg(Color::Yellow),
            )),
        ]
    }

    fn delete_card() -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "🗑️ 删除此项目记录",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "从 path-list 移除该项目",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
        ]
    }

    fn render_config_form(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let is_username = self.input_mode == InputMode::ConfigUsername;
        let active = Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD);
        let inactive = Style::default();
        let username_style = if is_username { active } else { inactive };
        let password_style = if is_username { inactive } else { active };

        let masked_password = "*".repeat(self.password_input.len());
        let mut lines: Vec<Line<'_>> = vec![
            Line::from(vec![Span::styled(
                "首次启动：请配置 HTTP 认证凭据",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  用户名: "),
                Span::styled(self.username_input.clone(), username_style),
            ]),
            Line::from(vec![
                Span::raw("  密码:   "),
                Span::styled(masked_password, password_style),
            ]),
            Line::from(""),
        ];
        if let Some(err) = &self.config_error {
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {err}"),
                Style::default().fg(Color::Red),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  Tab 切换字段    Enter 确认    Esc 退出",
                Style::default().fg(Color::DarkGray),
            )));
        }

        let form = Paragraph::new(lines).block(
            Block::default()
                .title("首次配置")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(form, area);
    }

    fn render_port_popup(&self, frame: &mut Frame<'_>) {
        let is_verify = self.input_mode == InputMode::ServePort;
        let (title, hint) = if is_verify {
            ("验证 serve 端口", "输入已运行 opencode serve 的端口号")
        } else {
            ("启动端口", "输入数字 1-65535")
        };

        let area = frame.area();
        let w = 50u16;
        let h = 5u16;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);

        let lines: Vec<Line<'_>> = vec![
            Line::from(vec![Span::styled(
                hint,
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::raw("  端口: "),
                Span::styled(
                    self.port_input.clone(),
                    Style::default().bg(Color::Cyan).fg(Color::Black).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "  Enter 确认    Esc 取消",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        let form = Paragraph::new(lines).block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(form, rect);
    }

    fn render_logs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner_height = area.height.saturating_sub(2).max(1) as usize;
        let lines: Vec<Line<'_>> = self
            .log_buffer
            .tail(inner_height)
            .into_iter()
            .map(Line::from)
            .collect();
        let log = Paragraph::new(lines)
            .block(Block::default().title("日志 [显示全部: l]").borders(Borders::ALL));
        frame.render_widget(log, area);
    }

    fn render_full_log(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        let hint = Paragraph::new(Span::styled(
            "日志（全屏）  Esc / q / l 退出    ↑/↓ 滚动",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(hint, chunks[0]);

        let height = chunks[1].height as usize;
        let total = self.log_buffer.tail(500);
        let start = total.len().saturating_sub(height + self.log_scroll);
        let end = total.len().saturating_sub(self.log_scroll);
        let lines: Vec<Line<'_>> = total[start..end]
            .iter()
            .map(|s| Line::from(s.clone()))
            .collect();
        let log = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
        frame.render_widget(log, chunks[1]);
    }
}


