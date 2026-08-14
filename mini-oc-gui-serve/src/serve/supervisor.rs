//! Lifecycle supervisor for `opencode serve` (and optional `rathole`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};

use crate::error::AppError;
use crate::serve::process::{ChildProcess, ProcessSpec, spawn_traced};

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

    /// Launch `opencode serve --port <port>`. Returns the PID.
    ///
    /// # Errors
    /// Returns [`AppError::Conflict`] if the port is busy or
    /// [`AppError::Io`] if the binary cannot be spawned.
    pub async fn launch_opencode(&self, port: u16) -> Result<u32, AppError> {
        Self::check_port(port).await?;
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let spec = ProcessSpec::new("opencode")
            .arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .cwd(cwd);
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
                tracing::debug!(
                    "taskkill /T /F {pid}: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                );
            }
        }
    }

    let _ = child.kill().await;
}
