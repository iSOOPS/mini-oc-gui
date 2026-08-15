//! Platform-aware resolution of the bundled `rathole` binary + config.
//!
//! `rathole` is a platform-specific native binary (macOS arm64 `.bin`,
//! Windows `.exe`, …). The serve binary ships with a `rathole/` bundle at
//! the project root that separates binaries per platform:
//!
//! ```text
//! rathole/
//! ├── bin/
//! │   ├── macos/rathole      # macOS (aarch64-apple-darwin)
//! │   └── windows/rathole.exe # Windows (x86_64-pc-windows-msvc)
//! └── settings/*.toml         # tunnel configs (33-/40-/41- prefix)
//! ```
//!
//! The default path is chosen at compile time via [`std::env::consts::OS`],
//! so a macOS build resolves the macOS binary and a Windows build resolves
//! `rathole.exe` — no runtime platform sniffing needed. Both defaults can be
//! overridden at runtime with the `RATHOLE_BIN` / `RATHOLE_CONFIG`
//! environment variables (see [`default_bin`] / [`default_config`]).

/// Subdirectory of `rathole/bin/` holding this platform's binary.
#[cfg(target_os = "windows")]
const PLATFORM_DIR: &str = "windows";
#[cfg(target_os = "macos")]
const PLATFORM_DIR: &str = "macos";
#[cfg(target_os = "linux")]
const PLATFORM_DIR: &str = "linux";
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
const PLATFORM_DIR: &str = "unknown";

/// Binary file name on this platform (Windows appends `.exe`).
#[cfg(target_os = "windows")]
const BIN_NAME: &str = "rathole.exe";
#[cfg(not(target_os = "windows"))]
const BIN_NAME: &str = "rathole";

/// Default tunnel config (relative to the serve working directory).
const DEFAULT_CONFIG: &str = "rathole/settings/33-9464.toml";

/// Resolve the rathole binary path for the current platform.
///
/// Precedence: `RATHOLE_BIN` env var, then the bundled platform-specific
/// path `rathole/bin/<os>/<binary>`.
#[must_use]
pub fn default_bin() -> String {
    std::env::var("RATHOLE_BIN")
        .unwrap_or_else(|_| format!("rathole/bin/{PLATFORM_DIR}/{BIN_NAME}"))
}

/// Resolve the rathole tunnel config path.
///
/// Precedence: `RATHOLE_CONFIG` env var, then `rathole/settings/33-9464.toml`.
#[must_use]
pub fn default_config() -> String {
    std::env::var("RATHOLE_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_name_matches_platform() {
        if cfg!(target_os = "windows") {
            assert_eq!(BIN_NAME, "rathole.exe");
        } else {
            assert_eq!(BIN_NAME, "rathole");
        }
    }

    #[test]
    fn default_config_is_stable() {
        assert_eq!(default_config(), "rathole/settings/33-9464.toml");
    }

    #[test]
    fn default_bin_points_into_bundle() {
        // 无 RATHOLE_BIN 环境变量时，必须落到 rathole/bin/<os>/ 下。
        let bin = default_bin();
        assert!(bin.starts_with("rathole/bin/"), "unexpected path: {bin}");
        assert!(bin.ends_with(BIN_NAME), "unexpected binary name: {bin}");
    }
}
