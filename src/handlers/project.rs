//! `GET /project` — list all known projects.

use axum::{Json, extract::State};

use crate::auth::basic::BasicAuth;
use crate::domain::Project;
use crate::error::AppError;
use crate::handlers::AppState;

/// List every project currently tracked in `path-list.md`, with their sessions.
///
/// The BasicAuth extractor verifies the request — on success the handler runs
/// and returns a `Vec<Project>` serialized as JSON.
#[tracing::instrument(skip_all, fields(_auth = _auth.username.as_str()))]
pub async fn list_projects(
    State(state): State<AppState>,
    _auth: BasicAuth,
) -> Result<Json<Vec<Project>>, AppError> {
    let entries = state.store.list().await?;
    let projects: Vec<Project> = entries.into_iter().map(Project::from).collect();
    Ok(Json(projects))
}
