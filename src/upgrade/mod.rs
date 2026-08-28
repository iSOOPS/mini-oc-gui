//! Upgrade orchestrator — `opencode upgrade` + `oh-my-openagent` update.

pub mod omo;
pub mod opencode;

pub use omo::{detect_bun, detect_npm, upgrade_omo};
pub use opencode::upgrade_opencode;

use std::path::PathBuf;

use crate::error::AppError;

/// Outcome of an upgrade step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeResult {
    /// A version bump was detected after running the upgrade.
    Upgraded,
    /// No version change after the upgrade attempt.
    AlreadyLatest,
    /// The upgrade step failed with a human-readable message.
    Failed(String),
}

impl UpgradeResult {
    /// `true` if the upgrade either succeeded or confirmed we were already
    /// at the latest version.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Upgraded | Self::AlreadyLatest)
    }
}

/// Resolve a bare command name (e.g. `"opencode"`) to its on-disk executable
/// path.  This helper exists because on Windows, `tokio::process::Command::new`
/// (and `std::process::Command::new`) ultimately call Win32 `CreateProcess`,
/// which **does NOT consult `PATHEXT`** — meaning a tool installed via
/// `npm install -g` (which writes `opencode.cmd` / `npx.cmd` / `bun.cmd` …
/// shims into a PATH directory) will report `program not found` even though
/// `cmd.exe` (which *does* honour `PATHEXT`) can find it just fine.
///
/// Windows strategy:
///   1. Run `where.exe <name>` — the Windows-native resolver that *does*
///      apply `PATHEXT` and prints the resolved path(s) on stdout.
///      **Important**: `where.exe` 不只列 PATHEXT 命中 —— nvm4w / Git Bash
///      等会在 PATH 里放"伪可执行"（如 `C:\nvm4w\nodejs\opencode`，一个
///      `#!/bin/sh` 开头、几百字节的 bash shim）。这些**没有** PATHEXT
///      后缀，CreateProcess 直接启动会报 `ERROR_BAD_EXE_FORMAT` (193
///      "不是有效的 exe 程序")。所以这里必须**按 PATHEXT 后缀过滤**
///      where.exe 的结果，只接受 Windows 原生能识别的可执行格式。
///   2. (Fallback) Walk `PATH` manually, appending each `PATHEXT` suffix and
///      checking `is_file()`. Used when `where.exe` itself cannot be reached
///      (e.g., minimal Windows containers without it on PATH).
///
/// On Unix, the OS does PATH lookup via `execvp` on `Command::new` — we just
/// pass the bare name through; downstream `Command::new` will resolve it.
pub fn resolve_command(name: &str) -> Result<PathBuf, AppError> {
    #[cfg(windows)]
    {
        // PATHEXT 列表（用于过滤 where.exe 的结果 + Step 2 手动遍历）。
        let exts: Vec<String> = std::env::var_os("PATHEXT")
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();

        // Step 1: try the native resolver, **filtering by PATHEXT suffix**.
        // `where.exe` may emit multiple lines; iterate all and pick the first
        // one whose extension matches a Windows-recognised executable type.
        if let Ok(out) = std::process::Command::new("where.exe")
            .arg(name)
            .output()
        {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let lower = line.to_ascii_lowercase();
                    if exts.iter().any(|ext| lower.ends_with(ext)) {
                        return Ok(PathBuf::from(line));
                    }
                    // 命中但扩展名不在 PATHEXT（典型：Git Bash shim），跳过
                    // 继续找下一个匹配。
                }
            }
        }

        // Step 2: walk PATH with PATHEXT manually.
        let path_var = std::env::var_os("PATH").unwrap_or_default();

        for dir in std::env::split_paths(&path_var) {
            for ext in &exts {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }

        Err(AppError::Internal(format!(
            "未在 PATH 中找到 `{name}` (Windows 上 where + PATHEXT 解析都失败；请确认已 `npm install -g` 安装 .cmd shim，或显式设置 OPENCODE_BIN 指向 .exe / .cmd)"
        )))
    }

    #[cfg(not(windows))]
    {
        let _ = name; // silence unused warning on non-Windows
        Ok(PathBuf::from(name))
    }
}
