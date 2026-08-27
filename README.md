# mini-oc-gui-serve

A Rust web + TUI application that replicates the functionality of `oc-serve-tui-actuator`
(an opencode serve launcher + project selector + path-list manager). Built with **Axum 0.7+**
for the HTTP layer and **ratatui** for the terminal UI.

## Features

- **`mini-oc-gui-serve` (binary)** — One-stop TUI launcher + web server:
  - 🚀 Start `opencode serve` (optionally behind `rathole` tunnel)
  - ⬆️ Upgrade opencode + oh-my-openagent (bun/npm)
  - 🔐 HTTP Basic auth + Cookie session support
  - 📡 Syncs `path-list.md` with a remote SilverBullet (or any HTTP file store)
- **`path-list-actor` (binary)** — CLI for managing the path-list index (`add` / `list` / `remove`)

## Architecture

```
src/
├── main.rs              # entrypoint: launch TUI + Axum concurrently
├── lib.rs               # crate root
├── bin/
│   └── path-list-actor.rs
├── domain/              # Project, Session, PathEntry, AppError
├── storage/             # path-list.md atomic R/W + SilverBullet sync
├── auth/                # HTTP Basic + Cookie session middleware
├── handlers/            # Axum handlers: /project, /session, /api/session
├── serve/               # OpenCode + Rathole process supervisor
├── upgrade/             # OpenCode + omo upgrade flow
└── ui/                  # ratatui TUI (replaces gum)

rathole/                  # Bundled rathole tunnel binary + configs
├── bin/
│   ├── macos/rathole     # macOS (aarch64-apple-darwin) binary
│   └── windows/rathole.exe # Windows binary (see rathole/bin/windows/README.md)
└── settings/*.toml       # tunnel configs (33-/40-/41- prefix = different remotes)
```

The bundled `rathole/` directory is resolved platform-aware at compile time
(`serve/rathole.rs`): a macOS build picks `bin/macos/rathole`, a Windows build
picks `bin/windows/rathole.exe`. Override either path via `RATHOLE_BIN` /
`RATHOLE_CONFIG`.

## Quickstart

```bash
# Build release binary
cargo build --release

# 1. (First-time only) Generate HTTP Basic auth + persist to .oc-serve-auth.env
./target/release/mini-oc-gui-serve --generate-auth

# 2a. Run the unified TUI (Axum + ratatui in one process) — needs a VT-capable terminal
./target/release/mini-oc-gui-serve

# 2b. Or run ONLY the HTTP server (no TUI) — works in any terminal
./target/release/mini-oc-gui-serve --no-tui

# Manage path-list directly
./target/release/path-list-actor add /abs/path/to/project
./target/release/path-list-actor list
./target/release/path-list-actor remove /abs/path/to/project

# Override config via env (env vars take precedence over .oc-serve-auth.env)
ATTACH_URL=http://remote:9464 ./target/release/mini-oc-gui-serve
OC_DEFAULT_DIR=/path/to/project ./target/release/mini-oc-gui-serve
DEFAULT_PORT=9464 ./target/release/mini-oc-gui-serve
```

## CLI flags

| Flag                     | Purpose                                                              |
| ------------------------ | -------------------------------------------------------------------- |
| `--no-tui`               | Skip the TUI; run only the HTTP server in the foreground.            |
| `--no-http`              | Skip binding the HTTP listener; TUI only.                            |
| `--generate-auth`        | Generate a random password, write `.oc-serve-auth.env`, then exit.    |
| `--auth-env <PATH>`      | Override the auth-env file (also `OC_SERVE_AUTH_ENV` env var).       |

## Auth credential resolution order

`OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD` are resolved in this order:

1. The current process environment (`OPENCODE_SERVER_USERNAME=foo ./mini-oc-gui-serve`).
2. The file at `--auth-env <PATH>`, or `$OC_SERVE_AUTH_ENV`, or `./.oc-serve-auth.env`.

If nothing is found, the program prints a clear error pointing to the expected file path.

## HTTP API

| Method | Path                       | Description                          | Auth |
|--------|----------------------------|--------------------------------------|------|
| GET    | `/health`                  | Liveness probe                       | No   |
| GET    | `/project`                 | List known projects from path-list   | Basic/Session |
| GET    | `/session?directory=...`   | List sessions for a project          | Basic/Session |
| POST   | `/api/session`             | Create a new session                 | Basic/Session |
| GET    | `/.fs/serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md` | SilverBullet-compatible file store | Cookie |

## Configuration

| Env Var                | Default                       | Description |
|------------------------|-------------------------------|-------------|
| `DEFAULT_PORT`         | `9464`                        | opencode serve port |
| `ATTACH_URL`           | `http://127.0.0.1:9464`       | URL used by `opencode attach` |
| `OC_DEFAULT_DIR`       | `$HOME/.config/opencode`      | Default fallback path |
| `OPENCODE_SERVER_USERNAME` | `opencode`                 | HTTP Basic username |
| `OPENCODE_SERVER_PASSWORD` | (auto-generated, in `.oc-serve-auth.env`) | HTTP Basic password |
| `SB_URL`               | `https://md.isoops.com`       | SilverBullet remote URL |
| `SB_USER` / `SB_PASSWORD` | —                          | SilverBullet credentials |
| `OC_CONFIG_DIR`        | `$HOME/.config/opencode`      | opencode config dir |
| `OC_CACHE_DIR`         | `$HOME/.cache/opencode`       | opencode cache dir |
| `RATHOLE_BIN`          | `rathole/bin/macos/rathole` (macOS) / `rathole/bin/windows/rathole.exe` (Windows) | rathole binary path (platform-aware) |
| `RATHOLE_CONFIG`       | `rathole/settings/global.toml` | rathole tunnel config（由设置面板生成） |
| `OC_OMO_SKIP_VERIFY`   | `0`                           | Skip omo upgrade verification |
| `RUST_LOG`             | `info`                        | tracing-subscriber filter |

## Design notes

- **Atomic writes**: `path-list.md` is always written via tempfile + `fs::rename` to avoid corruption.
- **Concurrent safety**: a `RwLock` guards the in-memory cache; a `fs2` flock guards the file.
- **Failure tolerance**: if SilverBullet is unreachable, we fall back to the local cache and warn.
- **Single source of truth**: `path-list.md` (local) ↔ remote PUT/GET at `/.fs/serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md`; merges by `path` key.
- **Legacy-path migration**: on first startup after upgrading, the client reads the pre-namespaced `/serv/opencode/path-list.md` once, merges those entries into the new layout (dedup by `path`, union sections, min/max timestamps), and seeds the new path if it is empty. Idempotent within a process lifetime — subsequent calls are no-ops. The legacy file on the server is left in place; operators may remove it manually.
- **Process cleanup**: spawned children get tracked PIDs; SIGINT/SIGTERM trigger a graceful kill chain.

## License

MIT
