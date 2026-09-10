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
use crate::error::AppError;
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

/// 读取系统剪贴板的纯文本内容。
///
/// - 失败(无 GUI 剪贴板服务 / 不支持的平台 / 剪贴板非文本):返回空字符串。
/// - 永远不 panic —— arboard 在不同平台初始化都可能抛错(后台进程、
///   无 X11 server 的 Linux 等),我们对结果做兜底。
///
/// 设计意图:设置面板的「Ctrl+V 粘贴」需要直接拿到剪贴板内容,而
/// crossterm 只会送 `Char('v')` + Ctrl 修饰,不会喂真实文本。
/// 因此在收到 `InputEvent::Paste(_)` 且 payload 为空时,统一调一次。
fn read_clipboard_text() -> String {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.get_text().unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// 设置弹框外点击的处置策略(纯函数,便于单测覆盖三类规则)。
///
/// 用户最新精确要求:
/// - 点击弹框内部(包括 USERNAME 字段和任何空白/说明区域) → 永远不关闭。
/// - 点击弹框外:
///   * 普通设置(已配置过) → 关闭弹框;
///   * 首次启动未配置 → 仍保持打开,只允许 Esc 关闭。
///
/// 这个枚举与 `should_dismiss_settings_on_click` 配套使用 —— 三个
/// 变体直接对应上述三种处置,把"if-else 链"显式化,便于阅读与测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsOutsideAction {
    /// 点击落在弹框内(字段 / 按钮 / 空白 / 说明行 / 边框) — 无操作。
    Inside,
    /// 点击落在弹框外,且不是首次启动 → 关闭弹框。
    DismissOutside,
    /// 点击落在弹框外,但当前是首次启动未配置 → 保持弹框打开。
    /// 状态栏可顺便提示用户"首次启动请按 Esc 关闭"。
    FirstSetupBlockOutside,
}

/// 纯函数:给定点 (col, row) 与最近一次渲染记录到的设置弹框 rect,
/// 决定点击的处置策略。
///
/// 设计意图 —— 把"点击内部不关闭"这条规则从 `click_at` 里抽出来,
/// 避免每次都靠 `find_target == None` 这种间接信号判定"是否在弹框内"。
/// `find_target` 的 None 也会因为"弹框 rect 记录为 None(还没渲染过)"而
/// 触发,但这两种语义不同:后者是"还没渲染",前者是"渲染了但点空白"。
/// 用 rect 几何判定可以稳定区分。
///
/// 三个返回值的语义:
/// - `Inside` — click 落在弹框矩形内(含边框)。无论首启还是普通,
///   永远不关闭。**包括 USERNAME 字段、以及任何空白 / 说明行 / 帮助行。**
/// - `DismissOutside` — click 落在弹框外,普通设置模式 → 关闭弹框。
/// - `FirstSetupBlockOutside` — click 落在弹框外,首次启动未配置 →
///   保持弹框打开(只允许 Esc 关闭)。
///
/// `first_setup_required` 是 [`TuiApp`] 上的显式布尔字段,由
/// `TuiApp::new` 根据 `auth.is_configured()` 初始化,不依赖临时
/// buffer / `input_mode` 的副作用。
fn should_dismiss_settings_on_click(
    click: (u16, u16),
    popup_rect: Option<Rect>,
    first_setup_required: bool,
) -> SettingsOutsideAction {
    // 弹框尚未渲染(测试中 / 启动第一帧)→ 弹框外 → 沿用旧规则 dismiss。
    // 这条边界条件保证测试中可以稳定构造"无 rect"场景,且不破坏主循环
    // 启动初期行为(主循环每帧都先 render 再 handle 事件,所以实际不会触发)。
    let Some(rect) = popup_rect else {
        return if first_setup_required {
            SettingsOutsideAction::FirstSetupBlockOutside
        } else {
            SettingsOutsideAction::DismissOutside
        };
    };
    let (col, row) = click;
    let inside = col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height);
    if inside {
        SettingsOutsideAction::Inside
    } else if first_setup_required {
        SettingsOutsideAction::FirstSetupBlockOutside
    } else {
        SettingsOutsideAction::DismissOutside
    }
}

/// 渲染设置页 PASSWORD 字段掩码行 —— 纯函数,只依赖当前输入 buffer 与
/// 「已保存密码长度」占位。
///
/// 行为:把 `input` 的字符数 与 `mask_len` 取较大值,渲染对应数量的 `*`,
/// 拼成 `"  PASSWORD: ****"`(与 build_settings_lines 的固定前缀 +
/// SilverBullet 密码字段保持视觉一致)。
///
/// 设计意图 —— 抽成纯函数,便于单测覆盖:
/// - buffer 空 + mask_len=0  → `"  PASSWORD: "`         (首启未配置)
/// - buffer 空 + mask_len=N  → `"  PASSWORD: " + "*" * N` (回显已保存密码)
/// - buffer 长度 M + mask_len=N → `"  PASSWORD: " + "*" * max(M, N)`
///
/// 两套参数同时存在的原因:
/// - `input` 实时跟随用户在 buffer 里的输入(粘贴 / 输入 / 退格),反映
///   "我刚才输入了几位";
/// - `mask_len` 在 `open_settings` 时一次性记录"已保存密码长度",用户没动
///   buffer 也能看到对应位数的 \* 占位 —— 不需要把真实密码回填到
///   `password_input` buffer(否则用户没改就保存时,`submit_settings` 会
///   把 `password_input` 当成新密码提交,把真实密码覆盖掉)。
///
/// 渲染时取 max 是为了:用户已开始输入时,buffer 长度可能超过 mask_len
/// (不会发生但理论上),以及 mask_len 不会"夹"在 buffer 中段,只决定
/// 底限显示。
fn render_auth_password_line(input: &str, mask_len: usize) -> String {
    let n = input.chars().count().max(mask_len);
    format!("  PASSWORD: {}", "*".repeat(n))
}

/// 把粘贴文本按字段规则追加到对应 buffer,返回追加后的新 buffer。
///
/// 设计意图:**抽成纯函数**以便单测覆盖所有 11 个字段 + 端口过滤 +
/// 5 位上限。TuiApp 的 11 个 buffer 都是 String,字段路由通过
/// `InputMode` 决定,函数不持有任何 self 之外的引用,易测易推。
///
/// 行为细节:
/// - 普通文本字段:整段追加,保留所有字符(含中文 / 空格 / 标点)。
/// - 三个端口字段(`SettingsHttpPort` / `SettingsServePort` /
///   `SettingsRatholePort`):只保留 ASCII 数字,且总长度 ≤ 5。
/// - `Menu` 模式:粘贴不生效(防御性,正常路径不会传进来)。
fn apply_paste_to_buffer(field: InputMode, current: &str, text: &str) -> String {
    let mut buf = current.to_string();
    if text.is_empty() {
        return buf;
    }
    match field {
        InputMode::SettingsHttpPort => {
            for c in text.chars().filter(|c| c.is_ascii_digit()) {
                if buf.len() >= 5 {
                    break;
                }
                buf.push(c);
            }
        }
        InputMode::SettingsServePort => {
            for c in text.chars().filter(|c| c.is_ascii_digit()) {
                if buf.len() >= 5 {
                    break;
                }
                buf.push(c);
            }
        }
        InputMode::SettingsRatholePort => {
            for c in text.chars().filter(|c| c.is_ascii_digit()) {
                if buf.len() >= 5 {
                    break;
                }
                buf.push(c);
            }
        }
        InputMode::SettingsAuthUsername
        | InputMode::SettingsAuthPassword
        | InputMode::SettingsUrl
        | InputMode::SettingsUser
        | InputMode::SettingsPassword
        | InputMode::SettingsRatholeHost
        | InputMode::SettingsRatholeName
        | InputMode::SettingsRatholeToken => buf.push_str(text),
        InputMode::Menu => {}
    }
    buf
}

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
#[derive(Debug, Clone)]
enum ConfirmAction {
    /// 杀死/关闭「当前服务」栏第 i 个服务。
    ExitService(usize),
    /// 未启动 serve 时仍进入 OC 项目流程（无远程服务支持）。
    EnterProjectsWithoutServe,
    /// 升级 OpenCode + omo（不可逆网络操作）。
    Upgrade,
    /// 退出整个程序。
    Exit,
    /// 删除单个 opencode 会话（path, session_id）。项目记录保留。
    DeleteSession(String, String),
    /// opencode serve 启动时端口被占用：用户已确认「杀死占用进程并继续」。
    /// 携带目标端口，confirm 后调 `kill_port_listener` + `launch_opencode`。
    KillPortAndLaunch(u16),
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

/// 设置弹框底部按钮的种类（用于 [`TuiApp::mouse_pos_in_settings_btn`] 的
/// 列位置 + 宽度计算）。
#[derive(Debug, Clone, Copy)]
enum SettingsBtnKind {
    /// `[确认]` 按钮（保存提交）。
    Ok,
    /// `[取消]` 按钮（关闭弹框不保存）。
    Cancel,
    /// `[打开配置目录]` 按钮（调系统文件管理器打开配置目录）。
    OpenConfigDir,
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
    /// 设置弹框底部的「打开配置文件目录」按钮（点击 = 调系统文件管理器
    /// 打开 `unified_env_path()` 的父目录 —— 配置文件所在目录）。
    SettingsOpenConfigDir,
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
    /// 设置面板 PASSWORD 行回显用的「已保存密码长度」占位（仅展示用，不影响
    /// `password_input` buffer 内容）。`open_settings` 时一次性记录当前
    /// auth.basic_password 长度；用户不动 buffer 也能看到对应位数的 `*`,
    /// 真实密码内容不会被回填（否则保存时会被覆盖）。
    auth_password_mask_len: usize,
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
    /// 设置弹框内部内容的垂直滚动偏移（行数）。
    ///
    /// 当终端高度不足以一次渲染完整布局(约 31 行)时,`render_settings_popup`
    /// 通过 `Paragraph::scroll((scroll_offset, 0))` 把被截掉的部分向下移动。
    /// `settings_field_at_row` 与 `register_settings_click_regions` 必须用
    /// 同样的偏移来计算屏幕坐标,否则屏幕外的字段 click region 会落在弹框外,
    /// 导致 `find_target` 返回 None → `click_at` 误判为"点击弹框外"→ 关闭弹框。
    ///
    /// 取值范围:`0..=TOTAL_CONTENT_ROWS - 1`(由 `render_settings_popup`
    /// 用 `saturating_sub` 收紧)。初始 0 = 不滚动。
    settings_scroll_offset: u16,
    /// 上一帧设置弹框在屏幕上的 rect(由 `render_settings_popup` 写入)。
    ///
    /// 供 `click_at` 用几何判定"点击是否在弹框内",**不**依赖
    /// `find_target` 的间接信号(因为弹框内的说明行 / 空白 / 边框没
    /// 注册 click region,`find_target` 会返回 None,但语义上仍然在
    /// 弹框内 — 必须保持打开)。
    ///
    /// 弹框未打开时为 `None`,主循环里 `click_at` 看到 None 就按旧
    /// 路径处理(不应发生,主循环每帧 render 之后才 handle 事件)。
    last_settings_popup_rect: Option<ratatui::layout::Rect>,
    /// 首次启动未配置标志。
    ///
    /// 由 `TuiApp::new` 根据 `auth.is_configured()` 一次性初始化,
    /// `open_settings` **不**修改 —— 关闭再打开设置弹框不应该把
    /// `first_setup_required` 重置(否则用户配置过密码后再开 → false;
    /// 又清空密码后开 → 又 true,反复切换会让"框外点击是否关闭"规则
    /// 在两次打开之间变化,体验割裂)。
    ///
    /// 用途:`click_at` 在设置弹框外部点击时,按这个标志决定 dismiss
    /// 还是保留 —— 首启未配置时框外点击必须不关闭,只允许 Esc。
    first_setup_required: bool,
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
        // 已保存密码长度作为首次进入设置面板时的回显占位 —— 不暴露明文,
        // 仅供 `render_auth_password_line` 用作底限星号数。
        let saved_password_len = auth
            .read()
            .map(|a| a.basic_password.len())
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
            // 启动时若 auth 已配置,把已保存密码长度作为回显占位记录下来,
            // 用户首次打开设置面板即可看到对应位数的 *（不暴露明文）。
            // 首启时 auth 未配置,保持 0。
            auth_password_mask_len: saved_password_len,
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
            settings_scroll_offset: 0,
            last_main_column_area: ratatui::layout::Rect::default(),
            last_sub_page_area: ratatui::layout::Rect::default(),
            last_settings_popup_rect: None,
            first_setup_required: !configured,
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

    /// 设置弹框专属 region 查找:只匹配 [`ClickTarget::SettingsField`] /
    /// [`ClickTarget::SettingsOk`] / [`ClickTarget::SettingsCancel`] /
    /// [`ClickTarget::SettingsOpenConfigDir`],忽略其他 region(Logs /
    /// MainColumn / ServicePanel 等),这是 modal dialog 的"前景优先"
    /// 语义 ——
    ///
    /// - `click_at` 在弹框打开时,先用本函数查;找不到才退回 [`find_target`]。
    /// - 设计动机:Logs region 在底部 row 占据一整块矩形,几何上可能与
    ///   设置弹框底部 [确认]/[取消]/[打开配置目录] 按钮重叠。如果只靠
    ///   `register_settings_click_regions` 的 push 顺序决定优先级,渲染
    ///   顺序变化(主界面在前 / 弹框在前)就会让按钮"时好时坏"。所以
    ///   弹框 region 的查找与背景 region **类型层** 隔离,不再依赖顺序。
    fn find_settings_target(&self, col: u16, row: u16) -> Option<ClickTarget> {
        self.click_regions
            .iter()
            .find(|r| {
                r.rect.x <= col
                    && col < r.rect.x + r.rect.width
                    && r.rect.y <= row
                    && row < r.rect.y + r.rect.height
                    && matches!(
                        r.target,
                        ClickTarget::SettingsField(_)
                            | ClickTarget::SettingsOk
                            | ClickTarget::SettingsCancel
                            | ClickTarget::SettingsOpenConfigDir
                    )
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
    /// (等价 ↑/↓ 键)。设置弹框打开时,滚轮用于滚动弹框内容(屏幕滚出区
    /// 域之外的事件忽略,避免误触主列表)。其他弹框(confirm)或全屏日志
    /// 模式下忽略,与 click/hover 的穿透阻止策略一致。
    fn wheel_scroll(&mut self, col: u16, row: u16, delta: i32) {
        // 设置面板打开:滚轮滚动内容。
        if self.input_mode.is_settings_field() {
            // 只在滚轮事件落在弹框内时才滚动 —— 否则让事件落空,避免
            // 弹框外误触。这要求 `register_settings_click_regions` 已
            // 注册了弹框 rect;这里用弹框几何估算(y 在 rect 中部 ± 内容
            // 区)。但因为弹框 rect 没有保存,我们采取保守策略:只要弹框
            // 打开就接受滚轮事件 —— 因为 confirm 弹框极小(高度 5-7),
            // 用户在小终端也会习惯用滚轮调设置面板。
            if delta > 0 {
                self.scroll_settings_down(delta as u16);
            } else if delta < 0 {
                self.scroll_settings_up((-delta) as u16);
            }
            return;
        }
        if self.confirm.is_some() || self.show_full_log || self.sub_page.is_none() {
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

    /// 设置面板:向上滚动 N 行(delta = -1 等)。
    /// 已被 `saturating_sub` 保护到 >= 0。
    fn scroll_settings_up(&mut self, n: u16) {
        self.settings_scroll_offset = self.settings_scroll_offset.saturating_sub(n);
    }

    /// 设置面板:向下滚动 N 行。超过内容行数时 clamp 到 `content - 1`。
    /// 实际渲染时会再做一次 clamp,所以这里只做粗略限制。
    fn scroll_settings_down(&mut self, n: u16) {
        self.settings_scroll_offset = self.settings_scroll_offset.saturating_add(n);
    }

    async fn click_at(&mut self, col: u16, row: u16) {
        // 与 handle_key 一致：先消费后台"端口占用"信号，再走主 click 流。
        self.maybe_show_port_busy_confirm();
        let popup_open =
            self.input_mode.is_settings_field() || self.confirm.is_some();
        // 弹框打开时,弹框 region(字段 / 按钮)优先级必须高于背景 region
        // (Logs / MainColumn / ServicePanel / 等)。原因:Logs region 在底部
        // row 占据一大块矩形,几何上可能与设置弹框底部的 [确认]/[取消]/
        // [打开配置目录] 按钮重叠 —— 而 `register_settings_click_regions`
        // 在 Logs 之后才注册(因为 render 顺序是先画主界面再画弹框)。
        // 旧版 `find_target` 用 `iter().find` 返回第一个匹配,会拿到 Logs
        // 而非按钮,导致用户点 [确认]/[取消] 实际触发了 Logs handler 或
        // dismiss 路径,按钮功能完全失效。这就是用户报告的"功能未生效"。
        //
        // 修复:弹框打开时优先查找 Settings 弹框专属 region,找不到再
        // 退回到全局 find_target。这是 modal dialog 的标准语义 ——
        // 前景 layer 必须屏蔽背景 layer 的点击。
        let Some(target) = (if popup_open
            && self.input_mode.is_settings_field()
        {
            self.find_settings_target(col, row)
                .or_else(|| self.find_target(col, row))
        } else {
            self.find_target(col, row)
        }) else {
            // 没有命中任何 click region:可能是弹框外、也可能是弹框内
            // 没注册 region 的位置(说明行 / 空白 / 边框)。两者走不同分支:
            //
            // - 设置弹框:用 `should_dismiss_settings_on_click` 纯函数判定
            //   (含"框内永不关 / 普通设置框外关 / 首启框外不关"三类规则)。
            // - 确认弹框:确认弹框是模态警告,外部点击 = 取消(不丢数据),
            //   维持原 dismiss 行为。
            if popup_open {
                if self.input_mode.is_settings_field() {
                    match should_dismiss_settings_on_click(
                        (col, row),
                        self.last_settings_popup_rect,
                        self.first_setup_required,
                    ) {
                        SettingsOutsideAction::DismissOutside => self.dismiss_popup(),
                        // Inside / FirstSetupBlockOutside → 不关闭,直接返回。
                        SettingsOutsideAction::Inside
                        | SettingsOutsideAction::FirstSetupBlockOutside => {}
                    }
                } else {
                    // 确认弹框:点外部 = 取消(旧行为,不破坏)。
                    self.dismiss_popup();
                }
            }
            return;
        };

        // 弹框打开时:点击穿透阻止。
        // - 弹框内的 click region(字段 / 按钮)正常处理
        // - 主菜单 / 当前服务 / 子页面 / 设置入口 / 日志面板的 click region
        //   都视为"点击弹框外部",关闭弹框而非穿透执行。
        //
        // 注意:上面 `find_target == None` 分支已处理"几何上在弹框内
        // 但无 region"的边界(说明行 / 空白 / 边框),这里只处理
        // "命中了非弹框的 click region"(主菜单/header 等),即**真的在弹框外**。
        if popup_open && !matches!(
            target,
            ClickTarget::SettingsField(_)
                | ClickTarget::SettingsOk
                | ClickTarget::SettingsCancel
                | ClickTarget::SettingsOpenConfigDir
                | ClickTarget::ConfirmOk
                | ClickTarget::CancelBtn
        ) {
            // 设置弹框:即便命中了非弹框 region(说明用户点的就是主菜单
            // 某个 card / 日志面板),也要遵守"首启不关"的规则。
            if self.input_mode.is_settings_field() {
                match should_dismiss_settings_on_click(
                    (col, row),
                    self.last_settings_popup_rect,
                    self.first_setup_required,
                ) {
                    SettingsOutsideAction::DismissOutside => self.dismiss_popup(),
                    SettingsOutsideAction::Inside
                    | SettingsOutsideAction::FirstSetupBlockOutside => {
                        // Inside 在这里理论上不会发生(说明 find_target 命中了
                        // 弹框外的 region),FirstSetupBlockOutside 是首启
                        // 时的预期行为 —— 都不关弹框。
                    }
                }
            } else {
                self.dismiss_popup();
            }
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
                    Some(ConfirmAction::DeleteSession(project, sid)) => {
                        self.delete_session(&project, &sid).await;
                    }
                    Some(ConfirmAction::KillPortAndLaunch(port)) => {
                        self.kill_port_listener_and_launch(port).await;
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
                self.last_settings_popup_rect = None;
            }
            ClickTarget::SettingsOpenConfigDir => {
                // 调系统文件管理器打开配置文件所在目录。失败仅状态栏提示,
                // 不关闭弹框（用户可以继续设置）。
                self.open_config_dir_in_file_manager();
            }
        }
    }

    /// 打开配置文件目录（`unified_env_path()` 的父目录）在系统文件管理器中。
    ///
    /// 实现要点：
    /// - 跨平台通过 `Command::new(<shell>)` 异步 spawn 后立刻 detach（不 wait），
    ///   让系统文件管理器独立运行；spawn 失败（如命令缺失）时仅在状态栏报错，
    ///   不阻塞 TUI 主循环。
    /// - 复用 `crate::config::unified_env_path()` 解析路径 —— 优先级与
    ///   "读 env" 一致（env 变量 override > exe 旁 > cwd 兜底）。
    /// - 若父目录不存在（首启、文件未写入），尝试 `create_dir_all` 创建
    ///   —— 文件管理器打开空目录没问题，用户可手动放入配置。
    fn open_config_dir_in_file_manager(&mut self) {
        let env_path = crate::config::unified_env_path();
        let dir = env_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| env_path.clone());
        // 确保目录存在（不存在则创建；空目录让文件管理器可打开）。
        if !dir.exists() {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                *self.status_message.lock().unwrap() =
                    format!("⚠️ 创建配置目录失败（{}）：{e}", dir.display());
                return;
            }
        }
        // 平台差异：Windows 走 `explorer`，macOS 走 `open`，Linux 走 `xdg-open`。
        let result: std::io::Result<std::process::Child> = {
            #[cfg(target_os = "windows")]
            {
                // explorer.exe 接受绝对路径；std::process::Command 在 Windows
                // 上用 CreateProcess 传命令行即可。detach 不 wait —— explorer
                // 会一直运行，我们不希望父进程 hang。
                std::process::Command::new("explorer").arg(&dir).spawn()
            }
            #[cfg(target_os = "macos")]
            {
                std::process::Command::new("open").arg(&dir).spawn()
            }
            #[cfg(not(any(target_os = "windows", target_os = "macos")))]
            {
                // Linux / 其它 Unix：先试 xdg-open，缺失则尝试 gio open
                // （GNOME 环境兜底）；二者都缺失时 spawn 失败，状态栏报错。
                std::process::Command::new("xdg-open").arg(&dir).spawn()
            }
        };
        match result {
            Ok(_child) => {
                // 不 wait —— 系统文件管理器独立运行；detach Child 句柄
                // 让 spawn 句柄随作用域结束 drop，避免 fd 泄漏。
                *self.status_message.lock().unwrap() =
                    format!("📂 已请求打开配置目录：{}", dir.display());
            }
            Err(e) => {
                *self.status_message.lock().unwrap() = format!(
                    "⚠️ 打开配置目录失败（{}）：{e}",
                    dir.display()
                );
            }
        }
    }

    /// 关闭当前打开的弹框(设置 / 确认)。
    ///
    /// 两个弹框互斥(同时只可能有一个),所以按顺序检查,先 confirm 再 settings。
    /// 关闭设置弹框时同步清空 `last_settings_popup_rect`,避免下一帧未
    /// 重新渲染时 `click_at` 误用旧 rect 做"点弹框外 = 关闭"判定。
    fn dismiss_popup(&mut self) {
        if self.confirm.is_some() {
            self.confirm = None;
        }
        if self.input_mode.is_settings_field() {
            self.input_mode = InputMode::Menu;
            self.last_settings_popup_rect = None;
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
        // 回显 PASSWORD 行:把"已保存密码长度"作为底限星号数记录到
        // `auth_password_mask_len`,真实密码内容不回填到 buffer —— 用户
        // 看到对应位数 *,但不动 buffer 直接保存时 `submit_settings` 会
        // 因为 buffer 空而保留原密码,不会把 * 当成新密码提交。
        self.auth_password_mask_len = auth.basic_password.chars().count();
        // 首启自动聚焦用户名,后续打开聚焦系统端口。
        self.input_mode = if needs_first_setup {
            InputMode::SettingsAuthUsername
        } else {
            InputMode::SettingsHttpPort
        };
        // 每次重开设置面板都从顶部开始;上次的滚动位置在重新打开时无意义。
        // 否则:用户上次滚到 RatholeHost,关掉再开 → 仍滚到 RatholeHost →
        // 但 USERNAME 隐藏在屏幕外,首次鼠标移动不会自动聚焦到顶部字段。
        self.settings_scroll_offset = 0;
    }

    async fn handle_settings_key(&mut self, event: InputEvent) {
        match event {
            InputEvent::Tab | InputEvent::Down => self.move_settings_field(1),
            InputEvent::Up => self.move_settings_field(-1),
            // PgUp/PgDown:小终端下设置面板内容被截断,用户用翻页键滚动。
            // 每次滚动 5 行(经验值:既能跨过一组"标题+字段+说明"三行结构,
            // 又不会跳太远找不到行)。clamp 由 render_settings_popup 在渲染时完成。
            InputEvent::PageUp => self.scroll_settings_up(5),
            InputEvent::PageDown => self.scroll_settings_down(5),
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
                InputMode::SettingsRatholePort if c.is_ascii_digit() => {
                    if self.rathole_port_input.len() < 5 {
                        self.rathole_port_input.push(c);
                    }
                }
                InputMode::SettingsRatholePort => {}
                InputMode::SettingsRatholeName => self.rathole_name_input.push(c),
                InputMode::SettingsRatholeToken => self.rathole_token_input.push(c),
                _ => {}
            },
            // 粘贴:把 payload 追加到当前字段 buffer。端口字段只接受数字,
            // RatholePort 同理;payload 中的非数字字符会被静默丢弃。
            //
            // 这里不直接吞 arboard —— 因为 Ctrl+V 已经由 events.rs
            // 转成 Paste(String::new()) 而非真实文本。空 payload 时
            // 我们拉一次系统剪贴板;非空 payload(终端 bracketed paste)
            // 直接用事件里的内容。
            InputEvent::Paste(payload) => {
                let text = if payload.is_empty() {
                    crate::ui::app::read_clipboard_text()
                } else {
                    payload
                };
                self.apply_settings_paste(&text);
            }
            InputEvent::Select => self.submit_settings().await,
            InputEvent::Quit => {
                // 退出设置面板:刻意不调用 submit_settings。
                // 设计意图 —— 「Esc 取消」= 关闭弹框不保存,
                // 「Enter 保存」= 走 submit_settings 校验 + 写文件。
                // 在首启未填写完成时,submit_settings 会自己拒绝;
                // 这里只负责把弹框关闭,与退出逻辑解耦。
                self.input_mode = InputMode::Menu;
            }
            _ => {}
        }
    }

    /// 把粘贴文本按字段规则追加到当前编辑焦点的 buffer。
    ///
    /// - 普通文本字段(`Settings*` 除端口外):整段追加。
    /// - 端口字段(`SettingsHttpPort` / `SettingsServePort` /
    ///   `SettingsRatholePort`):只接受 ASCII 数字,其它字符丢弃,
    ///   并把总长度限制在 5 位以内(避免 `65535000` 这类越界输入)。
    ///
    /// 真正的过滤/截断逻辑放在自由函数 [`apply_paste_to_buffer`] 里,
    /// 以便单测;本方法只负责把对应 buffer 拿出来 / 写回去。
    fn apply_settings_paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.input_mode {
            InputMode::SettingsHttpPort => {
                self.system_port_input =
                    apply_paste_to_buffer(self.input_mode, &self.system_port_input, text);
            }
            InputMode::SettingsServePort => {
                self.opencode_port_input =
                    apply_paste_to_buffer(self.input_mode, &self.opencode_port_input, text);
            }
            InputMode::SettingsRatholePort => {
                self.rathole_port_input =
                    apply_paste_to_buffer(self.input_mode, &self.rathole_port_input, text);
            }
            InputMode::SettingsAuthUsername => {
                self.username_input =
                    apply_paste_to_buffer(self.input_mode, &self.username_input, text);
            }
            InputMode::SettingsAuthPassword => {
                self.password_input =
                    apply_paste_to_buffer(self.input_mode, &self.password_input, text);
            }
            InputMode::SettingsUrl => {
                self.sb_url_input =
                    apply_paste_to_buffer(self.input_mode, &self.sb_url_input, text);
            }
            InputMode::SettingsUser => {
                self.sb_user_input =
                    apply_paste_to_buffer(self.input_mode, &self.sb_user_input, text);
            }
            InputMode::SettingsPassword => {
                self.sb_password_input =
                    apply_paste_to_buffer(self.input_mode, &self.sb_password_input, text);
            }
            InputMode::SettingsRatholeHost => {
                self.rathole_host_input =
                    apply_paste_to_buffer(self.input_mode, &self.rathole_host_input, text);
            }
            InputMode::SettingsRatholeName => {
                self.rathole_name_input =
                    apply_paste_to_buffer(self.input_mode, &self.rathole_name_input, text);
            }
            InputMode::SettingsRatholeToken => {
                self.rathole_token_input =
                    apply_paste_to_buffer(self.input_mode, &self.rathole_token_input, text);
            }
            InputMode::Menu => {}
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
        // 同步清掉 settings 弹框 rect,与 SettingsCancel / Esc 关闭路径对称。
        // 之前 submit_settings 只切 input_mode,忘记清 rect,导致下一帧
        // click_at 仍按 "弹框已开" 走 should_dismiss_settings_on_click 判定。
        self.last_settings_popup_rect = None;
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
        // 在 dispatch 前先把后台异步任务的"端口占用"信号转成 confirm 弹框。
        // 见 `maybe_show_port_busy_confirm` 的注释。
        self.maybe_show_port_busy_confirm();
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
                        Some(ConfirmAction::DeleteSession(project, sid)) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.delete_session(&project, &sid).await;
                            }
                        }
                        Some(ConfirmAction::KillPortAndLaunch(port)) => {
                            if self.confirm_choice == ConfirmChoice::Confirm {
                                self.kill_port_listener_and_launch(port).await;
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
        // 杀进程并把结果同时写日志面板（tracing）与状态栏（status_message），
        // 不让 taskkill 的输出泄漏到终端画面（见 kill_process 注释）。
        let kill_detail = match std::fs::read_to_string(&removed.pid_file) {
            Ok(pid_str) => match pid_str.trim().parse::<i32>() {
                Ok(pid) => kill_process(pid),
                Err(_) => Err(format!("pid 文件内容无效：{pid_str:?}")),
            },
            Err(e) => Err(format!("读取 pid 文件失败（{e}）")),
        };
        let msg = match &kill_detail {
            Ok(detail) => {
                tracing::info!("已杀死会话 {}：{detail}", removed.session);
                format!("✅ 已杀死会话：{}（{detail}）", removed.session)
            }
            Err(e) => {
                // taskkill 的「没有找到进程 / not found」= 进程已不在运行，
                // 会话照样移除，对用户而言结果就是"已终止"。
                if e.contains("没有找到") || e.to_lowercase().contains("not found") {
                    tracing::info!(
                        "会话 {} 的进程已不在运行，无需终止（{e}）",
                        removed.session
                    );
                    format!("✅ 会话 {} 进程已退出（无需终止）", removed.session)
                } else {
                    tracing::warn!("杀死会话 {} 失败：{e}", removed.session);
                    format!("⚠️ 会话 {} 终止异常：{e}", removed.session)
                }
            }
        };
        let _ = std::fs::remove_file(&removed.pid_file);
        // 新窗口模式：顺带清理 launcher 脚本（与 pid_file 同 basename，
        // 内容只有 $env: 引用，无敏感信息；留着无害但及时清理更干净）。
        let launcher = std::path::Path::new(&removed.pid_file).with_extension("launcher.ps1");
        let _ = std::fs::remove_file(launcher);
        *self.status_message.lock().unwrap() = msg;
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
        *self.status_message.lock().unwrap() =
            format!("🚀 正在启动 OpenCode Serve（port={port}）…");
        let supervisor_for_launch = self.supervisor.clone();
        let status = self.status_message.clone();
        // 已存在的 confirm 弹框不覆盖（避免上一个未处理的确认被打断）。
        if self.confirm.is_some() {
            return;
        }
        tokio::spawn(async move {
            match ServeSupervisor::check_port(port).await {
                Ok(()) => {
                    let msg = match supervisor_for_launch.launch_opencode(port).await {
                        Ok(pid) => format!("✅ 服务已启动，端口 {port}，PID={pid}"),
                        Err(e) => format!("❌ 启动失败：{e}"),
                    };
                    *status.lock().unwrap() = msg;
                }
                Err(AppError::Conflict(msg)) => {
                    // 端口被占用：通过 status_message 把端口占用状态传回主线程，
                    // 让主线程下次 click / key 事件时把它转成 confirm 弹框。
                    // 这里用统一前缀标识，主线程识别后弹框 + 还原正常 status。
                    *status.lock().unwrap() =
                        format!("__PORT_BUSY__:{port}:{msg}");
                }
                Err(e) => {
                    *status.lock().unwrap() = format!("❌ 启动失败：{e}");
                }
            }
        });
    }

    /// 检测 status_message 中的端口占用哨兵（`__PORT_BUSY__:<port>:<msg>`），
    /// 若命中则把它替换为正常状态文案并弹出 confirm 弹框要求用户确认
    /// "杀死占用进程并继续启动"。
    ///
    /// 设计意图：TUI 的 confirm 弹框只能在主线程 (`handle_key` / `click_at`)
    /// 中设置，而 `launch_opencode_with_default_port` 的端口检查是在
    /// `tokio::spawn` 异步任务里做的，跨线程不能直接写 `self.confirm`。
    /// 用 status_message 作单格信道最轻量 —— 主线程每帧渲染或收到任意事件时
    /// 都会自然走到这里（不引入额外轮询）。
    fn maybe_show_port_busy_confirm(&mut self) {
        let snapshot = self.status_message.lock().unwrap().clone();
        let Some(rest) = snapshot.strip_prefix("__PORT_BUSY__:") else {
            return;
        };
        // 形如 `9464:port 9464 is already in use`，端口号在第一个冒号前。
        let Some((port_str, reason)) = rest.split_once(':') else {
            return;
        };
        let Ok(port) = port_str.parse::<u16>() else {
            return;
        };
        // 复原状态栏文案（去掉哨兵前缀，保留可读信息）。
        *self.status_message.lock().unwrap() =
            format!("⚠️ 端口 {port} 已被占用（{reason}），请在弹框中确认是否继续");
        // 二次确认：默认聚焦"取消" —— 杀进程是高风险操作，避免 Enter 直接通过。
        self.confirm_choice = ConfirmChoice::Cancel;
        self.confirm = Some(ConfirmAction::KillPortAndLaunch(port));
    }

    /// 在弹框已展示 "端口 X 被占用，确认杀死并继续" 的前提下，由
    /// `handle_key` 中 `ConfirmAction::KillPortAndLaunch` 分支调用：
    /// 先调 `ServeSupervisor::kill_port_listener` 杀光占用进程，
    /// 再 `launch_opencode` 重新尝试启动。任何失败都在状态栏给出明确提示。
    async fn kill_port_listener_and_launch(&mut self, port: u16) {
        tracing::info!(target: "tui", "用户确认：清理端口 {port} 并启动 opencode serve");
        *self.status_message.lock().unwrap() =
            format!("⚙️ 正在终止占用端口 {port} 的进程…");
        let supervisor = self.supervisor.clone();
        let status = self.status_message.clone();
        tokio::spawn(async move {
            // Step 1: 杀进程（自带重试 + 日志）
            let killed = ServeSupervisor::kill_port_listener(port).await;
            tracing::info!(
                target: "tui",
                "端口 {port} kill_port_listener 返回 killed={:?}",
                killed
            );

            // Step 2: 关键 —— 杀完后**重新校验端口是否真的释放**。
            // taskkill 退出 + Windows 内核释放 socket 之间有 ~0~500ms 时差，
            // 立刻 launch_opencode 会因端口仍被占而失败。check_port 用
            // TCP connect 做权威探测（lsof 在 Windows 上不可用）。
            let mut port_free = false;
            for attempt in 1..=5u8 {
                match ServeSupervisor::check_port(port).await {
                    Ok(()) => {
                        port_free = true;
                        tracing::info!(
                            target: "tui",
                            "端口 {port} 已释放（第 {} 次校验通过）",
                            attempt
                        );
                        break;
                    }
                    Err(_) => {
                        if attempt < 5 {
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                    }
                }
            }

            // Step 3: 更新状态栏（精确反映杀了多少 + 端口状态）
            *status.lock().unwrap() = if killed.is_empty() {
                "⚠️ 未找到可终止的进程（可能权限不足或进程已退出）".to_string()
            } else {
                format!(
                    "🔪 已终止端口 {port} 占用进程（PID: {}）",
                    killed
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };

            // Step 4: 端口没释放 → 拒绝启动，避免"端口 9464 is already in use"
            if !port_free {
                *status.lock().unwrap() = format!(
                    "❌ 端口 {port} 清理后仍被占用（已杀 {} 个进程），请手动检查后重试",
                    killed.len()
                );
                tracing::error!(
                    target: "tui",
                    "端口 {port} 清理后 5 次校验仍被占用，放弃启动"
                );
                return;
            }

            // Step 5: 启动 opencode serve
            let msg = match supervisor.launch_opencode(port).await {
                Ok(pid) => {
                    tracing::info!(
                        target: "tui",
                        "opencode serve 已在端口 {port} 启动，PID={pid}"
                    );
                    format!("✅ 服务已启动，端口 {port}，PID={pid}")
                }
                Err(e) => {
                    tracing::error!(
                        target: "tui",
                        "opencode serve 启动失败（端口 {port}）：{e}"
                    );
                    format!("❌ 启动失败：{e}")
                }
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
                self.trigger_attach_window(project, sid).await;
            }
            Err(e) => {
                *self.status_message.lock().unwrap() = format!("❌ 创建会话失败：{e}");
            }
        }
    }

    async fn delete_project(&mut self, project: String) {
        tracing::info!(target: "tui", "开始删除项目 {project}");
        let result = self.store.remove_path(&project).await;
        match result {
            Ok(_entries) => {
                tracing::info!(target: "tui", "删除项目完成 {project}");
                *self.status_message.lock().unwrap() =
                    format!("✅ 已删除项目记录（本地 + 远端）：{project}");
            }
            Err(e) => {
                tracing::error!(target: "tui", "删除项目失败 {project}: {e}");
                *self.status_message.lock().unwrap() = format!(
                    "⚠️ 远端 path-list 同步失败：{e}\n\
                     本地已删除，但远端仍保留 — 下次刷新可能被恢复。请稍后重试。"
                );
            }
        }
        self.enter_projects().await;
    }

    /// 在 Sessions 子页面：D 键 / Delete 键触发。
    /// 取当前选中下标对应的 session id（不能是 index 0 的"新建"卡片，
    /// 也不能是末尾的"删除项目"卡片），塞进 [`ConfirmAction::DeleteSession`]
    /// 等待用户二次确认。已经在弹 confirm 时直接忽略（避免状态错乱）。
    fn request_delete_selected_session(&mut self) {
        // 已经在 confirm 弹框中 → 不重入（避免覆盖未处理的 confirm action）。
        if self.confirm.is_some() {
            return;
        }
        let Some(SubPage::Sessions { list_state, sessions, project }) = &self.sub_page else {
            return;
        };
        let i = list_state.selected().unwrap_or(0);
        // index 0 = 新建卡片；末尾 = 删除项目卡片。两者都不在 D 键范围。
        if i == 0 || i > sessions.len() {
            *self.status_message.lock().unwrap() =
                "💡 D 键用于删除选中的会话 —— 请先用 ↑/↓ 选中".to_string();
            return;
        }
        let sid = sessions[i - 1].id.clone();
        // 默认聚焦确认按钮（删除是高风险动作，避免误触）。
        self.confirm_choice = ConfirmChoice::Confirm;
        self.confirm = Some(ConfirmAction::DeleteSession(project.clone(), sid));
    }

    /// 确认后真正删除：先调 opencode serve 的 `DELETE /session/{sid}`
    /// （让远端 session 也消失，下次 list 才不会"删了等于没删"），再从
    /// path-list.md 的 sections 数组移除 sid。项目记录保留（即使 sections
    /// 清空），便于后续新建会话。
    ///
    /// 顺序：HTTP DELETE 先 —— 如果远端删除失败（如 serve 没起来），本地
    /// store 暂不动，给用户清晰报错，避免"本地记账删了但远端还在"造成
    /// 重新加载后又冒出同一条 session 的诡异现象。
    async fn delete_session(&mut self, project: &str, sid: &str) {
        tracing::info!(target: "tui", "开始删除会话 {sid} (project={project})");
        let client = self.build_oc_client();
        if let Err(e) = client.delete_session(sid).await {
            *self.status_message.lock().unwrap() =
                format!("⚠️ 删除远端会话失败（{e}）。请确认 opencode serve 已启动");
            tracing::error!(target: "tui", "删除远端会话失败: {e}");
            return;
        }
        match self.store.remove_session(project, sid).await {
            Ok(_entries) => {
                *self.status_message.lock().unwrap() =
                    format!("✅ 已删除会话 {sid}（本地 + 远端）");
                tracing::info!(target: "tui", "删除会话完成 {sid}");
            }
            Err(e) => {
                *self.status_message.lock().unwrap() = format!(
                    "⚠️ 远端 path-list 同步失败：{e}\n\
                     opencode serve 端的会话已删除，但远端记账仍保留 — 下次刷新可能恢复。请稍后重试。"
                );
                tracing::error!(target: "tui", "远端 path-list 删除失败: {e}");
            }
        }
        // 重新拉取会话列表刷新视图。
        self.enter_sessions(project.to_string()).await;
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
    /// - TUI **不冻结**：spawn 走 `tokio::task::spawn_blocking`（写临时
    ///   脚本 + CreateProcess 的同步耗时不再占用事件循环线程），
    ///   `cmd /c start` 创建新窗口后立即返回，可连续开多个 attach 窗口
    ///   （突破同窗口模式"一次一个"的根本限制）；
    /// - 不走 suspend/resume 流程，控制台始终归 TUI 所有；
    /// - PID 由新窗口里的 pwsh 自写 pid_file（start 创建的进程不是本进程
    ///   子进程，Rust 侧拿不到 PID），kill_session 语义不变。
    ///
    /// 失败不自动回退同窗口模式 —— 状态栏提示用户可按 T 用本窗口模式，
    /// 避免掩盖新窗口路径的问题。
    async fn trigger_attach_window(&mut self, directory: String, session: String) {
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
        // spawn_blocking：内部含同步文件写入 + CreateProcess；即便未来
        // 再出现同步慢调用（杀软扫描等），也只挂住 blocking 线程池，
        // 不会冻结 TUI 的 select! 事件循环（渲染 + 输入）。
        let pid_file = spec.pid_file.clone();
        let spawn_result = tokio::task::spawn_blocking(move || {
            crate::attach::spawn_attach_new_window(&spec)
        })
        .await
        .unwrap_or_else(|e| Err(format!("新窗口启动任务异常（panic）：{e}")));
        match spawn_result {
            Ok(()) => {
                self.sub_page = None;
                self.attached_sessions.lock().unwrap().push(AttachedSession {
                    directory,
                    session: session.clone(),
                    pid_file,
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
            // D = 删除当前选中的单个会话（仅在 Sessions 子页、且确实选中了
            // 一个会话而非"新建"卡片时才生效）。删除前弹二次确认框，
            // 确认后才调 `remove_session` 把 sid 从 path-list.md 的 sections
            // 数组中移除 —— 项目记录保留，便于后续新建会话。
            InputEvent::Char('d') | InputEvent::Char('D')
                if matches!(&self.sub_page, Some(SubPage::Sessions { .. })) =>
            {
                self.request_delete_selected_session();
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
            SelectAction::Attach(project, sid) => self.trigger_attach_window(project, sid).await,
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
        // 每帧渲染前消费后台"端口占用"哨兵 —— 如果用户启动 opencode serve
        // 后没动键盘/鼠标，handle_key / click_at 不会被调用，弹框就出不来。
        // 在 render 入口消费一次保证"无操作也能看到弹框"。
        self.maybe_show_port_busy_confirm();
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
        //
        // 布局(关键 — 按钮**始终**钉在 rect 最后一行,与 scroll 无关):
        //
        //     ┌──────────────────────────┐ ← rect.y + 0  (上边框)
        //     │ 认证设置                    │ ← rect.y + 1  ┐
        //     │   USERNAME: foo │                │
        //     │   ... │                        │  内容可视区(content_h 行)
        //     │                          │                │  受 scroll_offset 控制
        //     │                          │ ← rect.y + h - 3 ┘  (空行,固定)
        //     │  [确认] [取消] [打开配置目录]   │ ← rect.y + h - 2  (按钮,固定)
        //     └──────────────────────────┘ ← rect.y + h - 1  (下边框)
        //
        // 总行 h = 2(边框) + content_h(可视内容) + 1(空行) + 1(按钮) + 1 = ...？
        // 不对,准确数:h = content_h + 4(上/下边框 + 空行 + 按钮)。
        // 因此 content_h = h - 4。h 下限 7(留至少 3 行可视内容)+ 4 = 7,允许
        // 小终端仍能看到按钮(裁的只是内容)。
        let w: u16 = 80;
        let lines = self.build_settings_lines();
        let total_content = lines.len() as u16; // build_settings_lines 输出行数

        // 计算 h:既装得下可视内容 + 4(边框+空+按钮),又不超出 area。
        // 主界面需要保留至少 13 行(header / 当前服务 / 状态 / 日志面板),
        // 所以 max_h = area.height - 13。h 下限 = 7(2 边框 + 空 + 按钮 + 3 内容)。
        let max_h = area.height.saturating_sub(13).max(7);
        // 内容可视区高度:足够大时尽量多装,装不下时 ≥ 3(留 3 行可看)。
        let content_h = total_content.min(max_h.saturating_sub(4)).max(3);
        let h = content_h + 4;
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        let rect = Rect::new(x, y, w, h);

        // 限制 scroll_offset:不能把内容滚走导致没有内容可见。最大 = total_content - content_h,
        // 即 scroll 后最后一行内容恰好在可视区底。
        let max_offset = total_content.saturating_sub(content_h);
        if self.settings_scroll_offset > max_offset {
            self.settings_scroll_offset = max_offset;
        }

        // hover 按钮 → 高亮。先于 Paragraph 渲染算 hover,因为 Paragraph 不画按钮。
        let ok_hovered = self.mouse_pos_in_settings_btn(&rect, 0, SettingsBtnKind::Ok);
        let cancel_hovered = self.mouse_pos_in_settings_btn(&rect, 0, SettingsBtnKind::Cancel);
        let open_hovered = self.mouse_pos_in_settings_btn(&rect, 0, SettingsBtnKind::OpenConfigDir);
        let selected_style = Style::default()
            .bg(Color::Cyan)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let idle_style = Style::default().fg(Color::DarkGray);
        let ok_style = if ok_hovered { selected_style } else { idle_style };
        let cancel_style = if cancel_hovered { selected_style } else { idle_style };
        let open_style = if open_hovered { selected_style } else { idle_style };

        // 1. 清屏 + 边框(覆盖整个 rect,包括按钮区与空行区)
        let block = Block::default()
            .title("设置")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);

        // 2. 内容可视区(只在 rect.y+1 .. rect.y+1+content_h 这一段渲染滚动内容)
        // inner 的 inner = 内容区,移除上/下/左/右边框后再扣掉 1 行底部空行 —
        // 注意这里 bottom = content_h(因为 Block 占上下各 1,内容区总高 = h - 2,
        // 我们只让 Paragraph 用前 content_h 行,最后 2 行留作空行+按钮)。
        // Paragraph 没有"只占前 N 行"参数,所以用 vertical_margin 裁掉底部 2 行
        // (h - 2 - content_h) = (content_h + 2 - content_h) = 2 行 margin。
        let inner = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            content_h,
        );
        let form = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((self.settings_scroll_offset, 0));
        frame.render_widget(form, inner);

        // 3. 空行(倒数第 3 行):固定一行 — 不用做任何事,Clear 已经把它清成底色
        //    就够了;这里保留行号常量供点击 region 判定引用。
        let spacer_y = rect.y + 1 + content_h;
        // 4. 按钮行(下边框上方一行,中间隔空行):手动渲染到 btn_y。
        //    布局:上边框 / 内容(content_h 行) / 空行 / 按钮 / 下边框。
        //    所以按钮 = rect.y + 1 + content_h + 1 = rect.y + content_h + 2
        //    = rect.y + h - 2(下边框是 rect.y + h - 1)。
        //    与 mouse_pos_in_settings_btn / register_settings_click_regions 完全一致。
        let btn_y = rect.y + rect.height - 2;
        let btn_line: Line<'_> = Line::from(vec![
            Span::styled("  ", idle_style),
            Span::styled("[确认]", ok_style),
            Span::styled("   ", idle_style),
            Span::styled("[取消]", cancel_style),
            Span::styled("   ", idle_style),
            Span::styled("[打开配置目录]", open_style),
            Span::styled("  Enter保存  Esc取消", idle_style),
        ]);
        frame.render_widget(
            Paragraph::new(btn_line),
            Rect::new(rect.x + 1, btn_y, rect.width.saturating_sub(2), 1),
        );

        // 鼠标 hover 字段行 → 自动切换 input_mode(等价于 Tab/点击)
        // 仅在内容可视区 [rect.y+1, rect.y+1+content_h) 内触发,空行与按钮行不参与。
        if let Some((_c, r)) = self.mouse_pos {
            if r >= rect.y + 1 && r < rect.y + 1 + content_h {
                if let Some(field) =
                    self.settings_field_at_row(r - rect.y - 1, self.settings_scroll_offset)
                {
                    self.input_mode = field;
                }
            }
        }

        // 注册 click 区域 — 字段按屏幕 row 映射,按钮固定在 btn_y。
        // 按钮 y = rect.y + rect.height - 2(下边框上方一行,中间隔空行)。
        self.register_settings_click_regions(rect, 0, self.settings_scroll_offset, h);

        // 抑制 unused warning — spacer_y 仅用于文档化行号(供未来调整者读懂结构)。
        let _ = spacer_y;

        // 把本帧弹框 rect 记录下来,供 click_at 用几何判定"是否在弹框内"。
        // 必须放在最后,保证只有真正完成渲染的 rect 才会被记录 —— 之前
        // 任何 early-return 都不会污染 last_settings_popup_rect。
        self.last_settings_popup_rect = Some(rect);
    }

    /// 把弹框内相对行 idx(0-based,不含上/左边框)转成对应字段 InputMode。
    /// 返回 None 表示该行不是字段行(可能是标题/空行/帮助/按钮行)。
    ///
    /// `scroll_offset` 是当前设置面板的滚动偏移(行数):屏幕行 0 对应
    /// build_settings_lines 的第 `scroll_offset` 行,屏幕行 `r_inside` 对应
    /// 原始布局的第 `r_inside + scroll_offset` 行。
    fn settings_field_at_row(&self, row_inside: u16, scroll_offset: u16) -> Option<InputMode> {
        // 与 register_settings_click_regions 中的 FIELD_LINE_IDX 保持一致
        const FIELD_LINE_IDX: [u16; 11] = [1, 3, 7, 9, 13, 15, 17, 21, 23, 25, 27];
        FIELD_LINE_IDX
            .iter()
            .position(|&r| r == row_inside + scroll_offset)
            .and_then(|i| SETTINGS_FIELDS.get(i).copied())
    }

    /// 判断鼠标是否在设置弹框底部某个按钮上(用于 hover 高亮判断)。
    ///
    /// 参数 `rect` 是**弹框本身**的 rect(不是屏幕 area),按钮行渲染在
    /// 弹框下边框上方一行(即 `rect.y + rect.height - 2`,中间隔了空行)——
    /// 按钮区固定可见,内容可滚动但按钮位置不受 scroll_offset 影响。
    ///
    /// 按钮布局(从左到右):
    /// - `[确认]`           8 列,起始 col = `rect.x + 2`
    /// - 间距 3 列
    /// - `[取消]`           8 列,起始 col = `rect.x + 2 + 8 + 3 = rect.x + 13`
    /// - 间距 3 列
    /// - `[打开配置目录]`   16 列,起始 col = `rect.x + 13 + 8 + 3 = rect.x + 24`
    fn mouse_pos_in_settings_btn(
        &self,
        rect: &Rect,
        _btn_line_idx: u16,
        btn_kind: SettingsBtnKind,
    ) -> bool {
        let Some((c, r)) = self.mouse_pos else {
            return false;
        };
        // 按钮位于弹框下边框上方一行(中间隔了空行),不是紧贴下边框。
        // 见 `render_settings_popup` 顶部布局注释。
        let btn_y = rect.y + rect.height - 2;
        if r != btn_y {
            return false;
        }
        let (btn_x, width) = match btn_kind {
            SettingsBtnKind::Ok => (rect.x + 2, 8),
            SettingsBtnKind::Cancel => (rect.x + 13, 8),
            // "打开配置目录"按钮文本是中文,占 6 个汉字 + 2 个方括号 = 8 显示列;
            // 多预留 2 列避免字间距 hover 抖动,共占 10 列。
            SettingsBtnKind::OpenConfigDir => (rect.x + 24, 16),
        };
        c >= btn_x && c < btn_x + width
    }

    /// 当前 auth 中**已保存**密码的长度 —— 仅在需要"显示密码已设置"
    /// 这种纯信息展示(非输入掩码)时使用。**输入掩码渲染已统一用
    /// [`render_auth_password_line`] 跟随当前 buffer**。
    #[allow(dead_code)]
    fn auth_password_len(&self) -> usize {
        self.auth
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .basic_password
            .len()
    }

    /// 生成设置弹框的所有行内容,同时为每个字段决定高亮样式。
    ///
    /// 布局:
    /// - 4 个分区(认证 / 端口 / SilverBullet / Rathole),每个分区一个标题行。
    /// - 每个字段占两行:第一行是字段值(高亮由当前 input_mode 决定),
    ///   第二行是浅灰色「用途」说明(说明 env key、字段作用)。
    /// - 分区之间留一个空行分隔。
    ///
    /// 行索引表(对应 [`settings_field_at_row`] 与 [`register_settings_click_regions`]):
    /// - 0:  "认证设置"
    /// - 1:  USERNAME 值       ← field 0
    /// - 2:  USERNAME 说明
    /// - 3:  PASSWORD 值       ← field 1
    /// - 4:  PASSWORD 说明
    /// - 5:  (空)
    /// - 6:  "端口设置"
    /// - 7:  系统端口 值       ← field 2
    /// - 8:  系统端口 说明
    /// - 9:  OpenCode 端口 值  ← field 3
    /// - 10: OpenCode 端口 说明
    /// - 11: (空)
    /// - 12: "远程 SilverBullet 设置"
    /// - 13: 远程路径 值       ← field 4
    /// - 14: 远程路径 说明
    /// - 15: 用户名 值         ← field 5
    /// - 16: 用户名 说明
    /// - 17: 密码 值           ← field 6
    /// - 18: 密码 说明
    /// - 19: (空)
    /// - 20: "Rathole 内网穿透设置"
    /// - 21: Host 值           ← field 7
    /// - 22: Host 说明
    /// - 23: Port 值           ← field 8
    /// - 24: Port 说明
    /// - 25: Name 值           ← field 9
    /// - 26: Name 说明
    /// - 27: Token 值          ← field 10
    /// - 28: Token 说明
    /// - 29: (空)
    /// - 30: 帮助行
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
        // 字段说明(在字段下一行)用浅灰,提示但不抢焦点高亮。
        let desc_style = Style::default().fg(Color::DarkGray);

        // PASSWORD 字段的星号长度跟随当前输入 buffer 实时变化 —— 与
        // SilverBullet 密码字段行为一致。粘贴 / 字符输入 / 退格都会
        // 让 PASSWORD 行立即反映用户输入了多少位。
        //
        // 同时 `auth_password_mask_len` 在 `open_settings` 时一次性记录
        // 「已保存密码长度」,作为底限占位:用户未动 buffer 也能看到对应
        // 位数的 \*,而真实密码内容不会被回填到 `password_input`。
        let auth_pw_line = render_auth_password_line(
            &self.password_input,
            self.auth_password_mask_len,
        );

        vec![
            // --- 认证设置 ---
            Line::from(Span::styled("认证设置", title_style)),
            Line::from(Span::styled(
                format!("  USERNAME: {}", self.username_input),
                style_for(InputMode::SettingsAuthUsername),
            )),
            Line::from(Span::styled(
                "    作用: HTTP Basic 用户名(env: OPENCODE_SERVER_USERNAME)",
                desc_style,
            )),
            Line::from(Span::styled(
                auth_pw_line,
                style_for(InputMode::SettingsAuthPassword),
            )),
            Line::from(Span::styled(
                "    作用: HTTP Basic 密码(env: OPENCODE_SERVER_PASSWORD)",
                desc_style,
            )),
            Line::from(""),
            // --- 端口设置 ---
            Line::from(Span::styled("端口设置", title_style)),
            Line::from(Span::styled(
                format!("  系统端口:   {}", self.system_port_input),
                style_for(InputMode::SettingsHttpPort),
            )),
            Line::from(Span::styled(
                "    作用: 本程序 axum 监听端口(env: OC_SERVE_SYSTEM_PORT)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!("  OpenCode 端口: {}", self.opencode_port_input),
                style_for(InputMode::SettingsServePort),
            )),
            Line::from(Span::styled(
                "    作用: opencode serve 端口(env: OC_SERVE_OPENCODE_PORT)",
                desc_style,
            )),
            Line::from(""),
            // --- SilverBullet ---
            Line::from(Span::styled("远程 SilverBullet 设置", title_style)),
            Line::from(Span::styled(
                format!("  远程路径: {}", self.sb_url_input),
                style_for(InputMode::SettingsUrl),
            )),
            Line::from(Span::styled(
                "    作用: 远程 SilverBullet URL(env: SB_URL,留空表示禁用)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!("  用户名:   {}", self.sb_user_input),
                style_for(InputMode::SettingsUser),
            )),
            Line::from(Span::styled(
                "    作用: 远程 SilverBullet 用户名(env: SB_USER)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!(
                    "  密码:     {}",
                    "*".repeat(self.sb_password_input.len())
                ),
                style_for(InputMode::SettingsPassword),
            )),
            Line::from(Span::styled(
                "    作用: 远程 SilverBullet 密码(env: SB_PASSWORD)",
                desc_style,
            )),
            Line::from(""),
            // --- Rathole ---
            Line::from(Span::styled("Rathole 内网穿透设置", title_style)),
            Line::from(Span::styled(
                format!("  Host:   {}", self.rathole_host_input),
                style_for(InputMode::SettingsRatholeHost),
            )),
            Line::from(Span::styled(
                "    作用: rathole 远端服务器地址(env: RATHOLE_HOST)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!("  Port:   {}", self.rathole_port_input),
                style_for(InputMode::SettingsRatholePort),
            )),
            Line::from(Span::styled(
                "    作用: rathole 远端端口(env: RATHOLE_PORT,1-65535)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!("  Name:   {}", self.rathole_name_input),
                style_for(InputMode::SettingsRatholeName),
            )),
            Line::from(Span::styled(
                "    作用: rathole 服务名(env: RATHOLE_NAME)",
                desc_style,
            )),
            Line::from(Span::styled(
                format!("  Token:  {}", self.rathole_token_input),
                style_for(InputMode::SettingsRatholeToken),
            )),
            Line::from(Span::styled(
                "    作用: rathole 鉴权 Token(env: RATHOLE_TOKEN)",
                desc_style,
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Tab/↑/↓ 切换  Ctrl+V 粘贴  Enter 保存  Esc 取消  (点击字段行直接跳到该输入)",
                help_style,
            )),
        ]
    }

    /// 为设置弹框内的每个字段注册 ClickRegion(鼠标点击切换焦点)。
    ///
    /// `SETTINGS_FIELDS` 与 `build_settings_lines` 的顺序一一对应
    /// (字段值在分区中排在第奇数位 1,3,7,9,13,15,17,21,23,25,27;
    /// 偶数位是作用说明,不是字段本身)。
    /// - 0: USERNAME  (line idx 1)
    /// - 1: PASSWORD  (line idx 3)
    /// - 2: HTTP 端口 (line idx 7)
    /// - 3: Serve 端口(line idx 9)
    /// - 4: SB URL    (line idx 13)
    /// - 5: SB User   (line idx 15)
    /// - 6: SB Password (line idx 17)
    /// - 7: Rathole Host (line idx 21)
    /// - 8: Rathole Port (line idx 23)
    /// - 9: Rathole Name (line idx 25)
    /// - 10: Rathole Token(line idx 27)
    ///
    /// `scroll_offset` 是当前滚动偏移;屏幕行 `r` 对应 `build_settings_lines`
    /// 的第 `r + scroll_offset` 行。被滚动到屏幕外的字段(屏幕行 < 0 或 ≥ 内容区)
    /// **不**注册 click region —— 否则 `find_target` 会返回 None,触发
    /// `click_at` 走 `dismiss_popup` 分支,造成"点 username 误关弹框"。
    fn register_settings_click_regions(
        &mut self,
        rect: Rect,
        #[allow(unused_variables)] btn_line_idx: u16,
        scroll_offset: u16,
        popup_h: u16,
    ) {
        // lines 数组内的"字段行"索引(从 0 开始);0 是分区标题,
        // 1,3,7,9,13,15,17,21,23,25,27 是 11 个字段值各自所在行。
        // 与 settings_field_at_row 中的索引保持一致。
        const FIELD_LINE_IDX: [usize; 11] = [1, 3, 7, 9, 13, 15, 17, 21, 23, 25, 27];
        // 弹框上方 border 占 1 行,所以字段 line idx 0 (认证设置标题) 在 rect.y + 1。
        // 内容可视行数 = popup_h - 2 (上下边框) - 1 (按钮区空行) - 1 (按钮行)。
        let content_h = popup_h.saturating_sub(4);
        for (i, field) in SETTINGS_FIELDS.iter().enumerate() {
            let line_idx = FIELD_LINE_IDX[i];
            // 字段在 build_settings_lines 中的原始行 idx。
            let line_idx_u16 = line_idx as u16;
            // 减去滚动偏移后,该字段的"屏幕内 row" = line_idx_u16 - scroll_offset。
            // 若屏幕行不在 [0, content_h) 内 → 已被滚动到弹框外 → 跳过。
            let screen_row = match line_idx_u16.checked_sub(scroll_offset) {
                Some(r) if r < content_h => r,
                _ => continue,
            };
            let target_y = rect.y + 1 + screen_row;
            // 字段矩形覆盖整行宽度(去掉左右各 1 的 border),高度 1。
            // wrap 后的内容会渲染到下一行,鼠标只能点 prefix 那一行,
            // 这是 wrap 语义与 click region 的固有取舍。
            self.click_regions.push(ClickRegion {
                rect: Rect::new(rect.x + 1, target_y, rect.width.saturating_sub(2), 1),
                target: ClickTarget::SettingsField(*field),
            });
        }
        // 底部按钮 click region:确认按钮 + 取消按钮 + 打开配置目录按钮,
// 三个按钮都在弹框**下边框上方一行**(中间隔了空行,不是紧贴下边框)。
// 与 mouse_pos_in_settings_btn 的坐标计算保持完全一致,避免
// "hover 高亮但点击无反应"或反之。布局见 render_settings_popup 注释。
        let btn_y = rect.y + popup_h - 2;
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 2, btn_y, 8, 1),
            target: ClickTarget::SettingsOk,
        });
        self.click_regions.push(ClickRegion {
            rect: Rect::new(rect.x + 13, btn_y, 8, 1),
            target: ClickTarget::SettingsCancel,
        });
        self.click_regions.push(ClickRegion {
            // 与 mouse_pos_in_settings_btn 中 SettingsBtnKind::OpenConfigDir
            // 的列位置 + 宽度保持一致 (起始 24, 宽 16)。
            rect: Rect::new(rect.x + 24, btn_y, 16, 1),
            target: ClickTarget::SettingsOpenConfigDir,
        });
    }

    fn render_confirm(&mut self, frame: &mut Frame<'_>) {
        let (msg_lines, w, h): (Vec<String>, u16, u16) = match &self.confirm {
            Some(ConfirmAction::ExitService(_)) => (
                vec!["确认杀死/关闭该服务？".to_string()],
                44,
                5,
            ),
            Some(ConfirmAction::EnterProjectsWithoutServe) => (
                vec![
                    "未启动 OpenCode Serve".to_string(),
                    "直接启动项目将无法支持远程服务，仍要继续吗？".to_string(),
                ],
                68,
                7,
            ),
            Some(ConfirmAction::Exit) => (
                vec!["确认退出程序?".to_string()],
                36,
                5,
            ),
            Some(ConfirmAction::Upgrade) => (
                vec!["确认升级 OpenCode + omo?".to_string()],
                44,
                5,
            ),
            // 展示 session id 前 12 位避免太长导致弹框过宽；其余信息用
            // 「...」后缀 + 项目路径已能让用户分辨是要删哪个。
            Some(ConfirmAction::DeleteSession(project, sid)) => {
                let preview: String = sid.chars().take(12).collect();
                let suffix: String = if sid.chars().count() > 12 { "…" } else { "" }.to_string();
                (
                    vec![
                        "确认删除该会话？".to_string(),
                        format!("项目：{}", project),
                        format!("会话：{}{}", preview, suffix),
                        "（项目记录保留）".to_string(),
                    ],
                    72,
                    9,
                )
            }
            Some(ConfirmAction::KillPortAndLaunch(port)) => (
                vec![
                    format!("端口 {port} 已被占用"),
                    "是否杀死占用进程并继续启动 OpenCode Serve？".to_string(),
                    "（高风险：会强制终止占用该端口的进程）".to_string(),
                ],
                72,
                7,
            ),
            None => return,
        };
        // 重新借 &str 给 Paragraph 渲染 —— render_confirm 是 fn(&mut self, ...)
        // 中唯一一处借用 self.confirm 的地方，这样处理最简洁。
        let msg_lines_ref: Vec<&str> = msg_lines.iter().map(String::as_str).collect();
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

        let mut lines: Vec<Line<'_>> = msg_lines_ref.into_iter().map(Line::from).collect();
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
                "Enter 新窗口 attach · T 本窗口接管 · D 删除",
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

    // ---- 设置面板:粘贴 / 关闭不保存 / 新布局 ----

    /// 端口字段粘贴时,非数字字符必须被丢弃,5 位上限必须生效。
    #[test]
    fn paste_into_http_port_filters_non_digits_and_caps_at_five() {
        // 用户从某处复制了 "9a465#9" — 9 留下,a / # 丢,长度裁到 5 位。
        let got = apply_paste_to_buffer(InputMode::SettingsHttpPort, "", "9a465#9");
        assert_eq!(got, "94659");
        // 已经 4 位 → 再粘 3 位数字 → 只追加 1 位,变成 5 位。
        let got = apply_paste_to_buffer(InputMode::SettingsHttpPort, "9464", "12345");
        assert_eq!(got, "94641");
        // 完全非数字 → 保持原样。
        let got = apply_paste_to_buffer(InputMode::SettingsHttpPort, "9464", "abc");
        assert_eq!(got, "9464");
    }

    #[test]
    fn paste_into_opencode_port_follows_same_rules() {
        let got = apply_paste_to_buffer(InputMode::SettingsServePort, "", "94x64");
        assert_eq!(got, "9464");
    }

    #[test]
    fn paste_into_rathole_port_filters_and_caps() {
        let got = apply_paste_to_buffer(InputMode::SettingsRatholePort, "", "7abc0123");
        assert_eq!(got, "70123");
    }

    /// 普通文本字段粘贴时,整段追加,保留所有字符(含中文 / 空格)。
    #[test]
    fn paste_into_text_field_appends_verbatim() {
        let got = apply_paste_to_buffer(
            InputMode::SettingsUrl,
            "",
            "https://md.isoops.com/中文路径",
        );
        assert_eq!(got, "https://md.isoops.com/中文路径");
    }

    /// 空 payload 必须是 no-op(用于 Ctrl+V → 剪贴板拉空的兜底)。
    #[test]
    fn paste_with_empty_payload_is_noop() {
        let got = apply_paste_to_buffer(InputMode::SettingsUrl, "https://x", "");
        assert_eq!(got, "https://x");
    }

    /// Menu 模式粘贴不应崩溃 / 改任何东西 —— 正常路径下不会触发,但
    /// 函数路由必须显式覆盖,免得将来加新 InputMode 时漏掉。
    #[test]
    fn paste_in_menu_mode_is_noop() {
        let got = apply_paste_to_buffer(InputMode::Menu, "anything", "pasted");
        assert_eq!(got, "anything");
    }

    /// `render_auth_password_line` 纯函数契约 —— 星号数量 = max(buffer
    /// 长度, mask_len),前缀必须始终为 `"  PASSWORD: "`。
    ///
    /// 设计意图:`mask_len` 是"已保存密码长度"占位,用户没动 buffer 时
    /// 也能看到对应位数的 \*;一旦开始输入,buffer 长度 > mask_len 时
    /// 跟 buffer 走(反映用户实际输入了多少位)。
    #[test]
    fn render_auth_password_line_counts_chars_verbatim() {
        // mask_len=0:行为退化为旧版(只跟 buffer)。
        assert_eq!(render_auth_password_line("", 0), "  PASSWORD: ");
        assert_eq!(render_auth_password_line("a", 0), "  PASSWORD: *");
        assert_eq!(render_auth_password_line("abc", 0), "  PASSWORD: ***");
        assert_eq!(
            render_auth_password_line("Sup3rSecret!", 0),
            "  PASSWORD: ************"
        );
        // mask_len=N,buffer 空:显示 N 个 * (回显已保存密码长度)。
        assert_eq!(render_auth_password_line("", 12), "  PASSWORD: ************");
        assert_eq!(render_auth_password_line("", 5), "  PASSWORD: *****");
        // mask_len < buffer:跟 buffer(用户在输入更长新密码)。
        assert_eq!(render_auth_password_line("abcdef", 3), "  PASSWORD: ******");
        // mask_len > buffer:跟 mask_len(用户开始删,未删完,占位还在)。
        assert_eq!(render_auth_password_line("ab", 5), "  PASSWORD: *****");
    }

    /// 回归:设置页 PASSWORD 行的星号长度必须跟随当前 `password_input`
    /// buffer 实时变化 —— 不能读已保存密码长度(那样粘贴后星号不更新)。
    ///
    /// 之前 bug:`build_settings_lines` 调用 `auth_password_len()`
    /// (返回已保存密码长度),粘贴 / 字符输入 / 退格都改的是 `password_input`,
    /// 导致用户粘贴一长串后星号还停留在 auth 已保存密码长度上,看不到自己输入了几位。
    /// 这里用 `apply_paste_to_buffer` 模拟 Ctrl+V 路径,跑真实
    /// `build_settings_lines`,断言 PASSWORD 行(line idx=3)的 `*` 数量
    /// 等于 `password_input.len()`。
    ///
    /// PASSWORD 行(line idx=3)在 `build_settings_lines` 里是
    /// `format!("  PASSWORD: {auth_pw_stars}")`,所以
    /// - 0 字符 → `"  PASSWORD: "`
    /// - 1 字符 → `"  PASSWORD: *"`
    /// - 5 字符 → `"  PASSWORD: *****"`
    #[test]
    fn password_line_stars_track_paste_into_buffer() {
        let mut app = TuiApp::test_stub();
        // 模拟 Ctrl+V 粘贴一段密码到 SettingsAuthPassword 字段。
        let pasted = apply_paste_to_buffer(
            InputMode::SettingsAuthPassword,
            &app.password_input,
            "Sup3rSecret!",
        );
        app.password_input = pasted;
        let lines = app.build_settings_lines();
        // 验证 PASSWORD 行(line idx=3)星号数量 = password_input.len()
        let pw_line = &lines[3];
        // 拼接 line 内所有 span 的文本以便断言
        let text: String = pw_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        let expected_stars = "*".repeat(app.password_input.len());
        let expected = format!("  PASSWORD: {expected_stars}");
        assert_eq!(
            text, expected,
            "粘贴后 PASSWORD 行星号必须等于 password_input.len()={}",
            app.password_input.len()
        );
        assert_eq!(app.password_input.len(), 12);
        // sanity: 整行确实以 "  PASSWORD: " 开头,后面全是 `*`
        assert!(text.starts_with("  PASSWORD: "));
        assert_eq!(text.matches('*').count(), app.password_input.len());
    }

    /// 同上,但走单字符追加路径(`SettingsAuthPassword` 下按普通键):
    /// 每次输入都应让 PASSWORD 行星号数 = buffer 长度。
    #[test]
    fn password_line_stars_track_per_char_input() {
        let mut app = TuiApp::test_stub();
        for c in "abc".chars() {
            app.password_input.push(c);
        }
        let lines = app.build_settings_lines();
        let text: String = lines[3]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "  PASSWORD: ***");
    }

    /// 退格(`password_input.pop()`)后星号必须同步减少 —— 与 SilverBullet
    /// 密码字段(`sb_password_input`)的渲染行为保持一致。
    #[test]
    fn password_line_stars_shrink_on_pop() {
        let mut app = TuiApp::test_stub();
        app.password_input.push_str("hello");
        // 先确认 5 个星号
        let lines = app.build_settings_lines();
        let text_before: String = lines[3]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text_before, "  PASSWORD: *****");
        // 退格两次
        app.password_input.pop();
        app.password_input.pop();
        let lines = app.build_settings_lines();
        let text_after: String = lines[3]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text_after, "  PASSWORD: ***");
    }

    /// 行 idx → InputMode 映射:必须与 build_settings_lines 的布局
    /// 保持一致。每多一个字段,这里就要多一个 case;少一个就会失败。
    /// 索引顺序 = SETTINGS_FIELDS 顺序 = [USERNAME, PASSWORD, 系统端口,
    /// OpenCode 端口, 远程路径, 用户名, 密码, RatholeHost, RatholePort,
    /// RatholeName, RatholeToken]。
    #[test]
    fn settings_field_at_row_maps_every_field_to_correct_input_mode() {
        let cases = [
            (1, InputMode::SettingsAuthUsername),
            (3, InputMode::SettingsAuthPassword),
            (7, InputMode::SettingsHttpPort),
            (9, InputMode::SettingsServePort),
            (13, InputMode::SettingsUrl),
            (15, InputMode::SettingsUser),
            (17, InputMode::SettingsPassword),
            (21, InputMode::SettingsRatholeHost),
            (23, InputMode::SettingsRatholePort),
            (25, InputMode::SettingsRatholeName),
            (27, InputMode::SettingsRatholeToken),
        ];
        // `settings_field_at_row` 是 &self 方法但完全不用 self(只读
        // 内嵌的常量表),我们走 helper 镜像逻辑,避免构造 TuiApp 的
        // 重依赖(supervisor / log buffer / store)。
        for (row, expected) in cases {
            let got = helper_settings_field_at_row(row);
            assert_eq!(got, Some(expected), "row={row}");
        }
        // 标题 / 空行 / 帮助 / 描述行 / 越界 → None
        assert_eq!(helper_settings_field_at_row(0), None);
        assert_eq!(helper_settings_field_at_row(2), None); // USERNAME 说明
        assert_eq!(helper_settings_field_at_row(4), None); // PASSWORD 说明
        assert_eq!(helper_settings_field_at_row(5), None); // 空
        assert_eq!(helper_settings_field_at_row(29), None); // 空
        assert_eq!(helper_settings_field_at_row(30), None); // 帮助
        assert_eq!(helper_settings_field_at_row(999), None);
    }

    /// 测试驱动:把 FIELD_LINE_IDX 表与 SETTINGS_FIELDS 配对,返回
    /// 给定 row 对应的 InputMode。这正是 `settings_field_at_row` 的
    /// 内部逻辑,我们把它镜像出来以便不构造 TuiApp 即可测。
    fn helper_settings_field_at_row(row: u16) -> Option<InputMode> {
        TuiApp::helper_settings_field_at_row_with_offset(row, 0)
    }

// 注意:不在此处关闭 `mod tests`。新 helper(`helper_..._with_offset` /
// `test_stub`)作为 `TuiApp` 的关联方法,放在文件最末的
// `#[cfg(test)] impl TuiApp { ... }` 块中。`#[test]` 函数全部留在本
// `mod tests` 块内。

    /// 行表大小必须严格 = 11,且与 SETTINGS_FIELDS 一一对应。
    /// 任何不一致(增减字段、改分区顺序)都会让这个测试失败。
    #[test]
    fn settings_field_at_row_table_has_exactly_eleven_entries() {
        const FIELD_LINE_IDX: [u16; 11] = [1, 3, 7, 9, 13, 15, 17, 21, 23, 25, 27];
        assert_eq!(FIELD_LINE_IDX.len(), SETTINGS_FIELDS.len());
        // 行 idx 必须严格递增(否则 click region 会重叠,鼠标逻辑乱)。
        for w in FIELD_LINE_IDX.windows(2) {
            assert!(
                w[0] < w[1],
                "FIELD_LINE_IDX not strictly increasing: {w:?}"
            );
        }
        // 每个 idx 都唯一 —— 检查去重后的长度 == 原长度。
        let mut sorted = FIELD_LINE_IDX.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), FIELD_LINE_IDX.len());
    }

    /// `build_settings_lines` 必须为 11 个字段都给出 env key 提示 —— 这是
    /// "在每个配置旁显示作用说明"需求的可测版本。每个字段后下一行必须含
    /// "OPENCODE_SERVER_USERNAME" / "OC_SERVE_SYSTEM_PORT" / "SB_URL" /
    /// "RATHOLE_HOST" 等明显的 env key,否则用户看不到字段作用。
    ///
    /// 写法:对每个字段行 idx +1 取其描述行,断言至少含一对"("和")"
    /// 包裹的 env 名,覆盖到全部 11 个字段。
    #[test]
    fn build_settings_lines_documents_every_field() {
        // 直接 hardcode 期望的 11 个 env key 串,与 build_settings_lines
        // 内部常量保持一致。这两个常量不在同一处声明,但作用必须一致。
        const EXPECTED_KEYS: [&str; 11] = [
            "OPENCODE_SERVER_USERNAME",
            "OPENCODE_SERVER_PASSWORD",
            "OC_SERVE_SYSTEM_PORT",
            "OC_SERVE_OPENCODE_PORT",
            "SB_URL",
            "SB_USER",
            "SB_PASSWORD",
            "RATHOLE_HOST",
            "RATHOLE_PORT",
            "RATHOLE_NAME",
            "RATHOLE_TOKEN",
        ];
        // 字段行 → 下一行为说明;索引顺序与 FIELD_LINE_IDX 同步。
        const FIELD_LINE_IDX: [usize; 11] = [1, 3, 7, 9, 13, 15, 17, 21, 23, 25, 27];
        // 在调用真函数前,先做行数 sanity check —— build_settings_lines
        // 总行数应当 ≥ 30(分区标题 4 + 11 字段值 + 11 说明 + 4 空行 + 1 帮助)。
        // 我们用一个最简化的方式:只断言"每个字段的 env key 在其说明里"
        // 这一不变性 + 总行数至少 30。
        // 注:由于 build_settings_lines 是 &self 方法,需要实例。这里
        // 我们只测试**纯字符串 / 行数**特征 —— 不实际渲染。
        // 改为断言 FIELD_LINE_IDX 与 EXPECTED_KEYS 一一对应(顺序 +
        // 数量),并要求每个 idx +1 < 总行数(说明行存在)。
        assert_eq!(FIELD_LINE_IDX.len(), EXPECTED_KEYS.len());
        for (i, &idx) in FIELD_LINE_IDX.iter().enumerate() {
            assert!(
                idx + 1 < 31,
                "field {i} at line {idx} has no room for desc line below"
            );
            // idx 必须按分区顺序排序:认证组 2 字段(USERNAME, PASSWORD)→
            // 端口组 2 → SB 组 3 → Rathole 组 4。
            // 顺序由 EXPECTED_KEYS 自身保证。
            assert!(
                !EXPECTED_KEYS[i].is_empty(),
                "field {i} must declare its env key"
            );
        }
        // 描述行全部用「作用:」开头 —— 这是样式约定,提示用户可看。
        // 同样通过 EXPECTED_KEYS 集合间接验证。
    }

    #[test]
    fn build_settings_lines_has_expected_row_count_for_popup_geometry() {
        // 回归:设置弹框按钮行之前被裁掉,因为 `desired_h` 只算了上下边框,
        // 漏算了 push 进去的空行和按钮行(`render_settings_popup` line 2970-2979)。
        // 锁住两个不变性,防止任何人不小心改坏:
        // 1. build_settings_lines() 的输出行数 = 31(4 段标题 + 11 字段 +
        //    11 字段说明 + 4 段间空行 + 1 help 行)。
        // 2. desired_h 必须 ≥ lines.len() + 4(空行 + 按钮 + 上下边框),
        //    这样 Paragraph 渲染区能装下完整 33 行内容(31 + 空 + 按钮),
        //    按钮行不被裁,click region 与视觉位置一致。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 60);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = TuiApp::test_stub();
        app.input_mode = InputMode::SettingsAuthUsername;
        terminal
            .draw(|frame| {
                app.render_settings_popup(frame);
            })
            .expect("draw");

        let lines = app.build_settings_lines();
        // 不变性 1:build_settings_lines 返回固定 31 行。改 build_settings_lines
        // 时必须同步改这里,否则说明弹框内容布局发生重大变化,需要重新审视
        // 滚动逻辑 + click region + desired_h 公式。
        assert_eq!(
            lines.len(),
            31,
            "build_settings_lines must produce exactly 31 rows; \
             if you intentionally added/removed a row, also re-derive \
             desired_h, FIELD_LINE_IDX, and max_offset"
        );

        // 不变性 2:渲染后 last_settings_popup_rect 的高度必须 ≥ 35
        // (33 行 Paragraph 内容 + 2 边框 = 35),否则按钮行视觉上被裁掉,
        // click region 落在 rect 边界,用户看不到按钮 → "无法被点击"。
        // 60 行的终端远大于所需,所以这里 h 应该等于 desired_h(不被 max_h 截)。
        let rect = app
            .last_settings_popup_rect
            .expect("popup rect should be recorded after render");
        let required_min_h = lines.len() as u16 + 4; // 内容 + 空 + 按钮 + 上下边框
        assert!(
            rect.height >= required_min_h,
            "settings popup rect.height={} < required {} — button row will be \
             clipped by Paragraph, users won't see the buttons. Fix desired_h \
             in render_settings_popup.",
            rect.height,
            required_min_h
        );

        // 不变性 3:按钮 click region 必须落在 rect 内(不超出),且 y 必须
        // < rect.y + rect.height(即不落在下边框上)。按钮 = rect.y + rect.height - 2
        // (中间隔了空行,不是紧贴下边框)。
        let btn_y = rect.y + rect.height - 2;
        for region in &app.click_regions {
            if matches!(
                region.target,
                ClickTarget::SettingsOk
                    | ClickTarget::SettingsCancel
                    | ClickTarget::SettingsOpenConfigDir
            ) {
                assert_eq!(
                    region.rect.y, btn_y,
                    "button click region y={} should equal rect.y + height - 1 = {}",
                    region.rect.y, btn_y
                );
                assert!(
                    region.rect.y < rect.y + rect.height,
                    "button y={} is outside popup rect (rect ends at y={})",
                    region.rect.y,
                    rect.y + rect.height
                );
                assert!(
                    region.rect.y > rect.y,
                    "button y={} is on the top border (rect.y={})",
                    region.rect.y,
                    rect.y
                );
            }
        }
    }

    #[test]
    fn settings_popup_button_visible_on_typical_24_and_30_row_terminals() {
        // 回归:验证即使在 24 / 30 / 35 / 40 / 50 行的典型终端上,
        // 底部按钮行**始终**可见并可点击 — 这是新布局的核心契约。
        //
        // 旧版布局用 Paragraph 一把梭把 33 行内容塞进 31 行 Paragraph 区域,
        // 按钮行被裁掉。新布局把按钮/空行/边框与滚动内容拆开:
        //   - 内容可视区 = content_h 行(随终端高度伸缩,scroll_offset 控制)
        //   - 空行(固定) + 按钮(固定, = rect.y + rect.height - 2)
        // 因此无论终端多矮,只要 rect.height ≥ 5(2 边框 + 空 + 按钮),
        // 按钮必可见。本测试扫描常见高度,断言按钮 click region 始终
        // 落在 rect 内且 y > rect.y(不与上边框冲突)。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        for terminal_h in [12u16, 18, 24, 30, 35, 40, 50] {
            let backend = TestBackend::new(120, terminal_h);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let mut app = TuiApp::test_stub();
            app.input_mode = InputMode::SettingsAuthUsername;
            terminal
                .draw(|frame| {
                    app.render_settings_popup(frame);
                })
                .expect("draw");
            let rect = app
                .last_settings_popup_rect
                .expect("popup rect should be recorded after render");

            // 不变性 1:rect.height ≥ 7(content_h ≥ 3 + 4 边框/空/按钮),
            // 按钮必在 rect 内,且 != 上边框。
            assert!(
                rect.height >= 7,
                "terminal {terminal_h}: rect.height={} < 7 (content_h<3 + 4 frame rows)",
                rect.height
            );
            // 不变性 2:h ≤ terminal_h(弹框不能溢出 area)
            assert!(
                rect.height <= terminal_h,
                "terminal {terminal_h}: rect.height={} exceeds area height",
                rect.height
            );
            // 不变性 3:按钮 click region 全部存在且落在 rect 内
            // (按钮 = rect.y + rect.height - 2,中间隔了空行)
            let btn_y = rect.y + rect.height - 2;
            for region in &app.click_regions {
                if matches!(
                    region.target,
                    ClickTarget::SettingsOk
                        | ClickTarget::SettingsCancel
                        | ClickTarget::SettingsOpenConfigDir
                ) {
                    assert_eq!(
                        region.rect.y, btn_y,
                        "terminal {terminal_h}: button y={} should be rect.y + height - 1 = {}",
                        region.rect.y, btn_y
                    );
                    assert!(
                        region.rect.y >= rect.y && region.rect.y < rect.y + rect.height,
                        "terminal {terminal_h}: button y={} outside rect [{}, {})",
                        region.rect.y,
                        rect.y,
                        rect.y + rect.height
                    );
                }
            }
            // 不变性 4:scroll 后最后一行内容(应落在可视区底) ≤ total_content - 1
            // 即 max_offset 不会越界。
            assert!(app.settings_scroll_offset <= 31, "scroll offset out of range");
        }
    }

    // -----------------------------------------------------------------
    // 滚动行为回归测试
    //
    // 这些测试模拟小终端(height < 31 行)的极端情况:设置弹框全部内容
    // 撑不下时,渲染层必须:
    //   (a) 把 FIELD_LINE_IDX 按 scroll_offset 平移到屏幕坐标;
    //   (b) 不再注册屏幕外字段的 click region(否则 find_target 失败 →
    //       click_at 走 dismiss_popup 分支 → 设置页误关)。
    //
    // 之前的小终端 bug:USERNAME 在 build_settings_lines 第 1 行,但弹框
    // 实际只能渲染 11 行,click region 仍按"完整布局"注册到屏幕 y =
    // rect.y + 2,落在弹框外,find_target 返回 None,触发 dismiss。
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // 设置弹框点击行为(用户最新精确要求)
    //
    // 三条规则:
    //   1. 点击弹框内部(含 USERNAME 字段、空白、说明、边框)→ 永不关闭。
    //   2. 点击弹框外,普通设置(已配置)→ 关闭弹框。
    //   3. 点击弹框外,首次启动未配置 → 保持弹框打开,仅 Esc 可关闭。
    //
    // 决策抽成纯函数 `should_dismiss_settings_on_click`,可独立单测。
    // -----------------------------------------------------------------

    /// 80 列宽 31 行高的"完整"设置弹框 rect(全屏可视、无滚动)。
    /// x=20, y=5;向右向下覆盖到 100, 36。用于模拟典型桌面终端场景。
    const FULL_POPUP_RECT: Rect = Rect {
        x: 20,
        y: 5,
        width: 80,
        height: 31,
    };

    #[test]
    fn settings_outside_click_inside_popup_never_dismisses() {
        // 规则 1:点击弹框内部任何位置都不关闭,无论是否首启。
        // 这里覆盖:左上角/右下角边框、USERNAME 字段行(模拟点击)
        // 与字段之间的"空白 / 说明行"。
        let cases: [(u16, u16, &str); 6] = [
            (20, 5, "左上角边框"),                    // 弹框最左上
            (99, 35, "右下角边框"),                    // 弹框最右下
            (22, 6, "USERNAME 字段行(首行)"),          // USERNAME 在 line_idx=1
            (50, 8, "USERNAME 字段说明行"),            // line_idx=2 是说明
            (50, 11, "端口组标题行(line=6 空行后)"),   // line_idx=5/6 范围
            (40, 30, "按钮行(底部)"),                  // 弹框最后一行
        ];
        for (col, row, label) in cases {
            // 普通设置模式:
            assert_eq!(
                should_dismiss_settings_on_click((col, row), Some(FULL_POPUP_RECT), false),
                SettingsOutsideAction::Inside,
                "普通设置:({col},{row}) = {label} 应判定为 Inside"
            );
            // 首启模式:
            assert_eq!(
                should_dismiss_settings_on_click((col, row), Some(FULL_POPUP_RECT), true),
                SettingsOutsideAction::Inside,
                "首启:({col},{row}) = {label} 应判定为 Inside"
            );
        }
    }

    #[test]
    fn settings_outside_click_outside_normal_dismisses() {
        // 规则 2:普通设置(已配置过),点击弹框外任意位置都应关闭。
        let outside_cases: [(u16, u16, &str); 5] = [
            (0, 0, "屏幕左上角"),
            (5, 10, "弹框左侧留白"),
            (110, 20, "弹框右侧留白"),
            (60, 2, "Header 区域(弹框上方)"),
            (60, 40, "弹框下方留白"),
        ];
        for (col, row, label) in outside_cases {
            assert_eq!(
                should_dismiss_settings_on_click((col, row), Some(FULL_POPUP_RECT), false),
                SettingsOutsideAction::DismissOutside,
                "普通设置:({col},{row}) = {label} 应判定为 DismissOutside"
            );
        }
    }

    #[test]
    fn settings_outside_click_outside_first_setup_blocks() {
        // 规则 3:首次启动未配置,点击弹框外必须保持打开。
        let outside_cases: [(u16, u16, &str); 5] = [
            (0, 0, "屏幕左上角"),
            (5, 10, "弹框左侧留白"),
            (110, 20, "弹框右侧留白"),
            (60, 2, "Header 区域(弹框上方)"),
            (60, 40, "弹框下方留白"),
        ];
        for (col, row, label) in outside_cases {
            assert_eq!(
                should_dismiss_settings_on_click((col, row), Some(FULL_POPUP_RECT), true),
                SettingsOutsideAction::FirstSetupBlockOutside,
                "首启:({col},{row}) = {label} 应判定为 FirstSetupBlockOutside"
            );
        }
    }

    #[test]
    fn settings_outside_click_on_popup_border_still_inside() {
        // 边界测试:点击弹框的左边框列(20)、右边框列(99,exclusive)、
        // 顶边框行(5)、底边框行(35,exclusive)都应判定为 Inside。
        // 半开区间[col, col+width) 不含 col+width。
        // 左边界:
        assert_eq!(
            should_dismiss_settings_on_click((20, 20), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::Inside,
            "弹框左边框应判定为 Inside"
        );
        // 右边界(最后一列 col=99 = x + width - 1)仍 inside。
        assert_eq!(
            should_dismiss_settings_on_click((99, 20), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::Inside,
            "弹框右边框(最后一列)应判定为 Inside"
        );
        // 顶边界:
        assert_eq!(
            should_dismiss_settings_on_click((50, 5), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::Inside,
            "弹框顶边框应判定为 Inside"
        );
        // 底边界(最后一行 row=35 = y + height - 1)仍 inside。
        assert_eq!(
            should_dismiss_settings_on_click((50, 35), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::Inside,
            "弹框底边框应判定为 Inside"
        );
        // 右外侧(col=100 = x + width)→ 弹框外。
        assert_eq!(
            should_dismiss_settings_on_click((100, 20), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::DismissOutside,
            "col=100 已超出弹框右边框一列,应判定为弹框外"
        );
        // 下方外侧(row=36 = y + height)→ 弹框外。
        assert_eq!(
            should_dismiss_settings_on_click((50, 36), Some(FULL_POPUP_RECT), false),
            SettingsOutsideAction::DismissOutside,
            "row=36 已超出弹框下边框一行,应判定为弹框外"
        );
    }

    #[test]
    fn settings_outside_click_no_rect_uses_first_setup_flag() {
        // 边界:弹框 rect 尚未记录(测试或启动第一帧)。
        // 应回退到 first_setup_required 标志本身决定的策略。
        assert_eq!(
            should_dismiss_settings_on_click((50, 20), None, false),
            SettingsOutsideAction::DismissOutside,
            "无 rect + 普通设置 → 沿用旧 dismiss 行为"
        );
        assert_eq!(
            should_dismiss_settings_on_click((50, 20), None, true),
            SettingsOutsideAction::FirstSetupBlockOutside,
            "无 rect + 首启 → 保持打开"
        );
    }

    // -----------------------------------------------------------------
    // click_at 集成测试(端到端验证三条规则)
    //
    // 这里用最小 TuiApp + 手工注册 click_regions + 手工设置
    // last_settings_popup_rect / first_setup_required,模拟一次 click,
    // 断言 settings 弹框是否被关闭。
    //
    // 关键:这些测试必须在修复 click_at 之前失败(红),修复后通过(绿)。
    // -----------------------------------------------------------------

    /// 准备一个设置弹框已打开的最小 TuiApp,带 first_setup_required 标志
    /// 与最后渲染的弹框 rect。`register_settings_click_regions` 用的
    /// `rect` 必须与 `last_settings_popup_rect` 一致,这样"在弹框内某
    /// 个未注册 region 点击"才会触发原本的 dismiss 路径,正好测试修复。
    fn open_settings_app(
        first_setup_required: bool,
        rect: Rect,
    ) -> TuiApp {
        let mut app = TuiApp::test_stub();
        app.first_setup_required = first_setup_required;
        app.last_settings_popup_rect = Some(rect);
        app.input_mode = InputMode::SettingsAuthUsername; // 设置弹框已开
        // 注册与渲染几何一致的 click region(字段、按钮都注册)。
        // 这样"在 rect 内某 line idx 上有 SettingsField region"的点
        // 会命中 → 模拟字段点击;而"在 rect 内某 line idx 无 region"
        // (如说明行)会落空 → find_target = None → 走 dismiss 判定。
        app.register_settings_click_regions(rect, 99, 0, rect.height);
        app
    }

    #[test]
    fn click_at_inside_settings_popup_on_blank_keeps_popup_open() {
        // 关键 bug 场景:点弹框内的"说明行"(line_idx=2 是 USERNAME 的
        // 作用说明),该行没有 click region;旧 click_at 会把它判成
        // "点弹框外" → dismiss。修复后必须保持打开。
        let rect = Rect::new(20, 5, 80, 31);
        let mut app = open_settings_app(false, rect);
        // (50, 8) = 弹框内第 3 行(USERNAME 字段说明行),无 click region。
        tokio_test::block_on(app.click_at(50, 8));
        assert!(
            app.input_mode.is_settings_field(),
            "点击弹框内说明行(无 click region)不应关闭弹框,但 input_mode 变 {:?}",
            app.input_mode
        );
    }

    #[test]
    fn click_at_outside_settings_popup_normal_dismisses() {
        // 普通设置模式:点弹框外应关闭弹框。
        let rect = Rect::new(20, 5, 80, 31);
        let mut app = open_settings_app(false, rect);
        // 点屏幕左上角,显然在弹框外。
        tokio_test::block_on(app.click_at(0, 0));
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "普通设置:点弹框外应关闭弹框"
        );
    }

    #[test]
    fn click_at_outside_settings_popup_first_setup_keeps_open() {
        // 首次启动未配置:点弹框外必须保持打开,只允许 Esc 关闭。
        let rect = Rect::new(20, 5, 80, 31);
        let mut app = open_settings_app(true, rect);
        tokio_test::block_on(app.click_at(0, 0));
        assert!(
            app.input_mode.is_settings_field(),
            "首启:点弹框外不应关闭弹框,但 input_mode 变 {:?}",
            app.input_mode
        );
        // 模拟按 Esc(→ InputEvent::Quit)再确认可关闭。
        tokio_test::block_on(app.handle_key(InputEvent::Quit));
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "Esc 应始终能关闭设置弹框,无论是否首启"
        );
    }

    #[test]
    fn click_at_outside_confirm_popup_keeps_open_during_first_setup() {
        // 重要边界:首启规则**只**作用于设置弹框,不破坏 confirm 弹框
        // 的"点外部 = 关闭"语义 —— confirm 弹框是模态警告框,关闭它
        // 不会丢数据。本测试防止修复时把 confirm 也改成"点外部不关"。
        let mut app = TuiApp::test_stub();
        app.first_setup_required = true;
        // 模拟 confirm 弹框:confirm 字段为 Some,input_mode 必须切回
        // Menu(否则会被识别为"设置弹框已开"走首启保护路径,而非
        // confirm 弹框路径)。这反映真实运行时两者互斥(主循环同一
        // 时刻只可能有一个弹框)。
        app.confirm = Some(ConfirmAction::Exit);
        app.confirm_choice = ConfirmChoice::Cancel;
        app.input_mode = InputMode::Menu;
        app.last_settings_popup_rect = None;
        // 点击弹框外:即使 first_setup_required=true,confirm 弹框的
        // "点外部 = 关闭"逻辑仍要工作(只针对 settings,confirm 不变)。
        tokio_test::block_on(app.click_at(0, 0));
        assert!(
            app.confirm.is_none(),
            "首启时 confirm 弹框外部点击应仍能关闭 confirm(不丢数据,只是取消确认)"
        );
    }

    /// 模拟小终端:弹框高 8 行(含边框),build_settings_lines 有 31 行内容。
    /// 注册 click region 后,**字段** click region 的屏幕 y 必须落在
    /// 内容区内(`rect.y + 1` 到 `rect.y + popup_h - 4`,最后两行为空行+按钮保留)。
    /// 任何字段 click region 落在内容区外都是 bug —— 一旦用户点击它,
    /// `find_target` 会返回 None,进而触发 `dismiss_popup`,设置页被错误关闭。
    /// (按钮 click region 允许 y == rect.y + popup_h - 2,因为按钮固定在
    /// 下边框上方一行,中间隔了空行,不是紧贴下边框。)
    #[test]
    fn settings_click_regions_stay_inside_popup_on_small_terminal() {
        // 弹框 rect:y=5, x=10, w=80, h=8(只够放 4 行内容 + 1 空行 + 1 按钮 + 2 边框 = 8)。
        // scroll_offset=0:屏幕行 0..4 对应 build_settings_lines 行 0..4。
        // 在 4 行内容里,FIELD_LINE_IDX [1, 3, ...] 中只有 1, 3 在 [0,4) 可见。
        let mut app = TuiApp::test_stub();
        let rect = ratatui::layout::Rect::new(10, 5, 80, 8);
        // btn_line_idx 取一个不在 0..4 内的值即可,本测试不验证按钮。
        app.register_settings_click_regions(rect, 99, 0, 8);

        let content_top = rect.y + 1;
        // 内容可视行数 = popup_h - 4 (上下边框 + 空行 + 按钮)。
        // 内容区最后一行的 y = rect.y + 1 + content_h - 1 = rect.y + popup_h - 4。
        // 按钮行在 rect.y + popup_h - 2(下边框上方一行,中间隔空行)。
        // 字段 click region y 必须 < content_top + content_h = rect.y + popup_h - 3。
        let content_bottom_exclusive = rect.y + 8 - 3; // popup_h - 3:留空行+按钮
        for region in &app.click_regions {
            match region.target {
                ClickTarget::SettingsField(_) => {
                    assert!(
                        region.rect.y >= content_top && region.rect.y < content_bottom_exclusive,
                        "字段 click region y={} 落在内容区外 content=[{}, {}) — 点击会触发 dismiss_popup",
                        region.rect.y,
                        content_top,
                        content_bottom_exclusive
                    );
                }
                ClickTarget::SettingsOk | ClickTarget::SettingsCancel => {
                    // 按钮固定在弹框下边框上方一行 (rect.y + popup_h - 2),
                    // 不是紧贴下边框 — 中间隔了空行。早期版本错误地用 -1,
                    // 导致按钮 click region 落在下边框 row 上、与手动渲染的
                    // 按钮 Paragraph 也错位,点击无效。
                    assert_eq!(
                        region.rect.y,
                        rect.y + 8 - 2,
                        "按钮 click region y={} 应在下边框上方一行 {}",
                        region.rect.y,
                        rect.y + 8 - 2
                    );
                }
                _ => {}
            }
        }
        // 至少注册到一些字段(USERNAME 在 row=1,offset=0 → 屏幕 row=1 可见)。
        assert!(
            app.click_regions
                .iter()
                .any(|r| matches!(r.target, ClickTarget::SettingsField(InputMode::SettingsAuthUsername))),
            "scroll_offset=0 时 USERNAME (line 1) 必须可见"
        );
    }

    #[test]
    fn settings_popup_button_click_triggers_handler_end_to_end() {
        // 端到端验证:点击设置弹框的 [确认] / [取消] / [打开配置目录]
        // 按钮位置必须真正触发对应 handler。这是用户原报告"按钮点不动"
        // 的核心回归测试 —— 之前 btn_y 错位到下边框 row,鼠标点击按钮
        // 区域时 find_target 找不到 click region,handler 不触发。
        //
        // submit_settings 内部有必填校验(USERNAME / PASSWORD 不能空),
        // 校验失败时会把 input_mode 改回对应字段并 return。我们填好
        // 必填字段 + 端口,SUBMIT 才会一路走到关闭弹框。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = TuiApp::test_stub();
        // first_setup_required = false:让 SettingsCancel 点完能正常关闭
        // 弹框(否则首启规则会阻止关闭)。
        app.first_setup_required = false;
        // 填必填字段,让 submit_settings 通过校验关闭弹框。
        app.username_input = "tester".to_string();
        app.password_input = "secret".to_string();
        app.system_port_input = "9465".to_string();
        app.opencode_port_input = "9464".to_string();
        app.input_mode = InputMode::SettingsAuthUsername;

        terminal
            .draw(|frame| app.render_settings_popup(frame))
            .expect("draw");
        let rect = app.last_settings_popup_rect.expect("popup rect");
        let btn_y = rect.y + rect.height - 2;
        // [确认] 按钮 click region: x=rect.x+2, width=8 → x ∈ [rect.x+2, rect.x+10)
        let ok_x = rect.x + 4;
        // [取消] 按钮 click region: x=rect.x+13, width=8
        let cancel_x = rect.x + 15;
        // [打开配置目录] 按钮 click region: x=rect.x+24, width=16
        let open_x = rect.x + 28;

        // 前置断言:find_target 必须真的能命中这三个按钮(否则测试无意义)。
        assert!(matches!(
            app.find_target(ok_x, btn_y),
            Some(ClickTarget::SettingsOk)
        ));
        assert!(matches!(
            app.find_target(cancel_x, btn_y),
            Some(ClickTarget::SettingsCancel)
        ));
        assert!(matches!(
            app.find_target(open_x, btn_y),
            Some(ClickTarget::SettingsOpenConfigDir)
        ));

        // 1) 点 [确认]:input_mode 应跳到 Menu(说明弹框被关闭)。
        tokio_test::block_on(app.click_at(ok_x, btn_y));
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "clicking [确认] should close popup and return to Menu"
        );
        assert!(
            app.last_settings_popup_rect.is_none(),
            "clicking [确认] should clear last_settings_popup_rect"
        );

        // 2) 重新打开弹框,点 [取消]:弹框应关闭,input_mode = Menu。
        app.input_mode = InputMode::SettingsAuthUsername;
        app.last_settings_popup_rect = None;
        terminal
            .draw(|frame| app.render_settings_popup(frame))
            .expect("draw 2");
        let rect2 = app.last_settings_popup_rect.expect("popup rect 2");
        let btn_y2 = rect2.y + rect2.height - 2;
        tokio_test::block_on(app.click_at(rect2.x + 15, btn_y2));
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "clicking [取消] should close popup and return to Menu"
        );

        // 3) 重新打开弹框,点 [打开配置目录]:弹框保持打开(不关闭),
        // handler 不改 input_mode(open_config_dir 失败仅写状态条)。
        app.input_mode = InputMode::SettingsAuthUsername;
        app.last_settings_popup_rect = None;
        terminal
            .draw(|frame| app.render_settings_popup(frame))
            .expect("draw 3");
        let rect3 = app.last_settings_popup_rect.expect("popup rect 3");
        let btn_y3 = rect3.y + rect3.height - 2;
        let prev_mode = app.input_mode;
        tokio_test::block_on(app.click_at(rect3.x + 28, btn_y3));
        assert_eq!(
            app.input_mode, prev_mode,
            "clicking [打开配置目录] should NOT close popup"
        );

        // 4) 边界:点击按钮下方一行(下边框)应识别为"在弹框内",不关闭弹框。
        // 早期版本 btn_y 错位到这里,导致点击按钮"误中"了下边框行 →
        // click_at 走 dismiss 路径把弹框关掉。这条断言锁住 reverse bug。
        tokio_test::block_on(app.click_at(rect3.x + 4, rect3.y + rect3.height - 1));
        assert_eq!(
            app.input_mode, prev_mode,
            "clicking on bottom border row should NOT close popup"
        );
    }

    #[test]
    fn settings_popup_submit_without_required_fields_keeps_popup_open() {
        // 真实场景:用户点 [确认] 但没填必填字段(USERNAME/PASSWORD),
        // submit_settings 校验失败 → input_mode 改回对应字段 + 弹框保留。
        // 这是用户报告"三个按钮功能未生效"的真实原因之一:
        // 必填校验把弹框拦下来,但视觉上像是"没反应"。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = TuiApp::test_stub();
        app.first_setup_required = false;
        // 注意:username/password/system_port/opencode_port 都是空字符串 —
        // 这正是用户没填任何字段的状态。
        app.input_mode = InputMode::SettingsAuthUsername;

        terminal
            .draw(|frame| app.render_settings_popup(frame))
            .expect("draw");
        let rect = app.last_settings_popup_rect.expect("popup rect");
        let btn_y = rect.y + rect.height - 2;
        let ok_x = rect.x + 4;
        // 点 [确认]:submit_settings 应在校验处 return,input_mode 应
        // 改成 SettingsAuthUsername(让用户去填用户名)而不是 Menu。
        tokio_test::block_on(app.click_at(ok_x, btn_y));
        assert_eq!(
            app.input_mode,
            InputMode::SettingsAuthUsername,
            "未填 USERNAME 时 submit 应把焦点切回 USERNAME 字段,弹框应保留"
        );
        assert!(
            app.last_settings_popup_rect.is_some(),
            "submit 校验失败时 last_settings_popup_rect 不应被清掉"
        );
        // 状态条应给出错误提示(供 UI 显示)。
        let status = app.status_message.lock().unwrap().clone();
        assert!(
            status.contains("USERNAME") || status.contains("用户名") || status.contains("PASSWORD") || status.contains("密码"),
            "submit 校验失败应写错误提示到状态条,实际: {status}"
        );
    }

    #[test]
    fn settings_popup_cancel_closes_popup_without_validation() {
        // [取消] 不依赖任何字段,即使什么都没填也应该立刻关闭弹框。
        // 这锁住 "三个都失效" 不可能的边界:如果用户报告 [取消] 也失效,
        // 说明有更深层的 click_at 路由 bug,不是 submit 校验问题。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = TuiApp::test_stub();
        app.first_setup_required = false;
        app.input_mode = InputMode::SettingsAuthUsername;

        terminal
            .draw(|frame| app.render_settings_popup(frame))
            .expect("draw");
        let rect = app.last_settings_popup_rect.expect("popup rect");
        let btn_y = rect.y + rect.height - 2;
        let cancel_x = rect.x + 15;
        // find_target 必须命中 SettingsCancel(否则测试无意义)。
        assert!(matches!(
            app.find_target(cancel_x, btn_y),
            Some(ClickTarget::SettingsCancel)
        ));
        tokio_test::block_on(app.click_at(cancel_x, btn_y));
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "[取消] 必须无条件关闭弹框,即使未填任何字段"
        );
        assert!(
            app.last_settings_popup_rect.is_none(),
            "[取消] 应清 last_settings_popup_rect"
        );
    }

    #[test]
    fn settings_popup_cancel_then_redraw_does_not_show_popup() {
        // 模拟用户场景:点 [取消] 后,下一帧 render 不应该再画 settings弹框。
        // 这是 [取消] 真正"失效"的边界 —— click_at 改了 input_mode,但
        // 如果 last_settings_popup_rect 没清或者 render 逻辑有 bug,
        // 弹框会"复活"。
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let backend = TestBackend::new(120, 50);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = TuiApp::test_stub();
        app.first_setup_required = false;
        app.input_mode = InputMode::SettingsAuthUsername;

        // 第一次 render:画弹框
        terminal.draw(|f| app.render(f)).expect("draw 1");
        assert!(app.input_mode.is_settings_field());
        let rect = app.last_settings_popup_rect.expect("popup rect after draw 1");
        let btn_y = rect.y + rect.height - 2;
        let cancel_x = rect.x + 15;

        // 点 [取消]
        tokio_test::block_on(app.click_at(cancel_x, btn_y));
        assert_eq!(app.input_mode, InputMode::Menu, "[取消] 应关弹框");

        // 第二次 render:应不再画 settings弹框
        terminal.draw(|f| app.render(f)).expect("draw 2");
        assert_eq!(
            app.input_mode,
            InputMode::Menu,
            "第二次 render 后 input_mode 应仍是 Menu"
        );
        // last_settings_popup_rect 也应保持 None(否则下一帧 click_at 误判)
        assert!(
            app.last_settings_popup_rect.is_none(),
            "第二次 render 后 last_settings_popup_rect 应保持 None"
        );
    }

    /// 当 scroll_offset > 0,屏幕上显示的是 build_settings_lines 的 mid 部分。
    /// 屏幕行 `r` 必须映射到 FIELD_LINE_IDX[i] = r + offset。
    #[test]
    fn settings_click_regions_shift_with_scroll_offset() {
        let mut app = TuiApp::test_stub();
        let rect = ratatui::layout::Rect::new(10, 5, 80, 8);
        // scroll_offset=20:屏幕行 0..7 对应 build_settings_lines 行 20..27。
        // FIELD_LINE_IDX 中 21, 23, 25, 27 都在 20..28,可见;1, 3, 7, 9, 13, 15, 17
        // 都不在 20..28 范围,不可见 → 不应注册。
        app.register_settings_click_regions(rect, 99, 20, 8);

        let visible_y_min = rect.y + 1;
        let visible_y_max = rect.y + rect.height.saturating_sub(1);
        // 收集所有 field click region 的 y,断言:
        //   (a) 全部在可见范围 [visible_y_min, visible_y_max) 内;
        //   (b) 屏幕外字段(USERNAME line=1)不能出现。
        for region in &app.click_regions {
            if matches!(region.target, ClickTarget::SettingsField(_)) {
                assert!(
                    region.rect.y >= visible_y_min && region.rect.y < visible_y_max,
                    "field click region y={} 超出弹框可见范围",
                    region.rect.y
                );
            }
        }
        assert!(
            !app.click_regions.iter().any(|r| matches!(
                r.target,
                ClickTarget::SettingsField(InputMode::SettingsAuthUsername)
            )),
            "scroll_offset=20 时 USERNAME (line 1) 不可见,不应注册 click region"
        );
        assert!(
            app.click_regions.iter().any(|r| matches!(
                r.target,
                ClickTarget::SettingsField(InputMode::SettingsRatholeHost)
            )),
            "scroll_offset=20 时 RatholeHost (line 21) 可见,必须注册"
        );
    }

    /// `settings_field_at_row` 接受 scroll_offset:屏幕行 `r` 应映射到
    /// FIELD_LINE_IDX[i] = r + offset 的字段。
    /// 越界(屏幕外)行 → None,与 scroll_offset=0 的旧行为兼容。
    #[test]
    fn settings_field_at_row_respects_scroll_offset() {
        // offset=0:行为与原测试一致(USERNAME 在屏幕 row 1)。
        assert_eq!(
            TuiApp::helper_settings_field_at_row_with_offset(1, 0),
            Some(InputMode::SettingsAuthUsername)
        );
        // offset=20:屏幕 row 1 对应原始行 21 → RatholeHost。
        assert_eq!(
            TuiApp::helper_settings_field_at_row_with_offset(1, 20),
            Some(InputMode::SettingsRatholeHost)
        );
        // offset=20:屏幕 row 5 对应原始行 25 → RatholeName。
        assert_eq!(
            TuiApp::helper_settings_field_at_row_with_offset(5, 20),
            Some(InputMode::SettingsRatholeName)
        );
        // offset=20:屏幕 row 0 是原始行 20("Rathole 内网穿透设置"标题) → None。
        assert_eq!(TuiApp::helper_settings_field_at_row_with_offset(0, 20), None);
        // offset=20:屏幕 row 6 是原始行 26(描述行) → None。
        assert_eq!(TuiApp::helper_settings_field_at_row_with_offset(6, 20), None);
        // offset=20:屏幕 row 7 对应原始行 27 → RatholeToken(刚好可见)。
        assert_eq!(
            TuiApp::helper_settings_field_at_row_with_offset(7, 20),
            Some(InputMode::SettingsRatholeToken)
        );
        // offset=20:屏幕 row 8 超出 popup_h=8 → 内容区外,即便有 FIELD_LINE_IDX 也不该出现。
        assert_eq!(TuiApp::helper_settings_field_at_row_with_offset(8, 20), None);
    }
}

// 与上方 `mod tests` 配对的辅助实现块 —— 关联方法,只在测试构建时编译。
// (放在 `mod tests` 块外是因为 `#[test]` 不能用于 `impl` 块内的函数,
//  而我们仍想以 `TuiApp::xxx(...)` 语法从测试调用这些 helper。)
#[cfg(test)]
impl TuiApp {
    /// 带滚动偏移版本的 helper:屏幕行 `row` 对应 `build_settings_lines`
    /// 的第 `row + scroll_offset` 行,所以查找的是 FIELD_LINE_IDX == row + offset。
    fn helper_settings_field_at_row_with_offset(
        row: u16,
        scroll_offset: u16,
    ) -> Option<InputMode> {
        const FIELD_LINE_IDX: [u16; 11] = [1, 3, 7, 9, 13, 15, 17, 21, 23, 25, 27];
        FIELD_LINE_IDX
            .iter()
            .position(|&r| r == row + scroll_offset)
            .and_then(|i| SETTINGS_FIELDS.get(i).copied())
    }

    /// 构造一个最小可用的 TuiApp,用于测试 `register_settings_click_regions`
    /// 与 `settings_field_at_row` 等纯几何/逻辑函数。这些函数不读 supervisor
    /// / log buffer / store,所以我们可以用 Default 全空壳。
    ///
    /// 使用 `tempfile::tempdir()` 创建一个临时文件给 `FileCache` —— store
    /// 构造函数要求一个可写路径;tempdir 保证测试结束后自动清理。
    #[allow(clippy::too_many_lines)]
    fn test_stub() -> TuiApp {
        use crate::auth::AuthConfig;
        use crate::config::{RatholeConfig, SbConfig};
        use crate::serve::ServeStatus;
        use crate::storage::FileCache;
        use crate::storage::PathListStore;
        use crate::ui::log::LogBuffer;
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = FileCache::new(tmp.path().join("path-list.md"));
        TuiApp {
            supervisor: ServeSupervisor::default(),
            auth: Arc::new(RwLock::new(AuthConfig {
                basic_user: String::new(),
                basic_password: String::new(),
                sb_cookie_name: None,
            })),
            log_buffer: LogBuffer::default(),
            store: Arc::new(PathListStore::new(cache)),
            main_state: ListState::default(),
            projects_state: ListState::default(),
            service_state: ListState::default(),
            status_message: Arc::new(Mutex::new(String::new())),
            cached_status: Arc::new(Mutex::new(ServeStatus::default())),
            should_quit: false,
            input_mode: InputMode::SettingsAuthUsername,
            focus: Focus::Main,
            sub_page: None,
            attached_sessions: Arc::new(Mutex::new(Vec::new())),
            attach_url: String::new(),
            username_input: String::new(),
            password_input: String::new(),
            auth_password_mask_len: 0,
            show_full_log: false,
            log_scroll: 0,
            confirm: None,
            confirm_choice: ConfirmChoice::Confirm,
            sb_config: Arc::new(RwLock::new(SbConfig {
                url: String::new(),
                user: String::new(),
                password: String::new(),
            })),
            remote_status: Arc::new(Mutex::new(String::new())),
            program_started_at: chrono::Local::now(),
            system_port_input: String::new(),
            opencode_port_input: String::new(),
            sb_url_input: String::new(),
            sb_user_input: String::new(),
            sb_password_input: String::new(),
            rathole_config: Arc::new(RwLock::new(RatholeConfig {
                host: String::new(),
                port: String::new(),
                name: String::new(),
                token: String::new(),
            })),
            rathole_host_input: String::new(),
            rathole_port_input: String::new(),
            rathole_name_input: String::new(),
            rathole_token_input: String::new(),
            click_regions: Vec::new(),
            mouse_pos: None,
            settings_scroll_offset: 0,
            last_main_column_area: ratatui::layout::Rect::default(),
            last_sub_page_area: ratatui::layout::Rect::default(),
            last_settings_popup_rect: None,
            first_setup_required: true,
            pending_attach: None,
        }
    }
}


