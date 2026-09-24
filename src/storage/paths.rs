//! Remote path builder for the per-(user, OS, machine) MD store layout.
//!
//! Two layouts are supported:
//! - **Legacy** (`path_list`): `serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md`,
//!   where `sb_user` comes from the sb config embedded in the user info returned
//!   by `/api/user/info` (see [`RemotePaths::from_sb_config`]) and `pcname` is
//!   the local OS username.
//! - **New** (`path_list_new_format`):
//!   `serv/opencode/{user_id}/{pctype}/{device_name}/path-list.md`, where
//!   `user_id` is `info.id` from `/api/user/info` and `device_name` is the
//!   bound device name (`DEVICE_NAME`). The new layout is preferred whenever
//!   `user_id` is set (see [`RemotePaths::with_user_info`]).
//!
//! **历史布局变更（suffix 修复）**：新格式曾一度使用无 `.md` 后缀的
//! `path-list` 路径。文件在远端磁盘上真实存在且 `.fs` API 可读写，但
//! SilverBullet 只把 `.md` 文件索引为 page，导致无后缀文件在任何
//! 服务商界面不可见（被误判为"未存储"）。现已恢复 `.md` 后缀；旧
//! 无后缀路径仅保留构造器 [`RemotePaths::path_list_new_format_nosuffix`]
//! 供 [`crate::storage::sync::PathListStore::migrate_v3_suffix`] 一次性
//! 迁移读取，勿用于写入。

use whoami::username;

use crate::account::RemoteSbConfig;

/// Compile-time platform tag used as a path segment.
///
/// Returns `"macos"` on macOS, `"windows"` on Windows, `"unknown"` on any
/// other target (project only ships for the two named).
#[must_use]
pub fn pctype() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// Local OS username via the `whoami` crate; falls back to `"unknown"`
/// when the lookup returns an empty string (e.g. headless CI).
#[must_use]
pub fn pcname() -> String {
    let s = username().trim().to_string();
    if s.is_empty() {
        "unknown".to_string()
    } else {
        s
    }
}

/// Per-tenant remote path layout builder.
///
/// Holds either the new-format identity (`user_id` + `device_name`, see
/// [`RemotePaths::with_user_info`]) or the legacy identity
/// (`sb_user` + runtime `pcname`, see [`RemotePaths::new`]); `pctype` is
/// derived from the compile target at construction time. When `user_id` is
/// non-empty the new layout is preferred by [`RemotePaths::path_list`].
#[derive(Debug, Clone)]
pub struct RemotePaths {
    /// 账户用户ID（`/api/user/info` 返回的 `info.id`）。新格式路径段。
    pub user_id: String,
    /// Compile-time platform tag (shared by both layouts).
    pub pctype: &'static str,
    /// 已绑定的设备名称（`account_config.device_name`）。新格式路径段。
    pub device_name: String,
    /// Legacy：SilverBullet 登录名（`sb.username`）。仅用于旧路径兼容。
    pub sb_user: String,
    /// Legacy：本地 OS 用户名（`whoami`）。仅用于旧路径兼容。
    pub pcname: String,
}

impl RemotePaths {
    /// Build a legacy path plan rooted at `sb_user`.
    #[must_use]
    pub fn new(sb_user: impl Into<String>) -> Self {
        Self {
            user_id: String::new(),
            pctype: pctype(),
            device_name: String::new(),
            sb_user: sb_user.into(),
            pcname: pcname(),
        }
    }

    /// Build a new-format path plan rooted at `user_id / pctype / device_name`.
    ///
    /// When `user_id` is non-empty, [`path_list`](Self::path_list) prefers the
    /// new layout `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`.
    #[must_use]
    pub fn with_user_info(user_id: impl Into<String>, device_name: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            pctype: pctype(),
            device_name: device_name.into(),
            sb_user: String::new(),
            pcname: String::new(),
        }
    }

    /// Build a legacy path plan from the sb config in the user info returned
    /// by `/api/user/info` (rooted at the sb username).
    #[must_use]
    pub fn from_sb_config(info: &RemoteSbConfig) -> Self {
        Self::new(info.username.clone())
    }

    /// New-format path (no leading slash):
    /// `serv/opencode/{user_id}/{pctype}/{device_name}/path-list.md`.
    ///
    /// 必须带 `.md` 后缀 —— SilverBullet 只把 `.md` 文件索引为 page，
    /// 无后缀文件虽可通过 `.fs` API 读写，但在服务商界面不可见。
    #[must_use]
    pub fn path_list_new_format(&self) -> String {
        format!(
            "serv/opencode/{}/{}/{}/path-list.md",
            self.user_id, self.pctype, self.device_name
        )
    }

    /// v3 过渡期的旧布局（无 `.md` 后缀）：
    /// `serv/opencode/{user_id}/{pctype}/{device_name}/path-list`。
    ///
    /// 仅供 [`crate::storage::sync::PathListStore::migrate_v3_suffix`]
    /// 读取历史数据，**勿用于写入**（写无后缀路径的文件在服务商界面
    /// 不可见）。
    #[must_use]
    pub fn path_list_new_format_nosuffix(&self) -> String {
        format!(
            "serv/opencode/{}/{}/{}/path-list",
            self.user_id, self.pctype, self.device_name
        )
    }

    /// Legacy-format path (no leading slash):
    /// `serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md`.
    #[must_use]
    pub fn path_list_legacy(&self) -> String {
        format!(
            "serv/opencode/{}/{}/{}/path-list.md",
            self.sb_user, self.pctype, self.pcname
        )
    }

    /// Full relative path (no leading slash) used for the path-list JSON
    /// store. Prefers the new layout when `user_id` is set; otherwise falls
    /// back to the legacy `.md` layout, e.g.
    /// `serv/opencode/alice/macos/alice-mbp/path-list.md`.
    #[must_use]
    pub fn path_list(&self) -> String {
        if self.user_id.is_empty() {
            self.path_list_legacy()
        } else {
            self.path_list_new_format()
        }
    }

    /// Convenience: same as [`path_list`] but with a leading slash.
    #[must_use]
    pub fn path_list_with_slash(&self) -> String {
        format!("/{}", self.path_list())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pctype_matches_compile_target() {
        let got = pctype();
        if cfg!(target_os = "macos") {
            assert_eq!(got, "macos");
        } else if cfg!(target_os = "windows") {
            assert_eq!(got, "windows");
        } else {
            assert_eq!(got, "unknown");
        }
    }

    #[test]
    fn pcname_is_nonempty() {
        let n = pcname();
        assert!(!n.is_empty(), "pcname() must not be empty in test env");
    }

    #[test]
    fn remote_paths_layout() {
        let rp = RemotePaths::new("alice");
        let p = rp.path_list();
        let segs: Vec<&str> = p.split('/').collect();
        assert_eq!(segs.len(), 6);
        assert_eq!(segs[0], "serv");
        assert_eq!(segs[1], "opencode");
        assert_eq!(segs[2], "alice");
        assert!(matches!(segs[3], "macos" | "windows"));
        assert!(!segs[4].is_empty());
        assert_eq!(segs[5], "path-list.md");
    }

    #[test]
    fn remote_paths_with_slash_keeps_leading() {
        let rp = RemotePaths::new("alice");
        let p = rp.path_list_with_slash();
        assert!(p.starts_with('/'));
        assert!(p.ends_with("path-list.md"));
        assert_eq!(p, format!("/{}", rp.path_list()));
    }

    #[test]
    fn new_format_path_includes_user_id_pctype_device() {
        let rp = RemotePaths::with_user_info("u-123", "my-dev-pc");
        assert_eq!(
            rp.path_list_new_format(),
            format!("serv/opencode/u-123/{}/my-dev-pc/path-list.md", pctype())
        );
    }

    #[test]
    fn nosuffix_variant_is_migration_only_layout() {
        let rp = RemotePaths::with_user_info("u-123", "my-dev-pc");
        let nosuffix = rp.path_list_new_format_nosuffix();
        assert_eq!(
            nosuffix,
            format!("serv/opencode/u-123/{}/my-dev-pc/path-list", pctype())
        );
        assert_eq!(
            format!("{nosuffix}.md"),
            rp.path_list_new_format(),
            "nosuffix + \".md\" 必须与新格式路径一致（迁移搬运的源/目标对齐）"
        );
    }

    #[test]
    fn legacy_path_still_uses_sb_user_pcname() {
        let rp = RemotePaths::new("alice");
        assert!(rp.path_list_with_slash().contains("/alice/"));
        assert!(rp.path_list_with_slash().contains("/path-list.md"));
        // 旧构造不携带 user_id → 始终走 legacy 布局。
        assert_eq!(rp.path_list(), rp.path_list_legacy());
    }

    #[test]
    fn path_list_prefers_new_format_when_user_id_set() {
        let rp = RemotePaths::with_user_info("u-1", "dev-a");
        assert_eq!(rp.path_list(), rp.path_list_new_format());
        assert!(rp.path_list_with_slash().starts_with('/'));
        assert!(rp.path_list().ends_with("/path-list.md"));
    }
}