//! Session domain types — stored as elements in `PathEntry.sections`
//! and returned by the `GET /session` HTTP endpoint.

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

/// A single opencode session (or section attached to a path).
///
/// Serialized with camelCase field names to match the remote
/// SilverBullet `path-list.md` shape consumed by `lib-path-list.sh`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Session {
    /// Unique session id (e.g. `ses_<22 base62 chars>`).
    #[serde(rename = "id")]
    pub id: String,

    /// Human-readable session title.
    #[serde(rename = "title")]
    pub title: String,

    /// Absolute path to the session's working directory.
    #[serde(rename = "directory")]
    pub directory: String,

    /// When the session was created.
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<FixedOffset>,

    /// When the session was last updated.
    #[serde(rename = "updatedAt")]
    pub updated_at: DateTime<FixedOffset>,
}

impl Session {
    /// Build a new Session with both timestamps set to `now`.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        directory: impl Into<String>,
        now: DateTime<FixedOffset>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            directory: directory.into(),
            created_at: now,
            updated_at: now,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    #[test]
    fn session_serializes_camelcase() {
        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let dt = off.with_ymd_and_hms(2026, 9, 13, 10, 0, 0).unwrap();
        let s = Session {
            id: "ses_abc".into(),
            title: "demo".into(),
            directory: "/tmp/proj".into(),
            created_at: dt,
            updated_at: dt,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(json.contains("\"id\":\"ses_abc\""));
        assert!(json.contains("\"title\":\"demo\""));
        assert!(json.contains("\"directory\":\"/tmp/proj\""));
        assert!(json.contains("\"createdAt\":"));
        assert!(json.contains("\"updatedAt\":"));
        assert!(!json.contains("created_at"));
        assert!(!json.contains("updated_at"));
    }

    #[test]
    fn session_roundtrip() {
        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let dt = off.with_ymd_and_hms(2026, 9, 13, 10, 0, 0).unwrap();
        let s = Session {
            id: "ses_xyz".into(),
            title: "roundtrip".into(),
            directory: "/srv/proj".into(),
            created_at: dt,
            updated_at: dt,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        let back: Session = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, s);
    }

    #[test]
    fn session_new_sets_both_timestamps_to_now() {
        let off = FixedOffset::east_opt(-5 * 3600).expect("offset");
        let dt = off.with_ymd_and_hms(2026, 1, 15, 14, 30, 0).unwrap();
        let s = Session::new("ses_n", "new", "/p", dt);
        assert_eq!(s.id, "ses_n");
        assert_eq!(s.title, "new");
        assert_eq!(s.directory, "/p");
        assert_eq!(s.created_at, dt);
        assert_eq!(s.updated_at, dt);
    }
}
