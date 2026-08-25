//! `GET /health` — unauthenticated liveness probe.

use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

/// Liveness probe. Returns `{ "status": "ok", ... }` with HTTP 200.
#[tracing::instrument(skip_all)]
pub async fn health() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": "mini-oc-gui-serve",
            "version": env!("CARGO_PKG_VERSION"),
        })),
    )
}
