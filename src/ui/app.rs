//! TUI app state and render loop.

use std::path::PathBuf;
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
    widgets::{Block, Borders, Clear, ListState, Padding, Paragraph, Wrap},
};

use crate::attach::{AttachedSession, OcSession, OpencodeClient, choose_folder, kill_process};
use crate::auth::AuthConfig;
use crate::config::{
    PortsConfig, RatholeConfig, SbConfig, read_persisted_env, write_persisted_env,
};
use crate::domain::{PathEntry, PathValidator};
use crate::serve::{
    ServeStatus, ServeSupervisor, rathole_default_bin, rathole_default_config,
};
use crate::storage::{PathListStore, RemotePaths};
use crate::storage::remote::RemoteClient;
use crate::ui::events::InputEvent;
use crate::ui::log::LogBuffer;
use crate::ui::menu::{MenuAction, MenuItem};
use crate::upgrade::{UpgradeResult, upgrade_opencode, upgrade_omo};

const MAIN_ITEMS: [MenuItem; 3] = [
    MenuItem::OcServe,
    MenuItem::Rathole,
    MenuItem::UpgradeOpenCodeAndOmo,
];
const PROJECTS_ITEMS: [MenuItem; 1] = [MenuItem::OcProjects];

/// 卡片宽度下限。
///
/// 低于此宽度(< 边框 2 + padding 2 + 标题最少 10 列)就切到紧凑模式,
/// 因为完整 card 内的 3 行内容(包括 "当前:运行中 端口 9464")会被截断。
///
/// 取 22 是因为最长标题 "⏹ 停止 OpenCode Serve" 在 UTF-8 等宽字体下约 18 显示列,
/// 加上 2 列边框 + 2 列 padding = 22 列刚好容下。
const MIN_CARD_WIDTH: u16 = 22;

/// 「新建路径」子页中「系统路径选择」卡片的副标题文案，按平台切换：
/// * macOS — Finder
/// * Windows — 资源管理器
/// * Linux / 其它 — 中性 "系统文件管理器" 描述
#[cfg(target_os = "macos")]
const SYS_PICKER_DESC: &str = "打开 Finder 选择项目目录";
#[cfg(target_os = "windows")]
const SYS_PICKER_DESC: &str = "打开资源管理器选择项目目录";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const SYS_PICKER_DESC: &str = "打开系统文件管理器选择项目目录";

/// TUI 交互模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    /// 主菜单导航。
    Menu,
    /// 设置：HTTP Basic 用户名（`OPENCODE_SERVER_USERNAME`）。
    SettingsAuthUsername,
    /// 设置：HTTP Basic 密码（`OPENCODE_SERVER_PASSWORD`）。
    SettingsAuthPassword,
    /// 设置：系统端口。
    SettingsHttpPort,
    /// 设置：OpenCode 服务端口。
    SettingsServePort,
    /// 设置：远程 SilverBullet 路径。
    SettingsUrl,
    /// 设置：SilverBullet 用户名。
    SettingsUser,
    /// 设置：SilverBullet 密码。
    SettingsPassword,
    /// 设置：rathole 远端 Host。
    SettingsRatholeHost,
    /// 设置：rathole 远端 Port。
    SettingsRatholePort,
    /// 设置：rathole 服务名 Name。
    SettingsRatholeName,
    /// 设置：rathole 鉴权 Token。
    SettingsRatholeToken,
}

impl InputMode {
    /// 是否为设置面板的某个字段（port / SB / Rathole）。
    fn is_settings_field(self) -> bool {
        matches!(
            self,
            InputMode::SettingsAuthUsername
                | InputMode::SettingsAuthPassword
                | InputMode::SettingsHttpPort
                | InputMode::SettingsServePort
                | InputMode::SettingsUrl
                | InputMode::SettingsUser
                | InputMode::SettingsPassword
                | InputMode::SettingsRatholeHost
                | InputMode::SettingsRatholePort
                | InputMode::SettingsRatholeName
                | InputMode::SettingsRatholeToken
        )
    }
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
    /// 未启动 serve 时仍进入 OC 项目流程（无远程服务支持）。
    EnterProjectsWithoutServe,
    /// 升级 OpenCode + omo（不可逆网络操作）。
    Upgrade,
    /// 退出整个程序。
    Exit,
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
    /// 设置弹框内某个字段（用于鼠标点击/hover 切换焦点）。
    SettingsField(InputMode),
    /// 日志面板（点击触发 `l` 快捷键）。
    Logs,
    /// 确认弹框的确认按钮（点击 = Select）。
    ConfirmOk,
    /// 确认弹框的取消按钮（点击 = Esc 关闭弹框）。
    CancelBtn,
    /// 设置弹框底部的「确认」按钮（点击 = 保存提交）。
    SettingsOk,
    /// 设置弹框底部的「取消」按钮（点击 = 关闭弹框不保存）。
    SettingsCancel,
}

/// 设置弹框内可编辑字段的有序列表（决定 ↑/↓ / Tab / 点击的循环顺序）。
const SETTINGS_FIELDS: [InputMode; 11] = [
    InputMode::SettingsAuthUsername,
    InputMode::SettingsAuthPassword,
    InputMode::SettingsHttpPort,
    InputMode::SettingsServePort,
    InputMode::SettingsUrl,
    InputMode::SettingsUser,
    InputMode::SettingsPassword,
    InputMode::SettingsRatholeHost,
    InputMode::SettingsRatholePort,
    InputMode::SettingsRatholeName,
    InputMode::SettingsRatholeToken,
];

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

/// 待执行的 attach 会话 —— 由 trigger_attach 填充，run() 主循环检测后
/// 调用 `run_attach` 接管控制台跑 `opencode attach`。
///
/// 为什么需要这个 flag：trigger_attach 是 `&mut self` 方法，被 click/key
/// handler 在事件循环里调用，没法直接拿 `DefaultTerminal`（terminal
/// 是 run() 的局部变量，borrow checker 不允许跨 await 持有）。所以改成：
/// trigger_attach 把 attach 信息打包塞进 `pending_attach`，run() 主循环
/// 下一轮迭代开头检测这个 flag，调 `run_attach(&mut terminal)` 执行。
struct PendingAttach {
    url: String,
    directory: String,
    session: String,
    user: String,
    password: String,
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
    /// 上一帧主菜单列的渲染区域(`render_top_row` 中 `top_cols[0]`),
    /// 供 `focus_move` 在不渲染的情况下推算当前 capacity。
    /// 初始为 0x0 表示"还没渲染过"——此时 visible_main_items 返回空,
    /// focus_move 安全 no-op。
    last_main_column_area: ratatui::layout::Rect,
    /// 上一帧子页面(OC 项目等)列表的渲染区域,供滚轮事件做命中判断。
    /// 初始为 0x0 表示"还没渲染过"——滚轮命中检查自然失败,安全 no-op。
    last_sub_page_area: ratatui::layout::Rect,
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
    /// 设置：系统端口输入缓冲。
    system_port_input: String,
    /// 设置：OpenCode 服务端口输入缓冲。
    opencode_port_input: String,
    /// 设置：SB URL 输入缓冲。
    sb_url_input: String,
    /// 设置：SB 用户名输入缓冲。
    sb_user_input: String,
    /// 设置：SB 密码输入缓冲。
    sb_password_input: String,
    /// rathole 全局配置（设置弹框热更新 + 生成 global.toml）.
    rathole_config: Arc<RwLock<RatholeConfig>>,
    /// 设置：rathole Host 输入缓冲。
    rathole_host_input: String,
    /// 设置：rathole Port 输入缓冲。
    rathole_port_input: String,
    /// 设置：rathole Name 输入缓冲。
    rathole_name_input: String,
    /// 设置：rathole Token 输入缓冲。
    rathole_token_input: String,
    /// 当前帧的可点击区域（渲染时填充，鼠标事件查询）。
    click_regions: Vec<ClickRegion>,
    /// 最近一次鼠标移动的位置（用于日志面板边框 hover 高亮）。
    mouse_pos: Option<(u16, u16)>,
    /// 待执行的 attach 会话；run() 主循环检测到非 None 后接管控制台跑 attach。
    pending_attach: Option<PendingAttach>,
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
        rathole_config: Arc<RwLock<RatholeConfig>>,
    ) -> Self {
        let mut main_state = ListState::default();
        main_state.select(Some(0));
        let mut projects_state = ListState::default();
        projects_state.select(Some(0));
        let mut service_state = ListState::default();
        service_state.select(Some(0));

        let configured = auth.read().map(|a| a.is_configured()).unwrap_or(false);
        let existing_user = auth
            .read()
            .map(|a| a.basic_user.clone())
            .unwrap_or_default();
        let (input_mode, status) = if configured {
            (InputMode::Menu, "就绪".to_string())
        } else {
            (
                InputMode::SettingsAuthUsername,
                "首次启动：请填写 OPENCODE_SERVER_USERNAME / PASSWORD".to_string(),
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
            username_input: existing_user,
            password_input: String::new(),
            show_full_log: false,
            log_scroll: 0,
            confirm: None,
            confirm_choice: ConfirmChoice::Confirm,
            sb_config,
            remote_status: Arc::new(Mutex::new(String::new())),
            program_started_at: chrono::Local::now(),
            system_port_input: PortsConfig::load().system_port.to_string(),
            opencode_port_input: PortsConfig::load().opencode_port.to_string(),
            sb_url_input: String::new(),
            sb_user_input: String::new(),
            sb_password_input: String::new(),
            rathole_config,
            rathole_host_input: String::new(),
            rathole_port_input: String::new(),
            rathole_name_input: String::new(),
            rathole_token_input: String::new(),
            click_regions: Vec::new(),
            mouse_pos: None,
            last_main_column_area: ratatui::layout::Rect::default(),
            last_sub_page_area: ratatui::layout::Rect::default(),
            pending_attach: None,
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

        // 启动时假定鼠标在屏幕中心,让第一帧就有 hover 高亮
        // (不必等用户的第一次真实移动,程序也不会修改 list state / focus)。
        if let Ok((cols, rows)) = crossterm::terminal::size() {
            self.mouse_pos_refresh(cols / 2, rows / 2);
        }

        while !self.should_quit {
            // 优先检查 pending_attach：trigger_attach 把 attach 信息塞进来后，
            // run() 主循环下一轮迭代开头检测到，调 `run_attach` 接管控制台。
            // 这里不能直接 `if let Some(...) = self.pending_attach.take()`，
            // 因为 `run_attach` 需要 `&mut terminal`，terminal 是 run() 的
            // 局部变量；需要把 terminal 借用传进去。
            if self.pending_attach.is_some() {
                self.run_attach(&mut terminal).await;
                // 强制重画下一帧（TUI resume 后内容可能变化大）
                if let Err(e) = terminal.draw(|f| self.render(f)) {
                    tracing::error!("post-attach draw error: {e}");
                }
                continue;
            }

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
                        // 终端重新获得焦点(切回窗口),复用上次坐标
                        // 刷新一次 hover 高亮 — crossterm 切窗口后
                        // 不一定会立刻发 Moved 事件。
                        crossterm::event::Event::FocusGained => {
                            if let Some((c, r)) = self.mouse_pos {
                                self.mouse_pos_refresh(c, r);
                            }
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
            let path = RemotePaths::new(cfg.user.as_str()).path_list_with_slash();
            match remote.get(&path).await {
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
            crossterm::event::MouseEventKind::ScrollUp => {
                self.wheel_scroll(mouse.column, mouse.row, -1);
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                self.wheel_scroll(mouse.column, mouse.row, 1);
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
        self.mouse_pos = Some((col, row));
        let Some(target) = self.find_target(col, row) else { return };
        // 弹框(确认 / 设置)存在时,鼠标 hover 不穿透修改主菜单 / 当前服务
        // 的 list state / focus(只在弹框内有效)。弹框消失后下次 hover 恢复。
        let popup_open =
            self.input_mode.is_settings_field() || self.confirm.is_some();
        match target {
            ClickTarget::MainColumn(i) if !popup_open => {
                self.main_state.select(Some(i));
                self.focus = Focus::Main;
            }
            ClickTarget::ProjectsColumn(i) if !popup_open => {
                self.projects_state.select(Some(i));
                self.focus = Focus::Projects;
            }
            ClickTarget::ServicePanel(i) if !popup_open => {
                self.service_state.select(Some(i));
                self.focus = Focus::ServicePanel;
            }
            ClickTarget::SubPage(i) if !popup_open => self.set_sub_page_selected(i),
            _ => {}
        }
    }

    /// 仅刷新 `mouse_pos` 缓存,不改 list state / focus。
    ///
    /// 用于:(1) 启动时假定鼠标在屏幕中心 — 让第一帧渲染就能有高亮,
    /// 不必等用户的第一次真实移动;(2) 切回窗口(FocusGained)时复用上次的
    /// 坐标重画一次,弥补 crossterm 不发 Moved 事件的场景。
    fn mouse_pos_refresh(&mut self, col: u16, row: u16) {
        self.mouse_pos = Some((col, row));
    }

    /// 鼠标滚轮滚动:光标位于子页面列表区域内时,上/下滚动移动选中项
    /// (等价 ↑/↓ 键)。弹框打开或全屏日志模式下忽略,与 click/hover 的
    /// 穿透阻止策略一致。
    fn wheel_scroll(&mut self, col: u16, row: u16, delta: i32) {
        let popup_open =
            self.input_mode.is_settings_field() || self.confirm.is_some();
        if popup_open || self.show_full_log || self.sub_page.is_none() {
            return;
        }
        let area = self.last_sub_page_area;
        if col >= area.x
            && col < area.x + area.width
            && row >= area.y
            && row < area.y + area.height
        {
            self.sub_page_move(delta);
        }
    }

    async fn click_at(&mut self, col: u16, row: u16) {
        let popup_open =
            self.input_mode.is_settings_field() || self.confirm.is_some();
        let Some(target) = self.find_target(col, row) else {
            // 弹框打开时,点击空白区域 = 等价 Esc,关闭弹框
            if popup_open {
                self.dismiss_popup();
            }
            return;
        };

        // 弹框打开时:点击穿透阻止。
        // - 弹框内的 click region(字段 / 按钮)正常处理
        // - 主菜单 / 当前服务 / 子页面 / 设置入口 / 日志面板的 click region
        //   都视为"点击弹框外部",关闭弹框而非穿透执行。
        if popup_open && !matches!(
            target,
            ClickTarget::SettingsField(_)
                | ClickTarget::SettingsOk
                | ClickTarget::SettingsCancel
                | ClickTarget::ConfirmOk
                | ClickTarget::CancelBtn
        ) {
            self.dismiss_popup();
            return;
        }

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
            ClickTarget::SettingsField(field) => self.input_mode = field,
            ClickTarget::Logs => {
                self.show_full_log = true;
                self.log_scroll = 0;
            }
            ClickTarget::ConfirmOk => {
                // 等价于按 Select:从 confirm 取 action,根据类型分发
                let action = self.confirm.take();
                match action {
                    Some(ConfirmAction::ExitService(i)) => self.exit_service(i),
                    Some(ConfirmAction::EnterProjectsWithoutServe) => {
                        self.enter_projects().await;
                    }
                    Some(ConfirmAction::Exit) => {
                        self.should_quit = true;
                    }
                    Some(ConfirmAction::Upgrade) => {
                        self.start_upgrade();
                    }
                    None => {}
                }
            }
            ClickTarget::CancelBtn => {
                self.confirm = None;
            }
            ClickTarget::SettingsOk => {
                // 等价于按 Enter 保存
                self.submit_settings().await;
            }
            ClickTarget::SettingsCancel => {
                // 等价于按 Esc 关闭弹框
                self.input_mode = InputMode::Menu;
            }
        }
    }

    /// 关闭当前打开的弹框(设置 / 确认)。
    ///
    /// 两个弹框互斥(同时只可能有一个),所以按顺序检查,先 confirm 再 settings。
    fn dismiss_popup(&mut self) {
        if self.confirm.is_some() {
            self.confirm = None;
        }
        if self.input_mode.is_settings_field() {
            self.input_mode = InputMode::Menu;
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
        let rc = self.rathole_config.read().unwrap_or_else(|e| e.into_inner()).clone();
        self.rathole_host_input = rc.host;
        self.rathole_port_input = rc.port;
        self.rathole_name_input = rc.name;
        self.rathole_token_input = rc.token;
        let ports = PortsConfig::load();
        self.system_port_input = ports.system_port.to_string();
        self.opencode_port_input = ports.opencode_port.to_string();
        // 把 auth 也回填到 buffer(密码 buffer 始终为空,要求用户重新输入才能改)。
        // 用户名 buffer 直接显示当前值,首次启动时是空字符串。
        let auth = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone();
        let needs_first_setup = auth.basic_user.is_empty() || auth.basic_password.is_empty();
        self.username_input = auth.basic_user;
        self.password_input.clear();
        // 首启自动聚焦用户名,后续打开聚焦系统端口。
        self.input_mode = if needs_first_setup {
            InputMode::SettingsAuthUsername
        } else {
            InputMode::SettingsHttpPort
        };
    }

    async fn handle_settings_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Tab | InputEvent::Down => self.move_settings_field(1),
            InputEvent::Up => self.move_settings_field(-1),
            InputEvent::Backspace => match self.input_mode {
                InputMode::SettingsAuthUsername => {
                    self.username_input.pop();
                }
                InputMode::SettingsAuthPassword => {
                    self.password_input.pop();
                }
                InputMode::SettingsHttpPort => {
                    self.system_port_input.pop();
                }
                InputMode::SettingsServePort => {
                    self.opencode_port_input.pop();
                }
                InputMode::SettingsUrl => {
                    self.sb_url_input.pop();
                }
                InputMode::SettingsUser => {
                    self.sb_user_input.pop();
                }
                InputMode::SettingsPassword => {
                    self.sb_password_input.pop();
                }
                InputMode::SettingsRatholeHost => {
                    self.rathole_host_input.pop();
                }
                InputMode::SettingsRatholePort => {
                    self.rathole_port_input.pop();
                }
                InputMode::SettingsRatholeName => {
                    self.rathole_name_input.pop();
                }
                InputMode::SettingsRatholeToken => {
                    self.rathole_token_input.pop();
                }
                _ => {}
            },
            InputEvent::Char(c) => match self.input_mode {
                InputMode::SettingsHttpPort if c.is_ascii_digit() => {
                    if self.system_port_input.len() < 5 {
                        self.system_port_input.push(c);
                    }
                }
                InputMode::SettingsServePort if c.is_ascii_digit() => {
                    if self.opencode_port_input.len() < 5 {
                        self.opencode_port_input.push(c);
                    }
                }
                InputMode::SettingsAuthUsername => self.username_input.push(c),
                InputMode::SettingsAuthPassword => self.password_input.push(c),
                InputMode::SettingsUrl => self.sb_url_input.push(c),
                InputMode::SettingsUser => self.sb_user_input.push(c),
                InputMode::SettingsPassword => self.sb_password_input.push(c),
                InputMode::SettingsRatholeHost => self.rathole_host_input.push(c),
                InputMode::SettingsRatholePort => self.rathole_port_input.push(c),
                InputMode::SettingsRatholeName => self.rathole_name_input.push(c),
                InputMode::SettingsRatholeToken => self.rathole_token_input.push(c),
                _ => {}
            },
            InputEvent::Select => self.submit_settings().await,
            InputEvent::Quit => {
                // 退出设置面板:若首启未填写完成,submit_settings 会另
                // 外拦截,保证无法进入主菜单。
                self.input_mode = InputMode::Menu;
            }
            _ => {}
        }
    }

    /// 在 9 个设置字段之间循环切换（delta = +1 下移 / -1 上移）。
    ///
    /// 找不到当前位置时（理论上不会发生，因为 `open_settings` 总是从
    /// `SETTINGS_FIELDS[0]` 开始），兜底回到第一个字段。
    fn move_settings_field(&mut self, delta: i32) {
        let len = SETTINGS_FIELDS.len() as i32;
        let cur = SETTINGS_FIELDS
            .iter()
            .position(|m| *m == self.input_mode)
            .unwrap_or(0) as i32;
        let next = ((cur + delta).rem_euclid(len) + len) % len;
        self.input_mode = SETTINGS_FIELDS[next as usize];
    }

    async fn submit_settings(&mut self) {
        // ---- 0. 认证字段校验（首启必须填写）----
        // 用户名从 auth 内存读到的 buffer;密码 buffer 永远从空开始,需
        // 要用户重新输入才能修改。`is_first_setup` 强制要求密码 buffer
        // 非空（即用户在本次会话内实际输入过密码）。
        let auth_user_now = self
            .auth
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .basic_user
            .clone();
        let auth_pass_now = self
            .auth
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .basic_password
            .clone();
        let username_from_buffer = self.username_input.trim().to_string();
        let password_from_buffer = self.password_input.clone();
        // 最终生效的 username:buffer 优先(buffer 可能用户改了);password
        // 必须 buffer 非空才采用,否则保持 auth 内存里原值。
        let final_username = if username_from_buffer.is_empty() {
            auth_user_now.clone()
        } else {
            username_from_buffer.clone()
        };
        if final_username.is_empty() {
            *self.status_message.lock().unwrap() =
                "❌ 必须填写 OPENCODE_SERVER_USERNAME".to_string();
            self.input_mode = InputMode::SettingsAuthUsername;
            return;
        }
        let final_password = if password_from_buffer.is_empty() {
            auth_pass_now.clone()
        } else {
            password_from_buffer.clone()
        };
        if final_password.is_empty() {
            *self.status_message.lock().unwrap() =
                "❌ 必须填写 OPENCODE_SERVER_PASSWORD（密码 buffer 必须输入才能改）".to_string();
            self.input_mode = InputMode::SettingsAuthPassword;
            return;
        }
        // 立即把认证写回内存(供当前进程的 axum / OpenCodeClient 立即使用)
        {
            let mut guard = self.auth.write().unwrap_or_else(|e| e.into_inner());
            guard.basic_user = final_username.clone();
            guard.basic_password = final_password.clone();
        }

        // ---- 1. 端口校验 ----
        let system_port_str = self.system_port_input.trim().to_string();
        let opencode_port_str = self.opencode_port_input.trim().to_string();
        let system_port = match system_port_str.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                *self.status_message.lock().unwrap() =
                    "❌ 系统端口无效（1-65535）".to_string();
                return;
            }
        };
        let opencode_port = match opencode_port_str.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                *self.status_message.lock().unwrap() =
                    "❌ OpenCode 服务端口无效（1-65535）".to_string();
                return;
            }
        };
        if system_port == opencode_port {
            *self.status_message.lock().unwrap() = format!(
                "❌ 系统端口 与 OpenCode 服务端口 不能相同（都是 {system_port}）"
            );
            return;
        }

        // ---- 2. Rathole 配置：校验 + 生成 global.toml ----
        let rc = RatholeConfig {
            host: self.rathole_host_input.trim().to_string(),
            port: self.rathole_port_input.trim().to_string(),
            name: self.rathole_name_input.trim().to_string(),
            token: self.rathole_token_input.clone(),
        };
        let rc_filled = [&rc.host, &rc.port, &rc.name, &rc.token]
            .iter()
            .all(|s| !s.is_empty());
        let rc_empty = [&rc.host, &rc.port, &rc.name, &rc.token]
            .iter()
            .all(|s| s.is_empty());
        if !rc_filled && !rc_empty {
            *self.status_message.lock().unwrap() =
                "❌ Rathole 配置需全部填写或全部留空".to_string();
            return;
        }
        let mut rathole_msg = String::new();
        if rc_filled {
            let ok_port = rc.port.parse::<u16>().map(|p| p > 0).unwrap_or(false);
            if !ok_port {
                *self.status_message.lock().unwrap() =
                    "❌ Rathole Port 无效（1-65535）".to_string();
                return;
            }
            // local_addr 端口优先用本次保存的 opencode_port,其次用当前 supervisor
            // 状态,再次 fallback 硬编码 9464。
            let local_port = opencode_port.to_string();
            let rathole_cfg_path = crate::config::rathole_config_path();
            if let Err(e) = rc.write_config_file(&rathole_cfg_path, &local_port) {
                *self.status_message.lock().unwrap() =
                    format!("❌ 生成 global.toml 失败：{e}");
                return;
            }
            *self.rathole_config.write().unwrap_or_else(|e| e.into_inner()) = rc.clone();
            rathole_msg = format!("Rathole → {}:{}", self.rathole_host_input, self.rathole_port_input);
        }

        // ---- 3. SB 配置：连接测试 ----
        let cfg = SbConfig {
            url: self.sb_url_input.trim().to_string(),
            user: self.sb_user_input.trim().to_string(),
            password: self.sb_password_input.clone(),
        };
        let sb_filled = !cfg.url.is_empty() && !cfg.user.is_empty() && !cfg.password.is_empty();
        let sb_empty = cfg.url.is_empty() && cfg.user.is_empty() && cfg.password.is_empty();
        if !sb_filled && !sb_empty {
            *self.status_message.lock().unwrap() =
                "❌ SilverBullet 配置需全部填写或全部留空".to_string();
            return;
        }
        let mut sb_msg = String::new();
        if sb_filled {
            let mut remote = RemoteClient::with_credentials(
                cfg.url.clone(),
                cfg.user.clone(),
                cfg.password.clone(),
            );
            let path = RemotePaths::new(cfg.user.as_str()).path_list_with_slash();
            let test_result = remote.get(&path).await;
            let connected = matches!(&test_result, Ok((status, _)) if (200..400).contains(status));
            if !connected {
                let base = match &test_result {
                    Ok((0, _)) => "❌ 连接测试失败：无法访问远端（网络/超时），未保存".to_string(),
                    Ok((status, _)) => format!("❌ 连接测试失败：HTTP {status}，未保存"),
                    Err(e) => format!("❌ 连接测试失败：{e}，未保存"),
                };
                let msg = if rathole_msg.is_empty() {
                    base
                } else {
                    format!("{base}（但 {rathole_msg} 已保存）")
                };
                *self.status_message.lock().unwrap() = msg;
                return;
            }
            sb_msg = format!("SB → {}", cfg.url);
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
        }

        // ---- 4. 读现有 env -> 改 port -> 整文件回写 ----
        let env_path = crate::config::unified_env_path();
        let mut persisted = read_persisted_env(&env_path);
        // 保留当前进程里已经配置好的 username/password（首次配置表单写过）
        // 与 SB / Rathole / cookie_name（已校验通过）
        if persisted.username.is_empty() {
            let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
            persisted.username = guard.basic_user.clone();
        }
        if persisted.password.is_empty() {
            let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
            persisted.password = guard.basic_password.clone();
        }
        if persisted.sb_cookie_name.is_none() {
            persisted.sb_cookie_name = self
                .auth
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .sb_cookie_name
                .clone();
        }
        if sb_filled {
            persisted.sb = cfg.clone();
        }
        if rc_filled {
            persisted.rathole = rc.clone();
        }
        persisted.system_port = system_port_str;
        persisted.opencode_port = opencode_port_str;

        let write_result = write_persisted_env(&env_path, &persisted);

        self.input_mode = InputMode::Menu;
        let port_msg = format!(
            "系统={system_port} OpenCode={opencode_port}（重启生效）"
        );
        let final_msg = match (sb_msg.is_empty(), rathole_msg.is_empty()) {
            (true, true) => port_msg,
            (false, true) => format!("{port_msg}；{sb_msg}"),
            (true, false) => format!("{port_msg}；{rathole_msg}"),
            (false, false) => format!("{port_msg}；{sb_msg}；{rathole_msg}"),
        };
        *self.status_message.lock().unwrap() = match write_result {
            Ok(()) => format!("✅ {final_msg}"),
            Err(e) => format!("⚠️ {final_msg}（写文件失败：{e}）"),
        };
        if sb_filled {
            self.verify_remote();
        }
    }

    async fn handle_key(&mut self, event: InputEvent) {
        if self.confirm.is_some() {
            match event {
                InputEvent::Left | InputEvent::Right | InputEvent::Tab => {
                    self.confirm_choice = self.confirm_choice.toggle();
                }
                InputEvent::Select => {
                    let action = self.confirm.take();
                    match action {
                        Some(ConfirmAction::ExitService(i)) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.exit_service(i);
                            }
                        }
                        Some(ConfirmAction::EnterProjectsWithoutServe) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.enter_projects().await;
                            }
                        }
                        Some(ConfirmAction::Exit) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.should_quit = true;
                            }
                        }
                        Some(ConfirmAction::Upgrade) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.start_upgrade();
                            }
                        }
                        None => {}
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
            _ => self.handle_settings_key(event).await,
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
            InputEvent::Quit | InputEvent::Char('q') => {
                self.confirm = Some(ConfirmAction::Exit);
                self.confirm_choice = ConfirmChoice::Cancel;
            }
            _ => {}
        }
    }

    fn focus_move(&mut self, delta: i32) {
        match self.focus {
            Focus::Main => {
                // 主菜单导航空间 = 当前帧实际渲染的 items 下标集合,
                // 而不是 MAIN_ITEMS.len() —— 窗口太矮时 Upgrade 被裁掉,
                // 上/下方向键就不应该 wrap 过去。
                let visible = self.visible_main_items();
                if visible.is_empty() {
                    return;
                }
                let cur = self
                    .main_state
                    .selected()
                    .and_then(|sel| visible.iter().position(|&i| i == sel))
                    .unwrap_or(0);
                let next_idx = (cur as i32 + delta).rem_euclid(visible.len() as i32) as usize;
                self.main_state.select(Some(visible[next_idx]));
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

    /// 主菜单当前帧可见的下标集合。
    ///
    /// 与 [`TuiApp::render_top_row`] 中 `select_visible_items(MAIN_ITEMS, capacity)`
    /// 同步 —— 重复一份 5 行高度的 card 假设,确保 `focus_move` 的导航空间
    /// 与实际渲染一致。
    fn visible_main_items(&self) -> Vec<usize> {
        Self::compute_visible_for_area(&MAIN_ITEMS, self.last_main_column_area)
    }

    /// 纯函数:给定 items 与目标渲染区域,推出应渲染的下标集合。
    ///
    /// 抽出来便于测试 —— `TuiApp::visible_main_items` 只是它的"绑定到 MAIN_ITEMS"
    /// 便捷封装,核心算法都集中在此。
    ///
    /// 触发紧凑模式的条件由 [`MIN_CARD_WIDTH`] 与 `card_h=5` 决定:
    /// - inner_height < 5(连 1 张正常 card 都放不下)
    /// - 或 inner_width < MIN_CARD_WIDTH(card 内部内容会被截断)
    fn compute_visible_for_area(items: &[MenuItem], area: Rect) -> Vec<usize> {
        let inner_height = area.height.saturating_sub(2);
        let inner_width = area.width.saturating_sub(2);
        let compact_mode = inner_height < 5 || inner_width < MIN_CARD_WIDTH;
        if compact_mode {
            // 紧凑模式:1 行 1 essential(无 outer 边框),扣掉 1 行标题后
            // capacity = area.height - 1(对应 render_card_column 内的紧凑布局)。
            let capacity = area.height.saturating_sub(1) as usize;
            Self::select_visible_items_compact(items, capacity)
        } else {
            Self::select_visible_items(items, (inner_height / 5) as usize)
        }
    }

    async fn focus_select(&mut self) {
        match self.focus {
            Focus::Main => {
                // 只接受当前 visible 集合内的下标;若 selected 指向隐藏项
                // (用户缩小窗口后),回退到第一项,而不是激活看不见的按钮。
                let visible = self.visible_main_items();
                let cur = self
                    .main_state
                    .selected()
                    .and_then(|sel| visible.iter().find(|&&i| i == sel).copied());
                let target = cur.unwrap_or(visible.first().copied().unwrap_or(0));
                if let Some(item) = MAIN_ITEMS.get(target) {
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
                kill_process(pid);
            }
        }
        let _ = std::fs::remove_file(&removed.pid_file);
        // 新窗口模式：顺带清理 launcher 脚本（与 pid_file 同 basename，
        // 内容只有 $env: 引用，无敏感信息；留着无害但及时清理更干净）。
        let launcher = std::path::Path::new(&removed.pid_file).with_extension("launcher.ps1");
        let _ = std::fs::remove_file(launcher);
        *self.status_message.lock().unwrap() = format!("✅ 已杀死会话：{}", removed.session);
    }

    async fn activate_item(&mut self, item: MenuItem) {
        match MenuAction::from(item) {
            MenuAction::ToggleOcServe => {
                if self.status_snapshot().opencode_pid.is_some() {
                    self.stop_opencode();
                } else {
                    self.launch_opencode_with_default_port();
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
            MenuAction::Upgrade => {
                // 二次确认(不可逆网络操作);默认 Confirm 选中,回车即升级
                self.confirm = Some(ConfirmAction::Upgrade);
                self.confirm_choice = ConfirmChoice::Confirm;
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
            // serve 未启动：弹确认框提示（默认选中「取消」）。
            self.confirm = Some(ConfirmAction::EnterProjectsWithoutServe);
            self.confirm_choice = ConfirmChoice::Cancel;
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

    // --- 启动 opencode serve（直接读 env 默认端口，无弹框） ---

    fn launch_opencode_with_default_port(&mut self) {
        let port = PortsConfig::load().opencode_port;
        let status = self.status_message.clone();
        *status.lock().unwrap() = format!("🚀 正在启动 OpenCode Serve（port={port}）…");
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

    /// 新窗口版的"创建会话并 attach"（W 键在「新建会话」卡片上触发）。
    async fn create_and_attach_window(&mut self, project: String) {
        let client = self.build_oc_client();
        *self.status_message.lock().unwrap() = "🚀 正在创建会话（新窗口模式）…".to_string();
        match client.create_session(&project).await {
            Ok(sid) => {
                let _ = self.store.append_session(&project, &sid).await;
                let _ = self.store.touch_path(&project).await;
                self.trigger_attach_window(project, sid);
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
        // 不再调 `spawn_in_new_terminal` 弹新窗口 —— 用户从 explorer 双击启动
        // 时没有"当前 PowerShell"可开新 tab，最干净的方案是让 attach 接管
        // mini-oc-gui 自己的 conhost 控制台（同窗口、不弹新、TUI 短暂冻结后
        // resume）。`pending_attach` 携带 attach 信息，run() 主循环下一轮检测
        // 到后调 `run_attach` 接管控制台。
        self.pending_attach = Some(PendingAttach {
            url: self.attach_url.clone(),
            directory: directory.clone(),
            session: session.clone(),
            user: auth.basic_user,
            password: auth.basic_password,
        });
        self.sub_page = None;
        *self.status_message.lock().unwrap() =
            format!("🚀 接管控制台启动 attach 会话 {session}…");
    }

    /// 在新 PowerShell 窗口启动 attach（思路 2，W 键触发）。
    ///
    /// 与 [`Self::trigger_attach`]（同窗口接管）的关键差异：
    /// - TUI **不冻结**：`cmd /c start` 异步创建新窗口后立即返回，
    ///   可连续开多个 attach 窗口（突破同窗口模式"一次一个"的根本限制）；
    /// - 不走 suspend/resume 流程，控制台始终归 TUI 所有；
    /// - PID 由新窗口里的 pwsh 自写 pid_file（start 创建的进程不是本进程
    ///   子进程，Rust 侧拿不到 PID），kill_session 语义不变。
    ///
    /// 失败不自动回退同窗口模式 —— 状态栏提示用户可按 Enter 走稳定路径，
    /// 避免掩盖新窗口路径的问题。
    fn trigger_attach_window(&mut self, directory: String, session: String) {
        let auth = self.auth.read().unwrap_or_else(|e| e.into_inner()).clone();
        let base = std::env::temp_dir().join(format!("oc-attach-{session}"));
        let spec = crate::attach::AttachWindowSpec {
            url: self.attach_url.clone(),
            directory: directory.clone(),
            session: session.clone(),
            user: auth.basic_user,
            password: auth.basic_password,
            pid_file: base.with_extension("pid").to_string_lossy().into_owned(),
            launcher_script: base.with_extension("launcher.ps1").to_string_lossy().into_owned(),
        };
        match crate::attach::spawn_attach_new_window(&spec) {
            Ok(()) => {
                self.sub_page = None;
                self.attached_sessions.lock().unwrap().push(AttachedSession {
                    directory,
                    session: session.clone(),
                    pid_file: spec.pid_file.clone(),
                    started_at: chrono::Utc::now().timestamp(),
                });
                *self.status_message.lock().unwrap() =
                    format!("🪟 attach 会话 {session} 已在新窗口启动（TUI 保持可用）");
            }
            Err(e) => {
                *self.status_message.lock().unwrap() =
                    format!("❌ 新窗口启动失败：{e}（可按 T 用本窗口模式）");
            }
        }
    }

    /// 接管终端跑 attach：suspend TUI → spawn attach + wait → resume TUI。
    ///
    /// 这是**当前唯一稳定的 attach 启动方式**：attach 必须与用户直接交互，
    /// 任何"弹到独立窗口"的中间层方案（PowerShell 7 / Windows Terminal）
    /// 都会与 attach 共享控制台，导致渲染冲突（实测均不可用）。
    ///
    /// 这是一个**会冻结 TUI** 的同步流程：attach 接管控制台期间，mini-oc-gui
    /// 不响应任何 TUI 事件。attach 自己的 prompt / Ctrl+C 让它自然退出后，
    /// TUI resume 重新渲染。这是 tmux / screen 的工作方式 —— 用户进 attach
    /// 就是为了跟 opencode 交互，TUI 冻结可接受。
    ///
    /// **多开/多选限制**：attach 期间 TUI 冻结，无法在 TUI 里同时启动或管理
    /// 多个 attach 会话。详见 `Known limitations` 设计文档中讨论的"多会话
    /// 管理方案"（ConPTY 后台会话 + TUI 输入路由）。
    async fn run_attach(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
    ) {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        use crossterm::execute;
        use crossterm::terminal::{
            EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        };

        let info = match self.pending_attach.take() {
            Some(i) => i,
            None => return,
        };

        // pid_file 在 spawn attach 后由 run_attach_blocking 写入，
        // 这里预先算路径供后续 kill_session 使用。
        let pid_file = std::env::temp_dir()
            .join(format!("oc-attach-{}.pid", info.session))
            .to_string_lossy()
            .into_owned();

        // 1) Suspend TUI —— 顺序很重要：先 leave alt screen 再 disable raw mode，
        //    否则 raw mode 下 leave 之后控制台回到 cooked mode 看着会很乱。
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture);

        // 2) Spawn attach + wait。`spawn_blocking` 把 wait 放到专用线程，
        //    不阻塞 tokio runtime 的其他后台任务（supervisor status refresh、
        //    log buffer 等），但 TUI 主循环本身不响应（控制台被 attach 接管）。
        let url = info.url.clone();
        let directory = info.directory.clone();
        let session_id = info.session.clone();
        let user = info.user;
        let password = info.password;
        let pid_file_for_blocking = pid_file.clone();
        let result =
            tokio::task::spawn_blocking(move || {
                crate::attach::run_attach_blocking(
                    &url,
                    &directory,
                    &session_id,
                    &user,
                    &password,
                    &pid_file_for_blocking,
                )
            })
            .await;

        // 3) Resume TUI —— 顺序与 suspend 对称：先 enable mouse + raw mode，
        //    再 enter alt screen，最后 clear 强制重画（否则 ratatui 内部状态
        //    与终端实际状态不一致，会显示陈旧画面）。
        let _ = enable_raw_mode();
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
        let _ = terminal.clear();

        // 4) 把这次 attach 加到 attached_sessions，供"当前服务"栏 + kill_session
        //    使用（万一 attach 卡死，用户切回 TUI 后可触发 taskkill /T /F）。
        let started_at = chrono::Utc::now().timestamp();
        self.attached_sessions.lock().unwrap().push(AttachedSession {
            directory: info.directory.clone(),
            session: info.session.clone(),
            pid_file: pid_file.clone(),
            started_at,
        });

        // 5) 更新状态栏
        let msg = match result {
            Ok(Ok(status)) if status.success() => {
                format!("✅ attach 会话 {} 已结束", info.session)
            }
            Ok(Ok(status)) => format!(
                "⚠️ attach 会话 {} 退出码 {:?}",
                info.session,
                status.code()
            ),
            Ok(Err(e)) => format!("❌ attach 启动失败：{e}"),
            Err(e) => format!("❌ attach 任务异常：{e}"),
        };
        *self.status_message.lock().unwrap() = msg;
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
            // Enter/→ 默认走**新窗口** attach（思路 2）；T = Takeover 显式选择
            // 同窗口接管（原模式，稳定回退）。仅 Sessions 子页生效 ——
            // Projects/ManualPath 等子页的字符键仍作文本输入/导航用。
            InputEvent::Select | InputEvent::Right => self.sub_page_select().await,
            InputEvent::Char('t') | InputEvent::Char('T')
                if matches!(&self.sub_page, Some(SubPage::Sessions { .. })) =>
            {
                self.sub_page_select_takeover().await;
            }
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
            // attach 类操作默认走**新窗口**模式（思路 2，Enter/鼠标点击）。
            SelectAction::CreateSession(project) => self.create_and_attach_window(project).await,
            SelectAction::Attach(project, sid) => self.trigger_attach_window(project, sid),
            SelectAction::DeleteProject(project) => self.delete_project(project).await,
            SelectAction::ConfirmPath(input) => self.confirm_manual_path(input).await,
            SelectAction::None => {}
        }
    }

    /// T 键分支：对 Sessions 子页当前选中项执行**同窗口接管** attach（原模式）。
    ///
    /// Enter/鼠标默认新窗口；本路径是新窗口失败时的稳定回退（T = Takeover）。
    async fn sub_page_select_takeover(&mut self) {
        let action = match &self.sub_page {
            Some(SubPage::Sessions { list_state, sessions, project }) => {
                let i = list_state.selected().unwrap_or(0);
                if i == 0 {
                    SelectAction::CreateSession(project.clone())
                } else if i <= sessions.len() {
                    SelectAction::Attach(project.clone(), sessions[i - 1].id.clone())
                } else {
                    SelectAction::None
                }
            }
            _ => SelectAction::None,
        };
        match action {
            SelectAction::CreateSession(project) => self.create_and_attach(project).await,
            SelectAction::Attach(project, sid) => self.trigger_attach(project, sid),
            _ => {
                *self.status_message.lock().unwrap() =
                    "💡 T 键用于在会话列表中「新建会话」或选中会话后本窗口接管 attach".to_string();
            }
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
        let title = Self::item_title(item, status);
        match item {
            MenuItem::OcServe => vec![
                Line::from(Span::styled(title, title_style)),
                Line::from(Span::styled("启动 opencode serve 服务", desc_style)),
                Line::from(Span::styled(Self::item_status_line(item, status), status_style)),
            ],
            MenuItem::Rathole => vec![
                Line::from(Span::styled(title, title_style)),
                Line::from(Span::styled("启动 rathole 内网穿透", desc_style)),
                Line::from(Span::styled(Self::item_status_line(item, status), status_style)),
            ],
            MenuItem::OcProjects => vec![
                Line::from(Span::styled(title, title_style)),
                Line::from(Span::styled("选择项目并 attach 会话", desc_style)),
                Line::from(Span::styled("进入项目选择", status_style)),
            ],
            MenuItem::UpgradeOpenCodeAndOmo => vec![
                Line::from(Span::styled(title, title_style)),
                Line::from(Span::styled("升级 opencode 与 oh-my-openagent", desc_style)),
                Line::from(Span::styled("执行升级流程", status_style)),
            ],
        }
    }

    /// 卡片标题(action label,根据当前运行状态动态切换)。
    fn item_title(item: MenuItem, status: &ServeStatus) -> String {
        match item {
            MenuItem::OcServe => {
                if status.opencode_pid.is_some() {
                    "⏹ 停止 OpenCode Serve".to_string()
                } else {
                    "🚀 启动 OpenCode Serve".to_string()
                }
            }
            MenuItem::Rathole => {
                if status.rathole_pid.is_some() {
                    "⏹ 停止 Rathole 隧道".to_string()
                } else {
                    "🚀 启动 Rathole 隧道".to_string()
                }
            }
            MenuItem::OcProjects => "📂 OC 项目".to_string(),
            MenuItem::UpgradeOpenCodeAndOmo => "⬆️ 升级 OpenCode + omo".to_string(),
        }
    }

    /// 状态行("当前:运行中 端口 9464" / "当前:未运行")。
    fn item_status_line(item: MenuItem, status: &ServeStatus) -> String {
        match item {
            MenuItem::OcServe => match status.opencode_pid {
                Some(_) => format!(
                    "当前：运行中 端口 {}",
                    status.port.map(|p| p.to_string()).unwrap_or_default()
                ),
                None => "当前：未运行".to_string(),
            },
            MenuItem::Rathole => match status.rathole_pid {
                Some(pid) => format!("当前：运行中 PID {pid}"),
                None => "当前：未运行".to_string(),
            },
            MenuItem::OcProjects | MenuItem::UpgradeOpenCodeAndOmo => String::new(),
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
            // 子页面:操作区撑满 + 底部状态
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Header
                    Constraint::Min(8),    // 操作空间
                    Constraint::Length(7), // 状态(5 行内容 + 2 行边框)
                ])
                .split(frame.area())
        } else {
            // 主布局:Header + TopRow(操作区) + BottomRow(日志+状态)
            // TopRow 与 BottomRow 同样按 60% / 40% 分左右,
            // 左列(60%)在两行都是系统与服务/OC项目/日志/状态,
            // 右列(40%)在两行都是当前服务(或空)。
            //
            // 关键:TopRow 必须 Min(5) —— 至少能放 1 张完整 card(5 行),
            // 否则 `render_card_column` 会因 inner_height < 5 切到 compact mode,
            // 矮窗口下整个主菜单列变空。
            // BottomRow 用 Min(8) 而非 Length(12):Length 在总和超 area 时
            // 会硬抢 TopRow 空间(12 行 + Header 3 = 15 行下限,12 行窗口时
            // TopRow 直接被抢光)。Min(8) 让两者平起平坐,多出的空间给 TopRow。
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Header
                    Constraint::Min(5),    // TopRow 操作区(至少 1 张完整 card)
                    Constraint::Min(6),    // BottomRow 日志+状态(不足时日志/状态自动压缩)
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
        let settings_hovered = matches!(
            self.mouse_pos,
            Some((c, r)) if c >= header_cols[1].x && c < header_cols[1].x + header_cols[1].width
                && r >= header_cols[1].y && r < header_cols[1].y + header_cols[1].height
        );
        let settings_style = if settings_hovered {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("设置 [s]", settings_style)])),
            header_cols[1],
        );
        self.click_regions.push(ClickRegion {
            rect: header_cols[1],
            target: ClickTarget::Settings,
        });

        if self.sub_page.is_some() {
            self.render_sub_page(frame, chunks[1]);
            self.render_status_panel(frame, chunks[2]);
        } else {
            // 主布局下,把 chunks[1] + chunks[2] 合并传给 render_top_row,
            // 它内部 split 为:左列(服务与系统/OC 项目左右分栏 + 日志/状态上下分栏)
            // + 右列(当前服务撑满,与状态底部对齐)。
            let main_area = Rect::new(
                chunks[1].x,
                chunks[1].y,
                chunks[1].width,
                chunks[1].height + chunks[2].height,
            );
            self.render_top_row(frame, main_area);
        }

        if self.input_mode.is_settings_field() {
            self.render_settings_popup(frame);
        }

        if self.confirm.is_some() {
            self.render_confirm(frame);
        }
    }

    /// 主布局:左 70%(服务与系统 + OC 项目 / 日志 5 行 / 状态 5 行)
    ///   + 右 30%(当前服务撑满,顶部与状态底部对齐)。
    ///
    /// 关键:每层都至少给主菜单列 Min(5) —— 否则 TopRow 在 5-17 行窗口下,
    /// 70% × 60% × 55% 链条会把主菜单列压到 0 行,OcServe/Rathole 完全消失。
    /// 加了 Min(5) 之后,即使 TopRow 只有 5 行,主菜单列仍能完整放下 1 张 card。
    fn render_top_row(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // 整体 horizontal split:左 70% / 右 30%
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(area);

        // 左列 vertical split:
        //   - 顶操作区(服务与系统 + OC 项目)Min(5)
        //   - 日志固定 5 行内容(7 行含边框)
        //   - 状态固定 5 行内容(7 行含边框)
        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // 服务与系统 + OC 项目 — 至少 5 行
                Constraint::Length(7), // 日志(5 行内容 + 2 行边框)
                Constraint::Length(7), // 状态(5 行内容 + 2 行边框)
            ])
            .split(cols[0]);

        // 顶操作区:horizontal split,服务与系统(55%) + OC 项目(45%)
        // 主菜单列 Min(5) 确保即使顶操作区被严重压缩,主菜单也能放 1 张完整 card。
        let top_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(5),       // 服务与系统 — 至少 5 行
                Constraint::Percentage(45),
            ])
            .split(left[0]);

        let status = self.status_snapshot();
        let main_focused = self.focus == Focus::Main;
        let projects_focused = self.focus == Focus::Projects;
        let panel_focused = self.focus == Focus::ServicePanel;
        let sessions = self.attached_sessions.lock().unwrap().clone();

        // 记录主菜单列的渲染区域,供下一次 focus_move 用同一种
        // capacity 公式推算可见 items。`render_card_column` 内部会
        // 重新计算一次,所以这里冗余存一份只为键盘导航同步。
        self.last_main_column_area = top_cols[0];

        Self::render_card_column(
            frame,
            top_cols[0],
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
            top_cols[1],
            "OC 项目",
            &PROJECTS_ITEMS,
            &status,
            projects_focused,
            &mut self.projects_state,
            &mut self.click_regions,
            ColumnKind::Projects,
        );

        // 日志(中间) + 状态(底部)
        self.render_logs(frame, left[1]);
        self.render_status_panel(frame, left[2]);

        // 右列:当前服务撑满整个右列(从顶到底,与状态底部对齐)
        Self::render_service_panel(
            frame,
            cols[1],
            &status,
            &sessions,
            panel_focused,
            &mut self.service_state,
            &mut self.click_regions,
        );
    }

    fn render_status_panel(&self, frame: &mut Frame<'_>, area: Rect) {
        let pid = std::process::id();
        let now = chrono::Local::now();
        let duration = (now - self.program_started_at).num_seconds().max(0);
        let op = self.status_message.lock().unwrap().clone();
        let rathole_state = {
            let rc = self.rathole_config.read().unwrap_or_else(|e| e.into_inner());
            if rc.is_configured() {
                format!("Rathole: ✅ {}:{}（{}）", rc.host, rc.port, rc.name)
            } else {
                "Rathole: 未配置（请在设置中填写 Host/Port/Name/Token）".to_string()
            }
        };
        let remote = self.remote_status.lock().unwrap().clone();
        let status_text = vec![
            Line::from(format!(
                "PID: {pid}    启动时间: {}",
                self.program_started_at.format("%H:%M:%S"),
            )),
            Line::from(format!(
                "运行时长: {}",
                Self::format_duration(duration)
            )),
            Line::from(Span::styled(
                op,
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                rathole_state,
                Style::default().fg(Color::Cyan),
            )),
            Line::from(Span::styled(
                if remote.is_empty() {
                    "正在验证远端存储…".to_string()
                } else {
                    remote
                },
                Style::default().fg(Color::Yellow),
            )),
        ];
        let status_para = Paragraph::new(status_text)
            .block(Block::default().title("状态").borders(Borders::ALL));
        frame.render_widget(status_para, area);
    }

    fn render_settings_popup(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        // 宽 80 适配 80 列终端(实测环境);超长内容走 Paragraph::wrap 自动换行。
        // 高度动态 = 行数 + 2(border),但不超出终端可用高度。
        let w: u16 = 80;
        let mut lines = self.build_settings_lines();
        let btn_line_idx = lines.len() as u16; // 按钮行在底部(以 build_settings_lines 输出计)

        // 先根据 mouse_pos 决定底部按钮文本样式(hover 高亮)。
        // 注意:此处算的是"实际渲染后按钮所在的屏幕坐标",所以必须用最终的
        // `rect` (后续算出来),不能先用 `area`。
        let desired_h = lines.len() as u16 + 2;
        let max_h = area.height.saturating_sub(13).max(8);
        let h = desired_h.min(max_h);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);

        let ok_hovered = self.mouse_pos_in_settings_btn(&rect, btn_line_idx, true);
        let cancel_hovered = self.mouse_pos_in_settings_btn(&rect, btn_line_idx, false);
        let selected_style = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let idle_style = Style::default().fg(Color::DarkGray);
        let ok_style = if ok_hovered { selected_style } else { idle_style };
        let cancel_style = if cancel_hovered { selected_style } else { idle_style };

        lines.push(Line::from("")); // 按钮上方留一行空
        lines.push(Line::from(vec![
            Span::styled("  ", idle_style),
            Span::styled("[确认]", ok_style),
            Span::styled("   ", idle_style),
            Span::styled("[取消]", cancel_style),
            Span::styled("  Enter保存  Esc取消", idle_style),
        ]));

        // 鼠标 hover 字段行 → 自动切换 input_mode(等价于 Tab/点击)
        if let Some((_c, r)) = self.mouse_pos {
            if r >= rect.y + 1 && r < rect.y + btn_line_idx + 1 {
                if let Some(field) = self.settings_field_at_row(r - rect.y - 1) {
                    self.input_mode = field;
                }
            }
        }

        // 先注册 click 区域(根据"显示位置"反推每行的 y 坐标)
        self.register_settings_click_regions(rect, lines.len(), h, btn_line_idx);

        let form = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("设置")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            );
        frame.render_widget(Clear, rect);
        frame.render_widget(form, rect);
    }

    /// 判断鼠标是否在设置弹框底部某个按钮上(用于 hover 高亮判断)。
    ///
    /// 参数 `rect` 是**弹框本身**的 rect(不是屏幕 area),按钮行坐标 = `rect.y + 1 + btn_line_idx + 1`。
    /// 按钮 click 区域宽度 8 列,与按钮文本列对齐。
    fn mouse_pos_in_settings_btn(
        &self,
        rect: &Rect,
        btn_line_idx: u16,
        is_ok: bool,
    ) -> bool {
        let Some((c, r)) = self.mouse_pos else {
            return false;
        };
        let btn_y = rect.y + 1 + btn_line_idx + 1;
        if r != btn_y {
            return false;
        }
        // 按钮起始列 + 宽度 8(覆盖"  [确认]"或"   [取消]")
        let btn_x = if is_ok {
            rect.x + 2
        } else {
            rect.x + 2 + 8 + 3 // [确认](8) + 间距(3)
        };
        c >= btn_x && c < btn_x + 8
    }

    /// 当前 auth 中**已保存**密码的长度(用于 PASSWORD 字段掩码显示)。
    ///
    /// 注意:**故意不读 buffer** —— buffer 是用户当前输入的内容(可能为空),
    /// 我们需要显示"已保存密码"的长度,让用户看到"密码已设,N 个字符",
    /// 改密码时 buffer 长度不反馈(避免长度信息泄露)。
    fn auth_password_len(&self) -> usize {
        self.auth
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .basic_password
            .len()
    }

    /// 把弹框内相对行 idx(0-based,不含上/左边框)转成对应字段 InputMode。
    /// 返回 None 表示该行不是字段行(可能是标题/空行/帮助/按钮行)。
    fn settings_field_at_row(&self, row_inside: u16) -> Option<InputMode> {
        // 与 register_settings_click_regions 中的 FIELD_LINE_IDX 保持一致
        const FIELD_LINE_IDX: [u16; 11] = [2, 3, 6, 7, 11, 12, 13, 16, 17, 18, 19];
        FIELD_LINE_IDX
            .iter()
            .position(|&r| r == row_inside)
            .and_then(|i| SETTINGS_FIELDS.get(i).copied())
    }

    /// 生成设置弹框的所有行内容,同时为每个字段决定高亮样式。
    fn build_settings_lines(&self) -> Vec<Line<'static>> {
        let active = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let inactive = Style::default();
        let style_for = |m: InputMode| {
            if self.input_mode == m {
                active
            } else {
                inactive
            }
        };
        let title_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let help_style = Style::default().fg(Color::DarkGray);

        vec![
            Line::from(Span::styled("认证设置", title_style)),
            Line::from(""),
            Line::from(Span::styled(
                format!("  USERNAME: {}", self.username_input),
                style_for(InputMode::SettingsAuthUsername),
            )),
            Line::from(Span::styled(
                format!(
                    "  PASSWORD: {}",
                    // 永远按 auth 中密码长度显示:buffer 为空时显示当前密码
                    // 长度(buffer 已 clear 清)→ 让用户知道已设密码且长度为 N;
                    // buffer 非空时也按 auth 长度(改密码过程中不反馈长度,避免
                    // 长度信息泄露)。
                    "*".repeat(self.auth_password_len())
                ),
                style_for(InputMode::SettingsAuthPassword),
            )),
            Line::from(""),
            Line::from(Span::styled("端口设置", title_style)),
            Line::from(""),
            Line::from(Span::styled(
                format!("  系统端口:    {}", self.system_port_input),
                style_for(InputMode::SettingsHttpPort),
            )),
            Line::from(Span::styled(
                format!("  OpenCode:  {}", self.opencode_port_input),
                style_for(InputMode::SettingsServePort),
            )),
            Line::from(""),
            Line::from(Span::styled("远程 SilverBullet 设置", title_style)),
            Line::from(""),
            Line::from(Span::styled(
                format!("  远程路径: {}", self.sb_url_input),
                style_for(InputMode::SettingsUrl),
            )),
            Line::from(Span::styled(
                format!("  用户名:   {}", self.sb_user_input),
                style_for(InputMode::SettingsUser),
            )),
            Line::from(Span::styled(
                format!("  密码:     {}", "*".repeat(self.sb_password_input.len())),
                style_for(InputMode::SettingsPassword),
            )),
            Line::from(""),
            Line::from(Span::styled("Rathole 内网穿透设置", title_style)),
            Line::from(""),
            Line::from(Span::styled(
                format!("  Host:   {}", self.rathole_host_input),
                style_for(InputMode::SettingsRatholeHost),
            )),
            Line::from(Span::styled(
                format!("  Port:   {}", self.rathole_port_input),
                style_for(InputMode::SettingsRatholePort),
            )),
            Line::from(Span::styled(
                format!("  Name:   {}", self.rathole_name_input),
                style_for(InputMode::SettingsRatholeName),
            )),
            Line::from(Span::styled(
                format!("  Token:  {}", self.rathole_token_input),
                style_for(InputMode::SettingsRatholeToken),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Tab/↑/↓ 切换字段  Enter 保存  Esc 取消  (点击字段行直接跳到该输入)",
                help_style,
            )),
        ]
    }

    /// 为设置弹框内的每个字段注册 ClickRegion(鼠标点击切换焦点)。
    ///
    /// `SETTINGS_FIELDS` 与 `build_settings_lines` 的顺序一一对应:
    /// - 0: USERNAME  (line idx 2)
    /// - 1: PASSWORD  (line idx 3)
    /// - 2: HTTP 端口 (line idx 6)
    /// - 3: Serve 端口(line idx 7)
    /// - 4: SB URL    (line idx 11)
    /// - 5: SB User   (line idx 12)
    /// - 6: SB Password (line idx 13)
    /// - 7: Rathole Host (line idx 16)
    /// - 8: Rathole Port (line idx 17)
    /// - 9: Rathole Name (line idx 18)
    /// - 10: Rathole Token(line idx 19)
    fn register_settings_click_regions(
        &mut self,
        rect: Rect,
        _lines_len: usize,
        _h: u16,
        btn_line_idx: u16,
    ) {
        // lines 数组内的"字段行"索引(从 0 开始);0 是标题,1 是空行。
        // 与 settings_field_at_row 中的索引保持一致。
        const FIELD_LINE_IDX: [usize; 11] = [2, 3, 6, 7, 11, 12, 13, 16, 17, 18, 19];
        // 弹框上方 border 占 1 行,所以字段 line idx 0 (认证设置标题) 在 rect.y + 1。
        for (i, field) in SETTINGS_FIELDS.iter().enumerate() {
            let line_idx = FIELD_LINE_IDX[i];
            let target_y = rect.y + 1 + line_idx as u16;
            // 字段矩形覆盖整行宽度(去掉左右各 1 的 border),高度 1。
            // wrap 后的内容会渲染到下一行,鼠标只能点 prefix 那一行,
            // 这是 wrap 语义与 click region 的固有取舍。
            self.click_regions.push(ClickRegion {
                rect: Rect::new(rect.x + 1, target_y, rect.width.saturating_sub(2), 1),
                target: ClickTarget::SettingsField(*field),
            });
        }
        // 底部按钮 click region:确认按钮(btn_line_idx + 1) + 取消按钮
        // 与 mouse_pos_in_settings_btn 的坐标计算保持完全一致,避免
        // "hover 高亮但点击无反应"或反之。
        let ok_y = rect.y + 1 + btn_line_idx + 1;
        let cancel_y = ok_y;
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 2, ok_y, 8, 1),
            target: ClickTarget::SettingsOk,
        });
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 13, cancel_y, 8, 1),
            target: ClickTarget::SettingsCancel,
        });
    }

    fn render_confirm(&mut self, frame: &mut Frame<'_>) {
        let (msg_lines, w, h): (Vec<&str>, u16, u16) = match self.confirm {
            Some(ConfirmAction::ExitService(_)) => {
                (vec!["确认杀死/关闭该服务？"], 44, 5)
            }
            Some(ConfirmAction::EnterProjectsWithoutServe) => (
                vec![
                    "未启动 OpenCode Serve",
                    "直接启动项目将无法支持远程服务，仍要继续吗？",
                ],
                68,
                7,
            ),
            Some(ConfirmAction::Exit) => {
                (vec!["确认退出程序?"], 36, 5)
            }
            Some(ConfirmAction::Upgrade) => {
                (vec!["确认升级 OpenCode + omo?"], 44, 5)
            }
            None => return,
        };
        let area = frame.area();
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("确认")
            .border_style(Style::default().fg(Color::Yellow));

        let btn_y = rect.y + 2 + msg_lines.len() as u16;
        let confirm_rect = Rect::new(rect.x + 1, btn_y, 11, 1);
        let cancel_rect = Rect::new(rect.x + 13, btn_y, 11, 1);
        // 鼠标 hover 按钮时,自动把 confirm_choice 切过去(类似键盘左右键)
        if let Some((c, r)) = self.mouse_pos {
            if r == btn_y {
                if c >= confirm_rect.x && c < confirm_rect.x + confirm_rect.width {
                    self.confirm_choice = ConfirmChoice::Confirm;
                } else if c >= cancel_rect.x && c < cancel_rect.x + cancel_rect.width {
                    self.confirm_choice = ConfirmChoice::Cancel;
                }
            }
        }

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

        let mut lines: Vec<Line<'_>> = msg_lines.into_iter().map(Line::from).collect();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![confirm_btn, Span::raw("   "), cancel_btn]));
        let para = Paragraph::new(lines).block(block);
        frame.render_widget(Clear, rect);
        frame.render_widget(para, rect);

        self.click_regions.push(ClickRegion {
            rect: confirm_rect,
            target: ClickTarget::ConfirmOk,
        });
        self.click_regions.push(ClickRegion {
            rect: cancel_rect,
            target: ClickTarget::CancelBtn,
        });
    }

    

    /// 按 capacity 从 `items` 里挑出应当渲染的下标集合。
    ///
    /// 算法:
    /// 1. 先把 essential 项(`OcServe` / `Rathole`)按原顺序全部入选
    ///    —— 用户的核心服务开关永远可见,即使窗口极矮也只能砍次要项。
    /// 2. 再把非 essential 项按原顺序追加,直到 `capacity` 用尽。
    /// 3. 如果 essential 本身就超出 capacity,只保留前 capacity 个
    ///    essential(极端兜底,理论上至少 2 行 × 5 行 = 10 行才能放下 essential)。
    ///
    /// 返回的是 *items 中的下标*,不是 MenuItem 本身;后续渲染时通过
    /// `items[item_idx]` 取回 MenuItem。
    fn select_visible_items(items: &[MenuItem], capacity: usize) -> Vec<usize> {
        if capacity == 0 {
            return Vec::new();
        }
        let mut visible = Vec::with_capacity(items.len().min(capacity));
        // 1. essential
        for (i, item) in items.iter().enumerate() {
            if visible.len() >= capacity {
                break;
            }
            if item.is_essential() {
                visible.push(i);
            }
        }
        // 2. 非 essential
        for (i, item) in items.iter().enumerate() {
            if visible.len() >= capacity {
                break;
            }
            if !item.is_essential() {
                visible.push(i);
            }
        }
        visible
    }

    /// 紧凑模式下的可见下标选择:只选 essential,非 essential 全部砍掉。
    ///
    /// 紧凑模式在窗口小于 [`MIN_CARD_WIDTH`] 或 inner 高度 < 1 张完整 card 时触发;
    /// 此时 essential 项每行 1 个,目标是不管窗口多小都至少有这两个按钮可点。
    fn select_visible_items_compact(items: &[MenuItem], capacity: usize) -> Vec<usize> {
        if capacity == 0 {
            return Vec::new();
        }
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.is_essential())
            .take(capacity)
            .map(|(i, _)| i)
            .collect()
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
        // 极端兜底:area 太小(高度 < 1 没空间画任何东西,或宽度 < 1)
        // → 完全不渲染,避免画一格空边框误导用户。
        if area.height == 0 || area.width == 0 {
            return;
        }

        let card_h = 5u16;

        // 紧凑模式触发条件:
        // - 高度不足以容纳 1 张完整 card(< 5 行,因为 outer 框本身要 2 行边框),
        // - 或宽度不足以容纳 card 标题("⏹ 停止 OpenCode Serve" ≈ 18 显示列 + 边框 2 + padding 2 = 22)。
        // 紧凑模式下高度成为瓶颈 —— 1 张 card 至少 3 行(2 outer 边框 + 1 内容),
        // 但 12 行窗口下 TopRow 顶操作区 inner 只有 3 行,装 1 张完整 card 后
        // 只能再装 0 张 —— 不够展示 OcServe+Rathole 两个核心按钮。
        // 因此紧凑模式 *不画 outer 边框*,每 essential 仅占 1 行。
        let inner_height = area.height.saturating_sub(2);
        let inner_width = area.width.saturating_sub(2);
        let compact_mode = inner_height < card_h || inner_width < MIN_CARD_WIDTH;

        // 紧凑模式:每 essential 1 行,无内 Block 边框 — 牺牲视觉一致性换最大装入数。
        let row_h: u16 = if compact_mode { 1 } else { card_h };

        // 紧凑模式:visible 必须按"可装入最多 essential"算 —— 见 `compute_visible_for_area`。
        let visible: Vec<usize> = if compact_mode {
            Self::select_visible_items_compact(items, area.height as usize)
        } else {
            Self::select_visible_items(items, (inner_height / row_h) as usize)
        };

        // title 后追加 "+N hidden" 提示被裁掉多少项。
        let hidden = items.len().saturating_sub(visible.len());
        let title_full = if hidden > 0 {
            format!("{title} (+{hidden} hidden)")
        } else {
            title.to_string()
        };

        if compact_mode {
            // 紧凑模式:不画 outer 边框,直接把多 essential 排成 list。
            // 顶部 1 行作为 "title "+"(+N hidden)" 标题,后续各 1 行 essential。
            // 这样 12 行窗口 TopRow=5,能装 4 essential 当前 2 个全展示)。
            let mut y = area.y;
            // 标题行
            let title_para = Paragraph::new(Line::from(Span::styled(
                title_full,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            frame.render_widget(title_para, Rect::new(area.x, y, area.width, 1));
            y += 1;
            for &item_idx in &visible {
                if y >= area.y + area.height {
                    break;
                }
                let row_area = Rect::new(area.x, y, area.width, 1);
                let target = match kind {
                    ColumnKind::Main => ClickTarget::MainColumn(item_idx),
                    ColumnKind::Projects => ClickTarget::ProjectsColumn(item_idx),
                };
                regions.push(ClickRegion { rect: row_area, target });
                let selected = focused && state.selected() == Some(item_idx);
                let style = if selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let prefix = if selected { "▶ " } else { "  " };
                let para = Paragraph::new(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(Self::item_title(items[item_idx], status), style),
                ]));
                frame.render_widget(para, row_area);
                y += 1;
            }
            return;
        }

        // 正常模式:画 outer 框 + 内部 card 网格。
        let outer = Block::default()
            .title(title_full)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);

        let mut y = inner.y;
        for &item_idx in &visible {
            if y + row_h > inner.y + inner.height {
                break;
            }
            let card_area = Rect::new(inner.x, y, inner.width, row_h);
            let target = match kind {
                ColumnKind::Main => ClickTarget::MainColumn(item_idx),
                ColumnKind::Projects => ClickTarget::ProjectsColumn(item_idx),
            };
            regions.push(ClickRegion { rect: card_area, target });
            let selected = focused && state.selected() == Some(item_idx);
            let border_style = if selected {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .padding(Padding::horizontal(1));
            let para = Paragraph::new(Self::item_card(items[item_idx], status)).block(block);
            frame.render_widget(para, card_area);
            y += row_h;
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
        self.last_sub_page_area = area;
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
                            SYS_PICKER_DESC,
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
            Line::from(Span::styled(
                "创建新会话并 attach",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "Enter 新窗口 · T 本窗口",
                Style::default().fg(Color::Yellow),
            )),
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
                "Enter 新窗口 attach · T 本窗口接管",
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

    fn render_logs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        // 主界面日志固定只显示最近 5 行(全屏日志模式不受此限制,见 render_full_log)。
        let inner_height = area.height.saturating_sub(2).clamp(1, 5) as usize;
        let lines: Vec<Line<'_>> = self
            .log_buffer
            .tail(inner_height)
            .into_iter()
            .map(Line::from)
            .collect();
        // 鼠标 hover 时边框高亮(青底色,与设置弹框同款)。
        // 弹框(设置 / 确认)打开时不显示高亮 — 与 click_at 的穿透阻止一致,
        // 避免视觉错觉"鼠标在日志上"但其实弹框在抢焦点。
        let popup_open =
            self.input_mode.is_settings_field() || self.confirm.is_some();
        let hovered = !popup_open
            && matches!(
                self.mouse_pos,
                Some((c, r)) if c >= area.x && c < area.x + area.width
                    && r >= area.y && r < area.y + area.height
            );
        let border_style = if hovered {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let log = Paragraph::new(lines).block(
            Block::default()
                .title("日志 [显示全部: l]")
                .borders(Borders::ALL)
                .border_style(border_style),
        );
        // 注册 click region（hover 高亮同区域 click_at 共享）
        self.click_regions.push(ClickRegion {
            rect: area,
            target: ClickTarget::Logs,
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `MAIN_ITEMS` 的真实顺序(用于交叉验证)。
    /// 这里冗余声明一份,只服务测试 —— 若 MAIN_ITEMS 顺序变化,
    /// 下列 case 中的下标常量需要同步更新,否则测试会失败提醒我们
    /// 重新评估 essential vs 非 essential 的排序策略。
    const ITEMS_OcServe: usize = 0;
    const ITEMS_Rathole: usize = 1;
    const ITEMS_Upgrade: usize = 2;

    #[test]
    fn select_visible_keeps_essential_when_oversubscribed() {
        // 容量只够放 1 张卡 → 必须保留 essential(OcServe),砍其他。
        let v = TuiApp::select_visible_items(&MAIN_ITEMS, 1);
        assert_eq!(v, vec![ITEMS_OcServe]);
    }

    #[test]
    fn select_visible_keeps_both_essential_when_capacity_two() {
        // 容量 = 2 → 两个 essential 全保留,Upgrade 砍掉。
        let v = TuiApp::select_visible_items(&MAIN_ITEMS, 2);
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn select_visible_includes_non_essential_when_room() {
        // 容量 = 3 → 全 3 项,顺序与 MAIN_ITEMS 一致。
        let v = TuiApp::select_visible_items(&MAIN_ITEMS, 3);
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole, ITEMS_Upgrade]);
    }

    #[test]
    fn select_visible_capacity_zero_returns_empty() {
        // inner 太矮(0 行)→ 全部裁掉,essential 也保不住。
        let v = TuiApp::select_visible_items(&MAIN_ITEMS, 0);
        assert!(v.is_empty());
    }

    #[test]
    fn select_visible_oversized_capacity_caps_at_items_len() {
        // 容量超出 items.len() → 不会越界或 panic,只返回所有 items。
        let v = TuiApp::select_visible_items(&MAIN_ITEMS, 99);
        assert_eq!(v.len(), MAIN_ITEMS.len());
    }

    // ---- compact_mode 测试 ----

    #[test]
    fn compact_select_keeps_both_essential_with_capacity_two() {
        // 容量 = 2(典型矮窗口 inner=2):两个 essential 都装下。
        let v = TuiApp::select_visible_items_compact(&MAIN_ITEMS, 2);
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn compact_select_drops_non_essential_even_with_capacity() {
        // 容量 = 3 也只能装 essential —— 紧凑模式不允许 Upgrade/OcProjects 出现。
        let v = TuiApp::select_visible_items_compact(&MAIN_ITEMS, 3);
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn compact_select_capacity_zero_returns_empty() {
        // inner 高度 = 0(连 1 行都放不下)→ 整个列只能空着。
        let v = TuiApp::select_visible_items_compact(&MAIN_ITEMS, 0);
        assert!(v.is_empty());
    }

    #[test]
    fn compact_select_truncates_essential_when_capacity_one() {
        // 容量 = 1(极矮)→ 只保留第一个 essential(OcServe),Rathole 也砍掉。
        let v = TuiApp::select_visible_items_compact(&MAIN_ITEMS, 1);
        assert_eq!(v, vec![ITEMS_OcServe]);
    }

    #[test]
    fn visible_main_items_uses_compact_when_area_is_short() {
        // area.height = 4 行 → inner = 2 → 紧凑模式 → 可见 2 行 essential。
        let v = TuiApp::compute_visible_for_area(&MAIN_ITEMS, rect(80, 4));
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn visible_main_items_uses_compact_when_area_is_narrow() {
        // area.width = 20 → inner = 18 < MIN_CARD_WIDTH(22) → 紧凑模式 → 2 essential。
        let v = TuiApp::compute_visible_for_area(&MAIN_ITEMS, rect(20, 30));
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn visible_main_items_uses_normal_when_area_is_well_sized() {
        // area = 80x30 → inner_width >= 22,inner_height >= 5 → 正常模式 → 3 项全可见。
        let v = TuiApp::compute_visible_for_area(&MAIN_ITEMS, rect(80, 30));
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole, ITEMS_Upgrade]);
    }

    #[test]
    fn visible_main_items_uses_normal_when_three_full_cards_fit() {
        // 高度 12 行 → inner=10 → capacity=2 → 选 essential × 2。
        let v = TuiApp::compute_visible_for_area(&MAIN_ITEMS, rect(40, 12));
        assert_eq!(v, vec![ITEMS_OcServe, ITEMS_Rathole]);
    }

    #[test]
    fn item_title_toggles_by_running_state() {
        // 标题必须根据运行状态切换:运行中显示"停止",未运行显示"启动"。
        let mut status = ServeStatus::default();
        assert!(TuiApp::item_title(MenuItem::OcServe, &status).contains("启动"));
        status.opencode_pid = Some(1234);
        assert!(TuiApp::item_title(MenuItem::OcServe, &status).contains("停止"));
    }

    /// 构造测试用的 Rect(0,0) 起点。
    fn rect(width: u16, height: u16) -> ratatui::layout::Rect {
        ratatui::layout::Rect::new(0, 0, width, height)
    }

    /// 模拟 `render` 里 main layout 的 chunks 分配(Header + TopRow + BottomRow),
    /// 返回 chunks 数组。这与主 `render` 用同样的 Constraint 序列,
    /// 这样测试可以断言"给定的窗口高度下,TopRow 真的能装下 N 张 card"。
    fn main_layout_chunks(area: ratatui::layout::Rect) -> Vec<ratatui::layout::Rect> {
        ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Length(3),
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Min(8),
            ])
            .split(area)
            .to_vec()
    }

    #[test]
    fn layout_toprow_is_at_least_5_lines_in_12_to_20_line_windows() {
        // 用户报问题的 12-20 行窗口:TopRow 必须 ≥ 5 行,才能放下 1 张完整 card
        // (此时 OcServe/Rathole 不会因 compact mode 提前塌缩)。
        for h in 12u16..=20 {
            let chunks = main_layout_chunks(rect(80, h));
            let toprow_h = chunks[1].height;
            assert!(
                toprow_h >= 5,
                "window height = {h}: TopRow should be ≥ 5 lines, got {toprow_h}"
            );
        }
    }

    #[test]
    fn layout_probe_main_column_per_height() {
        // 调试辅助:打印每个高度下主菜单列的实际尺寸。
        // 失败时这条信息能告诉我们在哪个高度开始的。
        for h in [12u16, 14, 16, 18, 20, 25, 30] {
            let chunks = main_layout_chunks(rect(80, h));
            let toprow = chunks[1];
            let cols = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Horizontal)
                .constraints([
                    ratatui::layout::Constraint::Percentage(70),
                    ratatui::layout::Constraint::Percentage(30),
                ])
                .split(toprow)
                .to_vec();
let left = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Length(7),
                ratatui::layout::Constraint::Length(7),
            ])
            .split(cols[0])
            .to_vec();
            let top_cols = ratatui::layout::Layout::default()
                .direction(ratatui::layout::Direction::Horizontal)
                .constraints([
                    ratatui::layout::Constraint::Min(5),
                    ratatui::layout::Constraint::Percentage(45),
                ])
                .split(left[0])
                .to_vec();
            let main_col = top_cols[0];
            let visible = TuiApp::compute_visible_for_area(&MAIN_ITEMS, main_col);
            eprintln!(
                "[layout_probe] h={h} toprow={} cols[0]={} left[0]={} main_col={}x{} visible={:?}",
                toprow.height, cols[0].height, left[0].height,
                main_col.width, main_col.height, visible
            );
        }
    }

    #[test]
    fn layout_main_column_renders_essential_at_14_lines() {
        // 14 行窗口(用户场景的典型值):模拟完整 layout 链
        // chunks → 顶操作区左列 → 主菜单列 → 调用 compute_visible_for_area
        // 应当至少选到 OcServe+Rathole。
        // 这是回归测试:之前 layout 用 Percentage 嵌套,12-17 行窗口下
        // 主菜单列被压到 0×0,OcServe/Rathole 完全消失。修复后
        // 主菜单列 Min(5) 保证至少 5 行。
        let chunks = main_layout_chunks(rect(80, 14));
        let toprow = chunks[1];
        let cols = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Horizontal)
            .constraints([
                ratatui::layout::Constraint::Percentage(70),
                ratatui::layout::Constraint::Percentage(30),
            ])
            .split(toprow)
            .to_vec();
        let left = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Vertical)
            .constraints([
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Length(7),
                ratatui::layout::Constraint::Length(7),
            ])
            .split(cols[0])
            .to_vec();
        let top_cols = ratatui::layout::Layout::default()
            .direction(ratatui::layout::Direction::Horizontal)
            .constraints([
                ratatui::layout::Constraint::Min(5),
                ratatui::layout::Constraint::Percentage(45),
            ])
            .split(left[0])
            .to_vec();
        let main_col = top_cols[0];
        let visible = TuiApp::compute_visible_for_area(&MAIN_ITEMS, main_col);
        assert!(
            visible.contains(&ITEMS_OcServe) && visible.contains(&ITEMS_Rathole),
            "main column at 14x80 must include OcServe+Rathole, got {visible:?} (col={main_col:?})"
        );
    }
}


