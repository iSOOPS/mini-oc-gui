//! Cookie session auth extractor.
//!
//! Parses `Cookie: <name>=<jwt>`, extracts the JWT, decodes its payload
//! without signature verification (intentional — we only need the `exp` claim
//! for an early expiry check; signature verification is delegated to the
//! upstream SilverBullet server).

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
        let config = parts
            .extensions
            .get::<AuthConfig>()
            .cloned()
            .ok_or_else(|| {
                AppError::Internal("AuthConfig missing from request extensions".to_string())
            })?;

        let cookie_name = config.sb_cookie_name.as_deref().ok_or(AppError::Unauthorized)?;

        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = cookie_value(cookie_header, cookie_name).ok_or(AppError::Unauthorized)?;

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
}
