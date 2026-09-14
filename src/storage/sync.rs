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

    /// Build the [`RemotePaths`] the store currently syncs to.
    ///
    /// Reflects the identity carried by the attached [`RemoteClient`]:
    /// when it has a `user_id` (set via
    /// [`RemoteClient::from_user_info_v2`](super::remote::RemoteClient::from_user_info_v2))
    /// the new layout `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`
    /// is used; otherwise the legacy sb-username layout.
    ///
    /// Uses `blocking_read` internally — call from synchronous code only
    /// (the async paths inside this module use `RemoteClient::remote_paths`
    /// on an already-acquired handle instead).
    ///
    /// # Errors
    /// Propagates [`AppError::Internal`] from [`RemoteClient::remote_paths`]
    /// when `user_id` is set but `device_name` is missing or whitespace-only
    /// (i.e. the new-format path identity is incomplete).
    pub fn build_remote_paths(&self) -> Result<RemotePaths, AppError> {
        let remote = self.remote.blocking_read();
        match remote.as_ref() {
            Some(r) => r.remote_paths(),
            None => Ok(RemotePaths::with_user_info("", "")),
        }
    }

    /// Clone the currently configured [`RemoteClient`], if any.
    ///
    /// For callers that need a remote-only read (e.g. the OC-projects entry
    /// flow) without going through the local-cache merge in [`refresh`].
    pub async fn remote_client(&self) -> Option<RemoteClient> {
        self.remote.read().await.clone()
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
                let path = match r.remote_paths() {
                    Ok(rp) => rp.path_list_with_slash(),
                    Err(e) => {
                        tracing::warn!(
                            target: "sync",
                            "refresh: cannot derive remote path ({}); falling back to local cache",
                            e
                        );
                        *self.inner.write().await = local.clone();
                        return Ok(RefreshReport {
                            from_remote: 0,
                            from_local: local_count,
                            merged: local_count,
                            seeded_remote: false,
                        });
                    }
                };
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

    /// 在远端创建空 path-list 条目（sections 为空）。
    ///
    /// 用户新选一个项目路径时调用：确保该路径以 `sections: []` 的空结构
    /// 出现在远端 path-list（新格式
    /// `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`）。
    ///
    /// 与 [`upsert_path`](Self::upsert_path) 的差异：远端 PUT 是
    /// **同步**的（`push_blocking`，3 次重试）—— 调用方能立刻知道是否
    /// 成功。若条目尚不存在会先补进本地快照（幂等，可与 `upsert_path`
    /// 连续调用不冲突），再整体推送快照，避免单条 PUT 覆盖远端全量。
    ///
    /// # Errors
    /// Returns [`AppError::PathValidation`] if `target` fails validation,
    /// or [`AppError::Internal`] if the remote push fails after retries.
    pub async fn create_remote_path(&self, target: &str) -> Result<(), AppError> {
        let target = PathValidator::validate(target)?;
        {
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
        }
        let snapshot = self.inner.read().await.clone();
        self.cache.write(&snapshot).await?;
        self.push_blocking(snapshot).await
    }

    /// 批量同步一个项目从 opencode serve 读到的 sessions 到 path-list。
    ///
    /// 合并语义与 [`append_session`](Self::append_session) 一致（按
    /// `session.id` 去重，同 id 时 `updated_at` 较新者胜出；路径缺失时
    /// 创建新条目），但**只落一次盘、推一次远端** —— 选已有项目进入
    /// 会话列表时的「整理后汇总上传」走这里，而不是逐条 append。
    ///
    /// # Errors
    /// Returns [`AppError::PathValidation`] if `target` fails validation.
    pub async fn sync_project_sessions(
        &self,
        target: &str,
        sessions: &[crate::domain::Session],
    ) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());
        match entries.iter_mut().find(|e| e.path == target) {
            Some(e) => {
                for session in sessions {
                    let mut replaced = false;
                    for sec in e.sections.iter_mut() {
                        if sec.id == session.id {
                            if session.updated_at > sec.updated_at {
                                *sec = session.clone();
                            }
                            replaced = true;
                            break;
                        }
                    }
                    if !replaced {
                        e.sections.push(session.clone());
                    }
                }
                e.last_opened_at = Some(now);
            }
            None => {
                entries.push(PathEntry {
                    path: target,
                    sections: sessions.to_vec(),
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

    /// Append (or upsert) a session in a path's `sections`.
    ///
    /// If a section with the same `session.id` already exists, it is
    /// replaced only when `session.updated_at` is strictly newer than the
    /// existing entry's `updated_at`; otherwise the existing entry is kept
    /// unchanged. If the path does not yet exist, it is created with this
    /// session as its only section. The path's `lastOpenedAt` is always
    /// refreshed to `now`.
    pub async fn append_session(
        &self,
        target: &str,
        session: &crate::domain::Session,
    ) -> Result<Vec<PathEntry>, AppError> {
        let target = PathValidator::validate(target)?;
        let mut entries = self.inner.write().await;
        let now = chrono::Local::now().with_timezone(chrono::Local::now().offset());
        match entries.iter_mut().find(|e| e.path == target) {
            Some(e) => {
                let mut replaced = false;
                for sec in e.sections.iter_mut() {
                    if sec.id == session.id {
                        if session.updated_at > sec.updated_at {
                            *sec = session.clone();
                        }
                        replaced = true;
                        break;
                    }
                }
                if !replaced {
                    e.sections.push(session.clone());
                }
                e.last_opened_at = Some(now);
            }
            None => {
                entries.push(PathEntry {
                    path: target,
                    sections: vec![session.clone()],
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
                e.sections.retain(|s| s.id != sid);
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
    ///
    /// **Bug 2 修复**:之前在没有 remote client 时静默 `return Ok(())`,
    /// 让调用方误以为远端已同步。现改为返回明确的 `AppError::Internal`,
    /// 调用方可显示"远程存储未配置"提示。
    async fn push_blocking(&self, entries: Vec<PathEntry>) -> Result<(), AppError> {
        let Some(remote_arc) = self.remote.read().await.clone() else {
            return Err(AppError::Internal(
                "远程存储未配置（需要先完成账户登录 + fetch_user_info）".to_string(),
            ));
        };

        let body = serde_json::to_string_pretty(&entries).map_err(|e| {
            tracing::error!(target: "sync", "push serialize failed: {}", e);
            AppError::Internal(format!("serialize path-list: {e}"))
        })?;

        let mut remote = remote_arc;
        let path = remote.remote_paths()?.path_list_with_slash();

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
            // Derive the path once: device_name must be selected upstream,
            // so re-deriving on every retry would only surface the same
            // configuration error repeatedly. A failure here aborts the
            // push — fire-and-forget upserts have nothing to retry.
            let path = match remote.remote_paths() {
                Ok(rp) => rp.path_list_with_slash(),
                Err(e) => {
                    tracing::error!(
                        target: "sync",
                        "async push aborted: cannot derive remote path ({}); \
                         local cache is authoritative until device is selected",
                        e
                    );
                    return;
                }
            };
            for attempt in 1..=3 {
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
        let new_path = remote.remote_paths()?.path_list_with_slash();
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

/// Merge two entry sets by `path` key.
///
/// For each `path`, sections are de-duplicated by `session.id`; when the
/// same id appears on both sides, the entry with the strictly larger
/// `updated_at` wins (ties keep the existing one). Path-level
/// `createdAt` = min, `lastOpenedAt` = max.
#[must_use]
pub fn merge_entries(mut remote: Vec<PathEntry>, mut local: Vec<PathEntry>) -> Vec<PathEntry> {
    remote.append(&mut local);
    let mut by_path: std::collections::BTreeMap<String, PathEntry> =
        std::collections::BTreeMap::new();
    for e in remote {
        match by_path.get_mut(&e.path) {
            Some(existing) => {
                // 合并 sections：按 id 去重，同 id 按 updated_at 较新者胜出
                for new_sec in e.sections {
                    match existing.sections.iter_mut().find(|s| s.id == new_sec.id) {
                        Some(existing_sec) => {
                            if new_sec.updated_at > existing_sec.updated_at {
                                *existing_sec = new_sec;
                            }
                        }
                        None => existing.sections.push(new_sec),
                    }
                }

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
    use crate::domain::Session;
    use chrono::{Duration, FixedOffset, TimeZone};

    fn e(path: &str, created: &str, last: &str, sections: &[&str]) -> PathEntry {
        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let dt = off.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        PathEntry {
            path: path.to_string(),
            sections: sections
                .iter()
                .map(|s| Session::new(*s, format!("session-{}", &s[..s.len().min(8)]), path.to_string(), dt))
                .collect(),
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
        let ids: Vec<&str> = a.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2"]);
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

    #[test]
    fn merge_unions_sessions_by_id_and_takes_newer_when_conflict() {
        let off = FixedOffset::east_opt(8 * 3600).unwrap();
        let t0 = off.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let t1 = t0 + Duration::days(5);

        let mk = |id: &str, title: &str, updated: chrono::DateTime<FixedOffset>| Session {
            id: id.into(),
            title: title.into(),
            directory: "/p".into(),
            created_at: updated,
            updated_at: updated,
        };

        let remote = vec![PathEntry {
            path: "/a".into(),
            sections: vec![mk("ses_1", "remote title", t0)],
            created_at: parse_dt("2026-08-01T00:00:00+0800"),
            last_opened_at: parse_dt("2026-08-10T00:00:00+0800"),
        }];

        let local = vec![PathEntry {
            path: "/a".into(),
            sections: vec![mk("ses_1", "local newer title", t1)],
            created_at: parse_dt("2026-08-02T00:00:00+0800"),
            last_opened_at: parse_dt("2026-08-12T00:00:00+0800"),
        }];

        let merged = merge_entries(remote, local);
        let a = merged.iter().find(|e| e.path == "/a").expect("/a present");
        assert_eq!(a.sections.len(), 1, "同 id 应合并为一份");
        assert_eq!(a.sections[0].title, "local newer title");
        assert_eq!(a.sections[0].updated_at, t1);
    }

    #[test]
    fn merge_unions_different_session_ids() {
        let off = FixedOffset::east_opt(8 * 3600).unwrap();
        let t = off.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap();
        let mk = |id: &str| Session::new(id, "t", "/p", t);

        let remote = vec![PathEntry {
            path: "/a".into(),
            sections: vec![mk("ses_1")],
            created_at: None,
            last_opened_at: None,
        }];
        let local = vec![PathEntry {
            path: "/a".into(),
            sections: vec![mk("ses_2")],
            created_at: None,
            last_opened_at: None,
        }];

        let merged = merge_entries(remote, local);
        let a = merged.iter().find(|e| e.path == "/a").expect("/a");
        let ids: Vec<&str> = a.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"ses_1"));
        assert!(ids.contains(&"ses_2"));
    }

    #[tokio::test]
    async fn append_session_dedupes_by_id_and_updates_existing_when_newer() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let t0 = off.with_ymd_and_hms(2026, 9, 13, 10, 0, 0).unwrap();
        let t1 = t0 + Duration::hours(1);

        let s1 = Session::new("ses_x", "old title", "/proj", t0);
        store.append_session("/proj", &s1).await.expect("append 1");

        let s2_newer = Session {
            id: "ses_x".into(),
            title: "new title".into(),
            directory: "/proj/sub".into(),
            created_at: t0,
            updated_at: t1,
        };
        store.append_session("/proj", &s2_newer).await.expect("append 2");

        let list = store.list().await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].sections.len(), 1, "同 id 不应产生重复 section");
        assert_eq!(list[0].sections[0].id, "ses_x");
        assert_eq!(list[0].sections[0].title, "new title");
        assert_eq!(list[0].sections[0].directory, "/proj/sub");
        assert_eq!(list[0].sections[0].updated_at, t1);
    }

    #[tokio::test]
    async fn append_session_keeps_existing_when_incoming_older() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let t0 = off.with_ymd_and_hms(2026, 9, 13, 10, 0, 0).unwrap();
        let t_old = t0 - Duration::hours(1);

        let newer = Session::new("ses_y", "newer", "/proj", t0);
        store.append_session("/proj", &newer).await.expect("append 1");

        let older = Session {
            id: "ses_y".into(),
            title: "older".into(),
            directory: "/proj".into(),
            created_at: t_old,
            updated_at: t_old,
        };
        store.append_session("/proj", &older).await.expect("append 2");

        let list = store.list().await.expect("list");
        assert_eq!(list[0].sections.len(), 1);
        assert_eq!(list[0].sections[0].title, "newer");
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

    /// Bug 2 修复:无 remote 时 remove_path 现在返回 Err,但本地 cache 仍更新。
    #[tokio::test]
    async fn remove_path_without_remote_returns_ok_and_drops_locally() {
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

        let after = store.remove_path("/proj/drop").await;
        // 修复后:无 remote 时返回 Err,但本地 cache 仍更新
        assert!(after.is_err(), "无 remote 时必须返回 Err");
        let _after = after.unwrap_err();
        // 检查本地 cache 已删除
        let on_disk = cache.read().await.expect("read cache");
        assert_eq!(on_disk.len(), 1);
        assert_eq!(on_disk[0].path, "/proj/keep");
    }

    /// Bug 2 修复:无 remote 时 remove_session 现在返回 Err,但本地 snapshot 仍过滤掉 sid。
    #[tokio::test]
    async fn remove_session_without_remote_returns_ok() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let t = off.with_ymd_and_hms(2026, 9, 13, 10, 0, 0).unwrap();

        store.upsert_path("/proj").await.expect("upsert");
        store
            .append_session("/proj", &Session::new("ses_keep", "k", "/proj", t))
            .await
            .expect("append keep");
        store
            .append_session("/proj", &Session::new("ses_drop", "d", "/proj", t))
            .await
            .expect("append drop");

        let result = store.remove_session("/proj", "ses_drop").await;
        // 修复后:无 remote 时返回 Err,但本地 snapshot 已过滤
        assert!(result.is_err(), "无 remote 时必须返回 Err");
        // 验证本地 cache 已过滤掉 ses_drop
        let list = store.list().await.expect("list");
        let entry = list.iter().find(|e| e.path == "/proj").expect("/proj");
        let ids: Vec<&str> = entry.sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["ses_keep"]);
    }

    /// Bug 2 修复:create_remote_path 在无 remote 时现在返回 Err 而不是 Ok(())。
    /// 本测试更新断言以反映新行为,但保留「本地仍 seeds 空条目」的语义验证。
    #[tokio::test]
    async fn create_remote_path_without_remote_seeds_empty_entry_locally() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let result = store.create_remote_path("/proj/new-empty").await;
        // 修复后:无 remote 时返回 Err
        assert!(result.is_err(), "无 remote 时必须返回 Err");
        // 但本地 cache 仍写入(由 create_remote_path 在 push_blocking 之前的代码完成)
        let list = store.list().await.expect("list");
        let entry = list.iter().find(|e| e.path == "/proj/new-empty").expect("entry");
        assert!(entry.sections.is_empty(), "新项目远端结构 sections 应为空");
        assert!(entry.created_at.is_some());

        // 幂等：重复调用不产生重复条目。
        let result2 = store.create_remote_path("/proj/new-empty").await;
        assert!(result2.is_err(), "idempotent: 无 remote 时仍返回 Err");
        let list = store.list().await.expect("list");
        assert_eq!(
            list.iter().filter(|e| e.path == "/proj/new-empty").count(),
            1
        );
    }

    /// Bug 2 修复:无 remote 时 create_remote_path 返回 Err,但本地 list 仍包含已有条目。
    #[tokio::test]
    async fn create_remote_path_keeps_existing_entries_in_snapshot() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        store.upsert_path("/proj/keep").await.expect("upsert keep");
        let result = store.create_remote_path("/proj/new-empty").await;
        // 修复后:无 remote 时返回 Err
        assert!(result.is_err(), "无 remote 时必须返回 Err");

        // 但本地 list 仍包含已有条目(不受 push_blocking 失败影响)
        let list = store.list().await.expect("list");
        assert!(list.iter().any(|e| e.path == "/proj/keep"));
        assert!(list.iter().any(|e| e.path == "/proj/new-empty"));
    }

    #[tokio::test]
    async fn sync_project_sessions_bulk_merges_and_dedupes() {
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache.clone());

        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let t0 = off.with_ymd_and_hms(2026, 9, 14, 9, 0, 0).unwrap();
        let t1 = t0 + Duration::hours(2);

        // 先有一条本地记录的同 id session（旧标题）。
        store
            .append_session("/proj", &Session::new("ses_a", "old", "/proj", t0))
            .await
            .expect("seed");

        // 模拟 opencode serve 读回的会话：一条同 id 更新（应胜出）、
        // 一条新 id（应并入）。
        let fetched = vec![
            Session::new("ses_a", "fresh-from-serve", "/proj", t1),
            Session::new("ses_b", "another", "/proj", t0),
        ];
        let snapshot = store
            .sync_project_sessions("/proj", &fetched)
            .await
            .expect("sync");

        let entry = snapshot.iter().find(|e| e.path == "/proj").expect("/proj");
        let a = entry.sections.iter().find(|s| s.id == "ses_a").expect("ses_a");
        assert_eq!(a.title, "fresh-from-serve");
        assert!(entry.sections.iter().any(|s| s.id == "ses_b"));

        // 本地缓存与内存快照一致。
        let on_disk = cache.read().await.expect("read cache");
        let entry = on_disk.iter().find(|e| e.path == "/proj").expect("/proj");
        assert_eq!(entry.sections.len(), 2);
    }

    #[tokio::test]
    async fn sync_project_sessions_creates_entry_when_missing() {
        // 远端/本地都还没有该项目条目时，汇总上传应创建新条目。
        let dir = tempfile::TempDir::new().expect("tmpdir");
        let cache = FileCache::new(dir.path().join("path-list.md"));
        let store = PathListStore::new(cache);

        let off = FixedOffset::east_opt(8 * 3600).expect("offset");
        let t = off.with_ymd_and_hms(2026, 9, 14, 9, 0, 0).unwrap();
        let fetched = vec![Session::new("ses_x", "x", "/proj", t)];

        let snapshot = store
            .sync_project_sessions("/proj", &fetched)
            .await
            .expect("sync");
        let entry = snapshot.iter().find(|e| e.path == "/proj").expect("/proj");
        assert_eq!(entry.sections.len(), 1);
        assert_eq!(entry.sections[0].id, "ses_x");
    }

    /// Bug 2 (storage 层):修复前 push_blocking 在没有 remote client 时静默
    /// return Ok(()) —— 调用方无从知晓远端同步未发生。
    /// 修复后返回 AppError::Internal("远程存储未配置...")。
    #[tokio::test]
    async fn push_blocking_without_remote_returns_error() {
        use crate::storage::cache::FileCache;
        use crate::storage::sync::PathListStore;
        // 不调用 with_remote —— 默认无 remote
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("path-list.md");
        let cache = FileCache::new(&cache_path);
        let store = PathListStore::new(cache);
        // create_remote_path 内部最终调 push_blocking
        let result = store.create_remote_path("/tmp/test-project").await;
        assert!(
            result.is_err(),
            "无 remote 时 create_remote_path 必须返回 Err,实际: {result:?}"
        );
        let err = result.unwrap_err();
        // AppError::Internal(String) 变体
        assert!(
            matches!(err, crate::error::AppError::Internal(_)),
            "应返回 AppError::Internal,实际: {err:?}"
        );
    }
}
