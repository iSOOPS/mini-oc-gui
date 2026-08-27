//! `/.fs/*` — SilverBullet-compatible file store proxy.
//!
//! The default implementation delegates to the configured
//! [`crate::storage::PathListStore`]. This gives us `GET`/`PUT` over
//! `/.fs/serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md` with
//! no extra HTTP hop.

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

/// Accept only the new namespaced layout
/// `serv/opencode/<sb_user>/<macos|windows>/<pcname>/path-list.md`.
fn is_path_list_path(path: &str) -> bool {
    let rel = path.trim_start_matches('/');
    let segs: Vec<&str> = rel.split('/').collect();
    segs.len() == 6
        && segs[0] == "serv"
        && segs[1] == "opencode"
        && matches!(segs[3], "macos" | "windows")
        && segs[5] == "path-list.md"
        && !segs[2].is_empty()
        && !segs[4].is_empty()
}

#[cfg(test)]
mod tests {
    use super::is_path_list_path;

    #[test]
    fn accepts_new_namespaced_layout() {
        assert!(is_path_list_path(
            "serv/opencode/alice/macos/alice-mbp/path-list.md"
        ));
        assert!(is_path_list_path(
            "/serv/opencode/bob/windows/bob-pc/path-list.md"
        ));
    }

    #[test]
    fn rejects_legacy_layout() {
        assert!(!is_path_list_path("serv/opencode/path-list.md"));
        assert!(!is_path_list_path("/serv/opencode/path-list.md"));
    }

    #[test]
    fn rejects_wrong_pctype() {
        assert!(!is_path_list_path(
            "serv/opencode/alice/linux/alice-pc/path-list.md"
        ));
    }

    #[test]
    fn rejects_extra_segments() {
        assert!(!is_path_list_path(
            "serv/opencode/alice/macos/alice-pc/extra/path-list.md"
        ));
        assert!(!is_path_list_path(
            "serv/opencode/alice/macos/alice-pc/path-list.md/foo"
        ));
    }
}
