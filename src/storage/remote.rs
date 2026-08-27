//! SilverBullet-compatible remote file store client.
//!
//! Talks to `/.fs/<path>` with cookie session auth, mirroring the semantics
//! of `lib-path-list.sh::sb_curl`:
//! - 10s timeout
//! - auto-relogin on 401 (one retry)
//! - network errors return status `0` (caller treats as "unreachable")

use std::time::Duration;

use reqwest::{
    Client, ClientBuilder,
    header::{ACCEPT, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, SET_COOKIE},
};

use crate::error::AppError;

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
    /// namespaced remote path segment via `RemotePaths::new`.
    pub user: Option<String>,
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
        assert_eq!(
            c.url_for("/serv/opencode/path-list.md"),
            "https://md.isoops.com/.fs/serv/opencode/path-list.md"
        );
        assert_eq!(
            c.url_for("serv/opencode/path-list.md"),
            "https://md.isoops.com/.fs/serv/opencode/path-list.md"
        );
    }
}
