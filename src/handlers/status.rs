//! `GET /status` — unauthenticated supervisor state snapshot.
//!
//! 外部探针 / k8s liveness / 监控脚本可通过此端点查询子进程运行状态。

use axum::{Json, extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::handlers::AppState;

/// `/status` 响应体。字段命名 camelCase 与 spec §2.2 一致。
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// "running" if `opencode serve` 进程在跑，否则 "stopped"
    pub opencode_serve: &'static str,
    /// "running" if rathole 隧道在跑，否则 "stopped"
    pub rathole: &'static str,
    /// 子进程首次启动时间；都没有运行则为 None
    pub started_at: Option<DateTime<Utc>>,
    /// 主机 / 操作系统信息
    pub system: SystemInfo,
    /// 服务二进制版本
    pub version: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    /// 编译期 OS：windows | macos | linux | unknown
    pub os: &'static str,
    /// 编译期架构（来自 std::env::consts::ARCH）
    pub arch: &'static str,
    /// 运行时 hostname，失败 fallback "unknown"
    pub hostname: String,
}

#[tracing::instrument(skip_all)]
pub async fn status(
    State(state): State<AppState>,
) -> (StatusCode, Json<StatusResponse>) {
    let snap = state.supervisor.status().await;
    let opencode_serve = if snap.opencode_pid.is_some() {
        "running"
    } else {
        "stopped"
    };
    let rathole = if snap.rathole_pid.is_some() {
        "running"
    } else {
        "stopped"
    };

    let system = SystemInfo {
        os: pctype(),
        arch: std::env::consts::ARCH,
        hostname: whoami::fallible::hostname().unwrap_or_else(|_| "unknown".into()),
    };

    (
        StatusCode::OK,
        Json(StatusResponse {
            opencode_serve,
            rathole,
            started_at: snap.started_at,
            system,
            version: env!("CARGO_PKG_VERSION"),
        }),
    )
}

fn pctype() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::ServeSupervisor;
    use std::sync::Arc;

    #[tokio::test]
    async fn status_returns_stopped_when_no_children() {
        let supervisor = Arc::new(ServeSupervisor::new());
        let state = crate::handlers::AppState::test_stub(supervisor);
        let (code, Json(body)) = status(State(state)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.opencode_serve, "stopped");
        assert_eq!(body.rathole, "stopped");
        assert!(body.started_at.is_none());
        assert!(!body.system.hostname.is_empty());
    }

    #[tokio::test]
    async fn status_reports_running_pids() {
        let supervisor = Arc::new(ServeSupervisor::new());
        {
            let mut s = supervisor.status_for_test().await;
            s.opencode_pid = Some(12345);
            s.rathole_pid = Some(67890);
            s.started_at = Some(chrono::Utc::now());
            supervisor.set_status_for_test(s).await;
        }
        let state = crate::handlers::AppState::test_stub(supervisor);
        let (_, Json(body)) = status(State(state)).await;
        assert_eq!(body.opencode_serve, "running");
        assert_eq!(body.rathole, "running");
        assert!(body.started_at.is_some());
    }

    #[tokio::test]
    async fn status_includes_system_info() {
        let supervisor = Arc::new(ServeSupervisor::new());
        let state = crate::handlers::AppState::test_stub(supervisor);
        let (_, Json(body)) = status(State(state)).await;
        assert!(matches!(
            body.system.os,
            "windows" | "macos" | "linux" | "unknown"
        ));
        assert!(!body.system.arch.is_empty());
    }
}
