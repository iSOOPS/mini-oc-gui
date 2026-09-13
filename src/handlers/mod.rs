//! Axum HTTP handlers + router assembly.
//!
//! Public surface:
//! - [`AppState`] — shared state injected into every handler.
//! - [`router`] — assembles all routes into a `Router` ready to serve.
//!
//! Routes:
//! - `GET  /health`                  (no auth)
//! - `GET  /status`                  (no auth)
//! - `GET  /project`                 (HTTP Basic)
//! - `GET  /session?directory=...`   (HTTP Basic)
//! - `POST /api/session`             (HTTP Basic)
//! - `GET  /.fs/<path>`              (Cookie session)
//! - `PUT  /.fs/<path>`              (Cookie session)

pub mod fs_proxy;
pub mod health;
pub mod project;
pub mod session;
pub mod status;

use std::sync::{Arc, RwLock};

use axum::{Router, middleware, routing::{get, post}};
use tower_http::trace::TraceLayer;

use crate::auth::AuthConfig;
use crate::serve::ServeSupervisor;
use crate::storage::PathListStore;

/// State injected into every HTTP handler via `axum::extract::State`.
#[derive(Clone)]
pub struct AppState {
    /// Path-list store (local cache + optional remote sync).
    pub store: Arc<PathListStore>,
    /// Authentication configuration（运行时可变，首次配置填写后热更新）.
    pub auth: Arc<RwLock<AuthConfig>>,
    /// Default directory for `POST /api/session` when no location is given.
    pub default_dir: String,
    /// Process supervisor for opencode serve + rathole（`/status` 读取）.
    pub supervisor: Arc<ServeSupervisor>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("store", &self.store)
            .field("auth", &self.auth)
            .field("default_dir", &self.default_dir)
            .field("supervisor", &"Arc<ServeSupervisor>")
            .finish()
    }
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
        .route("/status", get(status::status))
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
    axum::extract::State(auth): axum::extract::State<Arc<RwLock<AuthConfig>>>,
    mut req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    let config = auth.read().unwrap_or_else(|e| e.into_inner()).clone();
    req.extensions_mut().insert(config);
    next.run(req).await
}

#[cfg(test)]
impl AppState {
    /// Test helper：构造最小可用 AppState 用于 handler 单元测试。
    /// 传入任意 supervisor 实例；其余字段使用测试合理默认值。
    pub fn test_stub(supervisor: Arc<ServeSupervisor>) -> AppState {
        use crate::auth::AuthConfig;
        let tmp = tempfile::tempdir().expect("tempdir");
        let cache = crate::storage::cache::FileCache::new(tmp.path().join("path-list.md"));
        let store = Arc::new(crate::storage::PathListStore::new(cache));
        AppState {
            store,
            auth: Arc::new(RwLock::new(AuthConfig {
                basic_user: String::new(),
                basic_password: String::new(),
            })),
            default_dir: String::new(),
            supervisor,
        }
    }
}
