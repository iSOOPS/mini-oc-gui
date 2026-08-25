//! `GET /session` + `POST /api/session` — session listing + creation.

use axum::{Json, extract::{Query, State}};
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::basic::BasicAuth;
use crate::domain::{CreateSessionRequest, CreateSessionResponse, Session, SessionData};
use crate::error::AppError;
use crate::handlers::AppState;

/// `GET /session?directory=<path>` query params.
#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    /// Absolute path of the project directory.
    pub directory: String,
}

/// List the sessions attached to a single project directory.
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

    let sessions: Vec<Session> = entry
        .sections
        .iter()
        .map(|sid| Session {
            id: sid.clone(),
            title: format!("session-{}", &sid[..sid.len().min(8)]),
            directory: entry.path.clone(),
            created_at: entry
                .created_at
                .unwrap_or_else(|| Utc::now().into())
                .with_timezone(&Utc),
            updated_at: entry
                .last_opened_at
                .unwrap_or_else(|| Utc::now().into())
                .with_timezone(&Utc),
        })
        .collect();

    Ok(Json(sessions))
}

/// Create a new session for the given directory (or the default fallback).
#[tracing::instrument(skip_all)]
pub async fn create_session(
    State(state): State<AppState>,
    _auth: BasicAuth,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<CreateSessionResponse>, AppError> {
    let dir = req
        .location
        .map(|l| l.directory)
        .unwrap_or_else(|| state.default_dir.clone());
    let title = req
        .title
        .unwrap_or_else(|| format!("TUI-Launched-{}", Utc::now().timestamp()));
    let session_id = format!("ses_{}", Uuid::new_v4().simple());

    state.store.append_session(&dir, &session_id).await?;
    state.store.touch_path(&dir).await?;

    Ok(Json(CreateSessionResponse {
        data: SessionData {
            id: session_id,
            title,
            directory: dir,
        },
    }))
}
