//! `/.fs/*` — SilverBullet-compatible file store proxy.
//!
//! The default implementation delegates to the configured
//! [`crate::storage::PathListStore`]. This gives us `GET`/`PUT` over
//! `/.fs/serv/opencode/path-list.md` with no extra HTTP hop.

use axum::{
    Json, Router, extract::{Path, State}, http::StatusCode, response::IntoResponse,
    routing::get,
};
use serde_json::json;

use crate::auth::session::SessionAuth;
use crate::error::AppError;
use crate::handlers::AppState;

/// Build the `/.fs/*` sub-router.
#[must_use]
pub fn router() -> Router<AppState> {
    Router::new().route("/*path", get(get_file).put(put_file))
}

/// `GET /.fs/<path>` — read a file from the local path-list cache.
///
/// Returns `application/json` mirroring the on-disk JSON shape.
#[tracing::instrument(skip_all)]
pub async fn get_file(
    State(state): State<AppState>,
    _auth: SessionAuth,
    Path(path): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if !is_path_list_path(&path) {
        return Err(AppError::NotFound);
    }
    let entries = state.store.list().await?;
    Ok((StatusCode::OK, Json(json!(entries))))
}

/// `PUT /.fs/<path>` — overwrite the local path-list cache.
///
/// Body must be a JSON array of [`crate::domain::PathEntry`].
#[tracing::instrument(skip_all)]
pub async fn put_file(
    State(state): State<AppState>,
    _auth: SessionAuth,
    Path(path): Path<String>,
    body: axum::body::Bytes,
) -> Result<StatusCode, AppError> {
    if !is_path_list_path(&path) {
        return Err(AppError::NotFound);
    }
    let entries: Vec<crate::domain::PathEntry> = serde_json::from_slice(&body)?;
    // Round-trip through the store so cache + remote get updated.
    for entry in &entries {
        state.store.upsert_path(&entry.path).await?;
        for sid in &entry.sections {
            state.store.append_session(&entry.path, sid).await?;
        }
    }
    // Remove paths that are no longer present.
    let current = state.store.list().await?;
    let new_paths: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.path.as_str()).collect();
    for c in current {
        if !new_paths.contains(c.path.as_str()) {
            state.store.remove_path(&c.path).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Only the fixed remote path is exposed through this proxy — exactly the same
/// path used by `lib-path-list.sh::SB_REMOTE_PATH`.
fn is_path_list_path(path: &str) -> bool {
    path.trim_start_matches('/') == "serv/opencode/path-list.md"
}
