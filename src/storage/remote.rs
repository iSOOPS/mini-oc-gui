//! SilverBullet-compatible remote file store client.
//!
//! Talks to `/.fs/<path>` with cookie session auth, mirroring the semantics
//! of `lib-path-list.sh::sb_curl`:
//! - 10s timeout
//! - auto-relogin on 401 (one retry)
//! - network errors return status `0` (caller treats as "unreachable")
//!
//! NOTE: the sb config (base_url / username / password) is now obtained
//! dynamically from the user info returned by `/api/user/info`
//! ([`crate::account::RemoteSbConfig`]), replacing the previous static
//! `SbConfig` (SB_URL / SB_USER / SB_PASSWORD env). The cookie name is
//! still derived from `base_url`. See [`RemoteClient::from_user_info`]
//! and [`remote_client_from_user_info`].

use std::time::Duration;

use reqwest::{
    Client, ClientBuilder,
    header::{ACCEPT, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE},
};

use crate::account::{RemoteSbConfig, RemoteUserInfo};
use crate::error::AppError;

use super::paths::RemotePaths;

/// HTTP status returned to callers. `0` indicates a network / timeout / no-credentials
/// failure (the same convention used by `lib-path-list.sh`).
pub type Status = u16;

/// Remote file store client (SilverBullet-shaped).
#[derive(Debug, Clone)]
pub struct RemoteClient {
    /// Base URL, e.g. `https://md.isoops.com`.
    pub base_url: String,
    /// Cookie name to send and look for in responses (e.g. `auth_md_isoops_com`).
    pub cookie_name: String,
    /// Current cookie value (`<name>=<jwt>`). `None` triggers auto-login.
    pub cookie: Option<String>,
    /// Cached credentials used by auto-relogin; also drives the
    /// legacy remote path segment via `RemotePaths::new`.
    pub user: Option<String>,
    /// 账户用户ID（`/api/user/info` 返回的 `info.id`）。
    /// 非空时远端 path-list 走新格式路径
    /// `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`。
    pub user_id: Option<String>,
    /// 已绑定的设备名称（`account_config.device_name`）。
    /// 新格式路径段；为空时回退 OS 用户名（`pcname()`）。
    pub device_name: Option<String>,
    password: Option<String>,
    /// Shared HTTP client.
    http: Client,
}

impl RemoteClient {
    /// Construct a client without auto-login credentials.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        let cookie_name = derive_cookie_name(&base_url);
        Self {
            base_url,
            cookie_name,
            cookie: None,
            user: None,
            user_id: None,
            device_name: None,
            password: None,
            http: ClientBuilder::new()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Construct a client with stored credentials for auto-relogin.
    #[must_use]
    pub fn with_credentials(base_url: impl Into<String>, user: String, password: String) -> Self {
        let mut c = Self::new(base_url);
        c.user = Some(user);
        c.password = Some(password);
        c
    }

    /// Construct a client from the sb config embedded in the user info
    /// returned by `/api/user/info`.
    ///
    /// Equivalent to [`with_credentials`](Self::with_credentials) with
    /// `info.base_url` / `info.username` / `info.password`.
    #[must_use]
    pub fn from_user_info(info: &RemoteSbConfig) -> Self {
        Self::with_credentials(
            info.base_url.clone(),
            info.username.clone(),
            info.password.clone(),
        )
    }

    /// Construct a client carrying the new-format path identity
    /// (`info.id` + `device_name`), so the remote path-list syncs to
    /// `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`.
    ///
    /// `device_name` 为空（账户尚未绑定设备）时，路径段由
    /// [`RemotePaths`] 构造阶段回退到 OS 用户名（`pcname()`）。
    #[must_use]
    pub fn from_user_info_v2(
        info: &RemoteUserInfo,
        device_name: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self::with_credentials(info.sb.base_url.clone(), info.sb.username.clone(), password.into())
            .with_user_and_device(info.id.clone(), device_name.into())
    }

    /// Attach the new-format path identity (`user_id` + `device_name`).
    fn with_user_and_device(mut self, user_id: String, device_name: String) -> Self {
        self.user_id = Some(user_id);
        self.device_name = Some(device_name);
        self
    }

    /// Build the [`RemotePaths`] used by path-list sync for this client.
    ///
    /// Prefers the new layout when `user_id` is set. The new layout
    /// **requires** a non-empty `device_name` — if `device_name` is empty
    /// or whitespace-only, this returns [`AppError::Internal`] instead of
    /// silently degrading to the OS username, so callers must ensure the
    /// device has been explicitly selected upstream (see
    /// `crate::account::RemoteUserInfo::devices`).
    ///
    /// When `user_id` is empty, falls back to the legacy sb-username
    /// layout (which uses `pcname()` because the legacy layout was always
    /// keyed to the local machine).
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] when `user_id` is non-empty but
    /// `device_name` is missing or whitespace-only.
    pub fn remote_paths(&self) -> Result<RemotePaths, AppError> {
        let user_id = self.user_id.as_deref().unwrap_or("");
        if user_id.is_empty() {
            // Legacy layout: no user_id → key off sb username + pcname.
            return Ok(RemotePaths::new(self.user.as_deref().unwrap_or("unknown")));
        }
        // New layout: device_name is REQUIRED. No silent fallback to pcname().
        let device = self
            .device_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::Internal(format!(
                    "device_name is required for new-format remote path \
                     (user_id={user_id:?}); refusing to fall back to OS \
                     username — caller must select a bound device first"
                ))
            })?;
        Ok(RemotePaths::with_user_info(user_id, device.to_string()))
    }

    /// Derive the SilverBullet cookie name from a base URL.
    ///
    /// Example: `https://md.isoops.com` → `auth_md_isoops_com`.
    #[must_use]
    pub fn derive_cookie_name(base_url: &str) -> String {
        derive_cookie_name(base_url)
    }

    fn url_for(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let rel = path.trim_start_matches('/');
        format!("{base}/.fs/{rel}")
    }

    /// POST `/.auth` form login → extract Set-Cookie → store on `self`.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] on network failure, missing cookie in
    /// response, or auth rejection.
    pub async fn login(&mut self, user: &str, password: &str) -> Result<(), AppError> {
        self.user = Some(user.to_string());
        self.password = Some(password.to_string());
        self.do_login(user, password).await
    }

    async fn do_login(&mut self, user: &str, password: &str) -> Result<(), AppError> {
        let login_url = format!("{}/.auth", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&login_url)
            .header(ACCEPT, "*/*")
            .form(&[("username", user), ("password", password)])
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("login network error: {e}")))?;

        let cookie = extract_cookie(resp.headers(), &self.cookie_name)
            .ok_or_else(|| AppError::Internal("login response missing session cookie".to_string()))?;
        self.cookie = Some(cookie);
        Ok(())
    }

    /// GET `/.fs/<path>`. Returns `(status, body)`. Status `0` = network error.
    ///
    /// # Errors
    /// Only returned for truly unrecoverable internal errors. Network failures
    /// are returned as `Ok((0, message))` so callers can transparently fall
    /// back to local cache.
    pub async fn get(&mut self, path: &str) -> Result<(Status, String), AppError> {
        let url = self.url_for(path);
        let first = self.send_req(reqwest::Method::GET, &url, None).await;
        self.handle(first, reqwest::Method::GET, &url, None).await
    }

    /// PUT `/.fs/<path>` with `body`. Returns status. Status `0` = network error.
    ///
    /// # Errors
    /// Same convention as [`get`](Self::get).
    pub async fn put(&mut self, path: &str, body: &str) -> Result<Status, AppError> {
        let url = self.url_for(path);
        let owned = body.to_string();
        let first = self.send_req(reqwest::Method::PUT, &url, Some(owned.clone())).await;
        let (status, _body) = self
            .handle(first, reqwest::Method::PUT, &url, Some(owned))
            .await?;
        Ok(status)
    }

    async fn send_req(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
    ) -> Result<(Status, String), AppError> {
        let mut req = self.http.request(method, url).header(ACCEPT, "*/*");
        if let Some(cookie) = &self.cookie {
            if let Ok(v) = HeaderValue::from_str(cookie) {
                req = req.header(COOKIE, v);
            }
        }
        if let Some(b) = body {
            req = req
                .header(CONTENT_TYPE, "text/markdown; charset=utf-8")
                .body(b);
        }
        let resp = req.send().await.map_err(|e| AppError::Internal(format!("network: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("read body: {e}")))?;
        Ok((status, text))
    }

    async fn handle(
        &mut self,
        first: Result<(Status, String), AppError>,
        method: reqwest::Method,
        url: &str,
        body: Option<String>,
    ) -> Result<(Status, String), AppError> {
        match first {
            Err(e) => {
                tracing::warn!("remote {} {} failed: {}", method, url, e);
                Ok((0, e.to_string()))
            }
            Ok((401, response_body)) => {
                // One re-login + retry.
                if self.relogin().await.is_err() {
                    return Ok((401, response_body));
                }
                match self.send_req(method.clone(), url, body).await {
                    Ok(r) => Ok(r),
                    Err(e) => {
                        tracing::warn!("retry after relogin failed: {}", e);
                        Ok((401, response_body))
                    }
                }
            }
            Ok(other) => Ok(other),
        }
    }

    async fn relogin(&mut self) -> Result<(), AppError> {
        let (u, p) = match (self.user.clone(), self.password.clone()) {
            (Some(u), Some(p)) => (u, p),
            _ => return Err(AppError::Internal("no credentials for relogin".to_string())),
        };
        self.do_login(&u, &p).await
    }
}

/// 从 /api/user/info 返回的用户信息创建 RemoteClient
#[must_use]
pub fn remote_client_from_user_info(info: &RemoteSbConfig) -> RemoteClient {
    RemoteClient::with_credentials(
        info.base_url.clone(),
        info.username.clone(),
        info.password.clone(),
    )
}

fn derive_cookie_name(base_url: &str) -> String {
    let after_scheme = base_url.split_once("://").map_or(base_url, |(_, r)| r);
    let host_and_port = after_scheme.split('/').next().unwrap_or(after_scheme);
    let host = host_and_port.split(':').next().unwrap_or(host_and_port);
    format!("auth_{}", host.replace('.', "_"))
}

fn extract_cookie(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let prefix = format!("{cookie_name}=");
    for value in headers.get_all(SET_COOKIE) {
        if let Ok(s) = value.to_str() {
            if let Some(rest) = s.strip_prefix(&prefix) {
                if let Some(cookie_part) = rest.split(';').next() {
                    return Some(format!("{prefix}{cookie_part}"));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RemotePaths;

    fn user_info_fixture(id: &str) -> RemoteUserInfo {
        serde_json::from_str(&format!(
            r#"{{
                "id": "{id}",
                "name": "Alice",
                "key": "k123",
                "sb": {{
                    "base_url": "https://md.example.com",
                    "username": "alice",
                    "password": "pw"
                }},
                "devices": [{{ "name": "my-dev-pc", "port": 9464, "bound": true }}]
            }}"#
        ))
        .expect("fixture")
    }

    #[test]
    fn cookie_name_from_url() {
        assert_eq!(
            RemoteClient::derive_cookie_name("https://md.isoops.com"),
            "auth_md_isoops_com"
        );
        assert_eq!(
            RemoteClient::derive_cookie_name("http://127.0.0.1:8080"),
            "auth_127_0_0_1"
        );
        assert_eq!(
            RemoteClient::derive_cookie_name("no-scheme.example.com"),
            "auth_no-scheme_example_com"
        );
    }

    #[test]
    fn url_for_normalizes() {
        let c = RemoteClient::new("https://md.isoops.com");
        let path = RemotePaths::new("").path_list_with_slash();
        assert_eq!(
            c.url_for(&path),
            format!("https://md.isoops.com/.fs{}", RemotePaths::new("").path_list_with_slash())
        );
        assert_eq!(
            c.url_for(path.trim_start_matches('/')),
            format!("https://md.isoops.com/.fs{}", RemotePaths::new("").path_list_with_slash())
        );
    }

    #[test]
    fn from_user_info_v2_sets_user_id_and_device_name() {
        let info = user_info_fixture("u-1");
        let c = RemoteClient::from_user_info_v2(&info, "my-dev-pc", "pw");
        assert_eq!(c.user_id.as_deref(), Some("u-1"));
        assert_eq!(c.device_name.as_deref(), Some("my-dev-pc"));
        // sb 凭据照旧填充（自动重登录 + legacy 路径兜底）。
        assert_eq!(c.user.as_deref(), Some("alice"));
        assert_eq!(c.base_url, "https://md.example.com");
    }

    #[test]
    fn remote_paths_prefers_new_format_with_device() {
        let info = user_info_fixture("u-123");
        let c = RemoteClient::from_user_info_v2(&info, "my-dev-pc", "pw");
        let rp = c.remote_paths().expect("device_name is non-empty");
        assert_eq!(
            rp.path_list_new_format(),
            format!("serv/opencode/u-123/{}/my-dev-pc/path-list", crate::storage::paths::pctype())
        );
    }

    #[test]
    fn remote_paths_errors_when_device_name_empty() {
        // 设备名为空 → 必须报错, 不能回退到 OS 用户名。
        let info = user_info_fixture("u-1");
        let c = RemoteClient::from_user_info_v2(&info, "", "pw");
        let err = c.remote_paths().expect_err("empty device_name must error");
        let msg = err.to_string();
        assert!(
            msg.contains("device_name is required"),
            "error must mention the requirement, got: {msg}"
        );
    }

    #[test]
    fn remote_paths_errors_when_device_name_whitespace_only() {
        // 设备名为纯空白 → 同上, 必须报错。
        let info = user_info_fixture("u-1");
        let c = RemoteClient::from_user_info_v2(&info, "   ", "pw");
        let err = c.remote_paths().expect_err("whitespace-only device_name must error");
        let msg = err.to_string();
        assert!(
            msg.contains("device_name is required"),
            "error must mention the requirement, got: {msg}"
        );
    }

    #[test]
    fn remote_paths_legacy_when_no_user_id() {
        let c = RemoteClient::with_credentials("https://md.example.com", "alice".into(), "pw".into());
        let rp = c.remote_paths().expect("no user_id → legacy path always succeeds");
        let p = rp.path_list_with_slash();
        assert!(p.contains("/alice/"));
        assert!(p.ends_with("/path-list.md"));
    }
}
