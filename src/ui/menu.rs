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
        ]
    }

    /// 该菜单项是否在 TUI 窗口空间不足时必须保留。
    ///
    /// - 核心服务开关(`OcServe` / `Rathole`)是用户的日常主操作,
    ///   任何终端尺寸下都必须可见。
    /// - 次要项(`OcProjects` / `UpgradeOpenCodeAndOmo`)在窗口
    ///   高度不足以容纳时,会被自适应裁剪掉。
    #[must_use]
    pub fn is_essential(self) -> bool {
        matches!(self, Self::OcServe | Self::Rathole)
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
}

impl From<MenuItem> for MenuAction {
    fn from(item: MenuItem) -> Self {
        match item {
            MenuItem::OcServe => Self::ToggleOcServe,
            MenuItem::Rathole => Self::ToggleRathole,
            MenuItem::OcProjects => Self::EnterProjects,
            MenuItem::UpgradeOpenCodeAndOmo => Self::Upgrade,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn essential_flags_are_stable() {
        assert!(MenuItem::OcServe.is_essential(), "OcServe 必须 essential");
        assert!(MenuItem::Rathole.is_essential(), "Rathole 必须 essential");
        assert!(
            !MenuItem::UpgradeOpenCodeAndOmo.is_essential(),
            "Upgrade 是次要项,窗口太小时应被裁掉"
        );
    }
}
