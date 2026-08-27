//! Combined local+remote store for `path-list.md`.
//!
//! Implements the exact same sync semantics as `lib-path-list.sh::path_list_read`:
//! - **A** remote non-empty + local empty → seed local from remote
//! - **C** remote empty + local non-empty → seed remote from local (async)
//! - **B** both non-empty → merge by `path`, sections = set union,
//!   `createdAt` = min, `lastOpenedAt` = max; push merged to remote
//! - network failure or non-2xx → fall back to local cache + warn

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::domain::{PathEntry, PathValidator};
use crate::error::AppError;

use super::cache::{FileCache, format_dt, max_non_empty, min_non_empty};
use super::paths::RemotePaths;
use super::remote::RemoteClient;

/// Concurrency-safe, file-backed, optionally remote-syncing store.
#[derive(Clone)]
pub struct PathListStore {
    cache: FileCache,
    remote: Arc<RwLock<Option<RemoteClient>>>,
    inner: Arc<RwLock<Vec<PathEntry>>>,
}

impl std::fmt::Debug for PathListStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathListStore")
            .field("cache", &self.cache)
            .field("remote_configured", &self.remote.blocking_read().is_some())
            .field("entry_count", &self.inner.blocking_read().len())
            .finish()
    }
}

impl PathListStore {
    /// Create a new store backed by `cache`. Remote is unset by default.
    #[must_use]
    pub fn new(cache: FileCache) -> Self {
        Self {
            cache,
            remote: Arc::new(RwLock::new(None)),
            inner: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Attach a remote (e.g. SilverBullet) for PUT/GET sync.
    pub async fn with_remote(&self, remote: RemoteClient) {
        *self.remote.write().await = Some(remote);
    }

    /// Return the in-memory snapshot of entries (no I/O).
    pub async fn list(&self) -> Result<Vec<PathEntry>, AppError> {
        Ok(self.inner.read().await.clone())
    }

    /// Sync from disk + remote; returns the canonical list.
    ///
    /// # Errors
    /// Returns [`AppError::Internal`] if remote auth is broken AND local is
    /// empty (no fallback available). All other network/IO failures
    /// degrade to local cache with a warning log.
    pub async fn refresh(&self) -> Result<RefreshReport, AppError> {
        // Read local cache.
        let local = self.cache.read().await.unwrap_or_default();
        let local_count = local.len();

        // Try remote GET.
        let (status, remote_value) = match self.remote.read().await.as_ref() {
            Some(remote) => {
                let mut r = remote.clone();
                let path = RemotePaths::new(r.user.as_deref().unwrap_or("unknown"))
                    .path_list_with_slash();
                match r.get(&path).await {
                    Ok((s, body)) => {
                        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Array(Vec::new()));
                        (s, v)
                    }
                    Err(e) => {
                        tracing::warn!("remote read errored: {}", e);
                        (0, Value::Array(Vec::new()))
                    }
                }
            }
            None => (200, Value::Array(Vec::new())),
        };

        // Decide.
        let remote_arr = match &remote_value {
            Value::Array(a) => a.clone(),
            _ => {
                tracing::warn!("remote body is not a JSON array; treating as empty");
                Vec::new()
            }
        };
        let remote_count = remote_arr.len();

        // "Not ok" branches.
        if status == 0 {
            tracing::warn!("remote unreachable; using local cache");
            *self.inner.write().await = local;
            return Ok(RefreshReport {
                from_remote: 0,
                from_local: local_count,
                merged: local_count,
                seeded_remote: false,
            });
        }
        if (status >= 400 && status != 404) || (status >= 500) {
            tracing::warn!("remote returned HTTP {}; using local cache", status);
            *self.inner.write().await = local;
            return Ok(RefreshReport {
                from_remote: 0,
                from_local: local_count,
                merged: local_count,
                seeded_remote: false,
            });
        }

        // Sort helper: by lastOpenedAt desc.
        let sort_desc = |mut v: Vec<PathEntry>| {
            v.sort_by(|a, b| {
                let ka = format_dt(a.last_opened_at);
                let kb = format_dt(b.last_opened_at);
                kb.cmp(&ka)
            });
            v
        };

        // A: remote non-empty + local empty
        if remote_count > 0 && local_count == 0 {
            let from_remote = json_arr_to_entries(&remote_value)?;
            let sorted = sort_desc(from_remote);
            self.cache.write(&sorted).await?;
            *self.inner.write().await = sorted.clone();
            return Ok(RefreshReport {
                from_remote: sorted.len(),
                from_local: 0,
                merged: sorted.len(),
                seeded_remote: false,
            });
        }

        // C: remote empty + local non-empty → seed remote
        if remote_count == 0 && local_count > 0 {
            let sorted = sort_desc(local);
            self.cache.write(&sorted).await?;
            *self.inner.write().await = sorted.clone();
            self.async_push(sorted.clone()).await;
            return Ok(RefreshReport {
                from_remote: 0,
                from_local: sorted.len(),
                merged: sorted.len(),
                seeded_remote: true,
            });
        }

        // both empty
        if remote_count == 0 && local_count == 0 {
            *self.inner.write().await = Vec::new();
            return Ok(RefreshReport::default());
        }

        // B: both non-empty → merge
        let remote_entries = json_arr_to_entries(&remote_value)?;
        let merged = merge_entries(remote_entries, local);
        self.cache.write(&merged).await?;
        *self.inner.write().await = merged.clone();
        self.async_push(merged.clone()).await;

        Ok(RefreshReport {
            from_remote: remote_count,
            from_local: local_count,
            merged: merged.len(),
            seeded_remote: false,
        })
    }

    /// Insert a new path if absent, or leave an existing one untouched.
    pub async fn upsert_path(&self, target: &str) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        if !entries.iter().any(|e| e.path == target) {
            let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());
            entries.push(PathEntry {
                path: target,
                sections: Vec::new(),
                created_at: Some(now),
                last_opened_at: Some(now),
            });
        }
        let snapshot = entries.clone();
        drop(entries);
        self.persist(&snapshot).await?;
        Ok(snapshot)
    }

    /// Refresh `lastOpenedAt` on an existing path (no-op if missing).
    pub async fn touch_path(&self, target: &str) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());
        let mut found = false;
        for e in entries.iter_mut() {
            if e.path == target {
                e.last_opened_at = Some(now);
                if e.created_at.is_none() {
                    e.created_at = Some(now);
                }
                found = true;
            }
        }
        let _ = found;
        let snapshot = entries.clone();
        drop(entries);
        self.persist(&snapshot).await?;
        Ok(snapshot)
    }

    /// Append a session id to a path's `sections` (deduplicated).
    pub async fn append_session(&self, target: &str, sid: &str) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());
        match entries.iter_mut().find(|e| e.path == target) {
            Some(e) => {
                if !e.sections.contains(&sid.to_string()) {
                    e.sections.push(sid.to_string());
                }
                e.last_opened_at = Some(now);
            }
            None => {
                entries.push(PathEntry {
                    path: target,
                    sections: vec![sid.to_string()],
                    created_at: Some(now),
                    last_opened_at: Some(now),
                });
            }
        }
        let snapshot = entries.clone();
        drop(entries);
        self.persist(&snapshot).await?;
        Ok(snapshot)
    }

    /// Remove a path entry. No-op if absent.
    pub async fn remove_path(&self, target: &str) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        entries.retain(|e| e.path != target);
        let snapshot = entries.clone();
        drop(entries);
        self.persist(&snapshot).await?;
        Ok(snapshot)
    }

    /// Persist the snapshot to local cache, then push to remote.
    async fn persist(&self, snapshot: &[PathEntry]) -> Result<(), AppError> {
        self.cache.write(snapshot).await?;
        self.async_push(snapshot.to_vec()).await;
        Ok(())
    }

    async fn async_push(&self, entries: Vec<PathEntry>) {
        let Some(remote_arc) = self.remote.read().await.clone() else {
            return;
        };
        tokio::spawn(async move {
            let body = match serde_json::to_string_pretty(&entries) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("push serialize failed: {}", e);
                    return;
                }
            };
            let mut remote = remote_arc;
            for attempt in 1..=3 {
                let path = RemotePaths::new(remote.user.as_deref().unwrap_or("unknown"))
                    .path_list_with_slash();
                match remote.put(&path, &body).await {
                    Ok(200..=299) => return,
                    Ok(status) if status == 0 => {
                        tracing::warn!("push attempt {}: network unreachable", attempt);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    Ok(status) => {
                        tracing::warn!("push attempt {}: HTTP {}", attempt, status);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    Err(e) => {
                        tracing::warn!("push attempt {}: {}", attempt, e);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
            tracing::error!("push failed after 3 attempts");
        });
    }
}

impl PathListStore {
    /// Construct with both local cache and remote client.
    #[must_use]
    pub fn with_remote_sync(cache_path: impl Into<PathBuf>, remote: RemoteClient) -> Self {
        let s = Self::new(FileCache::new(cache_path));
        // Synchronous init via try_write — best effort at construction time.
        if let Ok(mut slot) = s.remote.try_write() {
            *slot = Some(remote);
        }
        s
    }
}

/// What a [`PathListStore::refresh`] call did.
#[derive(Debug, Default, Clone, Serialize)]
pub struct RefreshReport {
    /// Entries sourced from the remote on this refresh.
    pub from_remote: usize,
    /// Entries sourced from local cache on this refresh.
    pub from_local: usize,
    /// Total entries in the in-memory snapshot after refresh.
    pub merged: usize,
    /// `true` when we pushed local entries to a previously-empty remote.
    pub seeded_remote: bool,
}

/// Convert a `serde_json::Value::Array` of objects into [`PathEntry`]s.
fn json_arr_to_entries(v: &Value) -> Result<Vec<PathEntry>, AppError> {
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        // Use serde_json::from_value to deserialize each item — entries written
        // by other clients may use slightly different field shapes; we accept
        // missing timestamps gracefully because of `#[serde(default)]`.
        match serde_json::from_value::<PathEntry>(item) {
            Ok(e) => out.push(e),
            Err(err) => {
                tracing::warn!("skipping malformed entry: {}", err);
            }
        }
    }
    Ok(out)
}

/// Merge two entry sets by `path` key. `createdAt` = min, `lastOpenedAt` = max,
/// `sections` = set union (preserves order).
#[must_use]
pub fn merge_entries(mut remote: Vec<PathEntry>, mut local: Vec<PathEntry>) -> Vec<PathEntry> {
    remote.append(&mut local);
    let mut by_path: std::collections::BTreeMap<String, PathEntry> =
        std::collections::BTreeMap::new();
    for e in remote {
        match by_path.get_mut(&e.path) {
            Some(existing) => {
                let merged_secs: Vec<String> = existing
                    .sections
                    .iter()
                    .chain(e.sections.iter())
                    .cloned()
                    .collect();
                let mut seen = std::collections::HashSet::new();
                let secs: Vec<String> = merged_secs
                    .into_iter()
                    .filter(|s| seen.insert(s.clone()))
                    .collect();
                existing.sections = secs;

                let a_created = format_dt(existing.created_at);
                let b_created = format_dt(e.created_at);
                if let Some(min) = min_non_empty([a_created.as_str(), b_created.as_str()]) {
                    existing.created_at = parse_dt(min);
                }
                let a_last = format_dt(existing.last_opened_at);
                let b_last = format_dt(e.last_opened_at);
                if let Some(max) = max_non_empty([a_last.as_str(), b_last.as_str()]) {
                    existing.last_opened_at = parse_dt(max);
                }
            }
            None => {
                by_path.insert(e.path.clone(), e);
            }
        }
    }
    let mut out: Vec<PathEntry> = by_path.into_values().collect();
    out.sort_by(|a, b| {
        let ka = format_dt(a.last_opened_at);
        let kb = format_dt(b.last_opened_at);
        kb.cmp(&ka)
    });
    out
}

fn parse_dt(s: &str) -> Option<DateTime<FixedOffset>> {
    crate::storage::cache::parse_dt(s)
}

// Tiny test for the merge function (no async, no I/O).
#[cfg(test)]
mod tests {
    use super::*;

    fn e(path: &str, created: &str, last: &str, sections: &[&str]) -> PathEntry {
        PathEntry {
            path: path.to_string(),
            sections: sections.iter().map(|s| s.to_string()).collect(),
            created_at: crate::storage::cache::parse_dt(created),
            last_opened_at: crate::storage::cache::parse_dt(last),
        }
    }

    #[test]
    fn merge_unions_sections_and_takes_min_max_timestamps() {
        let r = vec![
            e("/a", "2026-08-01T00:00:00+0800", "2026-08-10T00:00:00+0800", &["s1"]),
            e("/b", "2026-07-01T00:00:00+0800", "2026-07-10T00:00:00+0800", &["s3"]),
        ];
        let l = vec![
            e("/a", "", "2026-08-12T00:00:00+0800", &["s2"]),
            e("/c", "2026-06-01T00:00:00+0800", "2026-06-01T00:00:00+0800", &[]),
        ];
        let merged = merge_entries(r, l);

        let a = merged.iter().find(|e| e.path == "/a").expect("/a present");
        assert_eq!(a.sections, vec!["s1".to_string(), "s2".to_string()]);
        assert_eq!(
            format_dt(a.created_at),
            "2026-08-01T00:00:00+0800",
            "createdAt = min of non-empty (remote had 2026-08-01, local was empty)"
        );
        assert_eq!(
            format_dt(a.last_opened_at),
            "2026-08-12T00:00:00+0800",
            "lastOpenedAt = max"
        );

        assert!(merged.iter().any(|e| e.path == "/b"));
        assert!(merged.iter().any(|e| e.path == "/c"));
    }
}
