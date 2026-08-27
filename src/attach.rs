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

/// 在新终端标签页执行命令（macOS，优先 iTerm2，回退 Terminal.app）。
///
/// # Errors
/// 返回可读的错误消息。
#[cfg(target_os = "macos")]
pub fn spawn_in_new_terminal(command: &str) -> Result<(), String> {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");

    let iterm = format!(
        "tell application \"iTerm2\"\n\
         if exists current window then\n\
         tell current window to create tab with default profile\n\
         else\n\
         create window with default profile\n\
         end if\n\
         tell current session of current window to write text \"{escaped}\"\n\
         end tell"
    );
    let iterm_status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&iterm)
        .status();
    if matches!(iterm_status, Ok(st) if st.success()) {
        return Ok(());
    }

    let terminal = format!(
        "tell application \"Terminal\"\n\
         activate\n\
         do script \"{escaped}\"\n\
         end tell"
    );
    let st = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&terminal)
        .status()
        .map_err(|e| format!("osascript 执行失败: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err("无法打开新终端（请确认已安装 iTerm2 或 Terminal.app）".to_string())
    }
}

/// 在 Windows 弹出一个新的可见控制台窗口并执行命令。
///
/// 实现策略：
///
///  1. 把整条命令写到 `%TEMP%\oc-attach-<pid>.bat` 里。
///     `cmd /K "powershell -NoProfile -Command \"…\""` 这种内嵌 PowerShell 的
///     `script` 字符串经过 cmd 的二次参数解析、PowerShell 的 -Command 引号剥离
///     之后行为很难跨 Windows 版本预测；写成 `.bat` 后批处理器把文件内容当作
///     字面命令行执行，quoting 干净，PowerShell 直接看到原本写好的脚本字符串。
///  2. 拉起 `cmd.exe /K <bat>`，通过 `creation_flags` 同时打开独立可见控制台
///     并脱离 TUI 的 Ctrl+C/Break：
///
///       * `CREATE_NEW_CONSOLE = 0x0010` —— 子进程获得自己的控制台窗口；
///         否则会继承 TUI 的控制台，要么不开新窗口要么闪退。
///       * `CREATE_NEW_PROCESS_GROUP = 0x0200` —— TUI 上的 Ctrl+C / Ctrl+Break
///         不会传导到新会话，误杀 attach 子进程。
///
///  3. `/K` 让 cmd 在 `opencode attach` 异常退出后仍保留窗口，方便用户看错误
///     信息；用户手动关窗即结束。
///
/// 为什么不优先 `wt.exe`：Windows Terminal 在很多机器上是 Microsoft Store 的
/// `APPEXECUTION_ALIAS` stub，`spawn()` 会返回 `Ok` 但根本不打开标签页——继续
/// 返回 Ok 等于"无声地告诉用户成了"，反而是更大的故障源。所以直接走 `cmd.exe`。
#[cfg(target_os = "windows")]
pub fn spawn_in_new_terminal(command: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    // CREATE_NEW_CONSOLE | CREATE_NEW_PROCESS_GROUP
    const FLAGS: u32 = 0x0210;

    // 1. 写临时 .bat，避开 cmd / PowerShell 双层引号玄学。
    let pid = std::process::id();
    let bat_path = std::env::temp_dir().join(format!("oc-attach-{pid}.bat"));
    std::fs::write(&bat_path, command.as_bytes())
        .map_err(|e| format!("无法写入临时脚本 {}: {e}", bat_path.display()))?;

    // 2. 拉起新的 cmd.exe 跑这个 bat。Rust 的 Command 会把 `bat_path`
    //    自动包成带引号的 argv 项传给 CreateProcess，无需手动转义。
    let result = std::process::Command::new("cmd.exe")
        .arg("/K")
        .arg(&bat_path)
        .creation_flags(FLAGS)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开新终端：{e}"));

    // 3. bat 文件保留在 %TEMP% 下，重复运行会覆盖。删除不是必要的，且万一
    //    新 cmd 启动有延迟、读到一半被删，反而出更隐蔽的 bug。
    result
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn spawn_in_new_terminal(_command: &str) -> Result<(), String> {
    Err("新终端标签页打开仅在 macOS / Windows 上受支持".to_string())
}

/// 构造 attach 会话的「PID 文件路径 + 在新终端执行的命令」（跨平台）。
///
/// macOS：`bash -c 'echo $$ > pidfile; exec opencode attach ...'`（`$$` 即
/// opencode 进程 PID，`kill -9` 可直接终止）。
/// Windows：PowerShell 写自身 `$PID` 到 pidfile 再 `opencode attach`（作为
/// 子进程），`taskkill /PID <pid> /T /F` 终止整棵进程树。
#[cfg(target_os = "windows")]
pub fn attach_launch_spec(
    url: &str,
    directory: &str,
    session: &str,
    user: &str,
    password: &str,
) -> (String, String) {
    let pid_file = std::env::temp_dir()
        .join(format!("oc-attach-{session}.pid"))
        .to_string_lossy()
        .into_owned();
    // PowerShell 脚本：写自身 PID，再运行 opencode attach。
    let script = format!(
        "$PID | Set-Content -Encoding ascii '{pid_file}'; opencode attach '{url}' --dir '{directory}' --session '{session}' -u '{user}' -p '{password}'"
    );
    let command = format!("powershell -NoProfile -Command \"{script}\"");
    (pid_file, command)
}

/// macOS / Linux 版：保持原有 `bash -c` 方案不变。
#[cfg(not(target_os = "windows"))]
pub fn attach_launch_spec(
    url: &str,
    directory: &str,
    session: &str,
    user: &str,
    password: &str,
) -> (String, String) {
    let pid_file = format!("/tmp/oc-attach-{session}.pid");
    let attach_cmd = format!(
        "opencode attach \"{url}\" --dir \"{directory}\" --session \"{session}\" -u \"{user}\" -p \"{password}\""
    );
    let command = format!("bash -c 'echo $$ > {pid_file}; exec {attach_cmd}'");
    (pid_file, command)
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

/// Windows：通过 `Shell.Application` COM 弹出 Windows 原生"选择文件夹"对话框。
///
/// 用 PowerShell 子进程承载对话框（不会阻塞 TUI 的 tokio 事件循环）。
/// `Shell.Application.BrowseForFolder` 与 .NET `FolderBrowserDialog` 不同，
/// **不要求 STA 线程**，因此可以直接通过 `-Command` 跑，PowerShell 进程
/// 会等用户点完才退出——`tokio::process::Command::output()` 就能直接拿到
/// 用户选中的路径。
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

    let output = tokio::process::Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(SCRIPT)
        .output()
        .await
        .map_err(|e| format!("启动 PowerShell 失败（请确认 PowerShell 在 PATH 中）：{e}"))?;

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
