//! `Project` domain type — mirrors opencode serve's `GET /project` shape.

use serde::{Deserialize, Serialize};

use super::path_entry::PathEntry;
use super::session::Session;

/// A project known to opencode serve.
///
/// Constructed from a [`PathEntry`] by [`From<PathEntry>`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Stable project id (uses the absolute path as the unique key).
    pub id: String,
    /// Absolute path to the project directory.
    pub path: String,
    /// Human-readable project name (basename of the path).
    pub name: String,
    /// Sessions belonging to this project.
    pub sessions: Vec<Session>,
}

impl From<PathEntry> for Project {
    fn from(entry: PathEntry) -> Self {
        // POSIX basename: last segment after '/'.
        let name = std::path::Path::new(&entry.path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.path.clone());

        Self {
            id: entry.path.clone(),
            path: entry.path,
            name,
            sessions: Vec::new(),
        }
    }
}
