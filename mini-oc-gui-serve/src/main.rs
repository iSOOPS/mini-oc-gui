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
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

use mini_oc_gui_serve::{
    auth::AuthConfig,
    error::AppError,
    handlers::{AppState, router},
    serve::ServeSupervisor,
    storage::{PathListStore, cache::FileCache, remote::RemoteClient},
    ui::{TuiApp, TuiLogSink, TuiLogSinkFactory},
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

    // 1. Tracing — sink is shared with the TUI so background tasks (port
    //    check, launch, upgrades) don't write to stderr and corrupt the
    //    alternate-screen framebuffer. The same factory is reused for
    //    both the TUI and the `--no-tui` HTTP-only path.
    let log_sink = TuiLogSink::new();
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(TuiLogSinkFactory::new(log_sink.clone()))
        .init();

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
    if let (Ok(sb_url), Ok(sb_user), Ok(sb_password)) = (
        std::env::var("SB_URL"),
        std::env::var("SB_USER"),
        std::env::var("SB_PASSWORD"),
    ) {
        let remote = RemoteClient::with_credentials(sb_url, sb_user, sb_password);
        store.with_remote(remote).await;
        if let Err(e) = store.refresh().await {
            tracing::warn!("initial refresh failed: {e}");
        }
    }
    let store = Arc::new(store);

    // 5. Auth. Resolution order: env var → cli flag → exe-adjacent file
    //    → cwd-relative file. If the user did not pass `--auth-env` and
    //    no env var is set, we bootstrap a new auth file next to the
    //    running .exe so the program "just works" when launched by
    //    double-click (where the cwd is `target\release` or the install
    //    dir, not the project root). The bootstrap path is logged so the
    //    user can find it later.
    let auth = if cli.auth_env.is_some() || std::env::var("OC_SERVE_AUTH_ENV").is_ok() {
        if let Some(path) = cli.auth_env.as_ref() {
            AuthConfig::from_env_with_file(path)
        } else {
            AuthConfig::from_env()
        }
    } else {
        match AuthConfig::from_env() {
            Ok(cfg) => Ok(cfg),
            Err(_) => {
                // No env, no file: bootstrap a fresh `.oc-serve-auth.env`
                // next to the running binary.
                let target = std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".oc-serve-auth.env");
                generate_auth_to(&target)?;
                AuthConfig::from_env_with_file(&target)
            }
        }
    }
    .context("auth init")?;

    // 6. Supervisor + state.
    let supervisor = ServeSupervisor::new();
    let state = AppState {
        store: store.clone(),
        auth: auth.clone(),
        default_dir: default_dir.clone(),
    };

    // 7. Optionally bind the HTTP listener.
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

    // 8. Run TUI (blocks until user quits).
    let terminal = ratatui::init();
    // Enable mouse capture so list items can be clicked. `ratatui::init`
    // does not enable it by default; we restore it manually before
    // `ratatui::restore()` so the terminal returns to a sane state.
    let mut stdout = std::io::stdout();
    if let Err(e) = execute!(stdout, EnableMouseCapture) {
        tracing::warn!("failed to enable mouse capture: {e}");
    }
    let tui_app = TuiApp::with_log_sink(supervisor.clone(), log_sink.clone());
    let tui_result = tui_app.run(terminal).await;
    if let Err(e) = execute!(stdout, DisableMouseCapture) {
        tracing::warn!("failed to disable mouse capture: {e}");
    }
    ratatui::restore();

    // 9. Tear down.
    let _ = supervisor.shutdown().await;
    if let Some(h) = server_handle {
        h.abort();
    }

    tui_result.map_err(|e| AppError::Internal(e.to_string()).into())
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

/// Generate a random auth env file at `target` without printing to stdout
/// (suitable for the silent bootstrap path on first run).
fn generate_auth_to(target: &Path) -> Result<()> {
    use rand::Rng;

    let user = std::env::var("OPENCODE_SERVER_USERNAME").unwrap_or_else(|_| "opencode".to_string());
    let password: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(20)
        .map(char::from)
        .collect();

    let body = format!(
        "# Auto-generated by `mini-oc-gui-serve` on first run.\n\
         # Delete this file (or set $OPENCODE_SERVER_PASSWORD) to force\n\
         # a fresh password on the next launch.\n\
         OPENCODE_SERVER_USERNAME={user}\n\
         OPENCODE_SERVER_PASSWORD={password}\n"
    );
    std::fs::write(target, body).with_context(|| format!("write {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(target, perms).with_context(|| {
            format!("chmod 600 {}", target.display())
        })?;
    }
    tracing::info!(
        "auto-generated {} (user={user}); delete it to rotate credentials",
        target.display()
    );
    Ok(())
}