//! `GET /session` + `POST /api/session` — session listing + creation.

use axum::{Json, extract::{Query, State}};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::basic::BasicAuth;
use crate::domain::{CreateSessionRequest, Session};
use crate::error::AppError;
use crate::handlers::AppState;

/// `GET /session?directory=<path>` query params.
#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    /// Absolute path of the project directory.
    pub directory: String,
}

/// Return the sections attached to a single project directory.
///
/// Each section is the full Session descriptor stored in the path list
/// (id, title, directory, createdAt, updatedAt).
#[tracing::instrument(skip_all)]
pub async fn list_sessions(
    State(state): State<AppState>,
    _auth: BasicAuth,
    Query(q): Query<SessionQuery>,
) -> Result<Json<Vec<Session>>, AppError> {
    let entries = state.store.list().await?;
    let entry = entries
        .iter()
        .find(|e| e.path == q.directory)
        .ok_or(AppError::NotFound)?;

    Ok(Json(entry.sections.clone()))
}

/// Create a new session for the given directory (or the default fallback).
/// Both `createdAt` and `updatedAt` are set to the server's current time.
#[tracing::instrument(skip_all)]
pub async fn create_session(
    State(state): State<AppState>,
    _auth: BasicAuth,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<Session>, AppError> {
    let dir = req
        .location
        .map(|l| l.directory)
        .unwrap_or_else(|| state.default_dir.clone());
    let title = req
        .title
        .unwrap_or_else(|| format!("TUI-Launched-{}", Utc::now().timestamp()));
    let session_id = format!("ses_{}", Uuid::new_v4().simple());
    let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());

    let session = Session::new(&session_id, &title, &dir, now);

    state.store.append_session(&dir, &session).await?;
    state.store.touch_path(&dir).await?;

    Ok(Json(session))
}
