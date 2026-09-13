//! Cookie session auth extractor.
//!
//! Parses `Cookie: <name>=<jwt>`, extracts the JWT, decodes its payload
//! without signature verification (intentional — we only need the `exp` claim
//! for an early expiry check; signature verification is delegated to the
//! upstream SilverBullet server).
//!
//! Cookie name 不再来自静态配置（`AuthConfig::sb_cookie_name` 字段与
//! `SB_COOKIE_NAME` 环境变量已删除）。SilverBullet 的 cookie name 在登录后
//! 由 [`crate::storage::remote::RemoteClient`] 根据 sb `base_url` 动态派生
//! （`https://md.isoops.com` → `auth_md_isoops_com`，见
//! `RemoteClient::derive_cookie_name`）。因此本 extractor 按 SilverBullet
//! 的 `auth_*` 命名约定扫描 Cookie 头，接受任何携带未过期 JWT 的 `auth_*`
//! cookie。完整认证流：用户登录 → `/api/user/info` → 拿到 sb config →
//! RemoteClient 自动派生 cookie name → 本模块校验 token。

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts},
};
use base64::Engine as _;
use serde::Deserialize;

use crate::auth::AuthConfig;
use crate::error::AppError;

/// Extracted cookie session token.
#[derive(Debug, Clone)]
pub struct SessionAuth {
    /// Raw JWT value extracted from the cookie.
    pub token: String,
}

/// JWT claims we care about (only `exp`).
#[derive(Debug, Deserialize)]
struct JwtClaims {
    /// Unix timestamp (seconds). Optional — missing means non-expiring.
    exp: Option<u64>,
}

#[async_trait]
impl<S> FromRequestParts<S> for SessionAuth
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // AuthConfig 仍必须存在于 extensions——证明 auth 中间件已挂载，
        // 与 BasicAuth 保持一致的 fail-closed 语义（而非用于读取 cookie
        // name，后者现在按 `auth_*` 约定动态匹配）。
        parts.extensions.get::<AuthConfig>().ok_or_else(|| {
            AppError::Internal("AuthConfig missing from request extensions".to_string())
        })?;

        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = extract_session_token(cookie_header).ok_or(AppError::Unauthorized)?;

        // Validate JWT `exp` without verifying the signature.
        //
        // We trust the upstream session authority (the request ultimately
        // proxies to SilverBullet, which does the full cryptographic check);
        // here we only need a fast-fail for obviously-stale tokens.
        let claims = decode_jwt_exp(&token).ok_or(AppError::Unauthorized)?;
        if let Some(exp) = claims.exp {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if exp < now {
                return Err(AppError::Unauthorized);
            }
        }

        Ok(SessionAuth { token })
    }
}

/// Scan a `Cookie:` header for the first SilverBullet-style `auth_*` cookie
/// whose value is a three-segment JWT; return the raw token.
///
/// Cookie name 由 sb `base_url` 动态派生（见模块文档），所以这里按命名
/// 约定匹配，而非精确匹配某个固定名字——不同 SB 实例派生出不同的
/// `auth_<host>` cookie 都能通过。
fn extract_session_token(header: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            let name = k.trim();
            let value = v.trim();
            if name.starts_with("auth_") && value.split('.').count() == 3 {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Decode a JWT payload (`header.payload.signature`) and extract `exp`.
///
/// Returns `None` if the token is malformed. Signature verification is
/// intentionally skipped (see module docs).
fn decode_jwt_exp(token: &str) -> Option<JwtClaims> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(payload_b64))
        .ok()?;
    serde_json::from_slice::<JwtClaims>(&bytes).ok()
}

/// Extract the value of a named cookie from a `Cookie:` header value.
#[must_use]
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((k, v)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_cookie() {
        assert_eq!(
            cookie_value("auth_md_isoops_com=abc.def.ghi", "auth_md_isoops_com"),
            Some("abc.def.ghi".to_string())
        );
    }

    #[test]
    fn parses_among_multiple_cookies() {
        assert_eq!(
            cookie_value("a=1; auth_md_isoops_com=TOKEN; b=2", "auth_md_isoops_com"),
            Some("TOKEN".to_string())
        );
    }

    #[test]
    fn missing_cookie_returns_none() {
        assert_eq!(cookie_value("a=1; b=2", "auth_x"), None);
    }

    #[test]
    fn decodes_jwt_exp_claim() {
        // header {"alg":"none","typ":"JWT"} payload {"exp":4102444800}
        use base64::Engine as _;
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(b"{\"sub\":\"x\",\"exp\":4102444800}");
        let token = format!("{header}.{payload}.sig");
        let claims = decode_jwt_exp(&token).expect("decode");
        assert_eq!(claims.exp, Some(4102444800));
    }

    #[test]
    fn extracts_first_auth_prefixed_jwt() {
        assert_eq!(
            extract_session_token("a=1; auth_md_isoops_com=abc.def.ghi; b=2"),
            Some("abc.def.ghi".to_string())
        );
    }

    #[test]
    fn extracts_any_derived_cookie_name() {
        // cookie name 按约定动态派生，不同 host 都能命中
        assert_eq!(
            extract_session_token("auth_md_example_org=header.payload.sig"),
            Some("header.payload.sig".to_string())
        );
    }

    #[test]
    fn skips_non_jwt_auth_cookie() {
        assert_eq!(extract_session_token("auth_md_isoops_com=not-a-jwt"), None);
    }

    #[test]
    fn ignores_non_auth_prefixed_cookies() {
        assert_eq!(extract_session_token("session=abc.def.ghi"), None);
        assert_eq!(extract_session_token("oauth_token=abc.def.ghi"), None);
    }
}
