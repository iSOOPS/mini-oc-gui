//! 账户登录配置 - 远程用户信息同步
//!
//! 账户登录包括四个字段：
//! - account_id: 账户唯一ID（用户从云平台获取）
//! - account_key: 账户密钥（用于调用 /api/user/info 验证身份）
//! - remote_path: 远程API基础URL，默认 oc.isoops.com
//!   不带 scheme 时自动补 `https://`，也支持显式 `http://` / `https://`，
//!   支持域名或 IP:port
//! - device_name: 已绑定的设备服务名（首次绑定后写入，缺失时需重新触发设备选择）

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::config;
use crate::error::AppError;

/// 默认远程 API 地址（不带 scheme，调用时会自动补 `https://`）。
pub const DEFAULT_REMOTE_PATH: &str = "oc.isoops.com";

/// env key 名常量（与 `.env` 文件共享）。
pub mod keys {
    /// 账户唯一ID。
    pub const ACCOUNT_ID: &str = "ACCOUNT_ID";
    /// 账户密钥。
    pub const ACCOUNT_KEY: &str = "ACCOUNT_KEY";
    /// 远程API地址。
    pub const REMOTE_PATH: &str = "REMOTE_PATH";
    /// 已绑定的设备服务名。
    pub const DEVICE_NAME: &str = "DEVICE_NAME";
}

/// 账户登录配置（持久化到 `.env`）
#[derive(Debug, Clone, Default)]
pub struct AccountConfig {
    /// 账户唯一ID (env: ACCOUNT_ID)
    pub account_id: String,
    /// 账户密钥   (env: ACCOUNT_KEY)
    pub account_key: String,
    /// 远程API地址 (env: REMOTE_PATH)
    pub remote_path: String,
    /// 已绑定的设备服务名 (env: DEVICE_NAME)
    pub device_name: String,
}

/// 从 /api/user/info 返回的用户信息
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RemoteUserInfo {
    pub id: String,
    pub name: String,
    pub key: String,
    #[serde(default)]
    pub cloud_ip: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub devices: Vec<RemoteDevice>,
    pub sb: RemoteSbConfig,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RemoteDevice {
    /// 设备服务名（^[a-z0-9-]{1,64}$），用于绑定接口
    pub name: String,
    /// 服务端口（1-65535）
    pub port: u16,
    /// 是否已绑定（用户信息中含此字段）
    #[serde(default)]
    pub bound: bool,
}

/// 用户信息中的 silverbullet 配置
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RemoteSbConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
}

impl RemoteSbConfig {
    /// 三个字段都非空才算已配置。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.username.is_empty() && !self.password.is_empty()
    }
}

impl RemoteUserInfo {
    /// 展示名：`name` 去除首尾空白；为空时由调用方自行回退（如 account_id）。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name.trim().to_string()
    }
}

/// 设备选择触发信号 —— 写入共享槽，TUI 主循环每帧 render 轮询消费后
/// 弹出设备选择弹框。
///
/// 两种来源：
/// - main.rs 启动后台任务：检测到本地未绑定设备且远端清单非空时写入
///   （`force=false`，消费端仅在未绑定时弹窗）；
/// - 用户在设置面板点击「绑定设备 / 重新绑定」行：强制重新拉取后写入
///   （`force=true`，消费端**跳过已绑定检查**，无论本地是否已绑定都弹窗）。
#[derive(Debug, Clone)]
pub struct DevicePickerTrigger {
    /// 最新一次 /api/user/info 返回的完整用户信息（含 devices 清单）。
    pub user_info: RemoteUserInfo,
    /// 账户密钥（绑定接口 `/api/device-bind` 需要）。
    pub account_key: String,
    /// 远程 API 基础 URL（绑定接口需要）。
    pub remote_path: String,
    /// 用户主动触发（点击「重新绑定」）—— 跳过消费端的已绑定检查。
    pub force: bool,
}

/// 共享触发槽：后台任务写入（`*slot.lock() = Some(trigger)`），
/// TUI 每帧 `try_lock` + `take` 消费。`Option` 语义保证信号只被消费一次；
/// 用共享槽而非 mpsc channel —— TUI 渲染路径不便 await，且只需要
/// "最新一条"信号。
pub type DevicePickerTriggerSlot = Arc<Mutex<Option<DevicePickerTrigger>>>;

/// 验证用户信息中 sb 配置数据完整性。
/// 返回 Err(String) 描述问题；Ok(()) 表示通过。
///
/// 任务要求：sb 配置必须三字段都非空；设备清单至少 1 个。
pub fn validate_user_info_integrity(info: &RemoteUserInfo) -> Result<(), String> {
    // 检查 sb 配置
    if info.sb.base_url.is_empty() {
        return Err("用户信息中 sb.base_url 为空".to_string());
    }
    if info.sb.username.is_empty() {
        return Err("用户信息中 sb.username 为空".to_string());
    }
    if info.sb.password.is_empty() {
        return Err("用户信息中 sb.password 为空".to_string());
    }
    // 检查设备清单
    if info.devices.is_empty() {
        return Err("用户信息中设备清单为空（至少应有 1 个设备）".to_string());
    }
    // 检查设备 name 字段
    for (i, dev) in info.devices.iter().enumerate() {
        if dev.name.is_empty() {
            return Err(format!("设备清单中第 {} 个设备 name 为空", i + 1));
        }
    }
    Ok(())
}

impl AccountConfig {
    /// 三个基础字段都非空才算已配置（`device_name` 不参与 —— 用户信息
    /// 接口只需要 key；device-name 是后续设备绑定的产物）。
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.account_id.is_empty()
            && !self.account_key.is_empty()
            && !self.remote_path.is_empty()
    }

    /// 当前账户是否已绑定设备（device_name 非空）。
    #[must_use]
    pub fn has_bound_device(&self) -> bool {
        !self.device_name.is_empty()
    }

    /// 从环境变量加载（`ACCOUNT_ID` / `ACCOUNT_KEY` / `REMOTE_PATH` /
    /// `DEVICE_NAME`）。
    ///
    /// 缺失字段回退到 auth-env 文件（`OC_SERVE_AUTH_ENV` 覆盖路径，默认
    /// `./.env`，与 [`crate::auth::AuthConfig::from_env`]
    /// 同一约定）；`remote_path` 为空时取 [`DEFAULT_REMOTE_PATH`]。
    #[must_use]
    pub fn load() -> Self {
        let mut cfg = Self {
            account_id: std::env::var(keys::ACCOUNT_ID).unwrap_or_default(),
            account_key: std::env::var(keys::ACCOUNT_KEY).unwrap_or_default(),
            remote_path: std::env::var(keys::REMOTE_PATH).unwrap_or_default(),
            device_name: std::env::var(keys::DEVICE_NAME).unwrap_or_default(),
        };

        if !cfg.is_configured() {
            let path = config::unified_env_path();
            let from_file = Self::read_env_file(&path);
            if cfg.account_id.is_empty() {
                cfg.account_id = from_file.account_id;
            }
            if cfg.account_key.is_empty() {
                cfg.account_key = from_file.account_key;
            }
            if cfg.remote_path.is_empty() {
                cfg.remote_path = from_file.remote_path;
            }
            if cfg.device_name.is_empty() {
                cfg.device_name = from_file.device_name;
            }
        }
        cfg
    }

    /// 将账户配置合并写入 env 文件（Unix 下 chmod 600）。
    ///
    /// 与整文件覆盖不同：只更新/追加 `ACCOUNT_ID` / `ACCOUNT_KEY` /
    /// `REMOTE_PATH` / `DEVICE_NAME` 四个 key，文件中已有的 auth / sb /
    /// rathole 等 section 原样保留。已存在的 key 就地更新为当前值（空值 = 显式
    /// 清空）；文件中不存在的 key 仅在值非空时追加，避免写入空行噪音。
    ///
    /// # Errors
    /// 返回 [`AppError::Io`] 当文件读写失败。
    pub fn write_env_file(&self, path: &Path) -> Result<(), AppError> {
        // 读出已有行；文件不存在视为空文件；其他读错误直接上抛，
        // 避免在权限/IO 异常时盲目覆盖整文件。
        let mut lines: Vec<String> = match std::fs::read_to_string(path) {
            Ok(contents) => contents.lines().map(str::to_string).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(AppError::Io(e)),
        };

        let updates = [
            (keys::ACCOUNT_ID, self.account_id.trim()),
            (keys::ACCOUNT_KEY, self.account_key.trim()),
            (keys::REMOTE_PATH, self.remote_path.trim()),
            (keys::DEVICE_NAME, self.device_name.trim()),
        ];

        let mut header_pending = true;
        for (key, value) in updates {
            let mut replaced = false;
            for line in lines.iter_mut() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                let Some((k, _)) = trimmed.split_once('=') else {
                    continue;
                };
                if k.trim() == key {
                    *line = format!("{key}={value}");
                    replaced = true;
                }
            }
            if !replaced && !value.is_empty() {
                if header_pending {
                    lines.push(String::new());
                    lines.push("# --- account login ---".to_string());
                    header_pending = false;
                }
                lines.push(format!("{key}={value}"));
            }
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(AppError::Io)?;
        }
        let mut body = lines.join("\n");
        if !body.is_empty() {
            body.push('\n');
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

    /// 从 env 文件读取账户配置（忽略空行/注释、去除两侧引号，不报错）。
    ///
    /// 文件缺失或 `remote_path` 为空时取 [`DEFAULT_REMOTE_PATH`]。
    #[must_use]
    pub fn read_env_file(path: &Path) -> Self {
        let mut cfg = Self::default();
        let Ok(contents) = std::fs::read_to_string(path) else {
            cfg.remote_path = DEFAULT_REMOTE_PATH.to_string();
            return cfg;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_string();
            match k.trim() {
                keys::ACCOUNT_ID if cfg.account_id.is_empty() => cfg.account_id = v,
                keys::ACCOUNT_KEY if cfg.account_key.is_empty() => cfg.account_key = v,
                keys::REMOTE_PATH if cfg.remote_path.is_empty() => cfg.remote_path = v,
                keys::DEVICE_NAME if cfg.device_name.is_empty() => cfg.device_name = v,
                _ => {}
            }
        }
        if cfg.remote_path.is_empty() {
            cfg.remote_path = DEFAULT_REMOTE_PATH.to_string();
        }
        cfg
    }

    /// 验证 remote_path 格式：
    /// - 必须以 `http://` 或 `https://` 开头，或
    /// - 不带 scheme（自动视为 `https://`，即默认安全）。
    ///
    /// 其他 scheme（如 `ftp://`、`file://`）仍被拒绝。
    pub fn validate_remote_path(s: &str) -> Result<(), String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("远程路径不能为空".to_string());
        }
        let with_scheme =
            s.starts_with("http://") || s.starts_with("https://");
        let looks_like_url = with_scheme
            || s.starts_with("www.")
            || s.contains("://") && !s.starts_with("http://") && !s.starts_with("https://");
        if !with_scheme {
            if looks_like_url {
                // 含其他 scheme —— 拒绝。
                return Err("远程路径必须以 http:// 或 https:// 开头".to_string());
            }
            // 无 scheme —— 默认按 https:// 处理，由调用方补 scheme。
        }
        Ok(())
    }
}

/// 把无 scheme 的 remote_path 规整为带 `https://` 前缀的完整 URL，
/// 便于直接拼路径（如 `format!("{base}/api/user/info")`）。
/// 已带 `http://` / `https://` 时原样返回；尾随 `/` 会被去掉。
pub(crate) fn normalize_remote_path(s: &str) -> String {
    let trimmed = s.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

// --- 统一 env 文件的通用 kv 读写 ------------------------------------------
//
// [`AccountConfig::write_env_file`] 的底层机制抽出来的通用版：账户持久化
// 与端口 / auth / rathole 等 section 的增量更新共用，保证任何一处写入都
// 不会抹掉文件里其他 section 的配置（取代旧 `config::write_persisted_env`
// 的整文件覆盖式写入）。

/// 读取 env 文件并解析为 (key, value) 对（忽略空行与注释）。
///
/// 文件不存在或不可读时返回空列表，不报错。
#[must_use]
pub fn read_env_kv(path: &Path) -> Vec<(String, String)> {
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
            Some((
                k.trim().to_string(),
                v.trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            ))
        })
        .collect()
}

/// 按 key 增量更新 env 文件（统一持久化的底层机制）：
///
/// - `Some(value)`：upsert 该 key —— 已存在则原位替换；不存在且值非空时
///   追加到文件末尾（空值不追加，避免 `KEY=` 噪音行）；
/// - `None`：删除该 key 所在行；
/// - 未出现在 `updates` 里的 key 与注释行原样保留 —— 账户 / auth / 端口 /
///   rathole 等 section 互不覆盖。
///
/// Unix 下写完后 chmod 600。
///
/// # Errors
/// 返回 [`AppError::Io`] 当文件写入失败。
pub fn upsert_env_keys(path: &Path, updates: &[(String, Option<String>)]) -> Result<(), AppError> {
    let mut lines: Vec<String> = match std::fs::read_to_string(path) {
        Ok(contents) => contents.lines().map(str::to_string).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(AppError::Io(e)),
    };

    let mut appended_any = false;
    for (key, value) in updates {
        let mut found = false;
        for line in lines.iter_mut() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((k, _)) = trimmed.split_once('=') else {
                continue;
            };
            if k.trim() == key {
                found = true;
                if let Some(v) = value {
                    *line = format!("{key}={v}");
                }
            }
        }
        match value {
            Some(v) if !found && !v.is_empty() => {
                if !appended_any {
                    lines.push(String::new());
                    lines.push("# --- updated by mini-oc-gui-serve ---".to_string());
                    appended_any = true;
                }
                lines.push(format!("{key}={v}"));
            }
            Some(_) => {}
            None => {
                lines.retain(|line| {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        return true;
                    }
                    match trimmed.split_once('=') {
                        Some((k, _)) => k.trim() != key,
                        None => true,
                    }
                });
            }
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(AppError::Io)?;
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
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

/// `/api/user/info` 的请求体。
#[derive(serde::Serialize)]
struct LoginBody<'a> {
    key: &'a str,
}

/// 通过 account_key 调用 `/api/user/info` 接口获取用户信息。
///
/// # Arguments
/// * `remote_path` - 远程API基础URL（自动去除尾部 `/`；无 scheme 时自动补 `https://`）
/// * `account_key` - 账户密钥
///
/// # Returns
/// * `Ok(RemoteUserInfo)` - 成功获取完整用户信息
/// * `Err(AppError::Internal)` - 网络错误（超时/连接拒绝）、HTTP 4xx/5xx
///   （含 429 速率限制、401 密钥错误）、JSON 解析错误
/// * `Err(AppError::BadRequest)` - `remote_path` 格式非法
///
/// # Example
/// ```no_run
/// # async fn demo() -> Result<(), mini_oc_gui_serve::error::AppError> {
/// let info = mini_oc_gui_serve::account::fetch_user_info(
///     "oc.isoops.com",
///     "k1234567890123456789012345678901",
/// ).await?;
/// # Ok(())
/// # }
/// ```
pub async fn fetch_user_info(
    remote_path: &str,
    account_key: &str,
) -> Result<RemoteUserInfo, AppError> {
    AccountConfig::validate_remote_path(remote_path).map_err(AppError::BadRequest)?;
    let base = normalize_remote_path(remote_path);
    let url = format!("{base}/api/user/info");

    let resp = http_client()
        .post(&url)
        .json(&LoginBody { key: account_key }) // 自动设置 Content-Type: application/json
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("user/info network error: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        // 429 时尽量带上 Retry-After（须在 body 消费 response 前读取）。
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|s| format!("，retry after {s}s"))
            .unwrap_or_default();
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("POST {url} failed: HTTP {status}");

        let reason = match status.as_u16() {
            429 => format!("rate limited (HTTP 429{retry_after})"),
            401 | 403 => format!("unauthorized (HTTP {status}) — account_key 无效"),
            _ => format!("HTTP {status}"),
        };
        return Err(AppError::Internal(format!(
            "user/info request failed: {reason}: {}",
            body_excerpt(&body)
        )));
    }

    resp.json::<RemoteUserInfo>()
        .await
        .map_err(|e| AppError::Internal(format!("user/info response parse error: {e}")))
}

/// `/api/device-bind` 请求体
#[derive(serde::Serialize)]
pub struct DeviceBindBody<'a> {
    pub key: &'a str,
    pub user_id: &'a str,
    pub name: &'a str,
    pub bound: bool,
    /// 本机平台类型（"macos" / "windows" / "linux" / "unknown"）
    pub pctype: &'a str,
    /// 客户端上报的设备真实名称 —— 服务端（mini-oc-web）会写入该用户
    /// 对应设备条目的 `device-name` 字段（**必传**，否则接口返回 422）。
    ///
    /// 服务端约束（routes.rs 中 `device_bind` 的入口校验）：
    /// - 长度 1-64 字符；
    /// - 不含控制字符；
    /// - 不含 URL 结构字符（`/` `?` `#` `%` `\`）。
    #[serde(rename = "device-name")]
    pub device_name: &'a str,
}

/// 返回本机平台类型（编译期决定）
///
/// "macos" / "windows" / "linux" / "unknown"。编译期常量，运行期零开销；
/// 与 `src/storage/paths::pctype()` 保持一致以确保 device-bind 与
/// 远端 path-list 路径中的 pctype 段匹配（设备绑定记录的 pctype 必须
/// 与实际访问的 path-list 路径段一致，绑定才会生效）。
#[must_use]
pub fn pctype() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// 验证 `device-name` 字段的服务端约束（与 mini-oc-web `routes.rs`
/// 中 `device_bind` 的入口校验规则一致）。
///
/// 约束：
/// - 长度 1-64 字符；
/// - 不含控制字符；
/// - 不含 URL 结构字符（`/` `?` `#` `%` `\\`）。
///
/// 注意：mini-oc-web 还会把 `device-name` 用于服务端远端路径
/// (`serv/opencode/{uid}/{pctype}/{device-name}/path-list`)，
/// 因此这些字符限制实际是为了让路径拼接安全。
pub fn validate_device_name(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("device-name 不能为空".to_string());
    }
    if s.chars().count() > 64 {
        return Err("device-name 长度不能超过 64 字符".to_string());
    }
    if s.chars().any(char::is_control) {
        return Err("device-name 不能含控制字符".to_string());
    }
    if s.chars().any(|c| "/?#%\\".contains(c)) {
        return Err("device-name 不能含 URL 结构字符 (/ ? # % \\)".to_string());
    }
    Ok(())
}

/// `/api/device-bind` 成功响应
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceBindResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub device: Option<RemoteDevice>,
}

/// 调用 /api/device-bind 修改单个设备的绑定状态。
///
/// # Arguments
/// * `remote_path` - 远程API基础URL
/// * `account_key` - 账户密钥
/// * `user_id` - 用户ID
/// * `device_name` - 设备清单中的服务名（`devices[].name`，绑定目标，
///   与远端 `path-list` 路径中的 `device-name` 段不同 —— 路径段是
///   **客户端的 pcname**，见下）
/// * `client_device_name` - 客户端 pcname（OS 主机名/用户名）。服务端
///   会写入对应设备条目的 `device-name` 字段，并作为远端路径
///   `serv/opencode/{uid}/{pctype}/{device-name}/path-list` 的第三段。
///   **必传**（缺失会返回 422）。客户端调用时应使用
///   `crate::storage::paths::pcname()` 获取
/// * `pctype` - 本机平台类型（"macos" / "windows" / "linux" / "unknown"）
/// * `bound` - true=绑定，false=解绑
///
/// # Returns
/// * `Ok(DeviceBindResponse)` - 成功
/// * `Err(AppError::BadRequest)` - remote_path 格式非法 / device_name 不合规
/// * `Err(AppError::NotFound)` - HTTP 404：目标设备不在服务端清单
///   （调用方可据此跳过解绑直接绑定,详情见服务端日志）
/// * `Err(AppError::Internal)` - 网络错误、HTTP 4xx/5xx（401/403/404/422/429）、JSON 解析错误
///
/// 注意：每个调用只允许修改一个设备的状态。
pub async fn bind_device(
    remote_path: &str,
    account_key: &str,
    user_id: &str,
    device_name: &str,
    client_device_name: &str,
    pctype: &str,
    bound: bool,
) -> Result<DeviceBindResponse, AppError> {
    AccountConfig::validate_remote_path(remote_path).map_err(AppError::BadRequest)?;
    validate_device_name(client_device_name).map_err(AppError::BadRequest)?;
    let base = normalize_remote_path(remote_path);
    let url = format!("{base}/api/device-bind");

    let resp = http_client()
        .post(&url)
        .json(&DeviceBindBody {
            key: account_key,
            user_id,
            name: device_name,
            bound,
            pctype,
            device_name: client_device_name,
        })
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("device-bind network error: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("POST {url} failed: HTTP {status}");
        let code = status.as_u16();
        let reason = match code {
            401 => "unauthorized (HTTP 401) — account_key 无效".to_string(),
            403 => "forbidden (HTTP 403) — user_id 与 key 不匹配".to_string(),
            404 => format!("not_found (HTTP 404) — 设备 {device_name} 不在清单中"),
            429 => "rate limited (HTTP 429)".to_string(),
            _ => format!("HTTP {code}"),
        };
        let detail = format!("device-bind request failed: {reason}: {}", body_excerpt(&body));
        // 错误分类:404(目标设备不在服务端清单)返回 [`AppError::NotFound`],
        // 让调用方(切换绑定的"先解绑原设备"分支)能精确识别并跳过;
        // 其余状态保持 Internal(携带完整原因)。NotFound 是 unit 变体,
        // 详情先落日志再返回。
        if code == 404 {
            tracing::warn!("device-bind 404: {detail}");
            return Err(AppError::NotFound);
        }
        return Err(AppError::Internal(detail));
    }

    resp.json::<DeviceBindResponse>()
        .await
        .map_err(|e| AppError::Internal(format!("device-bind response parse error: {e}")))
}

/// 共享 HTTP 客户端（10 秒超时，与 SilverBullet 远端客户端一致）。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client")
    })
}

/// 错误响应体只保留前 ~200 字符，避免日志刷屏。
fn body_excerpt(body: &str) -> String {
    const MAX_CHARS: usize = 200;
    let mut s: String = body.chars().take(MAX_CHARS).collect();
    if s.len() < body.len() {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_remote_path_accepts_http_https_and_trims() {
        assert!(AccountConfig::validate_remote_path("https://oc.isoops.com").is_ok());
        assert!(AccountConfig::validate_remote_path("http://oc.isoops.com").is_ok());
        assert!(AccountConfig::validate_remote_path("  https://1.2.3.4:8080  ").is_ok());
    }

    #[test]
    fn validate_remote_path_accepts_scheme_less_hostname() {
        // 无 scheme 视为 https:// —— 默认远程路径 `oc.isoops.com` 必须合法。
        assert!(AccountConfig::validate_remote_path("oc.isoops.com").is_ok());
        assert!(AccountConfig::validate_remote_path("  1.2.3.4:8080  ").is_ok());
    }

    #[test]
    fn validate_remote_path_rejects_empty_and_wrong_scheme() {
        assert!(AccountConfig::validate_remote_path("").is_err());
        assert!(AccountConfig::validate_remote_path("   ").is_err());
        assert!(AccountConfig::validate_remote_path("ftp://oc.isoops.com").is_err());
        assert!(AccountConfig::validate_remote_path("file:///tmp/x").is_err());
    }

    #[test]
    fn normalize_remote_path_adds_https_for_bare_host() {
        assert_eq!(normalize_remote_path("oc.isoops.com"), "https://oc.isoops.com");
        assert_eq!(
            normalize_remote_path("  oc.isoops.com/  "),
            "https://oc.isoops.com"
        );
        // 已带 scheme 时原样返回（去掉尾随 /）。
        assert_eq!(
            normalize_remote_path("https://oc.isoops.com/"),
            "https://oc.isoops.com"
        );
        assert_eq!(
            normalize_remote_path("http://1.2.3.4:8080"),
            "http://1.2.3.4:8080"
        );
    }

    #[test]
    fn read_env_file_parses_keys_and_trims_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(
            &path,
            "# comment line\n\
             ACCOUNT_ID=\"user-123\"\n\
             ACCOUNT_KEY='abc'\n\
             REMOTE_PATH = https://oc.isoops.com \n\
             OTHER_KEY=keep\n",
        )
        .unwrap();

        let cfg = AccountConfig::read_env_file(&path);
        assert_eq!(cfg.account_id, "user-123");
        assert_eq!(cfg.account_key, "abc");
        assert_eq!(cfg.remote_path, "https://oc.isoops.com");
        assert!(cfg.is_configured());
    }

    #[test]
    fn read_env_file_missing_file_returns_default_remote_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AccountConfig::read_env_file(&dir.path().join("nope.env"));
        assert_eq!(cfg.account_id, "");
        assert_eq!(cfg.account_key, "");
        assert_eq!(cfg.remote_path, DEFAULT_REMOTE_PATH);
        assert!(!cfg.is_configured());
    }

    #[test]
    fn read_env_file_fills_default_remote_path_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(&path, "ACCOUNT_ID=u\nACCOUNT_KEY=k\n").unwrap();

        let cfg = AccountConfig::read_env_file(&path);
        assert_eq!(cfg.remote_path, DEFAULT_REMOTE_PATH);
        assert!(cfg.is_configured());
    }

    #[test]
    fn write_then_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        let cfg = AccountConfig {
            account_id: "id-1".to_string(),
            account_key: "key-1".to_string(),
            remote_path: "http://10.0.0.1:9000".to_string(),
            device_name: String::new(),
        };
        cfg.write_env_file(&path).unwrap();

        let back = AccountConfig::read_env_file(&path);
        assert_eq!(back.account_id, "id-1");
        assert_eq!(back.account_key, "key-1");
        assert_eq!(back.remote_path, "http://10.0.0.1:9000");
    }

    #[test]
    fn write_env_file_preserves_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(
            &path,
            "# Generated by `mini-oc-gui-serve`\n\
             OPENCODE_SERVER_USERNAME=opencode\n\
             OPENCODE_SERVER_PASSWORD=secret\n\
             SB_URL=https://md.isoops.com\n",
        )
        .unwrap();

        AccountConfig {
            account_id: "u1".to_string(),
            account_key: "k1".to_string(),
            remote_path: String::new(),
            device_name: String::new(),
        }
        .write_env_file(&path)
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# Generated by `mini-oc-gui-serve`"));
        assert!(contents.contains("OPENCODE_SERVER_USERNAME=opencode"));
        assert!(contents.contains("OPENCODE_SERVER_PASSWORD=secret"));
        assert!(contents.contains("SB_URL=https://md.isoops.com"));
        assert!(contents.contains("ACCOUNT_ID=u1"));
        assert!(contents.contains("ACCOUNT_KEY=k1"));
        // 空值且文件中原本没有的 key 不追加。
        assert!(!contents.contains("REMOTE_PATH="));
    }

    #[test]
    fn write_env_file_updates_existing_keys_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(
            &path,
            "ACCOUNT_ID=old\nACCOUNT_KEY=oldkey\nREMOTE_PATH=https://old\nOTHER=1\n",
        )
        .unwrap();

        AccountConfig {
            account_id: "new".to_string(),
            account_key: "newkey".to_string(),
            remote_path: "https://new".to_string(),
            device_name: String::new(),
        }
        .write_env_file(&path)
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("ACCOUNT_ID=new\n"));
        assert!(contents.contains("ACCOUNT_KEY=newkey\n"));
        assert!(contents.contains("REMOTE_PATH=https://new\n"));
        assert!(contents.contains("OTHER=1"));
        assert!(!contents.contains("old"));
        assert_eq!(contents.matches("ACCOUNT_ID=").count(), 1);
        assert_eq!(contents.matches("REMOTE_PATH=").count(), 1);
    }

    #[test]
    fn write_env_file_clears_existing_keys_with_empty_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(&path, "ACCOUNT_ID=u1\nACCOUNT_KEY=k1\n").unwrap();

        AccountConfig::default().write_env_file(&path).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("ACCOUNT_ID=\n"));
        assert!(contents.contains("ACCOUNT_KEY=\n"));
    }

    // --- device_name ---

    #[test]
    fn device_name_roundtrip_via_env_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        let cfg = AccountConfig {
            account_id: "id-1".to_string(),
            account_key: "key-1".to_string(),
            remote_path: "http://10.0.0.1:9000".to_string(),
            device_name: "pc-1".to_string(),
        };
        cfg.write_env_file(&path).unwrap();

        let back = AccountConfig::read_env_file(&path);
        assert_eq!(back.device_name, "pc-1");
        assert!(back.has_bound_device());
        // device_name 不参与 is_configured 判定（四个字段独立）。
        let unbound = AccountConfig {
            device_name: "pc-1".to_string(),
            ..AccountConfig::default()
        };
        assert!(unbound.has_bound_device());
        assert!(!unbound.is_configured());
    }

    #[test]
    fn device_name_empty_does_not_pollute_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(&path, "ACCOUNT_ID=u1\nACCOUNT_KEY=k1\n").unwrap();

        AccountConfig {
            account_id: "u1".to_string(),
            account_key: "k1".to_string(),
            remote_path: String::new(),
            device_name: String::new(),
        }
        .write_env_file(&path)
        .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("DEVICE_NAME"));
    }

    #[test]
    fn read_env_file_parses_device_name_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.env");
        std::fs::write(
            &path,
            "ACCOUNT_ID=u\nACCOUNT_KEY=k\nDEVICE_NAME=\"pc-1\"\nDEVICE_NAME=pc-2\n",
        )
        .unwrap();

        let cfg = AccountConfig::read_env_file(&path);
        assert_eq!(cfg.device_name, "pc-1");
        assert!(cfg.has_bound_device());
        assert!(!AccountConfig::default().has_bound_device());
    }

    #[test]
    fn remote_user_info_parses_full_payload() {
        let json = r#"{
            "id": "u-1",
            "name": "alice",
            "key": "0123456789abcdef0123456789abcdef",
            "cloud_ip": "1.2.3.4",
            "last_used_at": "2026-01-01T00:00:00Z",
            "devices": [{"name": "pc-1", "port": 9464}],
            "sb": {
                "base_url": "https://md.isoops.com",
                "username": "sbu",
                "password": "sbp"
            },
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2026-01-02T00:00:00Z"
        }"#;
        let info: RemoteUserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.id, "u-1");
        assert_eq!(info.name, "alice");
        assert_eq!(info.cloud_ip.as_deref(), Some("1.2.3.4"));
        assert_eq!(info.devices.len(), 1);
        assert_eq!(info.devices[0].name, "pc-1");
        assert_eq!(info.devices[0].port, 9464);
        assert_eq!(info.sb.base_url, "https://md.isoops.com");
        assert_eq!(info.sb.username, "sbu");
        assert_eq!(info.sb.password, "sbp");
    }

    #[test]
    fn remote_user_info_parses_minimal_payload() {
        let json = r#"{"id":"u","name":"n","key":"k","sb":{"base_url":"b","username":"u","password":"p"}}"#;
        let info: RemoteUserInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.cloud_ip, None);
        assert_eq!(info.last_used_at, None);
        assert!(info.devices.is_empty());
        assert_eq!(info.created_at, None);
        assert_eq!(info.updated_at, None);
        assert_eq!(info.sb.base_url, "b");
    }

    // --- fetch_user_info（仅错误路径；不引入 mock server 依赖） ---

    #[test]
    fn login_body_serializes_key_only() {
        let body = serde_json::to_string(&LoginBody { key: "abc" }).unwrap();
        assert_eq!(body, r#"{"key":"abc"}"#);
    }

    #[tokio::test]
    async fn fetch_user_info_maps_network_error_to_internal() {
        // 回环地址端口 1 实际必然拒绝连接；任何网络故障都必须映射为
        // AppError::Internal 而不是 panic。
        let err = fetch_user_info("http://127.0.0.1:1", "0123456789abcdef0123456789abcdef")
            .await
            .expect_err("connection must fail");
        assert!(matches!(err, AppError::Internal(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn fetch_user_info_rejects_invalid_remote_path() {
        let err = fetch_user_info("ftp://bad", "0123456789abcdef0123456789abcdef")
            .await
            .expect_err("invalid path must fail");
        assert!(matches!(err, AppError::BadRequest(_)), "got {err:?}");
    }

    // --- validate_user_info_integrity ---

    fn sample_info(sb: RemoteSbConfig, devices: Vec<RemoteDevice>) -> RemoteUserInfo {
        RemoteUserInfo {
            id: "u-1".to_string(),
            name: "alice".to_string(),
            key: "k".to_string(),
            cloud_ip: None,
            last_used_at: None,
            devices,
            sb,
            created_at: None,
            updated_at: None,
        }
    }

    fn sample_sb(base_url: &str, username: &str, password: &str) -> RemoteSbConfig {
        RemoteSbConfig {
            base_url: base_url.to_string(),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    #[test]
    fn validate_user_info_integrity_rejects_empty_sb() {
        let devices = vec![RemoteDevice {
            name: "pc-1".to_string(),
            port: 9464,
            bound: false,
        }];
        let empty_base = sample_info(sample_sb("", "u", "p"), devices.clone());
        assert_eq!(
            validate_user_info_integrity(&empty_base),
            Err("用户信息中 sb.base_url 为空".to_string())
        );
        let empty_username = sample_info(sample_sb("https://md", "", "p"), devices.clone());
        assert_eq!(
            validate_user_info_integrity(&empty_username),
            Err("用户信息中 sb.username 为空".to_string())
        );
        let empty_password = sample_info(sample_sb("https://md", "u", ""), devices);
        assert_eq!(
            validate_user_info_integrity(&empty_password),
            Err("用户信息中 sb.password 为空".to_string())
        );
    }

    #[test]
    fn validate_user_info_integrity_rejects_empty_devices() {
        let info = sample_info(sample_sb("https://md", "u", "p"), Vec::new());
        assert_eq!(
            validate_user_info_integrity(&info),
            Err("用户信息中设备清单为空（至少应有 1 个设备）".to_string())
        );
    }

    #[test]
    fn validate_user_info_integrity_accepts_valid_info() {
        let info = sample_info(
            sample_sb("https://md.isoops.com", "sbu", "sbp"),
            vec![RemoteDevice {
                name: "pc-1".to_string(),
                port: 9464,
                bound: true,
            }],
        );
        assert_eq!(validate_user_info_integrity(&info), Ok(()));
    }

    // --- RemoteDevice bound 字段 ---

    #[test]
    fn remote_device_parses_with_bound_field() {
        let dev: RemoteDevice =
            serde_json::from_str(r#"{"name":"pc-1","port":9464,"bound":true}"#).unwrap();
        assert_eq!(dev.name, "pc-1");
        assert_eq!(dev.port, 9464);
        assert!(dev.bound);
    }

    #[test]
    fn remote_device_parses_without_bound_field_defaults_to_false() {
        let dev: RemoteDevice = serde_json::from_str(r#"{"name":"pc-1","port":9464}"#).unwrap();
        assert_eq!(dev.name, "pc-1");
        assert_eq!(dev.port, 9464);
        assert!(!dev.bound);
    }

    // --- bind_device（仅请求体序列化；不引入 mock server 依赖） ---

    /// bind_device 的 404 → [`AppError::NotFound`] 分类回归测试。
    ///
    /// 场景:切换绑定时"先解绑原设备"返回 404(原设备已不在服务端清单),
    /// 调用方(ui/app.rs confirm_device_bind)靠 `matches!(e, AppError::NotFound)`
    /// 精确识别并跳过解绑、直接绑定新设备。此分类若回退成 Internal,
    /// 跳过逻辑会失效 —— 本测试锁定该契约。
    ///
    /// 用 std TcpListener 起一个一次性 mock server 返回 404,
    /// 不引入额外测试依赖。
    #[tokio::test]
    async fn bind_device_maps_404_to_not_found() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // 读掉请求头(不含 body 也无妨 —— 只回固定响应)。
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = r#"{"error":{"code":"not_found","message":"device ghost-dev not in list"}}"#;
                let resp = format!(
                    "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });

        let err = bind_device(
            &format!("http://{addr}"),
            "k1234567890123456789012345678901",
            "u-1",
            "ghost-dev",
            "my-pc",
            pctype(),
            false,
        )
        .await
        .expect_err("404 must be an error");

        assert!(
            matches!(err, AppError::NotFound),
            "404 必须映射为 AppError::NotFound(供解绑跳过逻辑识别),got: {err:?}"
        );
        let _ = handle.join();
    }

    #[test]
    fn device_bind_body_serializes_correctly() {
        let body = DeviceBindBody {
            key: "k123",
            user_id: "u-1",
            name: "pc-1",
            bound: true,
            pctype: "macos",
            device_name: "my-macbook",
        };
        let json = serde_json::to_string(&body).unwrap();
        // 注意：device_name 在 JSON 中序列化为 "device-name"（kebab-case）
        assert_eq!(
            json,
            r#"{"key":"k123","user_id":"u-1","name":"pc-1","bound":true,"pctype":"macos","device-name":"my-macbook"}"#
        );

        let unbind = DeviceBindBody {
            key: "k123",
            user_id: "u-1",
            name: "pc-1",
            bound: false,
            pctype: "windows",
            device_name: "office-pc",
        };
        assert_eq!(
            serde_json::to_string(&unbind).unwrap(),
            r#"{"key":"k123","user_id":"u-1","name":"pc-1","bound":false,"pctype":"windows","device-name":"office-pc"}"#
        );
    }

    #[test]
    fn device_bind_body_includes_device_name_field() {
// 显式锁定 device-name 字段存在 —— mini-oc-web 路由要求该字段
        // 必传，缺失会返回 422。回归测试：保护该字段不被无意删除。
        let body = DeviceBindBody {
            key: "k123",
            user_id: "u-1",
            name: "my-pc",
            bound: true,
            pctype: "macos",
            device_name: "my-macbook",
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["device-name"], "my-macbook", "device-name field required");
        // 同时确认 Rust 字段名是 device_name（不是 device-name）
        let s = serde_json::to_string(&body).unwrap();
        assert!(
            s.contains("device-name") && s.contains("my-macbook"),
            "JSON 字段名必须是 kebab-case device-name: {s}"
        );
    }

    #[test]
    fn device_bind_body_serializes_with_pctype() {
        let body = serde_json::to_string(&DeviceBindBody {
            key: "abc",
            user_id: "u-1",
            name: "my-pc",
            bound: true,
            pctype: "macos",
            device_name: "test-device",
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["key"], "abc");
        assert_eq!(v["pctype"], "macos");
        assert_eq!(v["name"], "my-pc");
        assert_eq!(v["bound"], true);
        assert_eq!(v["user_id"], "u-1");
        assert_eq!(v["device-name"], "test-device");
    }

    #[test]
    fn pctype_returns_known_platform_string() {
        let p = pctype();
        // 必须是 4 个已知值之一（编译期决定 OS 类型）
        assert!(matches!(p, "macos" | "windows" | "linux" | "unknown"), "got: {p}");
    }

    #[test]
    fn validate_device_name_accepts_normal_names() {
        // 合法名 —— 必须通过
        for ok in &[
            "my-pc",
            "office_laptop",
            "办公室MacBook",
            "dev-pc-1",
            "1",
            &"a".repeat(64), // 边界 64 字符
        ] {
            assert!(
                validate_device_name(ok).is_ok(),
                "合法名被拒绝: {ok:?}"
            );
        }
    }

    #[test]
    fn validate_device_name_rejects_illegal_names() {
        // 空 / 超长 / 控制字符 / URL 结构字符 —— 必须拒绝
        assert!(validate_device_name("").is_err());
        assert!(validate_device_name("   ").is_err(), "全空白 trim 后为空应拒绝");
        assert!(validate_device_name(&"a".repeat(65)).is_err(), "65 字符应拒绝");
        // URL 结构字符
        for bad in &["a/b", "a?b", "a#b", "a%b", "a\\b"] {
            assert!(
                validate_device_name(bad).is_err(),
                "含 URL 结构字符的 device-name 必须拒绝: {bad:?}"
            );
        }
        // 控制字符（换行 / 制表 / NUL）
        for bad in &["a\nb", "a\tb", "a\0b"] {
            assert!(
                validate_device_name(bad).is_err(),
                "含控制字符的 device-name 必须拒绝: {bad:?}"
            );
        }
    }

    #[test]
    fn validate_device_name_trims_whitespace() {
        // 前后空白应被 trim，再校验长度/字符
        assert!(validate_device_name("  pc-1  ").is_ok());
        assert!(validate_device_name("  ").is_err(), "trim 后为空应拒绝");
    }

    #[test]
    fn device_bind_response_parses_ok_payload() {
        let resp: DeviceBindResponse = serde_json::from_str(
            r#"{"ok":true,"user_id":"u-1","device":{"name":"pc-1","port":9464,"bound":true}}"#,
        )
        .unwrap();
        assert!(resp.ok);
        assert_eq!(resp.user_id, "u-1");
        let dev = resp.device.expect("device present");
        assert_eq!(dev.name, "pc-1");
        assert!(dev.bound);
    }

    /// 设置 `OC_SERVE_AUTH_ENV` 指向 temp 路径后,`AccountConfig::load()`
    /// 应优先从该路径读取 `.env`,忽略 exe_dir 与 cwd 下的同名文件。
    ///
    /// 当前实现(`load()` 内联两段式)已正确处理此优先级 —— 此测试为回归保护。
    /// 它必须**始终通过**;实施 Task 4 改动后不应被破坏。
    #[test]
    fn load_respects_oc_serve_auth_env_priority() {
        use std::sync::Mutex;
        // 全局 env var 修改需要串行,避免与其他测试干扰。
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let auth_env_path = tmp.path().join("auth.env");
        std::fs::write(
            &auth_env_path,
            "ACCOUNT_ID=user-from-auth-env\nACCOUNT_KEY=key-from-auth-env\n",
        )
        .unwrap();

        // 在 tempdir 上下文下保存旧值并切换到新值。
        let saved = std::env::var("OC_SERVE_AUTH_ENV").ok();
        // SAFETY: ENV_LOCK 仅串行化本测试体内的 env 写入。
        // 其它并行测试若也改 OC_SERVE_AUTH_ENV 仍可能冲突 —— 见测试顶部
        // docstring 关于 `--test-threads=1` 的约定。
        unsafe {
            std::env::set_var("OC_SERVE_AUTH_ENV", &auth_env_path);
        }

        let cfg = AccountConfig::load();
        assert_eq!(cfg.account_id, "user-from-auth-env");
        assert_eq!(cfg.account_key, "key-from-auth-env");

        // 还原 env var,防止污染后续测试。
        match saved {
            Some(v) => unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) },
            None => unsafe { std::env::remove_var("OC_SERVE_AUTH_ENV") },
        }
    }

    /// **核心回归测试** —— 模拟用户安装场景:进程 exe_dir 下存在 `.env`
    /// (无 `OC_SERVE_AUTH_ENV` 覆盖)。当前 `AccountConfig::load()`
    /// 只检查 cwd 而不查 exe_dir,**此测试在改动前必失败**。
    ///
    /// 串行化约束:本测试写 `current_exe().parent()/.env` —— 与 Task 1 的
    /// `OC_SERVE_AUTH_ENV` 写互不干扰,但两者都改进程 env,必须用
    /// `cargo test -- --test-threads=1` 串行执行。
    #[test]
    fn load_reads_env_from_exe_dir_when_no_override() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        // 必须清除 `OC_SERVE_AUTH_ENV` 才能走到 `unified_env_path()` 的 exe_dir 兜底。
        let saved_env = std::env::var("OC_SERVE_AUTH_ENV").ok();
        // SAFETY: `std::env::remove_var` 在 Rust 1.74+ 是 unsafe(进程全局
        // 状态)。本测试体内 ENV_LOCK 保证串行,测试结束前会 restore。
        // 跨测试串行化由 `--test-threads=1` 兜底,见 docstring。
        unsafe {
            std::env::remove_var("OC_SERVE_AUTH_ENV");
        }

        let exe_path = std::env::current_exe().expect("current_exe");
        let exe_dir = exe_path.parent().expect("exe parent");
        let env_path = exe_dir.join(".env");

        let backup = if env_path.exists() {
            Some(std::fs::read(&env_path).expect("backup"))
        } else {
            None
        };

        std::fs::write(
            &env_path,
            "ACCOUNT_ID=user-from-exe-dir\nACCOUNT_KEY=key-from-exe-dir\n",
        )
        .expect("write .env at exe_dir");

        let cfg = AccountConfig::load();
        assert_eq!(cfg.account_id, "user-from-exe-dir");
        assert_eq!(cfg.account_key, "key-from-exe-dir");

        match backup {
            Some(bytes) => std::fs::write(&env_path, bytes).expect("restore .env"),
            None => {
                let _ = std::fs::remove_file(&env_path);
            }
        }

        if let Some(v) = saved_env {
            // SAFETY: 同上 `remove_var` 注释。
            unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) }
        }
    }

    /// 兜底测试:无 `OC_SERVE_AUTH_ENV`、无法解析 exe_dir(模拟 sandbox)时,
    /// `AccountConfig::load()` 应回退到 cwd/.env。
    ///
    /// 实际场景:无法制造 sandbox 失败 —— `current_exe()` 在测试环境下
    /// 总是成功。所以此测试仅验证 cwd 兜底**在 env var 优先 + exe_dir
    /// 命中的语义后仍能工作** —— 即 cwd .env 里的字段在 exe_dir .env 不存在
    /// 时可被读取。
    ///
    /// 我们把 exe_dir/.env 临时移除(若存在),在 cwd 写 .env,验证读取。
    /// 由于 `std::env::current_exe()` 在 cargo test 下指向
    /// `target/debug/deps/<test_binary>`,exe_dir 是 deps 目录 —— 我们
    /// 提前确认 deps 目录**没有** `.env` 文件存在(若存在,test 跳过并打日志)。
    #[test]
    fn load_falls_back_to_cwd_env_when_no_exe_dir_or_env_var() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let saved_env = std::env::var("OC_SERVE_AUTH_ENV").ok();
        // SAFETY: 同 Task 1/2 注释 —— remove_var 在 Rust 1.74+ 是 unsafe,
        // 本测试体内 ENV_LOCK 串行,跨测试由 `--test-threads=1` 兜底。
        unsafe { std::env::remove_var("OC_SERVE_AUTH_ENV") }

        // cwd 写 .env。
        let cwd_env = std::env::current_dir().unwrap().join(".env");
        let cwd_backup = if cwd_env.exists() {
            Some(std::fs::read(&cwd_env).expect("backup cwd"))
        } else {
            None
        };
        std::fs::write(
            &cwd_env,
            "ACCOUNT_ID=user-from-cwd\nACCOUNT_KEY=key-from-cwd\n",
        )
        .expect("write cwd .env");

        // exe_dir 不在我们的控制下 —— 跳过。
        let exe_path = std::env::current_exe().unwrap();
        let exe_dir = exe_path.parent().unwrap();
        let exe_env = exe_dir.join(".env");
        if exe_env.exists() {
            eprintln!("[skip] exe_dir .env already exists; cannot isolate cwd fallback");
            // 恢复 cwd .env 后返回。
            match cwd_backup {
                Some(b) => std::fs::write(&cwd_env, b).unwrap(),
                None => { let _ = std::fs::remove_file(&cwd_env); }
            }
            if let Some(v) = &saved_env { unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) } }
            return;
        }

        let cfg = AccountConfig::load();
        assert_eq!(cfg.account_id, "user-from-cwd");
        assert_eq!(cfg.account_key, "key-from-cwd");

        // 恢复 cwd .env。
        match cwd_backup {
            Some(b) => std::fs::write(&cwd_env, b).unwrap(),
            None => { let _ = std::fs::remove_file(&cwd_env); }
        }
        if let Some(v) = saved_env {
            // SAFETY: 同 remove_var 注释。
            unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) }
        }
    }
}
