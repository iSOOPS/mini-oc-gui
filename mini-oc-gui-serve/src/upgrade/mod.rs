//! Upgrade orchestrator — `opencode upgrade` + `oh-my-openagent` update.

pub mod omo;
pub mod opencode;

pub use omo::{detect_bun, detect_npm, upgrade_omo};
pub use opencode::upgrade_opencode;

/// Outcome of an upgrade step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeResult {
    /// A version bump was detected after running the upgrade.
    Upgraded,
    /// No version change after the upgrade attempt.
    AlreadyLatest,
    /// The upgrade step failed with a human-readable message.
    Failed(String),
}

impl UpgradeResult {
    /// `true` if the upgrade either succeeded or confirmed we were already
    /// at the latest version.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Upgraded | Self::AlreadyLatest)
    }
}
