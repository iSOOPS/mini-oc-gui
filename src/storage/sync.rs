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

/// Legacy remote path used by versions of the app before the namespaced
/// remote layout was introduced. Read once during the first refresh
/// after upgrade; never written by new code.
pub(crate) const LEGACY_REMOTE_PATH: &str = "/serv/opencode/path-list.md";

/// Concurrency-safe, file-backed, optionally remote-syncing store.
#[derive(Clone)]
pub struct PathListStore {
    cache: FileCache,
    remote: Arc<RwLock<Option<RemoteClient>>>,
    inner: Arc<RwLock<Vec<PathEntry>>>,
    /// One-shot guard for the legacy-path migration. Set to `true` the
    /// first time [`migrate_from_legacy_remote`](Self::migrate_from_legacy_remote)
    /// runs so the migration runs exactly once per process lifetime.
    migration_done: Arc<RwLock<bool>>,
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
            migration_done: Arc::new(RwLock::new(false)),
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
        self.persist(&snapshot, false).await?;
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
        self.persist(&snapshot, false).await?;
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
        self.persist(&snapshot, false).await?;
        Ok(snapshot)
    }

    /// Remove a path entry. No-op if absent.
    ///
    /// The remote PUT is awaited synchronously (3 retries, 1s backoff);
    /// a network/server error is returned to the caller rather than
    /// silently logged, because otherwise the next refresh would
    /// resurrect the entry from the still-intact remote file.
    pub async fn remove_path(&self, target: &str) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        entries.retain(|e| e.path != target);
        let snapshot = entries.clone();
        drop(entries);
        tracing::info!(
            target: "sync",
            "remove_path: {} (snapshot now {} entries)",
            target,
            snapshot.len()
        );
        self.persist(&snapshot, true).await?;
        Ok(snapshot)
    }

    /// Remove a single session id from a path's `sections`.
    ///
    /// No-op if the path or session id does not exist (idempotent). The
    /// path entry itself is kept even when `sections` becomes empty — we
    /// do not auto-delete the project just because its last session was
    /// removed, since the user may add a new session later.
    ///
    /// Like [`remove_path`](Self::remove_path), the remote PUT is awaited
    /// synchronously; a network/server error is returned to the caller so
    /// a stale remote cannot resurrect the section on the next refresh.
    ///
    /// # Errors
    /// Returns [`AppError::PathValidation`] if `target` fails validation,
    /// or [`AppError::Internal`] if the remote push fails after retries.
    pub async fn remove_session(
        &self,
        target: &str,
        sid: &str,
    ) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        let mut found = false;
        for e in entries.iter_mut() {
            if e.path == target {
                let before = e.sections.len();
                e.sections.retain(|s| s != sid);
                if e.sections.len() != before {
                    found = true;
                }
                break;
            }
        }
        let _ = found;
        let snapshot = entries.clone();
        drop(entries);
        tracing::info!(
            target: "sync",
            "remove_session: {target} sid={sid} (found={found}, snapshot {} entries)",
            snapshot.len()
        );
        self.persist(&snapshot, true).await?;
        Ok(snapshot)
    }

    /// Persist the snapshot to local cache, then push to remote.
    ///
    /// `is_delete` flips the semantics for **delete** operations
    /// (`remove_path` / `remove_session`): those must propagate remote
    /// failures back to the caller — otherwise the user sees a green
    /// "deleted" status bar while the remote still holds the entry, and
    /// the next [`refresh`](Self::refresh) will resurrect it from the
    /// server. For **upsert** operations (add / touch / append) the
    /// push stays fire-and-forget — losing an add to a transient network
    /// blip is recoverable via the next refresh; failing loudly every
    /// time the network flickers would be much worse UX.
    async fn persist(&self, snapshot: &[PathEntry], is_delete: bool) -> Result<(), AppError> {
        self.cache.write(snapshot).await?;
        if is_delete {
            self.push_blocking(snapshot.to_vec()).await
        } else {
            self.async_push(snapshot.to_vec()).await;
            Ok(())
        }
    }

    /// Synchronous push used by delete operations. 3 attempts with 1s
    /// backoff, each transition logged at `info` / `warn` / `error`. On
    /// exhaustion returns `AppError::Internal` with a short, actionable
    /// message — the caller (TUI status bar) is expected to surface it.
    async fn push_blocking(&self, entries: Vec<PathEntry>) -> Result<(), AppError> {
        let Some(remote_arc) = self.remote.read().await.clone() else {
            // No remote configured: delete is "local-only" by definition.
            // Still log so it's clear in the audit trail why we silently
            // returned Ok.
            tracing::info!(
                target: "sync",
                "remote not configured; delete applied to local cache only ({} entries)",
                entries.len()
            );
            return Ok(());
        };

        let body = serde_json::to_string_pretty(&entries).map_err(|e| {
            tracing::error!(target: "sync", "push serialize failed: {}", e);
            AppError::Internal(format!("serialize path-list: {e}"))
        })?;

        let mut remote = remote_arc;
        let path = RemotePaths::new(remote.user.as_deref().unwrap_or("unknown"))
            .path_list_with_slash();

        tracing::info!(
            target: "sync",
            "delete push start: PUT {path} ({} entries)",
            entries.len()
        );

        for attempt in 1..=3 {
            match remote.put(&path, &body).await {
                Ok(200..=299) => {
                    tracing::info!(
                        target: "sync",
                        "delete push ok: PUT {path} succeeded on attempt {attempt}"
                    );
                    return Ok(());
                }
                Ok(status) if status == 0 => {
                    tracing::warn!(
                        target: "sync",
                        "delete push attempt {attempt}/3: network unreachable (PUT {path})"
                    );
                }
                Ok(status) => {
                    tracing::warn!(
                        target: "sync",
                        "delete push attempt {attempt}/3: PUT {path} returned HTTP {status}"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "sync",
                        "delete push attempt {attempt}/3: PUT {path} errored: {e}"
                    );
                }
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }

        tracing::error!(
            target: "sync",
            "delete push failed after 3 attempts: PUT {path} — local cache was already updated; \
             remote is out of sync until next refresh or manual retry"
        );
        Err(AppError::Internal(
            "remote path-list push failed after 3 attempts (see logs); \
             local cache updated but remote still holds the deleted entry"
                .to_string(),
        ))
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

    /// One-shot legacy-path migration.
    ///
    /// Reads the pre-namespaced remote file at [`LEGACY_REMOTE_PATH`] and,
    /// if it returns a non-empty JSON array, merges those entries into the
    /// local cache using the same semantics as [`merge_entries`]:
    /// dedup by `path`, `sections` = union, `createdAt` = min,
    /// `lastOpenedAt` = max. If the new namespaced path on the remote is
    /// empty, the merged set is pushed up so other clients on the new
    /// layout can pick it up.
    ///
    /// Idempotent: subsequent calls after the first one are no-ops.
    /// Network failures are non-fatal — a warning is logged and the
    /// migration is treated as done so we don't loop on every startup.
    ///
    /// # Errors
    /// Returns [`AppError`] only for unrecoverable local-cache write
    /// failures. Remote I/O is always best-effort.
    pub async fn migrate_from_legacy_remote(&self) -> Result<MigrationReport, AppError> {
        // Atomically flip the one-shot guard. If already set, no-op.
        {
            let mut done = self.migration_done.write().await;
            if *done {
                return Ok(MigrationReport::default());
            }
            *done = true;
        }

        let Some(remote_arc) = self.remote.read().await.clone() else {
            tracing::info!("legacy migration skipped: no remote configured");
            return Ok(MigrationReport::default());
        };

        let mut remote = remote_arc;
        let body = match remote.get(LEGACY_REMOTE_PATH).await {
            Ok((200, body)) => body,
            Ok((status, _)) => {
                tracing::info!(
                    "legacy migration: GET {LEGACY_REMOTE_PATH} returned HTTP {status}; nothing to migrate"
                );
                return Ok(MigrationReport::default());
            }
            Err(e) => {
                tracing::warn!("legacy migration: GET {LEGACY_REMOTE_PATH} failed: {e}");
                return Ok(MigrationReport::default());
            }
        };

        let legacy_value: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "legacy migration: body at {LEGACY_REMOTE_PATH} is not JSON: {e}"
                );
                return Ok(MigrationReport::default());
            }
        };
        let legacy_entries = match json_arr_to_entries(&legacy_value) {
            Ok(es) => es,
            Err(e) => {
                tracing::warn!("legacy migration: {e}");
                return Ok(MigrationReport::default());
            }
        };
        if legacy_entries.is_empty() {
            tracing::info!("legacy migration: remote returned empty array; nothing to merge");
            return Ok(MigrationReport::default());
        }

        let local = self.cache.read().await.unwrap_or_default();
        let merged = merge_entries(legacy_entries, local);
        let merged_count = merged.len();
        self.cache.write(&merged).await?;
        *self.inner.write().await = merged.clone();

        // If the new path is empty, seed it with the merged set so other
        // clients (e.g. running on a different machine under the same
        // sb_user) can pick it up on their next refresh.
        let new_path = RemotePaths::new(remote.user.as_deref().unwrap_or("unknown"))
            .path_list_with_slash();
        let need_seeding = match remote.get(&new_path).await {
            Ok((200, body)) => {
                let v: Value =
                    serde_json::from_str(&body).unwrap_or(Value::Array(Vec::new()));
                v.as_array().map_or(true, |a| a.is_empty())
            }
            Ok(_) | Err(_) => true,
        };
        if need_seeding {
            self.async_push(merged).await;
        }

        tracing::info!("legacy migration: merged entries into new layout");
        Ok(MigrationReport {
            migrated_entries: merged_count,
        })
    }
}

/// Outcome of [`PathListStore::migrate_from_legacy_remote`].
#[derive(Debug, Default, Clone, Serialize)]
pub struct MigrationReport {
    /// Number of entries in the in-memory snapshot after migration.
    /// `0` when no migration was needed (no remote, missing/empty legacy
    /// file, network failure, or already migrated).
    pub migrated_entries: usize,
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

    #[tokio::test]
    async fn migrate_is_noop_when_remote_unset() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let report = store.migrate_from_legacy_remote().await.expect("migrate");
        assert_eq!(report.migrated_entries, 0);

        // Second call must remain idempotent.
        let report = store.migrate_from_legacy_remote().await.expect("migrate 2");
        assert_eq!(report.migrated_entries, 0);
    }

    #[tokio::test]
    async fn migrate_is_idempotent_under_repeat_calls() {
        // The one-shot guard must flip on the first call, so the second
        // call is a no-op regardless of remote state.
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        // Manually flip the guard to simulate "already migrated this process".
        *store.migration_done.write().await = true;

        let report = store.migrate_from_legacy_remote().await.expect("migrate");
        assert_eq!(report.migrated_entries, 0);
    }

    #[tokio::test]
    async fn remove_path_without_remote_returns_ok_and_drops_locally() {
        // Without a remote configured, removing a path is a local-only
        // operation and must succeed. Locks the "is_delete=true path is
        // exercised, no remote -> Ok" branch of push_blocking.
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache.clone());

        store
            .upsert_path("/proj/keep")
            .await
            .expect("upsert /proj/keep");
        store
            .upsert_path("/proj/drop")
            .await
            .expect("upsert /proj/drop");

        let after = store
            .remove_path("/proj/drop")
            .await
            .expect("remove_path must succeed when no remote");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].path, "/proj/keep");

        // Local cache must reflect the delete — otherwise the next
        // process restart would resurrect the entry from disk.
        let on_disk = cache.read().await.expect("read cache");
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].path, "/proj/keep");
    }

    #[tokio::test]
    async fn remove_session_without_remote_returns_ok() {
        // Same contract for remove_session: no remote -> Ok, local
        // snapshot has the sid filtered out.
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        store.upsert_path("/proj").await.expect("upsert");
        store
            .append_session("/proj", "ses_keep")
            .await
            .expect("append keep");
        store
            .append_session("/proj", "ses_drop")
            .await
            .expect("append drop");

        let after = store
            .remove_session("/proj", "ses_drop")
            .await
            .expect("remove_session must succeed when no remote");
        let entry = after.iter().find(|e| e.path == "/proj").expect("/proj");
        assert_eq!(entry.sections, vec!["ses_keep".to_string()]);
    }
}
