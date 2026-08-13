//! Axum HTTP handlers + router assembly.
//!
//! Public surface:
//! - [`AppState`] — shared state injected into every handler.
//! - [`router`] — assembles all routes into a `Router` ready to serve.
//!
//! Routes:
//! - `GET  /health`                  (no auth)
//! - `GET  /project`                 (HTTP Basic)
//! - `GET  /session?directory=...`   (HTTP Basic)
//! - `POST /api/session`             (HTTP Basic)
//! - `GET  /.fs/<path>`              (Cookie session)
//! - `PUT  /.fs/<path>`              (Cookie session)

pub mod fs_proxy;
pub mod health;
pub mod project;
pub mod session;

use std::sync::Arc;

use axum::{Router, middleware, routing::{get, post}};
use tower_http::trace::TraceLayer;

use crate::auth::AuthConfig;
use crate::storage::PathListStore;

/// State injected into every HTTP handler via `axum::extract::State`.
#[derive(Clone, Debug)]
pub struct AppState {
    /// Path-list store (local cache + optional remote sync).
    pub store: Arc<PathListStore>,
    /// Authentication configuration.
    pub auth: AuthConfig,
    /// Default directory for `POST /api/session` when no location is given.
    pub default_dir: String,
}

/// Build the full Axum router with all routes wired up.
#[must_use]
pub fn router(state: AppState) -> Router {
    // Inject AuthConfig into request extensions via `from_fn_with_state`
    // so BasicAuth / SessionAuth extractors can read it without each route
    // re-reading env vars.
    let auth_layer = middleware::from_fn_with_state(state.auth.clone(), attach_auth_config);

    Router::new()
        .route("/health", get(health::health))
        .route("/project", get(project::list_projects))
        .route("/session", get(session::list_sessions))
        .route("/api/session", post(session::create_session))
        .nest("/.fs", fs_proxy::router())
        .layer(TraceLayer::new_for_http())
        .layer(auth_layer)
        .with_state(state)
}

/// Middleware that clones the [`AuthConfig`] into the request extensions.
async fn attach_auth_config(
    axum::extract::State(auth): axum::extract::State<AuthConfig>,
    mut req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    req.extensions_mut().insert(auth);
    next.run(req).await
}
