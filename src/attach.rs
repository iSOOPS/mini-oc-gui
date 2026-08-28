//! OpenCode serve 的 HTTP 客户端 + attach 命令描述。
//!
//! 复刻 `oc-serve-tui-actuator.sh` 的 attach 前置流程：
//! 拉取项目会话（GET /session）、创建会话（POST /api/session），
//! 最终交由主进程执行 `opencode attach`。

use serde::Deserialize;

/// 单个 opencode 会话（仅取 attach 流程关心的字段）。
#[derive(Debug, Clone, Deserialize)]
pub struct OcSession {
    /// 会话 id（如 `ses_<22 base62 chars>`）。
    pub id: String,
    /// 会话标题，缺失时为 None。
    #[serde(default)]
    pub title: Option<String>,
}

/// 退出 TUI 后需要执行的 `opencode attach` 命令参数。
#[derive(Debug, Clone)]
pub struct AttachCommand {
    pub url: String,
    pub directory: String,
    pub session: String,
    pub username: String,
    pub password: String,
}

/// 已在新终端启动的 attach 会话（用于「当前服务」栏展示）。
#[derive(Debug, Clone)]
pub struct AttachedSession {
    pub directory: String,
    pub session: String,
    /// PID 文件路径，用于 kill 该 attach 进程。
    pub pid_file: String,
    /// 启动时间（Unix 秒）。
    pub started_at: i64,
}


/// 按 PID 终止进程（跨平台）。attach 会话的「杀掉会话」用。
#[cfg(target_os = "windows")]
pub fn kill_process(pid: i32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

#[cfg(not(target_os = "windows"))]
pub fn kill_process(pid: i32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}

/// 弹出 macOS 目录选择对话框（Finder），返回用户选择的目录路径。
///
/// # Errors
/// 返回可读的错误消息（如用户取消）。
#[cfg(target_os = "macos")]
pub async fn choose_folder() -> Result<String, String> {
    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg("POSIX path of (choose folder with prompt \"选择项目目录\")")
        .output()
        .await
        .map_err(|e| format!("osascript 执行失败: {e}"))?;
    if !output.status.success() {
        return Err("已取消选择".to_string());
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        Err("未选择目录".to_string())
    } else {
        Ok(path)
    }
}

/// 在 Windows 上查找可用的 PowerShell 可执行文件。
///
/// 优先选择 PowerShell 7+ (`pwsh.exe`)，回退到 Windows PowerShell 5.1
/// (`powershell.exe`)。同时尊重环境变量 `OC_POWERSHELL_BIN` —— 用户可
/// 显式指定一个 PowerShell 路径（适用于自定义安装位置）。
///
/// # 返回
/// `(binary_name, version_label)`：如 `("pwsh.exe", "PowerShell 7+")` 或
/// `("powershell.exe", "Windows PowerShell 5.1")`。
fn resolve_powershell_bin() -> (&'static str, &'static str) {
    // 1) 显式环境变量优先 —— 用户说了算。
    if let Ok(custom) = std::env::var("OC_POWERSHELL_BIN") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            // 借用 'static 生命周期：直接把 trimmed 拷贝到泄漏的 Box<str>。
            let leaked: &'static str = Box::leak(trimmed.to_owned().into_boxed_str());
            return (leaked, "PowerShell (OC_POWERSHELL_BIN)");
        }
    }

    // 2) 优先 pwsh.exe（PowerShell 7+，跨平台一致的二进制名）。
    if let Ok(pwsh) = which_powershell("pwsh.exe") {
        let _ = pwsh; // 仅用作存在性探测
        return ("pwsh.exe", "PowerShell 7+");
    }

    // 3) 回退到 powershell.exe（Windows PowerShell 5.1，仅 Windows 自带）。
    if let Ok(ps) = which_powershell("powershell.exe") {
        let _ = ps;
        return ("powershell.exe", "Windows PowerShell 5.1");
    }

    // 4) 都没找到 —— 让 spawn 失败，再由调用方报告清晰错误。
    ("pwsh.exe", "PowerShell 7+")
}

/// 通过 `where` (Windows) / `which` (Unix) 检查可执行文件是否在 PATH 中。
/// 注意：仅用于探测存在性，不会修改全局 PATH。出错时返回 Err，让调用方
/// 走默认分支（pwsh 优先）。
fn which_powershell(name: &str) -> Result<std::path::PathBuf, ()> {
    #[cfg(target_os = "windows")]
    let mut probe = std::process::Command::new("where");
    #[cfg(not(target_os = "windows"))]
    let mut probe = std::process::Command::new("which");

    let output = probe.arg(name).output().ok().filter(|o| o.status.success());
    match output {
        Some(o) => {
            let path = String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|s| s.trim().to_owned());
            match path {
                Some(p) if !p.is_empty() => Ok(std::path::PathBuf::from(p)),
                _ => Err(()),
            }
        }
        None => Err(()),
    }
}

/// Windows：通过 `Shell.Application` COM 弹出 Windows 原生"选择文件夹"对话框。
///
/// 用 PowerShell 子进程承载对话框（不会阻塞 TUI 的 tokio 事件循环）。
/// `Shell.Application.BrowseForFolder` 与 .NET `FolderBrowserDialog` 不同，
/// **不要求 STA 线程**，因此可以直接通过 `-Command` 跑，PowerShell 进程
/// 会等用户点完才退出——`tokio::process::Command::output()` 就能直接拿到
/// 用户选中的路径。
///
/// **PowerShell 版本选择**：优先 PowerShell 7+ (`pwsh.exe`)，回退到
/// Windows PowerShell 5.1 (`powershell.exe`)。可通过环境变量
/// `OC_POWERSHELL_BIN=<path>` 显式指定使用的 PowerShell 二进制路径。
///
/// flags = 0x41：
/// * `BIF_RETURNONLYFSDIRS = 0x01` —— 只允许选目录，禁止"选文件"
/// * `BIF_NEWDIALOGSTYLE  = 0x40` —— 用 Vista+ 的新风格对话框（带"新建文件夹"按钮）
///
/// 用户取消时 PowerShell 退出码仍为 0，但 stdout 为空；stdout 非空 => 用户确认。
#[cfg(target_os = "windows")]
pub async fn choose_folder() -> Result<String, String> {
    const SCRIPT: &str = "\
        $shell = New-Object -ComObject Shell.Application; \
        $folder = $shell.BrowseForFolder(0, '选择项目目录', 0x41, 0); \
        if ($folder) { Write-Output $folder.Self.Path }";

    let (bin, version_label) = resolve_powershell_bin();
    let output = tokio::process::Command::new(bin)
        .arg("-NoProfile")
        .arg("-Command")
        .arg(SCRIPT)
        .output()
        .await
        .map_err(|e| {
            format!(
                "启动 {version_label}（{bin}）失败：{e}。\
                 请确认 PowerShell 已安装并在 PATH 中；\
                 可设置 OC_POWERSHELL_BIN 指向自定义路径。"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let trimmed = stderr.trim();
        if trimmed.is_empty() {
            return Err("已取消选择".to_string());
        }
        return Err(format!("目录选择失败：{trimmed}"));
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        // BrowseForFolder 用户点"取消"时 PowerShell 也会写一个空行，转成中文。
        Err("未选择目录".to_string())
    } else {
        Ok(path)
    }
}

/// Linux（及其它 Unix-like）：暂不实现 GUI 选目录，提示用户走"手动输入"。
/// 注意：此分支以前笼统地写成 "仅 macOS"，把 Windows 也误归到不支持——已经
/// 由上面的 `#[cfg(target_os = "windows")]` 实现替换。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub async fn choose_folder() -> Result<String, String> {
    Err("目录选择对话框在当前平台不可用，请使用「手动输入路径」".to_string())
}

/// 打 opencode serve HTTP API 的客户端（Basic auth）。
#[derive(Debug, Clone)]
pub struct OpencodeClient {
    base_url: String,
    username: String,
    password: String,
    http: reqwest::Client,
}

impl OpencodeClient {
    #[must_use]
    pub fn new(base_url: String, username: String, password: String) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            username,
            password,
            http: reqwest::Client::new(),
        }
    }

    /// `GET /project`（带 Basic auth）→ 验证 opencode serve 是否可用且凭据正确。
    ///
    /// # Errors
    /// 返回可读的错误消息。
    pub async fn health_check(&self) -> Result<(), String> {
        let url = format!("{}/project", self.base_url);
        let resp = self
            .http
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| format!("无法连接 opencode serve: {e}"))?;
        match resp.status().as_u16() {
            200 => Ok(()),
            401 | 403 => Err("认证失败（用户名/密码不匹配）".to_string()),
            s => Err(format!("opencode serve 返回 HTTP {s}")),
        }
    }

    /// `GET /session?directory=<dir>` → 会话列表。
    ///
    /// # Errors
    /// 返回可读的错误消息（供状态栏/日志展示）。
    pub async fn list_sessions(&self, directory: &str) -> Result<Vec<OcSession>, String> {
        let url = format!("{}/session", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("directory", directory)])
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await
            .map_err(|e| format!("GET /session 请求失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("GET /session 返回 HTTP {}", resp.status()));
        }
        resp.json::<Vec<OcSession>>()
            .await
            .map_err(|e| format!("GET /session 解析失败: {e}"))
    }

    /// `POST /api/session` → 创建会话，返回 session id。
    ///
    /// # Errors
    /// 返回可读的错误消息。
    pub async fn create_session(&self, directory: &str) -> Result<String, String> {
        let url = format!("{}/api/session", self.base_url);
        let title = format!("TUI-Launched-{}", chrono::Utc::now().timestamp());
        let body = serde_json::json!({
            "title": title,
            "location": { "directory": directory }
        });
        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.username, Some(&self.password))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("POST /api/session 请求失败: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("POST /api/session 返回 HTTP {}", resp.status()));
        }

        #[derive(Deserialize)]
        struct CreateResp {
            data: CreateData,
        }
        #[derive(Deserialize)]
        struct CreateData {
            id: String,
        }
        let r: CreateResp = resp
            .json()
            .await
            .map_err(|e| format!("POST /api/session 响应解析失败: {e}"))?;
        Ok(r.data.id)
    }
}

/// 同步运行 `opencode attach`，**接管当前进程的控制台**。
///
/// 设计意图：attach 必须与用户**直接交互**（用户在 attach 自己的 prompt
/// 里输入命令、看输出、按 `Ctrl+C`）。任何"弹到独立窗口"的方案都需要中间层
/// （PowerShell / Windows Terminal），这些中间层跟 attach 共享控制台时会
/// 出现"输入不显示 / 切换窗口才刷新"的渲染冲突（实测均无法稳定使用）。
///
/// 因此唯一稳定的方案是 **同窗口接管**：
///
/// 1. mini-oc-gui TUI 先 **suspend**（`LeaveAlternateScreen` + `disable_raw_mode`
///    + `DisableMouseCapture`），把控制台交还给"普通模式 + 主屏幕"；
/// 2. spawn `opencode attach ...`，stdin/stdout/stderr **inherit** 父进程的
///    控制台 —— attach 独占控制台读写；
/// 3. `child.wait()` 阻塞直到 attach 自然退出（用户在 attach 自己的 prompt
///    里 `/exit` 或 `Ctrl+C`）；
/// 4. TUI **resume**（`EnterAlternateScreen` + `enable_raw_mode` +
///    `EnableMouseCapture` + `terminal.clear()`），force redraw 恢复 TUI 视图。
///
/// **多开/多选限制**：attach 期间 TUI 冻结，无法在 TUI 里同时启动或管理多个
/// attach 会话。这是"同窗口接管"方案的根本取舍 —— 详见 README "Known limitations"
/// 章节以及设计文档中的"多会话管理方案"讨论。
///
/// 进程树（npm 全局装的 opencode 是 .cmd shim，Rust 启动 .cmd 时 Windows
/// 内部用 cmd.exe 解释）：
/// ```text
/// mini-oc-gui TUI (suspend 后，attach 期间不响应 TUI 输入)
/// └── cmd.exe (解释 .cmd shim；自身在等 node.exe 退出，不写控制台)
///     └── node.exe
///         └── opencode attach (独占控制台)
/// ```
///
/// `cmd.exe` 在 attach 期间**同步等** node.exe，自己不写控制台 buffer，
/// 不会与 attach 的渲染冲突 —— 这是与之前 `cmd.exe /K <bat>` 方案的关键区别：
/// 之前 cmd.exe /K 是被 spawn 来"持有" bat 跑完后的窗口，attach 退出时 cmd.exe
/// 会**主动刷新 prompt**覆盖 attach 残留；这里 cmd.exe 只是 npm shim 的隐式
/// 解释器，attach 退出时它跟着 node 一起退，不留任何 prompt 干扰。
///
/// `pid_file` 用于 `kill_session` 强杀 attach（万一 attach 卡死，用户切回
/// TUI 后能从"当前服务"栏触发 taskkill /T /F）。
///
/// # Errors
/// spawn 失败时返回 `std::io::Error`。
#[allow(clippy::too_many_arguments)]
pub fn run_attach_blocking(
    url: &str,
    directory: &str,
    session: &str,
    user: &str,
    password: &str,
    pid_file: &str,
) -> std::io::Result<std::process::ExitStatus> {
    use std::process::{Command, Stdio};

    // 解析 opencode 绝对路径（避免 npm `.cmd` shim 的 quoting 问题）。
    // Unix 上 resolve_command 直接返回裸名（execvp 自动解析）。
    let bin = crate::upgrade::resolve_command("opencode")
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;

    let mut child = Command::new(bin.to_string_lossy().as_ref())
        .arg("attach")
        .arg(url)
        .arg("--dir")
        .arg(directory)
        .arg("--session")
        .arg(session)
        .arg("-u")
        .arg(user)
        .arg("-p")
        .arg(password)
        // 关键：stdin/stdout/stderr **inherit** 父进程（mini-oc-gui）的控制台。
        // TUI 已经 suspend，控制台回到 cooked mode + main screen，attach 接管。
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;

    // 立即写 PID 文件：spawn 返回时 kernel 已分配 PID；如果是 .cmd shim，
    // child.id() 返回 cmd.exe（解释器）的 PID —— taskkill /T /F <cmd_pid>
    // 能沿着 cmd.exe → node.exe → attach 整棵树杀干净。
    let pid = child.id();
    if let Err(e) = std::fs::write(pid_file, pid.to_string()) {
        tracing::warn!("写入 attach pid_file {pid_file} 失败: {e}");
    }

    child.wait()
}

// ============================================================================
// 新窗口 attach 模式（思路 2）
// ============================================================================
//
// ## 历史失败路径（全部规避，勿回退）
//
// | # | 之前的做法 | 失败现象 | 根因 |
// |---|-----------|---------|------|
// | 1 | `powershell -Command "opencode serve ..."` 包装子进程 | serve 场景：中间 PowerShell 必然创建可见控制台，输出被 pipe 走 → 新控制台 buffer 长期无写入 → 渲染循环停摆 → "切窗口才刷新"（supervisor.rs 注释记录） | stdio piped + 子进程又分配了新控制台，两者错位 |
// | 2 | spawn pwsh + `CREATE_NEW_CONSOLE` 直启 attach | "启动正常、显示正常，但输入不显示、不刷新，切换窗口后才显示" | **Rust std 的 CreateProcess 默认 STARTF_USESTDHANDLES**：即使分配了新 conhost，子进程 stdio 仍指向父进程（mini-oc-gui）的 ConPTY/管道 → attach 的渲染输出全部流回父进程，新窗口 screen buffer 零写入 → conhost 无重绘；切换窗口触发焦点事件才用残留 buffer 全量重绘；raw mode 下回显由 TUI 负责，TUI 输出走错句柄 → "输入不显示" |
// | 3 | 复用 `serve::process::build_command` 的通用 spawn 路径 | 根本不弹窗 | 该路径全局加 `CREATE_NO_WINDOW (0x08000000)`（process.rs:171-174） |
// | 4 | `cmd.exe /K <bat>` 持有窗口 | attach 退出时 cmd 主动刷新 prompt，覆盖 attach 残留画面（attach.rs 历史注释记录） | cmd /K 在 attach 结束后回到交互态、重写控制台 |
// | 5 | 经 npm `.cmd` shim 启动 opencode | 放大 #2 的错位 | `pwsh → cmd.exe → node` 多一层 stdio 转发 |
// | 6 | 凭据走命令行 `-u -p` | 泄露风险 | 新窗口进程命令行可被 tasklist / WMI / 窗口属性直接看到 |
//
// ## 本实现的规避清单
//
// 1. **`cmd /c start`（ShellExecute 族）创建新窗口**：`start` 启动的新进程
//    stdio 由 Windows shell 连接到新分配的控制台，**完全不经过 Rust 进程的
//    任何句柄** —— 从源头消灭 #1/#2 的句柄错位。启动器 cmd.exe 本身
//    stdio 全 piped（`.output()`），附着在 TUI 控制台上但零写入、
//    `start` 异步返回后立即退出，不闪窗、不残留。
// 2. **不走 `serve::process::build_command`**：此处独立构造 Command，
//    不加 `CREATE_NO_WINDOW`（规避 #3）。
// 3. **凭据全部走环境变量**（`OC_ATTACH_*`）：环境块沿
//    Rust → cmd.exe → start → 新 pwsh 完整继承，命令行里只有脚本路径
//    （规避 #6）。launcher 脚本内容只引用 `$env:` 变量名，无敏感信息。
// 4. **优先 `.ps1` shim 直连 node**：`resolve_command` 结果若为 `.cmd`，
//    且同目录存在 npm 生成的 `.ps1`，改用 `.ps1`（pwsh 原生脚本，
//    `pwsh → node` 单层，规避 #5）；缺失时回退 `.cmd`——新窗口场景下
//    整条链的 stdio 都连着同一个新控制台，cmd.exe 同步等 node 退出、
//    自身不写控制台（同窗口模式已论证无害）。
// 5. **PID 由新窗口内的 pwsh 自写**：`start` 创建的进程不是 Rust 的子进程，
//    Rust 拿不到其 PID；脚本第一行把 `$PID` 写入 `OC_ATTACH_PIDFILE`
//    （兼容现有 kill_session 的 taskkill /T /F 语义，pwsh → node 整树回收）。
// 6. **`-NoExit` 保持窗口**：attach 退出/失败后窗口留在桌面，用户能看
//    到退出码与错误输出（规避"失败无声无息"）。

/// 新窗口 attach 的参数集。
///
/// 凭据不进命令行（见上 #6），全部经 `OC_ATTACH_*` 环境变量传递。
#[derive(Debug, Clone)]
pub struct AttachWindowSpec {
    /// opencode serve 地址（如 `http://127.0.0.1:9464`）。
    pub url: String,
    /// 项目目录。
    pub directory: String,
    /// 会话 id。
    pub session: String,
    /// HTTP Basic 用户名 → `OC_ATTACH_USER`。
    pub user: String,
    /// HTTP Basic 密码 → `OC_ATTACH_PASS`。
    pub password: String,
    /// 新窗口 pwsh 把自身 PID 写到这里（kill_session 用）。
    pub pid_file: String,
    /// 临时 launcher `.ps1` 路径（内容无敏感信息，可随 pid_file 一并清理）。
    pub launcher_script: String,
}

/// launcher 脚本模板：只引用 `$env:` 变量，不含任何敏感值。
///
/// - 第一行写 `$PID`（pwsh 自身）→ 兼容 kill_session 的 `taskkill /T /F`
///   （pwsh → node 进程树整杀）；
/// - `&` 调用符 + 变量路径：pwsh 自行处理含空格路径，无引号地狱；
/// - 结尾打印退出码；配合 `-NoExit` 窗口保留，错误可见。
#[cfg(target_os = "windows")]
fn launcher_ps1_body() -> &'static str {
    r#"$ErrorActionPreference = 'Stop'
$PID | Set-Content -NoNewline -Path $env:OC_ATTACH_PIDFILE
Write-Host ("== oc attach {0} ==" -f $env:OC_ATTACH_SESSION)
Write-Host ("dir: {0}" -f $env:OC_ATTACH_DIR)
Write-Host ("url: {0}" -f $env:OC_ATTACH_URL)
Write-Host ""
& $env:OC_ATTACH_BIN attach $env:OC_ATTACH_URL --dir $env:OC_ATTACH_DIR --session $env:OC_ATTACH_SESSION -u $env:OC_ATTACH_USER -p $env:OC_ATTACH_PASS
$code = $LASTEXITCODE
Write-Host ""
Write-Host ("attach 已退出（退出码 {0}），此窗口可安全关闭。" -f $code)
"#
}

/// 解析 PowerShell 完整路径（优先 pwsh 7+，回退 5.1，尊重 OC_POWERSHELL_BIN）。
#[cfg(target_os = "windows")]
fn resolve_powershell_full_path() -> Result<std::path::PathBuf, String> {
    if let Ok(custom) = std::env::var("OC_POWERSHELL_BIN") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return Ok(std::path::PathBuf::from(trimmed));
        }
    }
    if let Ok(p) = which_powershell("pwsh.exe") {
        return Ok(p);
    }
    if let Ok(p) = which_powershell("powershell.exe") {
        return Ok(p);
    }
    Err("未找到 PowerShell（pwsh.exe / powershell.exe 都不在 PATH）。可设置 OC_POWERSHELL_BIN 指定路径".to_string())
}

/// 解析 attach 用的 opencode 入口，优先 `.ps1` shim（pwsh 原生，见规避 #4）。
#[cfg(target_os = "windows")]
fn resolve_opencode_for_window() -> Result<std::path::PathBuf, String> {
    let bin = std::env::var("OPENCODE_BIN")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| crate::upgrade::resolve_command("opencode").ok())
        .ok_or_else(|| {
            "未找到 opencode（where + PATHEXT 解析失败）。可设置 OPENCODE_BIN 指向可执行文件".to_string()
        })?;
    let is_cmd = bin
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("cmd"))
        .unwrap_or(false);
    if is_cmd {
        let ps1 = bin.with_extension("ps1");
        if ps1.is_file() {
            return Ok(ps1);
        }
    }
    Ok(bin)
}

/// 在**新的 PowerShell 窗口**中启动 `opencode attach`（fire-and-forget）。
///
/// 调用后 TUI 不冻结：`cmd /c start` 异步创建新窗口后立即返回。
/// 失败（找不到 pwsh / opencode、写脚本失败、start 报错）同步返回 Err，
/// 由调用方在状态栏提示，**不自动回退**同窗口模式（避免掩盖问题）。
///
/// **环境前提**：调用进程必须附着真实控制台（TUI 模式天然满足——无论从
/// Windows Terminal / conhost 启动，还是 explorer 双击 console 程序分配的
/// 控制台）。`start` 在**无控制台（DETACHED / 全重定向）父上下文**中行为
/// 退化：新进程不获得独立控制台而是继承管道句柄（实测：cmd 返回 0 但
/// 脚本不执行、父会话 stdio 管道被 -NoExit 的子进程挂住）。若将来需要
/// 从无控制台上下文（HTTP API / 服务化）触发，必须改用 `wt.exe` 或
/// ConPTY 自托管方案，勿复用本函数。
///
/// # Errors
/// 返回可读的中文错误消息。
#[cfg(target_os = "windows")]
pub fn spawn_attach_new_window(spec: &AttachWindowSpec) -> Result<(), String> {
    // 1. 预解析全部路径 —— 错误在 Rust 侧先暴露（cmd/start 的 stderr 已被
    //    piped 收集，但预检的报错信息更精确）。
    let pwsh = resolve_powershell_full_path()?;
    let bin = resolve_opencode_for_window()?;

    // 2. 写 launcher 脚本（内容无敏感信息，见规避 #3/#6）。
    std::fs::write(&spec.launcher_script, launcher_ps1_body())
        .map_err(|e| format!("写入 launcher 脚本失败（{}）：{e}", spec.launcher_script))?;

    // 3. 窗口标题：项目目录名，便于用户在任务栏辨识。
    //    显式给 start 提供标题是关键 —— start 会把第一个带引号的参数当标题，
    //    不给的话 pwsh 路径（含空格时被引号包裹）会被误当成标题。
    let name = std::path::Path::new(&spec.directory)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| spec.directory.clone());
    let title = format!("oc-attach {name}");

    // 4. cmd /c start：ShellExecute 族创建新窗口（规避 #1/#2/#3）。
    //    `.output()` 等 cmd /c 退出（start 异步，秒回），同时收集
    //    "找不到 pwsh"之类的启动期错误。
    let output = std::process::Command::new("cmd")
        .arg("/c")
        .arg("start")
        .arg(title)
        .arg(&pwsh)
        .args(["-NoExit", "-NoProfile", "-ExecutionPolicy", "Bypass"])
        .arg("-File")
        .arg(&spec.launcher_script)
        // 环境变量链：Rust → cmd.exe → start → 新窗口 pwsh。
        .env("OC_ATTACH_BIN", &bin)
        .env("OC_ATTACH_URL", &spec.url)
        .env("OC_ATTACH_DIR", &spec.directory)
        .env("OC_ATTACH_SESSION", &spec.session)
        .env("OC_ATTACH_USER", &spec.user)
        .env("OC_ATTACH_PASS", &spec.password)
        .env("OC_ATTACH_PIDFILE", &spec.pid_file)
        .output()
        .map_err(|e| format!("启动 cmd /c start 失败：{e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!("新窗口启动失败（cmd start）：{detail}"));
    }
    Ok(())
}

/// 非 Windows 平台：新窗口模式暂未实现，提示回退同窗口模式。
#[cfg(not(target_os = "windows"))]
pub fn spawn_attach_new_window(_spec: &AttachWindowSpec) -> Result<(), String> {
    Err("新窗口模式当前仅支持 Windows，请用 Enter（同窗口 attach）".to_string())
}