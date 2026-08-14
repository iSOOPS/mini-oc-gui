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
    config::SbConfig,
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

    // 3. Resolve config from env + auth file.
    let port: u16 = std::env::var("DEFAULT_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9464);
    let default_dir = std::env::var("OC_DEFAULT_DIR").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|p| p.join(".config/opencode").to_string_lossy().into_owned())
            .unwrap_or_else(|| "/Users/samuel/.config/opencode".to_string())
    });
    let path_list_file = std::env::var("PATH_LIST_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("path-list.md"));

    // 4. Build storage.
    let cache = FileCache::new(&path_list_file);
    let store = PathListStore::new(cache);
    let sb_config = Arc::new(std::sync::RwLock::new(SbConfig::load()));
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
        let listener = TcpListener::bind(format!("0.0.0.0:{port}"))
            .await
            .with_context(|| format!("bind 0.0.0.0:{port}"))?;
        tracing::info!("HTTP server listening on 0.0.0.0:{port}");
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
            "running in --no-tui mode: HTTP server up at http://127.0.0.1:{port}/health, \
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