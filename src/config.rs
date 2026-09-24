//! 路径解析 + 运行期端口配置 + rathole 客户端配置。
//!
//! ## 持久化结构（v3：env 文件仅账户登录）
//!
//! 本模块只负责「放在哪、监听哪个端口」：
//! - 统一 env 文件与 rathole 配置文件的路径解析（[`unified_env_path`] /
//!   [`rathole_config_path`] / [`rathole_settings_dir`]）；
//! - 运行期端口（[`RuntimePorts`]）——**不再本地持久化**，每次启动由
//!   `/api/user/info` 设备清单中当前绑定设备的 `port`（系统端口）与
//!   `oc-port`（opencode 端口）下发；拉取失败时回退
//!   [`DEFAULT_SYSTEM_PORT`] / [`DEFAULT_OPENCODE_PORT`]；
//! - rathole 客户端配置（[`RatholeConfig`]，**已标记 deprecated**，
//!   后续将被云服务下发的新方案替代/移除）。
//!
//! 各类数据的归属：
//!
//! | 数据                                   | 归属                                        | 来源                                                  |
//! |----------------------------------------|---------------------------------------------|-------------------------------------------------------|
//! | 账户（account_id / key / remote_path / device_name） | [`crate::account::AccountConfig`] | env 文件 `# --- account login ---` 区块（唯一持久化项） |
//! | 端口（系统 / opencode）                 | 本模块 [`RuntimePorts`]                     | `/api/user/info` 设备清单（内存，不落盘）              |
//! | HTTP Basic（用户名 / 密码）             | [`crate::auth::AuthConfig`]                 | `/api/user/info` 用户信息映射（内存，不落盘）          |
//! | 远程存储（SilverBullet）凭据            | [`crate::account::RemoteSbConfig`]          | `/api/user/info` 动态下发（内存，不落盘）              |
//! | rathole                                | [`RatholeConfig`]（deprecated）             | 随 bundle 的 `global.toml`（过渡期）                   |
//!
//! 启动时 [`crate::account::prune_env_file_to_account_only`] 会把 env 文件
//! 清理为仅账户登录区块 —— 端口 / auth / rathole / sb 的旧持久化行全部删除。

use std::path::{Path, PathBuf};

use crate::account::RemoteUserInfo;
use crate::error::AppError;

/// 统一的持久化 env 文件名（仅承载账户登录信息）。
pub const UNIFIED_ENV_FILE: &str = ".env";

/// 生成的 rathole 客户端配置文件（供 rathole 二进制直接使用）。
///
/// 此常量只描述 *bundle 内相对路径*;运行时实际写到哪里由
/// [`rathole_config_path`] 决定(exe 旁的 release bundle 优先,源码树 CWD 兜底)。
pub const RATHOLE_CONFIG_FILE: &str = "rathole/settings/global.toml";

/// rathole bundle 内 `settings/` 目录的相对路径。
const RATHOLE_SETTINGS_DIR: &str = "rathole/settings";

/// 解析出 rathole 配置**实际写入路径**(供设置面板热更新使用)。
///
/// 优先级:
/// 1. `RATHOLE_CONFIG` 环境变量(用户显式 override)
/// 2. `<exe_dir>/rathole/settings/global.toml` — `cargo build` 时
///    [`build.rs`](../../build.rs) 已把 settings/ 目录复制到这里。
///    写入这里能让 release 产物自包含,设置随产物走。
/// 3. cwd 下 `rathole/settings/global.toml`(dev / `cargo run` 工作流)
/// 4. cwd 直接的 `global.toml`(向后兼容)
#[must_use]
pub fn rathole_config_path() -> std::path::PathBuf {
    // 1. 用户显式 override
    if let Ok(v) = std::env::var("RATHOLE_CONFIG") {
        let p = std::path::PathBuf::from(v);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    // 2. exe 旁的 release bundle(主路径)
    if let Some(exe_dir) = exe_dir() {
        let candidate = exe_dir.join(RATHOLE_CONFIG_FILE);
        if candidate.exists() {
            return candidate;
        }
    }
    // 3. cwd 下源码树 layout(dev / cargo run)
    let cwd_candidate = std::path::PathBuf::from(RATHOLE_CONFIG_FILE);
    if cwd_candidate.exists() {
        return cwd_candidate;
    }
    // 4. cwd 直接的 global.toml(legacy)
    let flat = std::path::PathBuf::from("global.toml");
    if flat.exists() {
        return flat;
    }
    // 5. 都不存在:返回"exe 旁的"作为默认写入位置(确保设置能落盘到 release bundle)
    if let Some(exe_dir) = exe_dir() {
        return exe_dir.join(RATHOLE_CONFIG_FILE);
    }
    cwd_candidate
}

/// rathole bundle 内 `settings/` 目录的解析路径(用于"创建 settings 目录"等场景)。
///
/// 优先级同 [`rathole_config_path`]:exe 旁的 release bundle 优先,源码树 CWD 兜底。
/// 若都不存在则返回"exe 旁的 release bundle 路径",以便后续写入。
#[must_use]
pub fn rathole_settings_dir() -> std::path::PathBuf {
    if let Some(exe_dir) = exe_dir() {
        let candidate = exe_dir.join(RATHOLE_SETTINGS_DIR);
        if candidate.exists() {
            return candidate;
        }
    }
    let cwd_candidate = std::path::PathBuf::from(RATHOLE_SETTINGS_DIR);
    if cwd_candidate.exists() {
        return cwd_candidate;
    }
    if let Some(exe_dir) = exe_dir() {
        return exe_dir.join(RATHOLE_SETTINGS_DIR);
    }
    cwd_candidate
}

/// Parent directory of the running executable, or `None` if unavailable.
fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// 解析出 env 文件**最终写入的绝对路径**。
///
/// 优先级:
/// 1. `OC_SERVE_AUTH_ENV` 环境变量(用户显式 override)
/// 2. 可执行文件同目录(避免 `cd` 后 env 文件散落)
/// 3. cwd(兜底,旧行为兼容)
///
/// 注意:此函数只决定"在哪写";"读"由 dotenvy 与 rust std 共同处理,
/// 读仍然兼容旧位置(迁移逻辑在 main.rs 启动早期完成)。
pub fn unified_env_path() -> PathBuf {
    // 1. 用户显式 override
    if let Ok(p) = std::env::var("OC_SERVE_AUTH_ENV") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    // 2. 二进制同目录(`current_exe` 的 parent);避免 `cd` 后 env 文件散落
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join(UNIFIED_ENV_FILE);
        }
    }
    // 3. cwd 兜底
    PathBuf::from(UNIFIED_ENV_FILE)
}

/// 默认系统端口（axum path-list 管理接口）——设备清单不可用时的回退值。
pub const DEFAULT_SYSTEM_PORT: u16 = 9465;

/// 默认 opencode 服务端口——设备清单不可用 / 绑定设备缺 `oc-port` 时的回退值。
pub const DEFAULT_OPENCODE_PORT: u16 = 9464;

/// 运行期端口状态（从设备清单解析；`main.rs` 启动时创建，`TuiApp` 共享读写）。
///
/// - `system_port`：本程序 axum 监听端口，**启动时定死**——运行期云端下发
///   变化仅提示重启生效（监听套接字无法热迁移）；
/// - `opencode_port`：`opencode serve` 启动端口，**即时生效**——设置面板
///   保存 / 重新绑定设备后更新，后续启动 serve / 云服务用新值；
/// - `device_found`：当前绑定设备（`DEVICE_NAME`）是否在最近一次成功拉取
///   的设备清单中。`false`（未绑定 / 清单中不存在）期间**禁止启动
///   serve 与云服务**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimePorts {
    /// axum 系统监听端口。
    pub system_port: u16,
    /// opencode serve 端口。
    pub opencode_port: u16,
    /// 绑定设备是否已在设备清单中找到。
    pub device_found: bool,
}

impl Default for RuntimePorts {
    fn default() -> Self {
        Self {
            system_port: DEFAULT_SYSTEM_PORT,
            opencode_port: DEFAULT_OPENCODE_PORT,
            device_found: false,
        }
    }
}

/// 从 `/api/user/info` 的设备清单解析当前绑定设备的端口。
///
/// 规则（按用户确认的业务流程）：
/// - 取**当前绑定设备**（`device_name` 与清单中 `devices[].name` 精确匹配）
///   的 `port` → 系统端口、`oc-port` → opencode 端口；
/// - `device_name` 为空（未绑定）或清单中无匹配 → 回退默认端口，
///   `device_found = false`（调用方据此禁止启动 serve / 云服务）；
/// - 绑定设备缺 `oc-port`（老数据）或值为 0 → opencode 端口回退默认并
///   记警告；`port` 为 0 同理。
///
/// fetch 失败（断网等）场景由调用方直接用 [`RuntimePorts::default()` 回退，
/// 并以 `DEVICE_NAME` 非空作乐观的 `device_found` 判定（见 `main.rs`）。
#[must_use]
pub fn ports_from_user_info(info: &RemoteUserInfo, device_name: &str) -> RuntimePorts {
    let wanted = device_name.trim();
    if wanted.is_empty() {
        tracing::warn!("未绑定设备（DEVICE_NAME 为空）——端口回退默认值");
        return RuntimePorts::default();
    }
    let Some(dev) = info.devices.iter().find(|d| d.name == wanted) else {
        tracing::warn!(
            "设备清单中未找到绑定设备 {wanted:?} —— 端口回退默认值（请在设置中重新绑定设备）"
        );
        return RuntimePorts::default();
    };

    let system_port = if dev.port == 0 {
        tracing::warn!("设备 {wanted} 的 port 为 0 —— 系统端口回退默认 {DEFAULT_SYSTEM_PORT}");
        DEFAULT_SYSTEM_PORT
    } else {
        dev.port
    };
    let opencode_port = match dev.oc_port {
        Some(p) if p != 0 => p,
        _ => {
            tracing::warn!(
                "设备 {wanted} 缺少有效 oc-port —— opencode 端口回退默认 {DEFAULT_OPENCODE_PORT}"
            );
            DEFAULT_OPENCODE_PORT
        }
    };
    tracing::info!(
        "绑定设备 {wanted} 下发端口：系统={system_port} opencode={opencode_port}"
    );
    RuntimePorts {
        system_port,
        opencode_port,
        device_found: true,
    }
}

/// rathole 内网穿透的客户端配置（设置面板热更新）。
///
/// **已废弃**：rathole 相关配置后续会被云服务下发的新方案替代/移除，
/// 保留仅为过渡期兼容（设置面板 + 启动流程仍在使用），新代码不要新增依赖。
#[deprecated(
    note = "rathole 配置后续将被替代/移除;过渡期保留,新代码不要新增依赖"
)]
#[derive(Debug, Clone, Default)]
pub struct RatholeConfig {
    /// 远端服务器 host（对应 `remote_addr` 的主机部分）。
    pub host: String,
    /// 远端服务器端口（对应 `remote_addr` 的端口部分）。
    pub port: String,
    /// 服务名（对应 `client.services.<name>`）。
    pub name: String,
    /// 鉴权 token（对应 `token`）。
    pub token: String,
}

#[allow(deprecated)]
impl RatholeConfig {
    /// 从环境变量 + 统一 env 文件加载配置。
    ///
    /// 环境变量优先；缺失时回退 [`UNIFIED_ENV_FILE`] 文件（相对 cwd）。
    #[must_use]
    pub fn load() -> Self {
        let mut cfg = Self {
            host: std::env::var("RATHOLE_HOST").unwrap_or_default(),
            port: std::env::var("RATHOLE_PORT").unwrap_or_default(),
            name: std::env::var("RATHOLE_NAME").unwrap_or_default(),
            token: std::env::var("RATHOLE_TOKEN").unwrap_or_default(),
        };
        let path = Path::new(UNIFIED_ENV_FILE);
        if path.is_file()
            && (cfg.host.is_empty() || cfg.port.is_empty() || cfg.name.is_empty() || cfg.token.is_empty())
        {
            if let Ok(contents) = std::fs::read_to_string(path) {
                for line in contents.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = line.split_once('=') {
                        let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                        match k.trim() {
                            "RATHOLE_HOST" if cfg.host.is_empty() => cfg.host = v.to_string(),
                            "RATHOLE_PORT" if cfg.port.is_empty() => cfg.port = v.to_string(),
                            "RATHOLE_NAME" if cfg.name.is_empty() => cfg.name = v.to_string(),
                            "RATHOLE_TOKEN" if cfg.token.is_empty() => cfg.token = v.to_string(),
                            _ => {}
                        }
                    }
                }
            }
        }
        cfg
    }

    /// 四个字段都非空才算已配置。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.host.is_empty() && !self.port.is_empty() && !self.name.is_empty() && !self.token.is_empty()
    }

    /// 生成 rathole 客户端配置的 TOML 文本。
    ///
    /// `local_port` 是 rathole 要转发的本地服务端口（即 serve 的启动端口）。
    #[must_use]
    pub fn to_toml(&self, local_port: &str) -> String {
        format!(
            "# global.toml\n\
             [client]\n\
             remote_addr = \"{}:{}\"\n\
             [client.services.{}]\n\
             token = \"{}\"\n\
             local_addr = \"127.0.0.1:{}\"\n",
            self.host, self.port, self.name, self.token, local_port
        )
    }

    /// 将 rathole 客户端配置写入 `RATHOLE_CONFIG_FILE`。
    ///
    /// # Errors
    /// 返回 [`AppError::Io`] 当文件写入失败。
    pub fn write_config_file(&self, path: &Path, local_port: &str) -> Result<(), AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::Io)?;
        }
        std::fs::write(path, self.to_toml(local_port)).map_err(AppError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{RemoteDevice, RemoteSbConfig};

    fn mk_info(devices: Vec<RemoteDevice>) -> RemoteUserInfo {
        RemoteUserInfo {
            id: "u-1".to_string(),
            name: "alice".to_string(),
            key: "k".to_string(),
            cloud_ip: None,
            last_used_at: None,
            devices,
            sb: RemoteSbConfig {
                base_url: "https://md".to_string(),
                username: "u".to_string(),
                password: "p".to_string(),
            },
            created_at: None,
            updated_at: None,
        }
    }

    fn mk_dev(name: &str, port: u16, oc_port: Option<u16>) -> RemoteDevice {
        RemoteDevice {
            name: name.to_string(),
            port,
            oc_port,
            bound: false,
            pctype: "macos".to_string(),
            device_name: None,
            desc: None,
        }
    }

    #[test]
    fn ports_from_bound_device_port_and_oc_port() {
        let info = mk_info(vec![
            mk_dev("other-pc", 18000, Some(19000)),
            mk_dev("my-pc", 20001, Some(20002)),
        ]);
        let got = ports_from_user_info(&info, "my-pc");
        assert_eq!(
            got,
            RuntimePorts {
                system_port: 20001,
                opencode_port: 20002,
                device_found: true,
            }
        );
    }

    #[test]
    fn ports_fall_back_when_device_not_bound() {
        let info = mk_info(vec![mk_dev("my-pc", 20001, Some(20002))]);
        // DEVICE_NAME 为空（未绑定）。
        let got = ports_from_user_info(&info, "");
        assert_eq!(got, RuntimePorts::default());
        assert!(!got.device_found);
    }

    #[test]
    fn ports_fall_back_when_bound_device_missing_from_list() {
        let info = mk_info(vec![mk_dev("my-pc", 20001, Some(20002))]);
        let got = ports_from_user_info(&info, "ghost-pc");
        assert_eq!(got, RuntimePorts::default());
        assert!(!got.device_found);
    }

    #[test]
    fn ports_fall_back_when_oc_port_missing_or_zero() {
        let info = mk_info(vec![
            mk_dev("no-oc", 21001, None),
            mk_dev("zero-oc", 21002, Some(0)),
        ]);
        assert_eq!(
            ports_from_user_info(&info, "no-oc").opencode_port,
            DEFAULT_OPENCODE_PORT
        );
        assert_eq!(
            ports_from_user_info(&info, "zero-oc").opencode_port,
            DEFAULT_OPENCODE_PORT
        );
        // 系统端口仍取设备下发值。
        assert_eq!(ports_from_user_info(&info, "no-oc").system_port, 21001);
    }

    #[test]
    fn ports_fall_back_when_port_zero() {
        let info = mk_info(vec![mk_dev("zero-port", 0, Some(7777))]);
        let got = ports_from_user_info(&info, "zero-port");
        assert_eq!(got.system_port, DEFAULT_SYSTEM_PORT);
        assert_eq!(got.opencode_port, 7777);
        assert!(got.device_found);
    }

    /// 默认值锁：防止端口常量被意外改动（回退场景的稳定契约）。
    #[test]
    fn ports_config_defaults_lock_invariants() {
        assert_eq!(DEFAULT_SYSTEM_PORT, 9465);
        assert_eq!(DEFAULT_OPENCODE_PORT, 9464);
        assert_ne!(DEFAULT_SYSTEM_PORT, DEFAULT_OPENCODE_PORT);
        assert_eq!(
            RuntimePorts::default(),
            RuntimePorts {
                system_port: DEFAULT_SYSTEM_PORT,
                opencode_port: DEFAULT_OPENCODE_PORT,
                device_found: false,
            }
        );
    }
}
