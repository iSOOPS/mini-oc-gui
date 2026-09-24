//! `mini-oc-gui-serve` — single-binary TUI + Axum server for the opencode
//! serve actuator workflow.
//!
//! Mirrors the original `oc-serve-start.sh` / `oc-serve-tui-actuator.sh`
//! pair in Rust: starts `opencode serve` (optionally behind `rathole`),
//! drives a ratatui menu, and exposes a small HTTP surface for project /
//! session inspection.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

use mini_oc_gui_serve::{
    account::{
        AccountConfig, DevicePickerTrigger, DevicePickerTriggerSlot, RemoteUserInfo,
        fetch_user_info, validate_user_info_integrity,
    },
    auth::AuthConfig,
    error::AppError,
    handlers::{AppState, router},
    serve::ServeSupervisor,
    storage::{PathListStore, cache::FileCache, remote::RemoteClient},
    ui::TuiApp,
};

#[derive(Parser, Debug)]
#[command(
    name = "mini-oc-gui-serve",
    about = "OpenCode serve launcher + path-list manager (Axum + ratatui)"
)]
struct Cli {
    /// Skip the TUI and run only the HTTP server in the foreground.
    ///
    /// Useful when launched in a non-VT terminal (e.g. cmd.exe, plain
    /// PowerShell host) — the HTTP API at `0.0.0.0:<port>` keeps serving
    /// even after the TUI bails out.
    #[arg(long)]
    no_tui: bool,

    /// Skip binding the HTTP listener (TUI only).
    #[arg(long)]
    no_http: bool,

    /// (Deprecated) Generate a random HTTP Basic password, write it to
    /// `.env` (chmod 600 on Unix) and exit.
    ///
    /// Kept for backward compatibility; account login (ACCOUNT_KEY) is
    /// now the preferred way to provision credentials.
    #[arg(long)]
    generate_auth: bool,

    /// Override the auth-env file location (defaults to
    /// `$OC_SERVE_AUTH_ENV` or `./.env`).
    #[arg(long, env = "OC_SERVE_AUTH_ENV")]
    auth_env: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 1. Tracing.
    // TUI 模式下把日志写入共享缓冲区（在日志面板渲染），避免 stderr 污染界面；
    // --no-tui 模式保持默认 stderr 输出。
    let log_buffer = mini_oc_gui_serve::ui::LogBuffer::new();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if cli.no_tui {
        fmt().with_env_filter(filter).with_target(false).init();
    } else {
        fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(log_buffer.clone())
            .init();
    }

    // 2. 释放内嵌图标到 <exe_dir>/assets/(缺失才写,允许用户自定义;
    //    失败仅告警 —— 图标缺失不影响服务可用性)。
    if let Err(e) = mini_oc_gui_serve::icons::export_default() {
        tracing::warn!("释放内嵌图标失败: {e}");
    }

    // 3. Optional: generate-and-exit.
    if cli.generate_auth {
        return generate_auth_and_exit(cli.auth_env.as_deref());
    }

    // 4. 清理统一 env 文件为**仅账户登录区块**（端口 / auth / rathole /
    //    sb 等旧持久化行全部删除 —— 这些数据改为每次启动从
    //    /api/user/info 内存构建），再经 dotenvy 注入进程环境。
    let unified_env_path = cli
        .auth_env
        .clone()
        .or_else(|| Some(mini_oc_gui_serve::config::unified_env_path()))
        .expect("unified_env_path always returns Some");

    // 一次性迁移:
    //   1) cwd 下的旧 .env → 新位置(可执行文件同目录)
    //   2) cwd 下的旧 .oc-serve-auth.env → 新位置(可执行文件同目录) ——
    //      兼容旧版本命名,完成后删除旧文件。
    // 满足「旧位置有文件 + 新位置没有」时才执行。
    // 这一步只在用户没设 OC_SERVE_AUTH_ENV / --auth-env 时生效。
    // 该文件承载 ACCOUNT_ID / ACCOUNT_KEY / REMOTE_PATH 账户登录信息。
    if cli.auth_env.is_none() && std::env::var("OC_SERVE_AUTH_ENV").is_err() {
        for legacy_filename in [
            ".env",
            ".oc-serve-auth.env",
        ] {
            let legacy_cwd_path = PathBuf::from(legacy_filename);
            if legacy_cwd_path.exists() && !unified_env_path.exists() {
                match std::fs::copy(&legacy_cwd_path, &unified_env_path) {
                    Ok(_) => {
                        let _ = std::fs::remove_file(&legacy_cwd_path);
                        tracing::info!(
                            "已将旧 env 从 cwd 迁移到 {}",
                            unified_env_path.display()
                        );
                    }
                    Err(e) => tracing::warn!("迁移 env 到新位置失败: {e}"),
                }
                break;
            }
        }
    }

    // v3 清理：env 文件仅保留 `# --- account login ---` 区块。
    if let Err(e) = mini_oc_gui_serve::account::prune_env_file_to_account_only(&unified_env_path)
    {
        tracing::warn!("清理 env 文件失败（继续运行）: {e}");
    }

    let _ = dotenvy::from_filename_override(&unified_env_path);

    // 5. 加载账户配置（ACCOUNT_ID / ACCOUNT_KEY / REMOTE_PATH / DEVICE_NAME）。
    let account_config = AccountConfig::load();

    // --no-tui 模式无交互终端，未配置账户时提前退出。
    if cli.no_tui && !account_config.is_configured() {
        anyhow::bail!(
            "未配置账户登录信息。\n\
             --no-tui 模式无法交互填写，请先运行 TUI 模式完成首次配置。"
        );
    }

    // 6. 同步拉取用户信息（账户已配置时）：
    //    端口（设备清单 port / oc-port）与 HTTP Basic 凭据都来自这次
    //    拉取的结果，axum 监听端口必须在其完成后才能确定，因此不能
    //    再放在后台任务里。拉取失败按业务规则降级：
    //    - 端口回退默认 9465 / 9464；
    //    - 设备绑定状态以本地 DEVICE_NAME 非空作乐观判定；
    //    - auth 回退进程 env（用户显式 export 时仍可用）。
    let account_config = Arc::new(std::sync::RwLock::new(account_config));

    // 设备选择触发槽：拉取成功且本地未绑定设备时写入，TUI 主循环
    // 每帧 render 轮询消费后弹出设备选择弹框。
    let device_picker_trigger: DevicePickerTriggerSlot =
        Arc::new(std::sync::Mutex::new(None));

    let mut runtime_ports = mini_oc_gui_serve::config::RuntimePorts::default();
    let mut fetched_info: Option<RemoteUserInfo> = None;

    let configured = account_config
        .read()
        .map(|a| a.is_configured())
        .unwrap_or(false);
    if configured {
        let account = account_config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let fetch_fut = fetch_user_info(&account.remote_path, &account.account_key);
        match tokio::time::timeout(std::time::Duration::from_secs(15), fetch_fut).await {
            Ok(Ok(info)) => {
                // 完整性验证 —— 不满足时仅记 warning 日志，不中断流程。
                if let Err(problems) = validate_user_info_integrity(&info) {
                    tracing::warn!(
                        "用户信息完整性问题（设备绑定/sb 配置可能异常）: {problems}"
                    );
                }

                // 端口：当前绑定设备的 port / oc-port。
                runtime_ports = mini_oc_gui_serve::config::ports_from_user_info(
                    &info,
                    &account.device_name,
                );

                // 本地未绑定设备（DEVICE_NAME 为空）且远端清单非空 →
                // 通知 TUI 弹出设备选择。
                if account.device_name.trim().is_empty() && !info.devices.is_empty() {
                    tracing::info!(
                        "本地未绑定设备，已请求 TUI 弹出设备选择（{} 个设备）",
                        info.devices.len()
                    );
                    *device_picker_trigger.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(DevicePickerTrigger {
                            user_info: info.clone(),
                            account_key: account.account_key.clone(),
                            remote_path: account.remote_path.clone(),
                            force: false,
                        });
                }
                fetched_info = Some(info);
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "获取用户信息失败（端口回退默认值，设备绑定按本地记录乐观判定）: {e}"
                );
                // fetch 失败的乐观降级：本地 DEVICE_NAME 非空即视为已绑定，
                // 允许启动 serve（端口用默认值）。
                runtime_ports.device_found = !account.device_name.trim().is_empty();
            }
            Err(_) => {
                tracing::warn!("获取用户信息超时（15s）——端口回退默认值，设备绑定按本地记录乐观判定");
                runtime_ports.device_found = !account.device_name.trim().is_empty();
            }
        }
    }
    let runtime_ports = Arc::new(std::sync::RwLock::new(runtime_ports));
    tracing::info!(
        "运行端口：系统={} opencode={} 绑定设备已确认={}",
        runtime_ports.read().map(|p| p.system_port).unwrap_or_default(),
        runtime_ports.read().map(|p| p.opencode_port).unwrap_or_default(),
        runtime_ports.read().map(|p| p.device_found).unwrap_or_default(),
    );

    // 6. 其余目录配置。系统监听端口（axum）已从设备清单解析进
    // `runtime_ports`，独立于 opencode 服务端口，避免两者同时监听
    // 同一端口导致「启动 serv」时报「端口被占用」。
    let system_port = runtime_ports
        .read()
        .map(|p| p.system_port)
        .unwrap_or(mini_oc_gui_serve::config::DEFAULT_SYSTEM_PORT);
    let default_dir = std::env::var("OC_DEFAULT_DIR").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|p| p.join(".config/opencode").to_string_lossy().into_owned())
            .unwrap_or_else(|| "/Users/samuel/.config/opencode".to_string())
    });
    // 解析 path-list 缓存路径:<exe_dir>/data/path-list.md (与 rathole bundle 一致的
    // exe-adjacent 布局)。`current_exe()` 不可用时回退到 CWD 相对路径 (仅 cargo test 场景)。
    let path_list_file = mini_oc_gui_serve::storage::default_path_list_path();

    // 一次性迁移:把旧位置 <workspace>/data/path-list.md 拷到新位置。
    // 满足以下全部条件才会执行:
    //   1. 新路径 <exe_dir>/data/path-list.md 当前不存在
    //   2. 旧路径 <exe_dir>/../../data/path-list.md 存在且是文件
    // 同步 std::fs (而非 tokio::fs) —— 启动早期 tokio 还未完全 spin up,且文件很小 (~8 KB)。
    if !path_list_file.exists() {
        if let Some(exe_dir) = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            let legacy = exe_dir
                .join("..")
                .join("..")
                .join("data")
                .join("path-list.md");
            if legacy.is_file() {
                if let Some(parent) = path_list_file.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        tracing::warn!("path-list migration: mkdir failed: {e}");
                    } else if let Err(e) = std::fs::copy(&legacy, &path_list_file) {
                        tracing::warn!("path-list migration: copy failed: {e}");
                    } else {
                        tracing::info!(
                            "path-list cache migrated from {} to {}",
                            legacy.display(),
                            path_list_file.display()
                        );
                    }
                }
            }
        }
    }

    // 7. Build storage.
    if let Some(parent) = path_list_file.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let cache = FileCache::new(&path_list_file);
    let store = PathListStore::new(cache);
    // 无论是否配置远端，都从本地 path-list.md 刷新一次缓存。
    if let Err(e) = store.refresh().await {
        tracing::warn!("initial refresh failed: {e}");
    }
    let store = Arc::new(store);

    // 账户已配置且启动时同步拉取成功：把 sb 配置接管远端 path-list 同步
    // （含一次性 legacy 迁移）放到后台 —— 网络往返不阻塞启动。
    // 拉取结果另 clone 一份供下方 auth 映射（spawn 会 move 原值）。
    let auth_info_for_mapping = fetched_info.clone();
    if let Some(info) = fetched_info {
        let device_name = account_config
            .read()
            .map(|a| a.device_name.clone())
            .unwrap_or_default();
        let store_for_user_info = store.clone();
        tokio::spawn(async move {
            // v2 构造携带 user_id + 路径设备段：设备段取**绑定设备条目在
            // 服务端记录的 `device-name`**（绑定时本机上报的 pcname 快照，
            // 服务端读取也用它组路径），未绑定/缺失时回退本地 pcname。
            // 注意不是 DEVICE_NAME（设备服务名）—— 那是绑定接口用的。
            let path_segment =
                mini_oc_gui_serve::account::remote_path_device_segment(&info, &device_name);
            tracing::info!("已获取用户信息: id={}, name={}", info.id, info.name);
            let remote = RemoteClient::from_user_info_v2(
                &info,
                path_segment,
                info.sb.password.clone(),
            );
            store_for_user_info.with_remote(remote).await;
            // One-shot legacy-path migration (runs only if remote is
            // configured). Idempotent: no-op on later restarts.
            if let Err(e) = store_for_user_info.migrate_from_legacy_remote().await {
                tracing::warn!("legacy migration failed: {e}");
            }
            // One-shot v3 suffix migration: move the extension-less
            // remote file (invisible to SilverBullet UI) to the
            // `.md` path. Per-identity guard; network failures retry.
            if let Err(e) = store_for_user_info.migrate_v3_suffix().await {
                tracing::warn!("v3 suffix migration failed: {e}");
            }
            if let Err(e) = store_for_user_info.refresh().await {
                tracing::warn!("remote refresh failed: {e}");
            }
        });
    }

    // 8. Auth：env 文件不再持久化凭据 —— 拉取成功时从用户信息映射
    //    （与设置面板同一映射：basic_user = user.name → user.id → 账户ID；
    //    basic_password = sb.password → 账户密钥）；否则回退进程 env。
    let mut auth = if let Some(path) = cli.auth_env.as_ref() {
        AuthConfig::from_env_with_file(path)
    } else {
        AuthConfig::from_env()
    }
    .context("auth init")?;
    if let Some(info) = auth_info_for_mapping.as_ref() {
        let account_id = account_config
            .read()
            .map(|a| a.account_id.clone())
            .unwrap_or_default();
        let basic_user = {
            let display = info.display_name();
            if !display.is_empty() {
                display
            } else if !info.id.trim().is_empty() {
                info.id.trim().to_string()
            } else {
                account_id
            }
        };
        let basic_password = if info.sb.password.is_empty() {
            account_config
                .read()
                .map(|a| a.account_key.clone())
                .unwrap_or_default()
        } else {
            info.sb.password.clone()
        };
        auth.basic_user = basic_user;
        auth.basic_password = basic_password;
    }
    // 运行时可变：首次配置填写后可立即热更新，无需重启。
    let auth = Arc::new(std::sync::RwLock::new(auth));

    // 9. Supervisor + state.
    let supervisor = ServeSupervisor::new();
    let state = AppState {
        store: store.clone(),
        auth: auth.clone(),
        default_dir: default_dir.clone(),
        supervisor: Arc::new(supervisor.clone()),
    };

    // 10. --no-tui 模式无交互终端，未配置凭据时提前安全退出，避免无认证监听。
    if cli.no_tui && !auth.read().map(|a| a.is_configured()).unwrap_or(false) {
        anyhow::bail!(
            "未配置认证凭据（OPENCODE_SERVER_USERNAME/PASSWORD）。\
             --no-tui 模式无法交互填写，请先运行 TUI 模式完成首次配置，或用 --generate-auth 生成凭据。"
        );
    }

    // 11. Optionally bind the HTTP listener.
    let server_handle = if !cli.no_http {
        let app = router(state);
        let listener = TcpListener::bind(format!("0.0.0.0:{system_port}"))
            .await
            .with_context(|| format!("bind 0.0.0.0:{system_port}"))?;
        tracing::info!("HTTP server listening on 0.0.0.0:{system_port}");
        Some(tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("axum server error: {e}");
            }
        }))
    } else {
        None
    };

    if cli.no_tui {
        tracing::info!(
            "running in --no-tui mode: HTTP server up at http://127.0.0.1:{system_port}/health, \
             Ctrl+C to stop"
        );
        // Park forever (until SIGINT) so axum keeps serving.
        let _ = tokio::signal::ctrl_c().await;
        if let Some(h) = server_handle {
            h.abort();
        }
        let _ = supervisor.shutdown().await;
        return Ok(());
    }

    // 12. Run TUI (blocks until user quits).
    // Header 用户名区：启动拉取的 `name` 字段（trim 后非空才算）。
    let startup_user_name = auth_info_for_mapping
        .as_ref()
        .map(|info| info.name.trim().to_string())
        .filter(|n| !n.is_empty());
    let terminal = ratatui::init();
    TuiApp::new(
        supervisor.clone(),
        auth.clone(),
        log_buffer.clone(),
        store.clone(),
        account_config.clone(),
        device_picker_trigger,
        runtime_ports,
        startup_user_name,
    )
    .run(terminal)
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    ratatui::restore();

    // 10. Tear down.
    let _ = supervisor.shutdown().await;
    if let Some(h) = server_handle {
        h.abort();
    }

    Ok(())
}

/// Generate a random 20-char HTTP Basic password and write it to
/// `.env` with mode 600.
fn generate_auth_and_exit(path: Option<&Path>) -> Result<()> {
    use rand::Rng;

    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".env"));
    let user = std::env::var("OPENCODE_SERVER_USERNAME").unwrap_or_else(|_| "opencode".to_string());
    let password: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();

    let body = format!(
        "# Generated by `mini-oc-gui-serve --generate-auth`\n\
         OPENCODE_SERVER_USERNAME={user}\n\
         OPENCODE_SERVER_PASSWORD={password}\n"
    );
    std::fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&target, perms).with_context(|| {
            format!("chmod 600 {}", target.display())
        })?;
    }

    println!("✓ wrote {} (user={user}, password={password})", target.display());
    Ok(())
}