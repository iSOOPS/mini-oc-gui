//! Step 1: `opencode upgrade`.

use std::time::Duration;

use tokio::process::Command;

use crate::error::AppError;
use crate::upgrade::{UpgradeResult, resolve_command};

/// Run `opencode upgrade`, returning `(result, before_version, after_version)`.
///
/// # Errors
/// Returns [`AppError::Internal`] if `opencode` is not on `PATH`, or
/// [`AppError::Io`] if the underlying process fails.
pub async fn upgrade_opencode() -> Result<(UpgradeResult, String, String), AppError> {
    let before = run_version().await?;
    tracing::info!("current OpenCode version: {before}");

    let status = tokio::time::timeout(Duration::from_secs(300), run_upgrade()).await;
    match status {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AppError::Internal(
                "opencode upgrade timed out after 300s".to_string(),
            ));
        }
    }

    let after = run_version().await?;
    let result = if before == after {
        UpgradeResult::AlreadyLatest
    } else {
        UpgradeResult::Upgraded
    };
    Ok((result, before, after))
}

async fn run_version() -> Result<String, AppError> {
    // Windows 上 opencode 通常以 `opencode.cmd` (npm 全局装的 PATHEXT shim) 而非
    // `opencode.exe` 存在；`Command::new` 走 CreateProcess，不查 PATHEXT，会报
    // "program not found"。`resolve_command` 会优先调 `where.exe`，否则手动遍历
    // PATH + PATHEXT，确保能找到 shim。Unix 上只是把名字透传。
    let bin = resolve_command("opencode")?;
    let out = Command::new(&bin)
        .arg("--version")
        .output()
        .await
        .map_err(|e| AppError::Internal(format!("opencode --version failed: {e}")))?;
    if !out.status.success() {
        return Err(AppError::Internal(format!(
            "opencode --version exited with status {}",
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn run_upgrade() -> Result<(), AppError> {
    let bin = resolve_command("opencode")?;
    let out = Command::new(&bin)
        .arg("upgrade")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| AppError::Internal(format!("opencode upgrade spawn failed: {e}")))?;
    if !out.stdout.is_empty() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            tracing::info!("opencode: {line}");
        }
    }
    if !out.stderr.is_empty() {
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            tracing::warn!("opencode: {line}");
        }
    }
    if !out.status.success() {
        return Err(AppError::Internal(format!(
            "opencode upgrade exited with status {}",
            out.status
        )));
    }
    Ok(())
}
