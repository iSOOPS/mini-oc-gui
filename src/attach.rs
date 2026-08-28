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
/// ```
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