//! HTTP Basic auth extractor.
//!
//! Parses `Authorization: Basic <base64(user:password)>`, verifies the
//! credentials against the [`AuthConfig`] attached to the request extensions,
//! and on success exposes the parsed `(user, password)` tuple.

use async_trait::async_trait;
use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts},
};

use crate::auth::AuthConfig;
use crate::error::AppError;

/// Extracted HTTP Basic credentials.
#[derive(Debug, Clone)]
pub struct BasicAuth {
    /// Verified username.
    pub username: String,
    /// Verified password.
    pub password: String,
}

#[async_trait]
impl<S> FromRequestParts<S> for BasicAuth
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

        let header_value = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let encoded = header_value
            .strip_prefix("Basic ")
            .ok_or(AppError::Unauthorized)?
            .trim();

        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let decoded = STANDARD.decode(encoded).map_err(|_| AppError::Unauthorized)?;
        let s = String::from_utf8(decoded).map_err(|_| AppError::Unauthorized)?;

        let (user, pass) = s
            .split_once(':')
            .ok_or(AppError::Unauthorized)
            .map(|(u, p)| (u.to_string(), p.to_string()))?;

        if user == config.basic_user && pass == config.basic_password {
            Ok(BasicAuth {
                username: user,
                password: pass,
            })
        } else {
            Err(AppError::Unauthorized)
        }
    }
}
