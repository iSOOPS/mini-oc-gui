//! Top-level menu items, mirroring `oc-serve-start.sh::main_menu`.

/// A single selectable entry in the main TUI menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    /// 🚀 Launch `opencode serve` only.
    LaunchOcServe,
    /// 🚀 Launch `opencode serve` + rathole tunnel.
    LaunchOcServeWithRathole,
    /// ⬆️ Upgrade OpenCode + oh-my-openagent.
    UpgradeOpenCodeAndOmo,
    /// 🚪 Exit.
    Exit,
}

impl MenuItem {
    /// Human-readable label with emoji.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::LaunchOcServe => "🚀 启动 OC Serve（默认）",
            Self::LaunchOcServeWithRathole => "🚀 启动 OC Serve + Rathole（全部）",
            Self::UpgradeOpenCodeAndOmo => "⬆️  升级 OpenCode + omo",
            Self::Exit => "🚪 退出",
        }
    }

    /// Return all menu items in display order.
    #[must_use]
    pub fn all() -> Vec<MenuItem> {
        vec![
            Self::LaunchOcServe,
            Self::LaunchOcServeWithRathole,
            Self::UpgradeOpenCodeAndOmo,
            Self::Exit,
        ]
    }
}

/// What a selected [`MenuItem`] should translate to in terms of side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Launch `opencode serve`. The bool is `with_rathole`.
    Launch(bool),
    /// Run the upgrade flow.
    Upgrade,
    /// Exit the TUI cleanly.
    Exit,
}

impl From<MenuItem> for MenuAction {
    fn from(item: MenuItem) -> Self {
        match item {
            MenuItem::LaunchOcServe => Self::Launch(false),
            MenuItem::LaunchOcServeWithRathole => Self::Launch(true),
            MenuItem::UpgradeOpenCodeAndOmo => Self::Upgrade,
            MenuItem::Exit => Self::Exit,
        }
    }
}
