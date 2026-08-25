//! 远程 SilverBullet 配置 + rathole 内网穿透配置 + 端口配置。

use std::path::{Path, PathBuf};

use crate::error::AppError;

/// 统一的持久化 env 文件名（合并 auth + port + sb + rathole 所有 key）。
pub const UNIFIED_ENV_FILE: &str = ".oc-serve-auth.env";

/// 默认的 SB 配置文件名（向后兼容别名；新代码统一使用 [`UNIFIED_ENV_FILE`]）。
pub const SB_ENV_FILE: &str = ".oc-serve-auth.env";

/// 默认的 Rathole 配置持久化文件名（向后兼容别名；新代码统一使用 [`UNIFIED_ENV_FILE`]）。
pub const RATHOLE_ENV_FILE: &str = ".oc-serve-auth.env";

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
/// 注意:此函数只决定"在哪写";"读"则由 dotenvy 与 rust std 共同处理,
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
    /// 2. 硬编码默认值
    ///
    /// 注意：`.oc-serve-auth.env` 文件的回退由 `main.rs` 在 auth 初始化前
    /// 通过 `dotenvy::from_filename_override` 统一注入到进程 env，
    /// 所以此处只需读 `std::env::var` 即可。
    #[must_use]
    pub fn load() -> Self {
        Self {
            system_port: std::env::var("OC_SERVE_SYSTEM_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SYSTEM_PORT),
            opencode_port: std::env::var("OC_SERVE_OPENCODE_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_OPENCODE_PORT),
        }
    }
}

/// 持久化的所有设置项（统一写入 [`UNIFIED_ENV_FILE`]）。
#[derive(Debug, Clone, Default)]
pub struct PersistedSettings {
    /// HTTP Basic 用户名（`OPENCODE_SERVER_USERNAME`）。
    pub username: String,
    /// HTTP Basic 密码（`OPENCODE_SERVER_PASSWORD`）。
    pub password: String,
    /// Cookie 名（`SB_COOKIE_NAME`，可选）。
    pub sb_cookie_name: Option<String>,
    /// 系统监听端口（`OC_SERVE_SYSTEM_PORT`，字符串以保留原始输入格式）。
    pub system_port: String,
    /// opencode 服务端口（`OC_SERVE_OPENCODE_PORT`，字符串以保留原始输入格式）。
    pub opencode_port: String,
    /// 远程 SilverBullet 配置。
    pub sb: SbConfig,
    /// rathole 客户端配置。
    pub rathole: RatholeConfig,
}

/// env 文件 key 名常量（合并文件中所有 section 共享）。
pub mod keys {
    pub const USERNAME: &str = "OPENCODE_SERVER_USERNAME";
    pub const PASSWORD: &str = "OPENCODE_SERVER_PASSWORD";
    pub const SB_COOKIE_NAME: &str = "SB_COOKIE_NAME";
    pub const SYSTEM_PORT: &str = "OC_SERVE_SYSTEM_PORT";
    pub const OPENCODE_PORT: &str = "OC_SERVE_OPENCODE_PORT";
    pub const SB_URL: &str = "SB_URL";
    pub const SB_USER: &str = "SB_USER";
    pub const SB_PASSWORD: &str = "SB_PASSWORD";
    pub const RATHOLE_HOST: &str = "RATHOLE_HOST";
    pub const RATHOLE_PORT: &str = "RATHOLE_PORT";
    pub const RATHOLE_NAME: &str = "RATHOLE_NAME";
    pub const RATHOLE_TOKEN: &str = "RATHOLE_TOKEN";
}

/// 读取 env 文件并解析为 (key, value) 对（忽略空行与注释）。
fn read_env_kv(path: &Path) -> Vec<(String, String)> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (k, v) = trimmed.split_once('=')?;
            Some((k.trim().to_string(), v.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))
        })
        .collect()
}

/// 把持久化设置整体写入 env 文件（按统一格式 + chmod 600）。
///
/// 总是整文件覆盖：调用方负责把"未改动的 key"也带上，避免丢失其他 section 的配置。
pub fn write_persisted_env(path: &Path, settings: &PersistedSettings) -> Result<(), AppError> {
    let mut body = String::from(
        "# Generated by `mini-oc-gui-serve`\n\
         # All settings (auth / ports / sb / rathole) live in this single file.\n\
         # Edit manually or via the TUI Settings panel (press `s`).\n",
    );
    body.push_str(&format!(
        "{}={}\n{}={}\n",
        keys::USERNAME, settings.username,
        keys::PASSWORD, settings.password,
    ));
    if let Some(cookie) = &settings.sb_cookie_name {
        body.push_str(&format!("{}={cookie}\n", keys::SB_COOKIE_NAME));
    }
    body.push_str(&format!(
        "{}={}\n{}={}\n",
        keys::SYSTEM_PORT, settings.system_port,
        keys::OPENCODE_PORT, settings.opencode_port,
    ));
    if settings.sb.is_configured() {
        body.push_str(&format!(
            "{}={}\n{}={}\n{}={}\n",
            keys::SB_URL, settings.sb.url,
            keys::SB_USER, settings.sb.user,
            keys::SB_PASSWORD, settings.sb.password,
        ));
    }
    if settings.rathole.is_configured() {
        body.push_str(&format!(
            "{}={}\n{}={}\n{}={}\n{}={}\n",
            keys::RATHOLE_HOST, settings.rathole.host,
            keys::RATHOLE_PORT, settings.rathole.port,
            keys::RATHOLE_NAME, settings.rathole.name,
            keys::RATHOLE_TOKEN, settings.rathole.token,
        ));
    }
    std::fs::write(path, body).map_err(AppError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

/// 从 env 文件读出持久化设置（缺失字段填空，不报错）。
#[must_use]
pub fn read_persisted_env(path: &Path) -> PersistedSettings {
    let mut settings = PersistedSettings::default();
    for (k, v) in read_env_kv(path) {
        match k.as_str() {
            keys::USERNAME => settings.username = v,
            keys::PASSWORD => settings.password = v,
            keys::SB_COOKIE_NAME => settings.sb_cookie_name = Some(v),
            keys::SYSTEM_PORT => settings.system_port = v,
            keys::OPENCODE_PORT => settings.opencode_port = v,
            keys::SB_URL => settings.sb.url = v,
            keys::SB_USER => settings.sb.user = v,
            keys::SB_PASSWORD => settings.sb.password = v,
            keys::RATHOLE_HOST => settings.rathole.host = v,
            keys::RATHOLE_PORT => settings.rathole.port = v,
            keys::RATHOLE_NAME => settings.rathole.name = v,
            keys::RATHOLE_TOKEN => settings.rathole.token = v,
            _ => {}
        }
    }
    settings
}

/// 远程 SilverBullet 配置（可热更新）。
#[derive(Debug, Clone, Default)]
pub struct SbConfig {
    pub url: String,
    pub user: String,
    pub password: String,
}

impl SbConfig {
    /// 从环境变量 + `SB_ENV_FILE` 文件加载配置。
    ///
    /// 环境变量优先；缺失时回退到 `SB_ENV_FILE` 文件。
    #[must_use]
    pub fn load() -> Self {
        let mut cfg = Self {
            url: std::env::var("SB_URL").unwrap_or_default(),
            user: std::env::var("SB_USER").unwrap_or_default(),
            password: std::env::var("SB_PASSWORD").unwrap_or_default(),
        };
        let path = Path::new(SB_ENV_FILE);
        if path.is_file()
            && (cfg.url.is_empty() || cfg.user.is_empty() || cfg.password.is_empty())
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
                            "SB_URL" if cfg.url.is_empty() => cfg.url = v.to_string(),
                            "SB_USER" if cfg.user.is_empty() => cfg.user = v.to_string(),
                            "SB_PASSWORD" if cfg.password.is_empty() => {
                                cfg.password = v.to_string()
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        cfg
    }

    /// 三个字段都非空才算已配置。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.url.is_empty() && !self.user.is_empty() && !self.password.is_empty()
    }

    /// 将配置覆盖写入 `SB_ENV_FILE`（Unix 下 chmod 600）。
    ///
    /// **已废弃**:设置面板现在通过 [`crate::config::write_persisted_env`]
    /// 一次性写入所有 key(避免不同 section 互相覆盖)。保留此函数
    /// 仅为外部测试 / 旧调用方兼容,**新代码不要使用**。
    ///
    /// # Errors
    /// 返回 [`AppError::Io`] 当文件写入失败。
    #[deprecated(
        note = "设置面板改用 write_persisted_env 统一写入；此函数只写 SB section 会抹掉其他配置"
    )]
    pub fn write_env_file(&self, path: &Path) -> Result<(), AppError> {
        let body = format!(
            "# Generated by `mini-oc-gui-serve` settings\n\
             SB_URL={}\n\
             SB_USER={}\n\
             SB_PASSWORD={}\n",
            self.url, self.user, self.password
        );
        std::fs::write(path, body).map_err(AppError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
    }
}

/// rathole 内网穿透的客户端配置（设置面板热更新）。
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

impl RatholeConfig {
    /// 从环境变量 + `RATHOLE_ENV_FILE` 文件加载配置。
    ///
    /// 环境变量优先；缺失时回退到 `RATHOLE_ENV_FILE` 文件。
    #[must_use]
    pub fn load() -> Self {
        let mut cfg = Self {
            host: std::env::var("RATHOLE_HOST").unwrap_or_default(),
            port: std::env::var("RATHOLE_PORT").unwrap_or_default(),
            name: std::env::var("RATHOLE_NAME").unwrap_or_default(),
            token: std::env::var("RATHOLE_TOKEN").unwrap_or_default(),
        };
        let path = Path::new(RATHOLE_ENV_FILE);
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

    /// 将配置覆盖写入 `RATHOLE_ENV_FILE`（Unix 下 chmod 600）。
    ///
    /// **已废弃**:设置面板现在通过 [`crate::config::write_persisted_env`]
    /// 一次性写入所有 key。保留此函数仅为向后兼容。
    ///
    /// # Errors
    /// 返回 [`AppError::Io`] 当文件写入失败。
    #[deprecated(
        note = "设置面板改用 write_persisted_env 统一写入；此函数只写 rathole section 会抹掉其他配置"
    )]
    pub fn write_env_file(&self, path: &Path) -> Result<(), AppError> {
        let body = format!(
            "# Generated by `mini-oc-gui-serve` settings\n\
             RATHOLE_HOST={}\n\
             RATHOLE_PORT={}\n\
             RATHOLE_NAME={}\n\
             RATHOLE_TOKEN={}\n",
            self.host, self.port, self.name, self.token
        );
        std::fs::write(path, body).map_err(AppError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
        Ok(())
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

/// 旧 SB / Rathole env 文件路径(向后兼容检查用)。
const LEGACY_SB_ENV_FILE: &str = ".oc-serve-sb.env";
const LEGACY_RATHOLE_ENV_FILE: &str = ".oc-serve-rathole.env";

/// 把旧式独立 env 文件中的 SB / Rathole 配置合并进 `target` 指定的统一 env,
/// 然后删除旧文件。
///
/// 一次性迁移:`main.rs` 启动早期调用一次,完成后下次启动 `Ok(false)` 不再处理。
///
/// 行为细节:
/// - 读 target 文件得到当前 persisted state(可能已有 USERNAME/PASSWORD 等)
/// - 若 `.oc-serve-sb.env` 存在,读取 SB_URL/SB_USER/SB_PASSWORD 合并
/// - 若 `.oc-serve-rathole.env` 存在,读取 RATHOLE_* 合并
/// - 写回 target(整文件覆盖)
/// - 删除两个旧文件
///
/// # Errors
/// 返回 [`AppError::Io`] 当文件读写失败。
pub fn migrate_legacy_env(target: &Path) -> Result<bool, AppError> {
    let sb_path = Path::new(LEGACY_SB_ENV_FILE);
    let rh_path = Path::new(LEGACY_RATHOLE_ENV_FILE);
    if !sb_path.is_file() && !rh_path.is_file() {
        return Ok(false);
    }
    let mut persisted = read_persisted_env(target);
    let mut merged = false;

    if sb_path.is_file() {
        if let Ok(contents) = std::fs::read_to_string(sb_path) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    match k {
                        "SB_URL" if persisted.sb.url.is_empty() => {
                            persisted.sb.url = v.to_string();
                            merged = true;
                        }
                        "SB_USER" if persisted.sb.user.is_empty() => {
                            persisted.sb.user = v.to_string();
                            merged = true;
                        }
                        "SB_PASSWORD" if persisted.sb.password.is_empty() => {
                            persisted.sb.password = v.to_string();
                            merged = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        let _ = std::fs::remove_file(sb_path);
    }

    if rh_path.is_file() {
        if let Ok(contents) = std::fs::read_to_string(rh_path) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    let k = k.trim();
                    let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                    match k {
                        "RATHOLE_HOST" if persisted.rathole.host.is_empty() => {
                            persisted.rathole.host = v.to_string();
                            merged = true;
                        }
                        "RATHOLE_PORT" if persisted.rathole.port.is_empty() => {
                            persisted.rathole.port = v.to_string();
                            merged = true;
                        }
                        "RATHOLE_NAME" if persisted.rathole.name.is_empty() => {
                            persisted.rathole.name = v.to_string();
                            merged = true;
                        }
                        "RATHOLE_TOKEN" if persisted.rathole.token.is_empty() => {
                            persisted.rathole.token = v.to_string();
                            merged = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        let _ = std::fs::remove_file(rh_path);
    }

    if merged {
        write_persisted_env(target, &persisted)?;
    }
    Ok(merged)
}
