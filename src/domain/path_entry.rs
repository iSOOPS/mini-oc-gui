//! `PathEntry` — a single record in `path-list.md` — and `PathValidator`,
//! which enforces the same rejection rules as the original shell scripts
//! (`oc-serve-tui-actuator.sh::validate_local_path` and
//! `path-list-actor.py::validate_path`).

use chrono::{DateTime, FixedOffset};
use path_clean::PathClean;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::error::AppError;

/// A single indexed path entry.
///
/// Serializes to JSON using camelCase field names to match the SilverBullet
/// remote format consumed by `lib-path-list.sh`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathEntry {
    /// The normalized absolute path.
    pub path: String,

    /// Session ids (or `seq_<hex>` placeholders) attached to this path.
    #[serde(default)]
    pub sections: Vec<String>,

    /// ISO-8601 creation timestamp with timezone offset (immutable).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "createdAt"
    )]
    pub created_at: Option<DateTime<FixedOffset>>,

    /// ISO-8601 last-opened timestamp with timezone offset (mutable).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "lastOpenedAt"
    )]
    pub last_opened_at: Option<DateTime<FixedOffset>>,
}

impl PathEntry {
    /// Construct a new entry with the given path and empty sections.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            sections: Vec::new(),
            created_at: None,
            last_opened_at: None,
        }
    }
}

/// Validates and normalizes user-supplied local paths.
///
/// The rejection rules are intentionally identical to the original shell
/// `validate_local_path` / Python `validate_path` implementations, so
/// the Rust UI accepts exactly the same inputs as the legacy scripts.
#[derive(Debug)]
pub struct PathValidator;

impl PathValidator {
    /// Validate `input` and return its normalized absolute form.
    ///
    /// # Errors
    /// Returns [`AppError::PathValidation`] if the path fails any rejection
    /// rule. Bare `~` is rejected explicitly; `~/<subpath>` is expanded to
    /// the user's home directory. Paths starting with `/`, `./`, or `../`
    /// are resolved against the current working directory and lexically
    /// cleaned via [`path_clean`].
    pub fn validate(input: &str) -> Result<String, AppError> {
        // 1. Protocol scheme (e.g. http://, file://) — never a local path.
        if input.contains("://") {
            return Err(AppError::PathValidation(
                "path contains protocol scheme \"://\"".to_string(),
            ));
        }

        // 2. Windows-style backslash — force POSIX.
        if input.contains('\\') {
            return Err(AppError::PathValidation(
                "path contains backslash; use POSIX forward slashes".to_string(),
            ));
        }

        // 3. Shell metacharacters that could enable injection.
        const SHELL_META: &str = "$`;&|<>(){}'\"";
        if input.chars().any(|c| SHELL_META.contains(c)) {
            return Err(AppError::PathValidation(
                "path contains shell metacharacters".to_string(),
            ));
        }

        // 4. Control characters (NUL, newline, etc.).
        if input.chars().any(|c| c.is_control()) {
            return Err(AppError::PathValidation(
                "path contains control characters".to_string(),
            ));
        }

        // 5. Bare `~` (without a subpath) — explicitly rejected to prevent
        //    `path-list.md` from recording `$HOME` itself, which would break
        //    the menu's home-folding logic.
        if input == "~" {
            return Err(AppError::PathValidation(
                "bare \"~\" refers to $HOME itself; use ~/Projects/MyApp or leave empty for default".to_string(),
            ));
        }

        // 6. Prefix check — must start with /, ./, ../, or ~/.
        let valid_prefix = input.starts_with('/')
            || input.starts_with("./")
            || input.starts_with("../")
            || input.starts_with("~/");
        if !valid_prefix {
            return Err(AppError::PathValidation(
                "path must start with /, ./, ../ or ~/".to_string(),
            ));
        }

        // 7. Expand to a clean absolute path.
        let raw = if let Some(rest) = input.strip_prefix("~/") {
            let home = home_dir().ok_or_else(|| {
                AppError::PathValidation(
                    "cannot determine home directory for ~ expansion".to_string(),
                )
            })?;
            home.join(rest)
        } else if input.starts_with('/') {
            PathBuf::from(input)
        } else {
            // ./ or ../
            let cwd = std::env::current_dir().map_err(|e| {
                AppError::PathValidation(format!("cannot determine current working directory: {e}"))
            })?;
            cwd.join(input)
        };

        let cleaned = raw.clean();
        // On Windows, path-clean rewrites forward slashes to backslashes.
        // Our validation contract is POSIX-style (we explicitly reject
        // backslashes in input), so we force forward slashes on the output.
        let result = cleaned
            .to_string_lossy()
            .into_owned()
            .replace('\\', "/");
        Ok(result)
    }
}

fn home_dir() -> Option<PathBuf> {
    // Prefer $HOME (matches the original bash script `${HOME:-/}`); fall back
    // to `dirs::home_dir()` for cross-platform sanity.
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_protocol_scheme() {
        assert!(matches!(
            PathValidator::validate("http://example.com"),
            Err(AppError::PathValidation(_))
        ));
        assert!(matches!(
            PathValidator::validate("file:///etc/passwd"),
            Err(AppError::PathValidation(_))
        ));
    }

    #[test]
    fn rejects_backslash() {
        assert!(matches!(
            PathValidator::validate(r"C:\Users\samuel"),
            Err(AppError::PathValidation(_))
        ));
    }

    #[test]
    fn rejects_shell_metacharacters() {
        for bad in [
            "$foo", "`foo`", ";foo", "&foo", "|foo", "<foo", ">foo",
            "(foo)", "{foo}", "'foo", "\"foo",
        ] {
            assert!(
                PathValidator::validate(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        assert!(matches!(
            PathValidator::validate("/foo\nbar"),
            Err(AppError::PathValidation(_))
        ));
        assert!(matches!(
            PathValidator::validate("/foo\tbar"),
            Err(AppError::PathValidation(_))
        ));
    }

    #[test]
    fn rejects_bare_tilde() {
        assert!(matches!(
            PathValidator::validate("~"),
            Err(AppError::PathValidation(_))
        ));
    }

    #[test]
    fn rejects_invalid_prefix() {
        assert!(matches!(
            PathValidator::validate("foo/bar"),
            Err(AppError::PathValidation(_))
        ));
        assert!(matches!(
            PathValidator::validate(""),
            Err(AppError::PathValidation(_))
        ));
    }

    #[test]
    fn accepts_absolute_path() {
        let got = PathValidator::validate("/home/user/project").expect("valid");
        assert_eq!(got, "/home/user/project");
    }

    #[test]
    fn accepts_cleaned_absolute_path() {
        let got = PathValidator::validate("/a//b/../c/./d").expect("valid");
        assert_eq!(got, "/a/c/d");
    }

    #[test]
    fn accepts_dot_relative() {
        let got = PathValidator::validate("./foo").expect("valid");
        assert!(std::path::Path::new(&got).is_absolute(), "expected absolute, got {got}");
        assert!(got.ends_with("foo"), "expected to end with 'foo', got {got}");
    }

    #[test]
    fn accepts_tilde_expansion() {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).or_else(dirs::home_dir) {
            let got = PathValidator::validate("~/projects/demo").expect("valid");
            let expected = home.join("projects").join("demo").clean();
            assert_eq!(PathBuf::from(&got), expected);
        }
    }
}
