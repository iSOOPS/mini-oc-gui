//! Top-level menu items, mirroring `oc-serve-start.sh::main_menu`.

/// A single selectable entry in the main TUI menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    /// OpenCode Serve（启动/停止动态切换）.
    OcServe,
    /// Rathole 隧道（启动/停止动态切换）.
    Rathole,
    /// OC 项目（进入项目选择 / attach 子页面）.
    OcProjects,
    /// Upgrade OpenCode + oh-my-openagent.
    UpgradeOpenCodeAndOmo,
    /// Exit.
    Exit,
}

impl MenuItem {
    /// Return all menu items in display order.
    #[must_use]
    pub fn all() -> Vec<MenuItem> {
        vec![
            Self::OcServe,
            Self::Rathole,
            Self::OcProjects,
            Self::UpgradeOpenCodeAndOmo,
            Self::Exit,
        ]
    }
}

/// What a selected [`MenuItem`] should translate to in terms of side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Toggle OpenCode Serve：运行中则停止，未运行则提示端口并启动。
    ToggleOcServe,
    /// Toggle Rathole：运行中则停止，未运行则启动。
    ToggleRathole,
    /// 进入 OC 项目子页面。
    EnterProjects,
    /// Run the upgrade flow.
    Upgrade,
    /// Exit the TUI cleanly.
    Exit,
}

impl From<MenuItem> for MenuAction {
    fn from(item: MenuItem) -> Self {
        match item {
            MenuItem::OcServe => Self::ToggleOcServe,
            MenuItem::Rathole => Self::ToggleRathole,
            MenuItem::OcProjects => Self::EnterProjects,
            MenuItem::UpgradeOpenCodeAndOmo => Self::Upgrade,
            MenuItem::Exit => Self::Exit,
        }
    }
}
