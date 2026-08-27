//! Step 2: oh-my-openagent update (bun preferred, npm fallback).

use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::error::AppError;
use crate::upgrade::{UpgradeResult, resolve_command};

/// Detect `bun` on PATH (or in well-known install locations).
#[must_use]
pub fn detect_bun() -> Option<PathBuf> {
    // 1. 标准 PATH/PATHEXT 查找 —— Windows 上能正确解析 `bun.exe` 和 `bun.cmd`
    //    (npm 全局装的 PATHEXT shim)。`Command::new` 不查 PATHEXT，所以这里
    //    必须用 resolve_command 而非裸 `Command::new("bun")`。
    if let Ok(p) = resolve_command("bun") {
        return Some(p);
    }

    // 2. 已知安装位置（bun 经常装在 ~/.bun/bin/ 但未必在 PATH）。
    //    Windows 下 `$HOME` 不一定存在，退回到 `$USERPROFILE`。
    for candidate in [
        "$HOME/.bun/bin/bun",
        "/opt/homebrew/bin/bun",
        "/usr/local/bin/bun",
    ] {
        let resolved = if let Some(rest) = candidate.strip_prefix("$HOME/") {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))?;
            PathBuf::from(home).join(rest)
        } else {
            PathBuf::from(candidate)
        };
        if resolved.is_file() {
            return Some(resolved);
        }
    }

    None
}

/// Detect `npm` on PATH.
#[must_use]
pub fn detect_npm() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for candidate_name in npm_candidate_names() {
            let candidate = dir.join(candidate_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Candidate binary names to look for when resolving `npm` on PATH.
fn npm_candidate_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["npm.cmd", "npm"]
    }
    #[cfg(not(windows))]
    {
        &["npm"]
    }
}

/// Upgrade oh-my-openagent, preferring bun over npm/npx.
///
/// # Errors
/// Returns [`AppError::Internal`] if neither bun nor npm is available, or
/// if both upgrade attempts fail.
pub async fn upgrade_omo(
    oc_config_dir: &PathBuf,
    oc_cache_dir: &PathBuf,
) -> Result<UpgradeResult, AppError> {
    if let Some(bun) = detect_bun() {
        tracing::info!("detected bun at {}", bun.display());
        tracing::info!("updating {}/node_modules/oh-my-openagent", oc_config_dir.display());
        if oc_config_dir.is_dir() {
            let status = Command::new(&bun)
                .arg("add")
                .arg("--cwd")
                .arg(oc_config_dir)
                .arg("oh-my-openagent@latest")
                .status()
                .await
                .map_err(|e| AppError::Internal(format!("bun add failed: {e}")))?;
            if status.success() {
                return Ok(UpgradeResult::Upgraded);
            }
            tracing::warn!("bun add failed; falling back to npm");
        }
    }

    let Some(npm) = detect_npm() else {
        return Err(AppError::Internal(
            "neither bun nor npm available; cannot upgrade omo".to_string(),
        ));
    };

    // Clean stale package cache.
    let packages_dir = oc_cache_dir.join("packages");
    if packages_dir.is_dir() {
        for prefix in ["oh-my-openagent", "oh-my-opencode"] {
            if let Ok(entries) = std::fs::read_dir(&packages_dir) {
                for e in entries.flatten() {
                    if e.file_name()
                        .to_string_lossy()
                        .starts_with(prefix)
                    {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            }
        }
    }

    // Windows 上 `npx` 通常是 npm 全局装的 `npx.cmd` shim；和 opencode 走
    // 同样的 CreateProcess 不查 PATHEXT 陷阱，先 resolve_command 拿到真实路径。
    let npx_path = resolve_command("npx")?;
    let npx_probe = timeout(
        Duration::from_secs(120),
        Command::new(&npx_path)
            .arg("--yes")
            .arg("oh-my-openagent@latest")
            .arg("version")
            .status(),
    )
    .await
    .map_err(|_| AppError::Internal("npx version probe timed out after 120s".to_string()))?
    .map_err(|e| AppError::Internal(format!("npx version probe failed: {e}")))?;
    if !npx_probe.success() {
        tracing::warn!("npx oh-my-openagent@latest version exited non-zero");
    }

    let node_modules = oc_config_dir.join("node_modules").join("oh-my-openagent");
    if node_modules.exists() {
        let status = Command::new(&npm)
            .arg("install")
            .arg("oh-my-openagent@latest")
            .arg("--save")
            .current_dir(oc_config_dir)
            .status()
            .await
            .map_err(|e| AppError::Internal(format!("npm install failed: {e}")))?;
        if status.success() {
            Ok(UpgradeResult::Upgraded)
        } else {
            Ok(UpgradeResult::Failed(
                "npm install exited non-zero; omo @latest will still auto-resolve on next start".to_string(),
            ))
        }
    } else {
        Ok(UpgradeResult::AlreadyLatest)
    }
}
