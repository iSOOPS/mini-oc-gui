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
    auth::AuthConfig,
    config::{RatholeConfig, SbConfig},
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

    /// Generate a random HTTP Basic password, write it to
    /// `.oc-serve-auth.env` (chmod 600 on Unix) and exit.
    #[arg(long)]
    generate_auth: bool,

    /// Override the auth-env file location (defaults to
    /// `$OC_SERVE_AUTH_ENV` or `./.oc-serve-auth.env`).
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

    // 2. Optional: generate-and-exit.
    if cli.generate_auth {
        return generate_auth_and_exit(cli.auth_env.as_deref());
    }

    // 3. 加载统一 env 文件,把 OC_SERVE_SYSTEM_PORT / OC_SERVE_OPENCODE_PORT 等 key
    // 注入到进程环境（仅在进程 env 尚未设置时生效,from_filename_override
    // 不会覆盖进程已有值）。
    let unified_env_path = cli
        .auth_env
        .clone()
        .map(PathBuf::from)
        .or_else(|| Some(mini_oc_gui_serve::config::unified_env_path()))
        .expect("unified_env_path always returns Some");

    // 一次性迁移 1:旧版独立的 .oc-serve-sb.env / .oc-serve-rathole.env 文件
    // 合并进统一 env 后删除。完成后 Ok(false) 不再处理。
    match mini_oc_gui_serve::config::migrate_legacy_env(&unified_env_path) {
        Ok(true) => tracing::info!("已将旧 SB / Rathole env 合并到 {}", unified_env_path.display()),
        Ok(false) => {}
        Err(e) => tracing::warn!("迁移旧 env 文件失败: {e}"),
    }

    // 一次性迁移 2:cwd 下的旧 .oc-serve-auth.env → 新位置(可执行文件同目录)
    // 旧位置(通常 ./)有文件 + 新位置没有 → 复制内容过去 + 删除旧文件。
    // 这一步只在用户没设 OC_SERVE_AUTH_ENV / --auth-env 时生效。
    if cli.auth_env.is_none() && std::env::var("OC_SERVE_AUTH_ENV").is_err() {
        let legacy_cwd_path = PathBuf::from(mini_oc_gui_serve::config::UNIFIED_ENV_FILE);
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
        }
    }

    let _ = dotenvy::from_filename_override(&unified_env_path);

    // 4. Resolve config from env + auth file.
    // 系统监听端口(axum path-list 管理接口)。独立于 opencode 服务端口
    // `OC_SERVE_OPENCODE_PORT`(默认 9464),避免两者同时监听同一端口,
    // 导致「启动 serv」时报「端口被占用」。
    let ports = mini_oc_gui_serve::config::PortsConfig::load();
    let system_port = ports.system_port;
    let default_dir = std::env::var("OC_DEFAULT_DIR").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|p| p.join(".config/opencode").to_string_lossy().into_owned())
            .unwrap_or_else(|| "/Users/samuel/.config/opencode".to_string())
    });
    let path_list_file = std::env::var("PATH_LIST_FILE")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // 默认 <exe_dir>/target/data/path-list.md — 不用 CWD 相对路径,
            // 避免 `cargo run` 在不同目录读到不同数据
            let exe_dir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()));
            match exe_dir {
                Some(dir) => dir.join("target").join("data").join("path-list.md"),
                None => PathBuf::from("target/data/path-list.md"),
            }
        });

    // 4. Build storage.
    if let Some(parent) = path_list_file.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let cache = FileCache::new(&path_list_file);
    let store = PathListStore::new(cache);
    let sb_config = Arc::new(std::sync::RwLock::new(SbConfig::load()));
    let rathole_config = Arc::new(std::sync::RwLock::new(RatholeConfig::load()));
    if let Ok(cfg) = sb_config.read() {
        if cfg.is_configured() {
            let remote = RemoteClient::with_credentials(cfg.url.clone(), cfg.user.clone(), cfg.password.clone());
            store.with_remote(remote).await;
        }
    }
    // 无论是否配置远端，都从本地 path-list.md 刷新一次缓存。
    if let Err(e) = store.refresh().await {
        tracing::warn!("initial refresh failed: {e}");
    }
    let store = Arc::new(store);

    // 5. Auth.
    let auth = if let Some(path) = cli.auth_env.as_ref() {
        AuthConfig::from_env_with_file(path)
    } else {
        AuthConfig::from_env()
    }
    .context("auth init")?;
    // 运行时可变：首次配置填写后可立即热更新，无需重启。
    let auth = Arc::new(std::sync::RwLock::new(auth));

    // 6. Supervisor + state.
    let supervisor = ServeSupervisor::new();
    let state = AppState {
        store: store.clone(),
        auth: auth.clone(),
        default_dir: default_dir.clone(),
    };

    // 7. --no-tui 模式无交互终端，未配置凭据时提前安全退出，避免无认证监听。
    if cli.no_tui && !auth.read().map(|a| a.is_configured()).unwrap_or(false) {
        anyhow::bail!(
            "未配置认证凭据（OPENCODE_SERVER_USERNAME/PASSWORD）。\
             --no-tui 模式无法交互填写，请先运行 TUI 模式完成首次配置，或用 --generate-auth 生成凭据。"
        );
    }

    // 8. Optionally bind the HTTP listener.
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

    // 9. Run TUI (blocks until user quits).
    let terminal = ratatui::init();
    TuiApp::new(
        supervisor.clone(),
        auth.clone(),
        log_buffer.clone(),
        store.clone(),
        sb_config.clone(),
        rathole_config.clone(),
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
/// `.oc-serve-auth.env` with mode 600.
fn generate_auth_and_exit(path: Option<&Path>) -> Result<()> {
    use rand::Rng;

    let target = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".oc-serve-auth.env"));
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