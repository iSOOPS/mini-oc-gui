//! Authentication: HTTP Basic + Cookie session.

pub mod basic;
pub mod session;

use std::path::Path;

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
    /// Resolve credentials from environment variables, with optional
    /// fallback to a `.oc-serve-auth.env` file (the same convention used
    /// by the original `oc-serve-start.sh`).
    ///
    /// Resolution order for `OPENCODE_SERVER_PASSWORD`:
    /// 1. `$OPENCODE_SERVER_PASSWORD` in the process environment.
    /// 2. `${auth_env}` file (defaults to `OC_SERVE_AUTH_ENV` env var or
    ///    `./.oc-serve-auth.env` next to the binary).
    ///
    /// `OPENCODE_SERVER_USERNAME` defaults to `opencode`.
    /// `SB_COOKIE_NAME` is optional.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] if no password can be resolved from
    /// either source.
    pub fn from_env() -> Result<Self, AppError> {
        let auth_env_path = std::env::var("OC_SERVE_AUTH_ENV")
            .ok()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(".oc-serve-auth.env"));

        Self::from_env_with_file(&auth_env_path)
    }

    /// Like [`Self::from_env`] but with an explicit auth-env file path.
    pub fn from_env_with_file(auth_env: &Path) -> Result<Self, AppError> {
        // Layer 1: process environment wins.
        let mut user = std::env::var("OPENCODE_SERVER_USERNAME").ok();
        let mut password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
        let mut cookie_name = std::env::var("SB_COOKIE_NAME").ok();

        // Layer 2: fall back to `.oc-serve-auth.env` for any missing value.
        if (user.is_none() || password.is_none() || cookie_name.is_none())
            && auth_env.is_file()
        {
            let contents = std::fs::read_to_string(auth_env).map_err(|e| {
                AppError::Internal(format!(
                    "OPENCODE_SERVER_PASSWORD not set; could not read {}: {e}",
                    auth_env.display()
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
            AppError::Internal(format!(
                "OPENCODE_SERVER_PASSWORD not set; set it in env or create {} with \
                 OPENCODE_SERVER_USERNAME=...\nOPENCODE_SERVER_PASSWORD=...",
                auth_env.display()
            ))
        })?;

        Ok(Self {
            basic_user,
            basic_password,
            sb_cookie_name: cookie_name,
        })
    }
}
