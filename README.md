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
- 🎨 App icon: 3-letter "MOT" mark, deep-navy background with indigo accent ring; auto-baked into Windows PE resources and shipped as macOS `.icns` / Linux `.png`. SVG source in `assets/icon.svg`.
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

### `opencode serve` child auth (account credentials)

`opencode serve` has **no CLI auth flag**; per the official docs
(<https://opencode.ai/docs/server/#authentication>) it enables HTTP Basic Auth
purely via the child-process env vars `OPENCODE_SERVER_USERNAME` /
`OPENCODE_SERVER_PASSWORD` (username defaults to `opencode`; **pure-numeric
account ids are valid** — the value is passed through without format
validation). When launching the standalone serve or the cloud service
(serve + rathole), mini-oc-gui injects the account id + account key from the
settings panel as these two env vars, so both local attach
(`OpencodeClient` / `opencode attach -u/-p`) and the cloud tunnel see the same
credentials. Stopping the cloud service tears down **both** rathole and
`opencode serve`.

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
| `DEFAULT_PORT`         | `9464`                        | (legacy, unused) |
| `ATTACH_URL`           | `http://127.0.0.1:<oc-port>`  | URL used by `opencode attach` |
| `OC_DEFAULT_DIR`       | `$HOME/.config/opencode`      | Default fallback path |
| `SB_URL`               | `https://md.isoops.com`       | (legacy fallback) SilverBullet remote URL |
| `OC_CONFIG_DIR`        | `$HOME/.config/opencode`      | opencode config dir |
| `OC_CACHE_DIR`         | `$HOME/.cache/opencode`       | opencode cache dir |
| `RATHOLE_BIN`          | `rathole/bin/<os>-<arch>/rathole[.exe]` | rathole binary path (platform-aware) |
| `RATHOLE_CONFIG`       | `rathole/settings/global.toml` | rathole tunnel config |
| `OC_OMO_SKIP_VERIFY`   | `0`                           | Skip omo upgrade verification |
| `RUST_LOG`             | `info`                        | tracing-subscriber filter |

### Ports & persisted data (v3)

Ports are **no longer configured locally** — the settings panel has no port
section. On startup (and after saving settings / rebinding a device) the app
calls `/api/user/info` and resolves the ports from the **currently bound
device** entry in the device list:

- `devices[].port` → system port (this app's axum listener; fixed at startup,
  changes prompt a restart)
- `devices[].oc-port` → `opencode serve` port (hot-applied: subsequent
  serve / cloud-service launches use the new value)

Fallbacks: if the user info cannot be fetched (offline / invalid key) the
defaults `9465` / `9464` apply and binding is assumed from the local
`DEVICE_NAME`; if the fetch succeeds but no device is bound, launching serve /
the cloud service is refused until a device is bound. A bound device missing
`oc-port` (old server data) falls back to `9464` with a warning.

The unified `.env` file persists **only** the `# --- account login ---`
section (`ACCOUNT_ID` / `ACCOUNT_KEY` / `REMOTE_PATH` / `DEVICE_NAME`).
Everything else (HTTP Basic credentials, ports, rathole keys, sb keys) is
built in memory from `/api/user/info` on every start; stale legacy lines are
pruned from the file at startup.

## Design notes

- **Atomic writes**: `path-list.md` is always written via tempfile + `fs::rename` to avoid corruption.
- **Concurrent safety**: a `RwLock` guards the in-memory cache; a `fs2` flock guards the file.
- **Failure tolerance**: if SilverBullet is unreachable, we fall back to the local cache and warn.
- **Single source of truth**: `path-list.md` (local) ↔ remote PUT/GET at `/.fs/serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md`; merges by `path` key.
- **Legacy-path migration**: on first startup after upgrading, the client reads the pre-namespaced `/serv/opencode/path-list.md` once, merges those entries into the new layout (dedup by `path`, union sections, min/max timestamps), and seeds the new path if it is empty. Idempotent within a process lifetime — subsequent calls are no-ops. The legacy file on the server is left in place; operators may remove it manually.
- **Local cache lives next to the binary**: `path-list.md` is written under `<exe_dir>/data/`, alongside the bundled rathole. Release and debug builds use separate directories; no CWD assumption.
- **Process cleanup**: spawned children get tracked PIDs; SIGINT/SIGTERM trigger a graceful kill chain.

## Application icon

The MOT mark is generated at build time from [`assets/icon.svg`](assets/icon.svg) into:
- `target/<profile>/assets/icon.png` — generic 512×512 PNG (Linux desktop, docs)
- `target/<profile>/assets/icon.ico` — Windows multi-resolution, embedded into `.exe` via `winresource`
- `target/<profile>/assets/icon.icns` — macOS multi-resolution

The icon bytes are additionally **embedded into the executable** (`include_bytes!`
via `$OUT_DIR`, see `src/icons.rs`): on every startup the binary re-extracts any
missing icon file to `<exe_dir>/assets/`. Shipping the bare executable alone is
therefore enough — the icon files reappear next to it on first run (existing
files are never overwritten, so a custom icon survives upgrades).

### macOS `.app` bundle (auto-generated)

`cargo build --release` also produces `target/release/MiniOC.app` — Finder /
Dock honor its icon out of the box. `Contents/MacOS/mini-oc-gui-serve` is a
relative symlink to the sibling artifact (cargo links the binary *after*
`build.rs` runs, so a real copy is impossible at that point); rebuilding the
executable automatically refreshes the bundle. To distribute the `.app`,
resolve the link into a real copy:

```sh
cp -L target/release/MiniOC.app/Contents/MacOS/mini-oc-gui-serve /tmp/exe-copy \
  && cp /tmp/exe-copy target/release/MiniOC.app/Contents/MacOS/mini-oc-gui-serve
# or archive preserving links: ditto -c -k --keepParent target/release/MiniOC.app MiniOC.zip
```

To change the icon, edit `assets/icon.svg` and rebuild — all platform artifacts
regenerate from this single source, and a content hash forces rustc to
re-embed the new bytes into the binary.

## License

MIT
