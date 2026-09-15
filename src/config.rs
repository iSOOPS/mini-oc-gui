//! 路径解析 + 端口配置 + rathole 客户端配置。
//!
//! ## 持久化结构（重构后）
//!
//! 本模块只负责「放在哪、监听哪个端口」：
//! - 统一 env 文件与 rathole 配置文件的路径解析（[`unified_env_path`] /
//!   [`rathole_config_path`] / [`rathole_settings_dir`]）；
//! - 系统端口 + opencode 服务端口（[`PortsConfig`]）；
//! - rathole 客户端配置（[`RatholeConfig`]，**已标记 deprecated**，
//!   后续将被云服务下发的新方案替代/移除）。
//!
//! 各类数据的持久化归属：
//!
//! | 数据                                   | 归属                                        | 持久化方式                                            |
//! |----------------------------------------|---------------------------------------------|-------------------------------------------------------|
//! | 账户（account_id / key / remote_path） | [`crate::account::AccountConfig`]           | `AccountConfig::write_env_file`（增量更新账户 key）    |
//! | HTTP Basic（用户名 / 密码 / Cookie 名） | env 文件对应 key（经由 `crate::account` 的 kv 工具增量更新） | 同一 env 文件                          |
//! | 端口                                   | 本模块 [`PortsConfig`]                      | env 文件 `OC_SERVE_*_PORT` key                        |
//! | 远程存储（SilverBullet）凭据            | [`crate::account::RemoteSbConfig`]（`/api/user/info` 动态下发） | 不再本地持久化（旧 `SB_*` key 仅作过渡读取） |
//! | rathole                                | [`RatholeConfig`]（deprecated）             | env 文件 `RATHOLE_*` key + `global.toml`              |
//!
//! 旧版的 `PersistedSettings` / `SbConfig` / `write_persisted_env` /
//! `read_persisted_env` / `migrate_legacy_env` 已删除：账户信息统一走
//! [`crate::account`]，其余 section 通过增量 upsert 写入统一 env 文件，
//! 不再整文件覆盖。

use std::path::{Path, PathBuf};

use crate::error::AppError;

/// 统一的持久化 env 文件名（账户 / auth + port + rathole 所有 key）。
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

/// 默认系统端口（axum path-list 管理接口）。
pub const DEFAULT_SYSTEM_PORT: u16 = 9465;

/// 默认 opencode 服务端口。
pub const DEFAULT_OPENCODE_PORT: u16 = 9464;

/// 端口配置（系统端口 + opencode 服务端口）。
///
/// 两个端口相互独立，避免同时监听同一端口。
#[derive(Debug, Clone, Copy)]
pub struct PortsConfig {
    /// axum 系统监听端口。
    pub system_port: u16,
    /// opencode 服务端口。
    pub opencode_port: u16,
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self {
            system_port: DEFAULT_SYSTEM_PORT,
            opencode_port: DEFAULT_OPENCODE_PORT,
        }
    }
}

impl PortsConfig {
    /// 从环境变量加载，缺失字段走各自默认值。
    ///
    /// 优先级：
    /// 1. 进程环境变量 `OC_SERVE_SYSTEM_PORT` / `OC_SERVE_OPENCODE_PORT`
    /// 2. 统一 env 文件（[`UNIFIED_ENV_FILE`]，路径由 [`unified_env_path`]
    ///    解析 —— 用户可在外部直接编辑 .env 后通过 TUI 设置面板看到新值，
    ///    不必重启进程）
    /// 3. 硬编码默认值
    ///
    /// 注意：`.env` 文件的回退也由 `main.rs` 在 auth 初始化前
    /// 通过 `dotenvy::from_filename_override` 注入到进程 env，
    /// 直接走第 1 优先级；本函数同时直接读文件，确保 TUI 设置面板
    /// 在不重启进程的情况下也能反映外部编辑的最新值。
    #[must_use]
    pub fn load() -> Self {
        let file_kv = load_env_file_kv();

        let system_port = resolve_port(
            keys::SYSTEM_PORT,
            DEFAULT_SYSTEM_PORT,
            &file_kv,
        );
        let opencode_port = resolve_port(
            keys::OPENCODE_PORT,
            DEFAULT_OPENCODE_PORT,
            &file_kv,
        );

        Self {
            system_port,
            opencode_port,
        }
    }
}

/// 在设置面板/启动时直接读取 .env 文件的 kv 对（不依赖进程 env 缓存）。
///
/// 复用 [`crate::account::read_env_kv`]：行级解析、空行/注释忽略、
/// 两侧引号 trim。文件不存在时返回空 Vec。
fn load_env_file_kv() -> Vec<(String, String)> {
    crate::account::read_env_kv(&unified_env_path())
}

/// 端口解析顺序：
/// 1. 进程环境变量（用户启动时 `OC_SERVE_SYSTEM_PORT=...` 等最高优先）
/// 2. env 文件（`.env`，TUI 设置面板直接读，避免进程 env 缓存掩盖外部编辑）
/// 3. 硬编码默认
///
/// 三处都解析失败时返回默认值（启动时进程 env 应已由 dotenvy 注入，
/// 文件路径由 `unified_env_path()` 解析；两者都拿不到就回退默认）。
fn resolve_port(env_key: &str, default: u16, file_kv: &[(String, String)]) -> u16 {
    if let Ok(v) = std::env::var(env_key) {
        if let Ok(p) = v.parse() {
            return p;
        }
    }
    for (k, v) in file_kv {
        if k == env_key {
            if let Ok(p) = v.parse() {
                return p;
            }
        }
    }
    default
}

/// env 文件中端口相关 key 名常量。
///
/// 账户 / auth / rathole 等 key 已迁至各自归属模块
/// （[`crate::account::keys`] 等），此处只保留端口。
pub mod keys {
    /// 系统监听端口。
    pub const SYSTEM_PORT: &str = "OC_SERVE_SYSTEM_PORT";
    /// opencode 服务端口。
    pub const OPENCODE_PORT: &str = "OC_SERVE_OPENCODE_PORT";
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

    /// 端口解析的纯函数测试 —— 不依赖进程 env、不写文件、不污染测试间状态。
    #[test]
    fn resolve_port_falls_through_to_default() {
        // env_key 不在 file_kv 里,也没有进程 env → 默认。
        let got = resolve_port("NONEXISTENT_PORT", 1234, &[]);
        assert_eq!(got, 1234);
    }

    #[test]
    fn resolve_port_picks_value_from_file_kv() {
        // 即使 env_key 未在进程 env 中设置,只要 file_kv 里有就生效。
        let kv = vec![(keys::OPENCODE_PORT.to_string(), "18800".to_string())];
        // 注:此测试可能在被测进程已设 OC_SERVE_OPENCODE_PORT 时失败 —— 若
        // CI 上未注入该 env 变量,下面才是默认;反之会因进程 env 优先级
        // 更高而跳过此处断言。单独跑 cargo test 时通常不会设置。
        if std::env::var(keys::OPENCODE_PORT).is_err() {
            let got = resolve_port(keys::OPENCODE_PORT, DEFAULT_OPENCODE_PORT, &kv);
            assert_eq!(got, 18800);
        }
    }

    #[test]
    fn resolve_port_ignores_invalid_value_and_falls_back() {
        // 解析失败时返回默认值,而不是 0。
        let kv = vec![(keys::SYSTEM_PORT.to_string(), "not-a-number".to_string())];
        if std::env::var(keys::SYSTEM_PORT).is_err() {
            let got = resolve_port(keys::SYSTEM_PORT, DEFAULT_SYSTEM_PORT, &kv);
            assert_eq!(got, DEFAULT_SYSTEM_PORT);
        }
    }

    /// PortsConfig 默认值锁:防止端口常量被意外改动 —— 系统端口硬锁定 9465,
    /// opencode 端口 9464,二者必须不同(否则 `submit_settings` 拒绝)。
    #[test]
    fn ports_config_defaults_lock_invariants() {
        assert_eq!(DEFAULT_SYSTEM_PORT, 9465);
        assert_eq!(DEFAULT_OPENCODE_PORT, 9464);
        assert_ne!(DEFAULT_SYSTEM_PORT, DEFAULT_OPENCODE_PORT);
    }

    /// 端口 key 名称稳定性 —— 这些 env key 已被 dotenvy 与外部脚本依赖,
    /// 改名会破坏现有用户的 .env 文件。
    #[test]
    fn port_keys_are_stable() {
        assert_eq!(keys::SYSTEM_PORT, "OC_SERVE_SYSTEM_PORT");
        assert_eq!(keys::OPENCODE_PORT, "OC_SERVE_OPENCODE_PORT");
    }
}
