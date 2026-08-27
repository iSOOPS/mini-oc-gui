//! Remote path builder for the per-(user, OS, machine) MD store layout.

use whoami::username;

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
/// Holds the SilverBullet username (a path segment) and lazily derives
/// `pctype` and `pcname` from the runtime environment at construction time.
#[derive(Debug, Clone)]
pub struct RemotePaths {
    sb_user: String,
    pctype: &'static str,
    pcname: String,
}

impl RemotePaths {
    /// Build a path plan rooted at `sb_user`.
    #[must_use]
    pub fn new(sb_user: impl Into<String>) -> Self {
        Self {
            sb_user: sb_user.into(),
            pctype: pctype(),
            pcname: pcname(),
        }
    }

    /// Full relative path (no leading slash) used for the path-list
    /// JSON store, e.g. `serv/opencode/alice/macos/alice-mbp/path-list.md`.
    #[must_use]
    pub fn path_list(&self) -> String {
        format!(
            "serv/opencode/{}/{}/{}/path-list.md",
            self.sb_user, self.pctype, self.pcname
        )
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
}