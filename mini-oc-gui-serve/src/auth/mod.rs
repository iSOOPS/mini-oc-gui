//! Authentication: HTTP Basic + Cookie session.

pub mod basic;
pub mod session;

use std::path::{Path, PathBuf};

use crate::error::AppError;

/// Resolved authentication configuration for the HTTP layer.
///
/// Loaded from environment variables via [`AuthConfig::from_env`]. Inserted
/// into request extensions by an auth layer so that extractors in
/// [`basic::BasicAuth`] and [`session::SessionAuth`] can read it.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// HTTP Basic auth username.
    pub basic_user: String,
    /// HTTP Basic auth password.
    pub basic_password: String,
    /// Cookie name to look for when validating session auth
    /// (e.g. `auth_md_isoops_com`). `None` disables cookie session auth.
    pub sb_cookie_name: Option<String>,
}

impl AuthConfig {
    /// Resolve credentials from environment variables, with fallback to a
    /// `.oc-serve-auth.env` file (the same convention used by the original
    /// `oc-serve-start.sh`).
    ///
    /// Resolution order for `OPENCODE_SERVER_PASSWORD`:
    /// 1. `$OPENCODE_SERVER_PASSWORD` in the process environment.
    /// 2. The file at `--auth-env <PATH>`, `$OC_SERVE_AUTH_ENV`,
    ///    `<exe_dir>/.oc-serve-auth.env`, or `./.oc-serve-auth.env` —
    ///    the **first** file that exists is used.
    ///
    /// `OPENCODE_SERVER_USERNAME` defaults to `opencode`.
    /// `SB_COOKIE_NAME` is optional.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] if no password can be resolved from
    /// either source. The error message prints every candidate path that
    /// was checked so the user can place the file or set the env var.
    pub fn from_env() -> Result<Self, AppError> {
        let (resolved_path, candidates) = resolve_default_auth_env_path();
        Self::from_env_with_file_reporting(&resolved_path, &candidates)
    }

    /// Like [`Self::from_env`] but with an explicit auth-env file path.
    pub fn from_env_with_file(auth_env: &Path) -> Result<Self, AppError> {
        let candidates = vec![auth_env.to_path_buf()];
        Self::from_env_with_file_reporting(auth_env, &candidates)
    }

    fn from_env_with_file_reporting(
        resolved: &Path,
        candidates: &[PathBuf],
    ) -> Result<Self, AppError> {
        // Layer 1: process environment wins.
        let mut user = std::env::var("OPENCODE_SERVER_USERNAME").ok();
        let mut password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
        let mut cookie_name = std::env::var("SB_COOKIE_NAME").ok();

        // Layer 2: fall back to the resolved `.oc-serve-auth.env` for any
        // missing value.
        if (user.is_none() || password.is_none() || cookie_name.is_none())
            && resolved.is_file()
        {
            let contents = std::fs::read_to_string(resolved).map_err(|e| {
                AppError::Internal(format!(
                    "OPENCODE_SERVER_PASSWORD not set; could not read {}: {e}",
                    resolved.display()
                ))
            })?;
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    match k {
                        "OPENCODE_SERVER_USERNAME" if user.is_none() => user = Some(v.to_string()),
                        "OPENCODE_SERVER_PASSWORD" if password.is_none() => {
                            password = Some(v.to_string())
                        }
                        "SB_COOKIE_NAME" if cookie_name.is_none() => {
                            cookie_name = Some(v.to_string())
                        }
                        "OPENCODE_SERVER_USERNAME"
                        | "OPENCODE_SERVER_PASSWORD"
                        | "SB_COOKIE_NAME" => {}
                        _ => {}
                    }
                }
            }
        }

        let basic_user = user.unwrap_or_else(|| "opencode".to_string());
        let basic_password = password.ok_or_else(|| {
            let mut msg = String::from(
                "OPENCODE_SERVER_PASSWORD not set; set $OPENCODE_SERVER_PASSWORD in env, or \
                 create one of these files with OPENCODE_SERVER_USERNAME=... and \
                 OPENCODE_SERVER_PASSWORD=...:\n",
            );
            for c in candidates {
                msg.push_str(&format!("  - {}\n", c.display()));
            }
            AppError::Internal(msg.trim_end().to_string())
        })?;

        Ok(Self {
            basic_user,
            basic_password,
            sb_cookie_name: cookie_name,
        })
    }
}

/// Resolve the auth-env file path used by [`AuthConfig::from_env`].
///
/// Search order (first existing file wins; otherwise the exe-adjacent path
/// is returned so the error message points somewhere predictable):
///
/// 1. `$OC_SERVE_AUTH_ENV` if set.
/// 2. `<exe_dir>/.oc-serve-auth.env` — works when the binary is launched
///    by double-clicking the .exe (the cwd is then `target\release` or
///    the install dir, not the project root).
/// 3. `./.oc-serve-auth.env` (current working directory).
///
/// Returns `(resolved_path, all_candidates)`. `all_candidates` is included
/// in the error message so the user knows exactly which paths were
/// checked.
fn resolve_default_auth_env_path() -> (PathBuf, Vec<PathBuf>) {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 1. Explicit env var wins.
    if let Ok(p) = std::env::var("OC_SERVE_AUTH_ENV") {
        let path = PathBuf::from(p);
        candidates.push(path.clone());
        if path.is_file() {
            return (path, candidates);
        }
    }

    // 2. Exe-adjacent path. `std::env::current_exe` always returns the
    //    path of the running .exe (or the test binary), never panics.
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    if let Some(dir) = exe_dir {
        let path = dir.join(".oc-serve-auth.env");
        candidates.push(path.clone());
        if path.is_file() {
            return (path, candidates);
        }
    }

    // 3. Cwd-relative fallback.
    let cwd_path = PathBuf::from(".oc-serve-auth.env");
    candidates.push(cwd_path.clone());
    (cwd_path, candidates)
}
