//! Local file-backed cache for the `path-list.md` JSON array.
//!
//! Mirrors `path-list-actor.py::load_index` / `save_index`:
//! - Atomic write via tempfile + `fs::rename` (POSIX-atomic on the same fs).
//! - On read failure, restore from `.bak`; failing that, error out.

use std::path::{Path, PathBuf};

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

use crate::domain::PathEntry;
use crate::error::AppError;

/// A file-backed cache that stores a JSON array of [`PathEntry`].
#[derive(Debug, Clone)]
pub struct FileCache {
    /// Path to the JSON file.
    path: PathBuf,
    /// Backup path used for crash recovery.
    backup: PathBuf,
}

impl FileCache {
    /// Create a cache rooted at `path`. The `.bak` path is derived by
    /// appending `.bak` to the file name.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut backup = path.clone();
        let fname = backup
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "path-list.md".to_string());
        backup.set_file_name(format!("{fname}.bak"));
        Self { path, backup }
    }

    /// The path this cache reads/writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the JSON array from disk. Tries `.bak` recovery on parse failure.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] if the file is missing/empty AND no
    /// `.bak` exists, or if both files are corrupt.
    pub async fn read(&self) -> Result<Vec<PathEntry>, AppError> {
        let text = match tokio::fs::read_to_string(&self.path).await {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(AppError::Io(e)),
        };

        match serde_json::from_str::<Vec<PathEntry>>(&text) {
            Ok(entries) => Ok(entries),
            Err(parse_err) => self.restore_from_backup(&parse_err).await,
        }
    }

    async fn restore_from_backup(&self, parse_err: &serde_json::Error) -> Result<Vec<PathEntry>, AppError> {
        let bak_text = tokio::fs::read_to_string(&self.backup)
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "path-list.md corrupt ({parse_err}) and .bak unreadable: {e}"
                ))
            })?;
        serde_json::from_str::<Vec<PathEntry>>(&bak_text).map_err(|e| {
            AppError::Internal(format!("path-list.md and .bak both corrupt: {e}"))
        })
    }

    /// Atomically write the JSON array to disk.
    ///
    /// Strategy: write to a sibling tempfile in the same directory, then
    /// `fs::rename` over the destination (atomic on POSIX). On success the
    /// `.bak` is removed; on failure the orphan tempfile is cleaned up.
    ///
    /// # Errors
    /// Returns [`AppError::Io`] or [`AppError::Json`] on serialization
    /// failures.
    pub async fn write(&self, entries: &[PathEntry]) -> Result<(), AppError> {
        // 1. Validate that we have a clean array (belt-and-braces — the caller
        //    already passed &[PathEntry] which is a Vec, but we re-check via
        //    serde_json round-trip to be safe).
        let text = serde_json::to_string_pretty(entries)?;

        // 2. Back up existing file (best-effort).
        if self.path.exists() {
            let _ = tokio::fs::copy(&self.path, &self.backup).await;
        }

        // 3. Write to tempfile then rename.
        let tmp = self.tmp_path();
        if let Err(e) = tokio::fs::write(&tmp, &text).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(AppError::Io(e));
        }

        if let Err(e) = tokio::fs::rename(&tmp, &self.path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(AppError::Io(e));
        }

        // 4. Drop backup on success.
        let _ = tokio::fs::remove_file(&self.backup).await;
        Ok(())
    }

    fn tmp_path(&self) -> PathBuf {
        let mut p = self.path.clone();
        let fname = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "path-list.md".to_string());
        p.set_file_name(format!("{fname}.{}.tmp", std::process::id()));
        p
    }
}

/// Resolve the canonical on-filesystem location for the path-list cache.
///
/// Returns `<exe_dir>/data/path-list.md` when the running binary's path is
/// known, otherwise a CWD-relative `data/path-list.md` fallback (used by
/// `cargo test` and similar).
///
/// The exe-adjacent location matches the layout used by the bundled
/// rathole binary (`<exe_dir>/rathole/...`) so that a release build is
/// self-contained and the cache lives next to the thing that reads it.
#[must_use]
pub fn default_path_list_path() -> PathBuf {
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        exe_dir.join("data").join("path-list.md")
    } else {
        PathBuf::from("data").join("path-list.md")
    }
}

/// Helper: timestamp at `now` formatted as ISO-8601 with timezone offset.
///
/// Matches the bash `date +%Y-%m-%dT%H:%M:%S%z` output used by
/// `lib-path-list.sh`.
#[must_use]
pub fn now_iso8601() -> DateTime<FixedOffset> {
    chrono::Local::now().with_timezone(chrono::Local::now().offset())
}

/// Helper: take the lexicographic minimum of a slice of optional ISO-8601
/// strings, ignoring empty values (matches jq's `min // ""` behavior).
#[must_use]
pub fn min_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values
        .into_iter()
        .filter(|s| !s.is_empty())
        .min()
}

/// Helper: take the lexicographic maximum of a slice of optional ISO-8601
/// strings, ignoring empty values (matches jq's `max // ""` behavior).
#[must_use]
pub fn max_non_empty<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    values
        .into_iter()
        .filter(|s| !s.is_empty())
        .max()
}

/// Helper: turn an `Option<DateTime<FixedOffset>>` into the JSON string form
/// (`+0800`, no colon), or `""` when `None`.
#[must_use]
pub fn format_dt(dt: Option<DateTime<FixedOffset>>) -> String {
    dt.map(|d| d.format("%Y-%m-%dT%H:%M:%S%z").to_string())
        .unwrap_or_default()
}

/// Helper: parse an ISO-8601 string into `DateTime<FixedOffset>`.
///
/// Accepts both RFC-3339 form (`+08:00`) and the bash `date +%z` form
/// (`+0800` without colon).
#[must_use]
pub fn parse_dt(s: &str) -> Option<DateTime<FixedOffset>> {
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .or_else(|| DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z").ok())
}

/// Convert a slice of `PathEntry` to a `serde_json::Value` (array).
#[must_use]
pub fn entries_to_value(entries: &[PathEntry]) -> Value {
    serde_json::to_value(entries).unwrap_or(Value::Array(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn write_and_read_roundtrip() {
        let dir = TempDir::new().expect("tmpdir");
        let path = dir.path().join("path-list.md");
        let cache = FileCache::new(&path);

        let entries = vec![
            PathEntry::new("/a/b"),
            PathEntry::new("/c/d"),
        ];
        cache.write(&entries).await.expect("write");
        let got = cache.read().await.expect("read");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].path, "/a/b");
    }

    #[tokio::test]
    async fn read_missing_file_returns_empty() {
        let dir = TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("missing.md"));
        let got = cache.read().await.expect("read");
        assert!(got.is_empty());
    }

    #[test]
    fn min_max_non_empty_ignores_blank() {
        let xs = ["", "2026-08-12T00:00:00+0800", "2026-01-01T00:00:00+0000"];
        assert_eq!(min_non_empty(xs.iter().copied()), Some("2026-01-01T00:00:00+0000"));
        assert_eq!(max_non_empty(xs.iter().copied()), Some("2026-08-12T00:00:00+0800"));
    }

    #[test]
    fn default_path_list_path_is_exe_adjacent() {
        let p = default_path_list_path();
        let exe = std::env::current_exe().expect("current_exe available in test");
        let expected = exe.parent().unwrap().join("data").join("path-list.md");
        assert_eq!(p, expected);
    }
}
