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

/// 在 Windows Terminal 新标签页执行命令。
///
/// 优先 `wt.exe` 新标签；未安装 Windows Terminal 时回退 `cmd /c start`
/// 新控制台窗口，保证功能可用。
#[cfg(target_os = "windows")]
pub fn spawn_in_new_terminal(command: &str) -> Result<(), String> {
    let wt = std::process::Command::new("wt.exe")
        .arg("-w")
        .arg("0")
        .arg("new-tab")
        .arg("--")
        .arg(command)
        .spawn();
    if wt.is_ok() {
        return Ok(());
    }

    // 回退：传统 cmd 新窗口。`start` 的第一个空串是窗口标题占位，
    // 防止 command 里的引号被误当成标题。
    std::process::Command::new("cmd.exe")
        .args(["/c", "start", "", command])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开新终端：{e}"))
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

/// 非 macOS：无原生 Finder 目录选择，返回可读错误，引导使用手动输入路径。
#[cfg(not(target_os = "macos"))]
pub async fn choose_folder() -> Result<String, String> {
    Err("目录选择对话框仅在 macOS 上受支持，请使用「手动输入路径」".to_string())
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
