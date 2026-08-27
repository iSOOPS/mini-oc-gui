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
///   2. (Fallback) Walk `PATH` manually, appending each `PATHEXT` suffix and
///      checking `is_file()`. Used when `where.exe` itself cannot be reached
///      (e.g., minimal Windows containers without it on PATH).
///
/// On Unix, the OS does PATH lookup via `execvp` on `Command::new` — we just
/// pass the bare name through; downstream `Command::new` will resolve it.
pub fn resolve_command(name: &str) -> Result<PathBuf, AppError> {
    #[cfg(windows)]
    {
        // Step 1: try the native resolver.
        if let Ok(out) = std::process::Command::new("where.exe")
            .arg(name)
            .output()
        {
            if out.status.success() {
                // `where` may emit multiple lines (one per hit); take the first.
                if let Some(line) = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .find(|s| !s.is_empty())
                {
                    return Ok(PathBuf::from(line));
                }
            }
        }

        // Step 2: walk PATH with PATHEXT manually.
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        let exts: Vec<String> = std::env::var_os("PATHEXT")
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();

        for dir in std::env::split_paths(&path_var) {
            for ext in &exts {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }

        Err(AppError::Internal(format!(
            "未在 PATH 中找到 `{name}` (Windows 上 where + PATHEXT 解析都失败)"
        )))
    }

    #[cfg(not(windows))]
    {
        let _ = name; // silence unused warning on non-Windows
        Ok(PathBuf::from(name))
    }
}
