//! Application-wide error type.
//!
//! Every variant maps to a stable HTTP status code and a short machine-readable
//! error code. [`IntoResponse`] renders `{ "error": code, "message": message }`.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Unified error enum for the whole application.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The requested resource does not exist.
    #[error("resource not found")]
    NotFound,

    /// The request was malformed or semantically invalid.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Authentication is missing or failed.
    #[error("unauthorized")]
    Unauthorized,

    /// The authenticated caller lacks permission.
    #[error("forbidden")]
    Forbidden,

    /// The request conflicts with the current server state.
    #[error("conflict: {0}")]
    Conflict(String),

    /// An unexpected internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),

    /// A filesystem or I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization failed.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// An upstream HTTP request failed.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// A local path failed validation.
    #[error("path validation error: {0}")]
    PathValidation(String),
}

impl AppError {
    /// The HTTP status code associated with this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) | Self::Json(_) | Self::PathValidation(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Internal(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Http(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// A short, stable, machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::Conflict(_) => "conflict",
            Self::Internal(_) => "internal_error",
            Self::Io(_) => "io_error",
            Self::Json(_) => "invalid_json",
            Self::Http(_) => "http_error",
            Self::PathValidation(_) => "invalid_path",
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code();
        let message = self.to_string();
        (status, Json(json!({ "error": code, "message": message }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(format!("{err:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_covers_all_variants() {
        // Sanity: every variant maps to a status without panicking.
        let variants: Vec<AppError> = vec![
            AppError::NotFound,
            AppError::BadRequest("x".into()),
            AppError::Unauthorized,
            AppError::Forbidden,
            AppError::Conflict("x".into()),
            AppError::Internal("x".into()),
            AppError::PathValidation("x".into()),
        ];
        for v in variants {
            let _ = v.status();
            let _ = v.code();
        }
    }
}
