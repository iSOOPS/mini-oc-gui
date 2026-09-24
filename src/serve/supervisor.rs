//! Lifecycle supervisor for `opencode serve` (and optional `rathole`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};

use crate::error::AppError;
use crate::serve::process::{ChildProcess, ProcessSpec, spawn_traced};

/// `opencode serve` 的 HTTP Basic 鉴权凭据（账户 id + 密钥）。
///
/// ## 机制（依据 opencode 官方文档 + 源码核实）
///
/// `opencode serve` **没有** `--usr` / `--username` / `--password` 之类的
/// CLI 鉴权标志；鉴权完全由子进程环境变量控制：
///
/// - `OPENCODE_SERVER_PASSWORD` —— 非空即启用 HTTP Basic Auth（hono
///   `basic-auth` 中间件）；
/// - `OPENCODE_SERVER_USERNAME` —— 用户名，缺省为 `opencode`。
///
/// 来源：
/// - <https://opencode.ai/docs/server/#authentication>
/// - <https://github.com/sst/opencode/blob/dev/packages/opencode/src/server/middleware.ts>
///   （`AuthMiddleware` 直接读取 `Flag.OPENCODE_SERVER_PASSWORD` /
///   `Flag.OPENCODE_SERVER_USERNAME`，即 `process.env` 透传）
///
/// ## 纯数字账户 id 可用性
///
/// 用户名从环境变量读取后**不经任何格式校验**直接传给 basic-auth 比较，
/// Basic Auth 本身只要求 `username:password` 的 base64 编码——因此
/// **纯数字账户 id（如 `123456`）完全可用**。
///
/// ## 与父进程 env 的关系
///
/// 本进程自身也可能持有 `OPENCODE_SERVER_*`（统一 env 文件经 dotenvy 注入，
/// 用于本应用 axum 服务器的鉴权）。[`ProcessSpec::env`] 通过
/// `Command::env` 显式设置的键会**覆盖**子进程继承到的同名键，因此把
/// 账户凭据注入到 serve 子进程后不会与本应用自身的鉴权串味。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeAuth {
    /// Basic Auth 用户名（设置中的账户 id；纯数字亦可）。
    pub username: String,
    /// Basic Auth 密码（设置中的账户密钥）。
    pub password: String,
}

impl ServeAuth {
    /// 用账户 id + 密钥构造鉴权凭据。
    #[must_use]
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }
}

/// Snapshot of supervisor state, suitable for the TUI status panel.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ServeStatus {
    /// PID of `opencode serve` if running.
    pub opencode_pid: Option<u32>,
    /// PID of `rathole` if running.
    pub rathole_pid: Option<u32>,
    /// Configured port.
    pub port: Option<u16>,
    /// When `opencode serve` was started.
    pub started_at: Option<DateTime<Utc>>,
}

/// Coordinates the lifecycle of `opencode serve` + optional `rathole`.
#[derive(Clone)]
pub struct ServeSupervisor {
    children: Arc<Mutex<HashMap<String, ChildProcess>>>,
    status: Arc<Mutex<ServeStatus>>,
    shutdown_tx: Arc<broadcast::Sender<()>>,
}

impl std::fmt::Debug for ServeSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeSupervisor").finish()
    }
}

impl ServeSupervisor {
    /// Construct a fresh, empty supervisor.
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            children: Arc::new(Mutex::new(HashMap::new())),
            status: Arc::new(Mutex::new(ServeStatus::default())),
            shutdown_tx: Arc::new(tx),
        }
    }

    /// Validate that a port is in range and not already in use.
    ///
    /// # Errors
    /// Returns [`AppError::BadRequest`] if the port is invalid, or
    /// [`AppError::Conflict`] if the port is busy.
    pub async fn check_port(port: u16) -> Result<(), AppError> {
        if port == 0 {
            return Err(AppError::BadRequest("port must be in 1..=65535".to_string()));
        }
        if is_port_busy(port).await {
            return Err(AppError::Conflict(format!("port {port} is already in use")));
        }
        Ok(())
    }

    /// 找出占用 `port` 的所有进程 PID，并强杀它们（Windows 用 `taskkill /T /F`，
    /// Unix 用 `kill -9`）。返回成功终止的 PID 列表。
    ///
    /// 设计意图：当用户在 TUI 弹框中确认「端口 X 被占用，是否杀死进程并继续？」
    /// 后调用此函数，把占用进程整树杀掉，再让 `launch_opencode` 重新检查端口
    /// （此时 `is_port_busy` 应返回 false）。
    ///
    /// 实现要点：
    /// - **PID 收集**：Windows 上用 `netstat -ano` + grep `:PORT` → 取最后一列 PID；
    ///   Unix 上用 `lsof -nP -tiTCP:<PORT> -sTCP:LISTEN`。两条命令都是同步阻塞，
    ///   但调用方在 tokio 上下文里（`spawn_blocking` 适合，但命令通常 < 100ms）。
    ///   为避免在 async 上下文中阻塞，这里用 `tokio::process::Command` 异步版本。
    /// - **去重 + 过滤**：多个 PID 可能指向同一进程组，杀两次无害（taskkill /T /F
    ///   已杀进程时返回错误，filter 掉）。
    /// - **不递归等待**：每个 PID 单独 spawn 杀进程命令，不 wait 退出码
    ///   （避免占用进程的子进程还在 spawn 时，taskkill 已 SIGKILL 父进程导致
    ///   孙进程成为孤儿；taskkill /T /F 自身会整树清理）。
    ///
    /// # Errors
    /// 找出占用 `port` 的所有进程 PID 并强杀它们（Windows `taskkill /T /F`，
    /// Unix `kill -9`），返回成功终止的 PID 列表。
    ///
    /// **与上一版的区别**：
    /// 1. `kill_pid_force` 现在严格检查 taskkill exit code + stderr —— 失败的
    ///    进程不会被加入 `killed` 列表，避免上层误判"已杀"。
    /// 2. 杀完后会重试一次 `find_port_listeners` —— 第一次可能漏掉 child PID
    ///    （如 opencode.exe 是 shim 启动的多层进程树，外层父进程退出后，
    ///    内层 node 进程才接管 socket），重试确保不留活口。
    /// 3. 每一步都打 tracing 日志，便于用户复盘 kill 流程。
    ///
    /// 调用方拿到结果后应再调 `is_port_busy` 做最终验证（socket 释放有延迟）。
    pub async fn kill_port_listener(port: u16) -> Vec<u32> {
        tracing::info!(target: "kill_port", "开始清理端口 {port} 占用进程");
        let mut all_killed = Vec::new();
        for attempt in 1..=2u8 {
            let pids = match find_port_listeners(port).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("查询端口 {port} 占用进程失败：{e}");
                    return all_killed;
                }
            };
            if pids.is_empty() {
                tracing::info!("端口 {port} 未发现占用进程（第 {attempt} 次扫描）");
                break;
            }
            tracing::info!(
                "端口 {port} 第 {attempt} 次扫描发现占用进程: {:?}",
                pids
            );
            for pid in pids {
                if all_killed.contains(&pid) {
                    continue;
                }
                match kill_pid_force(port, pid).await {
                    Ok(()) => {
                        all_killed.push(pid);
                        tracing::info!("端口 {port}: 已终止 PID {pid}");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "端口 {port}: 终止 PID {pid} 失败（已记日志，由上层决定是否重试）：{e}"
                        );
                    }
                }
            }
            // 第一次杀完后稍等 socket 释放 + 可能的 child 进程接管。
            if attempt == 1 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        if all_killed.is_empty() {
            tracing::warn!(
                target: "kill_port",
                "端口 {port} 清理完成：未终止任何进程（可能权限不足或 PID 已退出）"
            );
        } else {
            tracing::info!(
                target: "kill_port",
                "端口 {port} 清理完成：已终止 PID {:?}（共 {} 个）",
                all_killed,
                all_killed.len()
            );
        }
        all_killed
    }

    /// Launch `opencode serve --port <port>`, optionally with HTTP Basic
    /// auth injected via child-process env vars (see [`ServeAuth`]).
    ///
    /// # Errors
    /// Returns [`AppError::Conflict`] if the port is busy or
    /// [`AppError::Io`] if the binary cannot be spawned.
    pub async fn launch_opencode(
        &self,
        port: u16,
        auth: Option<&ServeAuth>,
    ) -> Result<u32, AppError> {
        // mutex guard: rathole 在跑时拒绝启动单体（云服务依赖单体,
        // 反向启单体无意义,且会破坏既有 rathole 监听）
        if self.status.lock().await.rathole_pid.is_some() {
            return Err(AppError::Conflict(
                "云服务正在运行,请先停止云服务后再启动单体".to_string(),
            ));
        }
        Self::check_port(port).await?;
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let spec = build_opencode_serve_spec(port, cwd, auth);
        let child = spawn_traced(spec).await?;
        let pid = child.pid;
        self.children.lock().await.insert("opencode".to_string(), child);

        // Wait up to 10s for the process to stay alive.
        if !wait_alive(&self.children, "opencode", 10).await {
            self.children.lock().await.remove("opencode");
            return Err(AppError::Internal(
                "opencode serve did not stay alive after launch".to_string(),
            ));
        }

        let mut status = self.status.lock().await;
        status.opencode_pid = Some(pid);
        status.port = Some(port);
        status.started_at = Some(Utc::now());
        Ok(pid)
    }

    /// Launch the `rathole` tunnel binary with the given config.
    ///
    /// # Errors
    /// Returns [`AppError::BadRequest`] if either path is missing,
    /// or [`AppError::Io`] on spawn failure.
    pub async fn launch_rathole(&self, bin: &str, config: &str) -> Result<u32, AppError> {
        // mutex guard: 已在跑时直接 Conflict,避免重复启
        if self.status.lock().await.rathole_pid.is_some() {
            return Err(AppError::Conflict(
                "rathole 隧道已在运行".to_string(),
            ));
        }
        if !std::path::Path::new(bin).exists() {
            return Err(AppError::BadRequest(format!("rathole binary not found: {bin}")));
        }
        if !std::path::Path::new(config).is_file() {
            return Err(AppError::BadRequest(format!("rathole config not found: {config}")));
        }
        let spec = ProcessSpec::new(bin).arg(config);
        let child = spawn_traced(spec).await?;
        let pid = child.pid;
        self.children.lock().await.insert("rathole".to_string(), child);

        // Bash waits 1.5s then checks; replicate with a 1.5s sleep then probe.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if !wait_alive(&self.children, "rathole", 1).await {
            self.children.lock().await.remove("rathole");
            return Err(AppError::Internal("rathole process exited unexpectedly".to_string()));
        }

        let mut status = self.status.lock().await;
        status.rathole_pid = Some(pid);
        Ok(pid)
    }

    /// Launch the "cloud service" combo: ensure single opencode is up, then stack rathole.
    ///
    /// - rathole already running -> Conflict (combo already live)
    /// - single not running -> launch single first; if that fails, the whole combo fails
    /// - single already running -> skip single launch, just add rathole
    ///
    /// `auth` 透传给单体 opencode serve（`Some` 时注入 Basic Auth 环境变量）。
    ///
    /// Returns `(opencode_pid, rathole_pid)`.
    pub async fn launch_cloud_service(
        &self,
        port: u16,
        bin: &str,
        config: &str,
        auth: Option<&ServeAuth>,
    ) -> Result<(u32, u32), AppError> {
        // 先决条件:云服务已启则拒绝（与 mutex 表保持一致）
        if self.status.lock().await.rathole_pid.is_some() {
            return Err(AppError::Conflict(
                "云服务正在运行,请先停止".to_string(),
            ));
        }
        // 单体若未启,先启单体;失败则整体失败
        let oc_pid = if self.status.lock().await.opencode_pid.is_some() {
            self.status.lock().await.opencode_pid.unwrap()
        } else {
            self.launch_opencode(port, auth).await.map_err(|e| {
                AppError::Conflict(format!(
                    "云服务启动失败:单体 OpenCode 未启动:{e}"
                ))
            })?
        };
        // 叠加 rathole;失败不回滚单体(单体已可用)
        let rt_pid = self.launch_rathole(bin, config).await.map_err(|e| {
            AppError::Conflict(format!(
                "云服务启动失败:单体已启(PID={oc_pid}),但 rathole 启动失败:{e}"
            ))
        })?;
        Ok((oc_pid, rt_pid))
    }

    /// Get a snapshot of the current supervisor status.
    pub async fn status(&self) -> ServeStatus {
        self.status.lock().await.clone()
    }

    /// Subscribe to graceful-shutdown notifications.
    #[must_use]
    pub fn subscribe_shutdown(&self) -> broadcast::Receiver<()> {
        self.shutdown_tx.subscribe()
    }

    /// 停止 opencode serve 进程（若在运行）。
    ///
    /// # Errors
    /// 错误仅记录，不传播 —— 停止是尽力而为。
    pub async fn stop_opencode(&self) -> Result<(), AppError> {
        tracing::info!("stopping opencode serve");
        let child = { self.children.lock().await.remove("opencode") };
        if let Some(mut c) = child {
            terminate_gracefully(&mut c.child).await;
        }
        let mut status = self.status.lock().await;
        status.opencode_pid = None;
        tracing::info!("opencode serve stopped");
        Ok(())
    }

    /// 停止 rathole 进程（若在运行）。
    ///
    /// # Errors
    /// 错误仅记录，不传播 —— 停止是尽力而为。
    pub async fn stop_rathole(&self) -> Result<(), AppError> {
        tracing::info!("stopping rathole");
        let child = { self.children.lock().await.remove("rathole") };
        if let Some(mut c) = child {
            terminate_gracefully(&mut c.child).await;
        }
        let mut status = self.status.lock().await;
        status.rathole_pid = None;
        tracing::info!("rathole stopped");
        Ok(())
    }

    /// 停止所有子进程（opencode + rathole）。
    pub async fn stop_all(&self) -> Result<(), AppError> {
        self.stop_opencode().await?;
        self.stop_rathole().await?;
        Ok(())
    }

    /// Graceful shutdown: SIGTERM (Unix) / TerminateProcess (Windows), then
    /// SIGKILL after a 5-second grace period.
    ///
    /// # Errors
    /// Errors are logged but not propagated — shutdown is best-effort.
    pub async fn shutdown(&self) -> Result<(), AppError> {
        let _ = self.shutdown_tx.send(());
        self.stop_all().await
    }
}

impl Default for ServeSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// 构造 `opencode serve --port <port>` 的 [`ProcessSpec`]（跨平台统一）。
///
/// `auth` 为 `Some` 时注入 `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`
/// 两个子进程环境变量 —— 这是 opencode serve 启用 HTTP Basic Auth 的**唯一**
/// 官方机制（无 CLI 标志；见 [`ServeAuth`] 文档）。`Command::env` 显式设置的键
/// 覆盖子进程继承到的同名键，因此父进程 env 中的旧值不会泄漏进 serve。
///
/// 解析路径的优先级：
/// 1. 环境变量 `OPENCODE_BIN`（绝对路径，跳过任何解析）。
/// 2. `crate::upgrade::resolve_command("opencode")`：Windows 上调用
///    `where.exe` + PATHEXT 解析 npm 生成的 `.cmd` / `.exe` shim 拿到
///    绝对路径；Unix 上直接返回裸名（execvp 自动解析）。
/// 3. 兜底用裸名 `"opencode"` —— spawn 直接失败时给出明确错误信息。
///
/// 为什么不走 PowerShell 包装：之前用 `powershell -Command "opencode serve ..."`
/// 是为了绕开 npm `.cmd` shim 的 quoting 问题，但副作用太多：
/// * PowerShell 进程自身是 `/SUBSYSTEM:CONSOLE` 程序，**必然**创建一个可见
///   控制台窗口，即便 stdio 已被 Rust 进程 pipe 走；
/// * 窗口里没有内容显示（输出全部流到 Rust 这边）→ Windows 控制台 buffer
///   长期无写入 → 渲染循环停摆 → 用户感觉"切换窗口才能刷新"；
/// * 多一层 .NET runtime 启动开销 + 进程树深一层（`mini-oc-gui → powershell
///   → cmd → node → opencode`），kill 链也跟着变长。
///
/// 现在改用 `resolve_command` 直接拿绝对路径，配合 `serve::process` 的
/// `CREATE_NO_WINDOW` flag，从源头消除中间进程和可见窗口。
///
/// 若用户希望走 PowerShell 包装以获得最严格的 shell quoting 兼容，可显式
/// 设置 `OPENCODE_BIN` 指向 `powershell.exe` 并自己组织参数 —— 本函数不
/// 再提供 PowerShell 默认行为。
fn build_opencode_serve_spec(port: u16, cwd: String, auth: Option<&ServeAuth>) -> ProcessSpec {
    let bin = std::env::var("OPENCODE_BIN")
        .ok()
        .map(PathBuf::from)
        .or_else(|| crate::upgrade::resolve_command("opencode").ok())
        .unwrap_or_else(|| PathBuf::from("opencode"));
    let mut spec = ProcessSpec::new(bin.to_string_lossy().into_owned())
        .arg("serve")
        .arg("--port")
        .arg(port.to_string())
        .cwd(cwd);
    if let Some(auth) = auth {
        spec = spec
            .env("OPENCODE_SERVER_USERNAME", auth.username.clone())
            .env("OPENCODE_SERVER_PASSWORD", auth.password.clone());
    }
    spec
}

async fn wait_alive(
    children: &Arc<Mutex<HashMap<String, ChildProcess>>>,
    key: &str,
    max_seconds: u64,
) -> bool {
    for _ in 0..max_seconds {
        // Take a peek inside the lock then drop it before sleeping so we
        // don't hold the mutex across an `.await`.
        let alive = {
            let mut map = children.lock().await;
            match map.get_mut(key) {
                Some(child) => child.is_alive(),
                None => return false,
            }
        };
        if alive {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn is_port_busy(port: u16) -> bool {
    // Best-effort: try `lsof` (Unix). If lsof returns 0 + non-empty output,
    // something is listening. If it returns 1 (not found) or errors, fall
    // through to a TCP connect probe.
    #[cfg(unix)]
    {
        if let Ok(out) = std::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .output()
        {
            if out.status.success() && !out.stdout.is_empty() {
                return true;
            }
        }
    }
    // Fallback: try a TCP connect to 127.0.0.1:<port>.
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// Cross-platform graceful terminate: SIGTERM + 5s grace + SIGKILL on Unix,
/// process-tree kill on Windows.
async fn terminate_gracefully(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        if let Some(pid) = child.id() {
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(_) => return,
            }
        }
    }

    // Windows: `Child::kill` only calls TerminateProcess on the *direct*
    // child. `opencode serve` (a Node process) commonly spawns its own
    // children that keep the port bound, so killing the parent alone leaks
    // the port. `taskkill /T /F` walks and force-kills the whole tree, which
    // is what actually releases the listener.
    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            if let Ok(out) = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output()
            {
                // info（非 debug）：TUI 日志面板默认 filter 是 info，
                // 让「已终止 PID …」在面板可见；stdio 已 piped，不会泄漏
                // 到终端画面。
                tracing::info!(
                    "taskkill /T /F {pid}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                );
            }
        }
    }

    let _ = child.kill().await;
}

/// 列出正在监听 `port` 的所有进程 PID。供 [`ServeSupervisor::kill_port_listener`] 使用。
///
/// - Windows：`netstat -ano` 输出用 GBK/UTF-8 解码（中文 Windows 默认 GBK；
///   PowerShell 7+ 的 netstat 通常输出 UTF-8；逐字节解析不需要关心编码）。
/// - Unix：`lsof -nP -tiTCP:<PORT> -sTCP:LISTEN` 输出是 PID 列表（每行一个）。
///
/// 两个平台都用 `tokio::process::Command` 异步跑，避免阻塞 TUI 事件循环。
async fn find_port_listeners(port: u16) -> Result<Vec<u32>, AppError> {
    #[cfg(windows)]
    {
        let out = tokio::process::Command::new("netstat")
            .args(["-ano", "-p", "TCP"])
            .output()
            .await
            .map_err(|e| AppError::Io(std::io::Error::other(format!("netstat 启动失败：{e}"))))?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!(":{}", port);
        let mut pids = std::collections::HashSet::new();
        for line in text.lines() {
            // netstat -ano 输出行格式：`  TCP    0.0.0.0:9464    0.0.0.0:0    LISTENING    1234`
            // 取最后一列作 PID；只取 LISTENING 状态的行（避免误伤已建立的连接）。
            let upper = line.to_ascii_uppercase();
            if !upper.contains("LISTENING") {
                continue;
            }
            if !line.contains(&needle) {
                continue;
            }
            if let Some(pid_str) = line.split_whitespace().last() {
                if let Ok(pid) = pid_str.parse::<u32>() {
                    if pid > 0 {
                        pids.insert(pid);
                    }
                }
            }
        }
        Ok(pids.into_iter().collect())
    }
    #[cfg(unix)]
    {
        let out = tokio::process::Command::new("lsof")
            .args([
                "-nP",
                &format!("-iTCP:{}", port),
                "-sTCP:LISTEN",
                "-t", // terse: only PIDs
            ])
            .output()
            .await
            .map_err(|e| AppError::Io(std::io::Error::other(format!("lsof 启动失败：{e}"))))?;
        if !out.status.success() {
            return Ok(Vec::new());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut pids = Vec::new();
        for line in text.lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                if pid > 0 {
                    pids.push(pid);
                }
            }
        }
        Ok(pids)
    }
}

/// 强杀单个 PID（进程树）。Windows 走 `taskkill /T /F`，Unix 走 `kill -9`。
///
/// **关键**：必须检查 exit code + stderr！taskkill 把成功信息写 stdout、
/// 错误信息写 stderr；旧实现只读 stdout、忽略退出码，导致 taskkill 失败
/// 时（如权限不足、PID 不存在）仍返回 Ok，上层以为杀成功了。
///
/// 返回 `Ok(())` 表示进程**确实**被终止（taskkill exit code 0）。
/// 返回 `Err(...)` 表示 kill 命令失败，错误消息包含 stdout + stderr 摘要
/// 供上层日志和用户提示使用。
async fn kill_pid_force(port: u16, pid: u32) -> Result<(), AppError> {
    #[cfg(windows)]
    {
        let out = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| AppError::Io(std::io::Error::other(format!("taskkill 启动失败：{e}"))))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let exit_code = out.status.code();
        if out.status.success() {
            tracing::info!("taskkill /T /F PID {pid} (port {port}) 成功: {stdout}");
            Ok(())
        } else {
            // 把 stderr 当作主要错误源（taskkill 把错误信息写 stderr），
            // 若 stderr 空则用 exit code。
            let detail = if !stderr.is_empty() {
                stderr
            } else if !stdout.is_empty() {
                stdout
            } else {
                format!("exit code {:?}", exit_code)
            };
            tracing::warn!(
                "taskkill /T /F PID {pid} (port {port}) 失败 (exit={:?}): {detail}",
                exit_code
            );
            Err(AppError::Internal(format!(
                "taskkill /T /F PID {pid} 失败：{detail}"
            )))
        }
    }
    #[cfg(unix)]
    {
        let out = tokio::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| AppError::Io(std::io::Error::other(format!("kill 启动失败：{e}"))))?;
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if out.status.success() {
            tracing::info!("kill -9 PID {pid} (port {port}) 成功");
            Ok(())
        } else {
            let detail = if !stderr.is_empty() {
                stderr
            } else {
                format!("exit code {:?}", out.status.code())
            };
            tracing::warn!("kill -9 PID {pid} (port {port}) 失败: {detail}");
            Err(AppError::Internal(format!(
                "kill -9 PID {pid} 失败：{detail}"
            )))
        }
    }
}

#[cfg(test)]
impl ServeSupervisor {
    /// Test helper: read the current status snapshot without spawning.
    pub async fn status_for_test(&self) -> ServeStatus {
        self.status.lock().await.clone()
    }

    /// Test helper: overwrite the status snapshot (e.g. simulate running children).
    pub async fn set_status_for_test(&self, s: ServeStatus) {
        *self.status.lock().await = s;
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn launch_opencode_rejects_when_rathole_already_running() {
        let sup = ServeSupervisor::new();
        let mut s = sup.status_for_test().await;
        s.rathole_pid = Some(9999);
        sup.set_status_for_test(s).await;

        let result = sup.launch_opencode(9464, None).await;
        assert!(matches!(result, Err(AppError::Conflict(_))));
    }

    #[tokio::test]
    async fn launch_rathole_rejects_when_already_running() {
        let sup = ServeSupervisor::new();
        let mut s = sup.status_for_test().await;
        s.rathole_pid = Some(8888);
        sup.set_status_for_test(s).await;

        let result = sup.launch_rathole("nonexistent-bin", "nonexistent.toml").await;
        assert!(matches!(result, Err(AppError::Conflict(_))));
    }

    #[tokio::test]
    async fn launch_cloud_service_rejects_when_rathole_already_running() {
        let sup = ServeSupervisor::new();
        let mut s = sup.status_for_test().await;
        s.rathole_pid = Some(7777);
        sup.set_status_for_test(s).await;

        let result = sup.launch_cloud_service(9464, "nonexistent-bin", "nonexistent.toml", None).await;
        assert!(matches!(result, Err(AppError::Conflict(_))));
    }

    #[tokio::test]
    async fn launch_cloud_service_skips_opencode_when_already_running() {
        // single already running -> skip opencode launch, go to rathole branch.
        // rathole binary nonexistent -> rathole launch fails with BadRequest
        // (mapped to Conflict by the combo wrapper). The point of this test:
        // confirm we did NOT bail with the "rathole already running" guard.
        let sup = ServeSupervisor::new();
        let mut s = sup.status_for_test().await;
        s.opencode_pid = Some(5555);
        sup.set_status_for_test(s).await;

        let result = sup.launch_cloud_service(9464, "nonexistent-bin", "nonexistent.toml", None).await;
        // combo wrapper maps rathole sub-failure to Conflict with
        // "云服务启动失败:单体已启" prefix. So we DO get Conflict —
        // but the message must indicate the rathole sub-failure, not the
        // "rathole already running" guard.
        match result {
            Err(AppError::Conflict(msg)) => {
                assert!(
                    msg.contains("rathole") || msg.contains("单体已启"),
                    "expected rathole sub-failure Conflict, got: {msg}"
                );
                assert!(
                    !msg.contains("已在运行"),
                    "got wrong Conflict reason (rathole-already-running): {msg}"
                );
            }
            other => panic!("expected Conflict with rathole sub-failure msg, got: {other:?}"),
        }
    }

    // --- build_opencode_serve_spec 的鉴权环境变量注入 ---

    fn spec_env(spec: &ProcessSpec, key: &str) -> Option<&str> {
        spec.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn build_opencode_serve_spec_injects_auth_env_vars() {
        let auth = ServeAuth::new("123456", "k789");
        let spec = build_opencode_serve_spec(9464, ".".to_string(), Some(&auth));
        // 账户 id 为纯数字时也必须原样注入（opencode serve 对用户名无格式校验）。
        assert_eq!(spec_env(&spec, "OPENCODE_SERVER_USERNAME"), Some("123456"));
        assert_eq!(spec_env(&spec, "OPENCODE_SERVER_PASSWORD"), Some("k789"));
        // 基本参数不受影响。
        assert_eq!(spec.args, vec!["serve", "--port", "9464"]);
    }

    #[test]
    fn build_opencode_serve_spec_without_auth_has_no_env_vars() {
        // auth=None（账户未配置）时不注入 —— serve 行为退回继承父进程 env，
        // 与旧版本完全一致。
        let spec = build_opencode_serve_spec(9464, ".".to_string(), None);
        assert!(spec_env(&spec, "OPENCODE_SERVER_USERNAME").is_none());
        assert!(spec_env(&spec, "OPENCODE_SERVER_PASSWORD").is_none());
    }
}
