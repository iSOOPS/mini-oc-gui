//! Session domain types — mirror opencode serve's session API contract.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single opencode session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session id (e.g. `ses_<22 base62 chars>`).
    pub id: String,
    /// Human-readable session title.
    pub title: String,
    /// Absolute path to the session's working directory.
    pub directory: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Request body for `POST /api/session`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional session title; the server generates one if omitted.
    pub title: Option<String>,
    /// Optional location override; the server falls back to the configured
    /// default directory if omitted.
    pub location: Option<Location>,
}

/// Spatial location of a session (mirrors `Location` in the v2 API contract).
#[derive(Debug, Clone, Deserialize)]
pub struct Location {
    /// Absolute path to the session's working directory.
    pub directory: String,
}

/// Response body for `POST /api/session`.
#[derive(Debug, Clone, Serialize)]
pub struct CreateSessionResponse {
    /// The created session descriptor.
    pub data: SessionData,
}

/// Minimal session descriptor returned to clients after creation.
#[derive(Debug, Clone, Serialize)]
pub struct SessionData {
    /// The newly created session id.
    pub id: String,
    /// Resolved session title.
    pub title: String,
    /// Resolved working directory.
    pub directory: String,
}
