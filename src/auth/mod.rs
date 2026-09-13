//! Authentication: HTTP Basic + Cookie session.
//!
//! 认证流（v2）：
//! 1. 用户在首次配置表单登录，服务端调用远端 `/api/user/info`；
//! 2. 从返回的用户信息（`id`, `name`）映射出 `basic_user` / `basic_password`
//!    ——`opencode serve` 的 HTTP Basic 认证依赖它们；
//! 3. 同时拿到 sb config（`sb.base_url` 等），Cookie session 的 cookie name
//!    由 [`crate::storage::remote::RemoteClient`] 根据 URL 动态派生，
//!    不再依赖静态的 `SB_COOKIE_NAME` 环境变量。

pub mod basic;
pub mod session;

use std::path::Path;

use crate::error::AppError;

/// Resolved authentication configuration for the HTTP layer.
///
/// Loaded from environment variables via [`AuthConfig::from_env`]. Inserted
/// into request extensions by an auth layer so that extractors in
/// [`basic::BasicAuth`] and [`session::SessionAuth`] can read it.
///
/// `basic_user` / `basic_password` 在首次登录成功后从 `/api/user/info`
/// 返回的用户信息（`id`, `name`）映射而来。Cookie session 的 cookie name
/// 不在此持有——由 [`crate::storage::remote::RemoteClient`] 根据 sb
/// `base_url` 动态派生。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// HTTP Basic auth username.
    pub basic_user: String,
    /// HTTP Basic auth password.
    pub basic_password: String,
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
    /// `OPENCODE_SERVER_USERNAME` 缺省时留空，由首次配置表单引导填写。
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] if the auth-env file exists but
    /// cannot be read.
    pub fn from_env() -> Result<Self, AppError> {
        let auth_env_path = std::env::var("OC_SERVE_AUTH_ENV")
            .ok()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(".oc-serve-auth.env"));

        Self::from_env_with_file(&auth_env_path)
    }

    /// Like [`Self::from_env`] but with an explicit auth-env file path.
    ///
    /// 只读取 `OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`。
    /// `SB_COOKIE_NAME` 已不再读取——cookie name 由
    /// [`crate::storage::remote::RemoteClient`] 根据 sb.base_url 动态派生，
    /// env 文件中遗留的 `SB_COOKIE_NAME=...` 行会被安全忽略。
    pub fn from_env_with_file(auth_env: &Path) -> Result<Self, AppError> {
        // Layer 1: process environment wins.
        let mut user = std::env::var("OPENCODE_SERVER_USERNAME").ok();
        let mut password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();

        // Layer 2: fall back to `.oc-serve-auth.env` for any missing value.
        if (user.is_none() || password.is_none()) && auth_env.is_file() {
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
                        "OPENCODE_SERVER_USERNAME" if user.is_none() => {
                            user = Some(v.to_string())
                        }
                        "OPENCODE_SERVER_PASSWORD" if password.is_none() => {
                            password = Some(v.to_string())
                        }
                        "OPENCODE_SERVER_USERNAME" | "OPENCODE_SERVER_PASSWORD" => {}
                        _ => {}
                    }
                }
            }
        }

        // 凭据缺失时留空字符串（而非报错退出），交由首次配置表单引导用户填写。
        let basic_user = user.unwrap_or_default();
        let basic_password = password.unwrap_or_default();

        Ok(Self {
            basic_user,
            basic_password,
        })
    }

    /// `true` 当用户名和密码都已配置（两者均非空）。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.basic_user.is_empty() && !self.basic_password.is_empty()
    }

    /// 将凭据写入 auth-env 文件（Unix 下 chmod 600）。
    ///
    /// **已废弃**:设置面板现在通过 [`crate::account`] 的 env kv 工具
    /// （`upsert_env_keys`）增量写入对应 key,文件里已有的账户 / rathole /
    /// port 等配置原样保留。保留此函数仅为向后兼容,**新代码不要使用**
    /// ——它只写 USERNAME/PASSWORD 两行,整文件覆盖会抹掉其他 section。
    ///
    /// # Errors
    /// 返回 [`AppError::Io`] 当文件写入失败。
    #[deprecated(
        note = "改用 crate::account::upsert_env_keys 增量写入,保留其他 section"
    )]
    pub fn write_env_file(
        &self,
        path: &Path,
        username: &str,
        password: &str,
    ) -> Result<(), AppError> {
        let body = format!(
            "# Generated by `mini-oc-gui-serve` first-run setup\n\
             OPENCODE_SERVER_USERNAME={username}\n\
             OPENCODE_SERVER_PASSWORD={password}\n"
        );
        std::fs::write(path, body).map_err(AppError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }
}
