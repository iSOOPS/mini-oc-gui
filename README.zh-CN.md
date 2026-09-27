# mini-oc-gui-serve

[English](README.md) | [简体中文](README.zh-CN.md)

一个 Rust 编写的 Web + TUI 应用，复刻 `oc-serve-tui-actuator` 的功能
（opencode serve 启动器 + 项目选择器 + path-list 管理器）。HTTP 层基于
**Axum 0.7+**，终端 UI 基于 **ratatui**。

## 功能特性

- **`mini-oc-gui-serve`（二进制）** — 一站式 TUI 启动器 + Web 服务器：
  - 🚀 启动 `opencode serve`（可选经 `rathole` 隧道暴露）
  - ⬆️ 升级 opencode 与 oh-my-openagent（bun/npm）
  - 🔐 支持 HTTP Basic 认证 + Cookie 会话
  - 📡 与远程 SilverBullet（或任意 HTTP 文件存储）同步 `path-list.md`
- 🎨 应用图标："MOT" 字标 — 石板灰圆角底板 + 绿色粗体字标。构建时自动烧入
  Windows PE 资源，并以 macOS `.icns` / Linux `.png` 形式产出。SVG 源文件
  在 `assets/icon.svg`。
- **`path-list-actor`（二进制）** — 管理 path-list 索引的 CLI（`add` / `list` / `remove`）

## 架构

```
src/
├── main.rs              # 入口：并发启动 TUI 与 Axum
├── lib.rs               # crate 根
├── bin/
│   └── path-list-actor.rs
├── domain/              # Project、Session、PathEntry、AppError
├── storage/             # path-list.md 原子读写 + SilverBullet 同步
├── auth/                # HTTP Basic + Cookie 会话中间件
├── handlers/            # Axum handlers：/project、/session、/api/session
├── serve/               # OpenCode + Rathole 进程守护
├── upgrade/             # OpenCode + omo 升级流程
└── ui/                  # ratatui TUI（替代 gum）

rathole/                  # 内置 rathole 隧道二进制 + 配置
├── bin/
│   ├── macos-aarch64/rathole    # macOS（aarch64-apple-darwin）二进制
│   └── windows-x86_64/rathole.exe # Windows x86_64 二进制
└── settings/global.toml # 隧道配置（由设置面板生成；
                         # 已 gitignore — 含各部署专属的端点信息）
```

内置的 `rathole/` 目录在编译期按平台解析（`serve/rathole.rs`）：macOS 构建选取
`bin/macos-aarch64/rathole`，Windows 构建选取 `bin/windows-x86_64/rathole.exe`。
可通过 `RATHOLE_BIN` / `RATHOLE_CONFIG` 覆盖路径。内置二进制来自开源项目
[rathole](https://github.com/rapiz1/rathole)，随时可替换为你自己的构建。

## 快速开始

```bash
# 构建 release 二进制
cargo build --release

# 1.（仅首次）生成 HTTP Basic 凭据并写入 .oc-serve-auth.env
./target/release/mini-oc-gui-serve --generate-auth

# 2a. 运行一体化 TUI（Axum + ratatui 同进程）— 需要支持 VT 的终端
./target/release/mini-oc-gui-serve

# 2b. 或只运行 HTTP 服务器（无 TUI）— 任意终端可用
./target/release/mini-oc-gui-serve --no-tui

# 直接管理 path-list
./target/release/path-list-actor add /abs/path/to/project
./target/release/path-list-actor list
./target/release/path-list-actor remove /abs/path/to/project

# 通过环境变量覆盖配置（env 优先级高于 .oc-serve-auth.env）
ATTACH_URL=http://<remote-host>:<oc-port> ./target/release/mini-oc-gui-serve
OC_DEFAULT_DIR=/path/to/project ./target/release/mini-oc-gui-serve
```

> **自部署说明：** 账户中心 API 端点（`REMOTE_PATH`，默认 `api.example.com`）与
> SilverBullet 基础 URL 均为占位符 — 使用前请在设置面板或 `.env` 中指向你自己的
> 部署。

## CLI 参数

| 参数                     | 用途                                                                 |
| ------------------------ | -------------------------------------------------------------------- |
| `--no-tui`               | 跳过 TUI；仅在前台运行 HTTP 服务器。                                  |
| `--no-http`              | 跳过 HTTP 监听；仅运行 TUI。                                          |
| `--generate-auth`        | 生成随机密码，写入 `.oc-serve-auth.env` 后退出。                       |
| `--auth-env <PATH>`      | 覆盖凭据 env 文件路径（也可用 `OC_SERVE_AUTH_ENV` 环境变量）。         |

## 认证凭据解析顺序

`OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD` 按以下顺序解析：

1. 当前进程环境变量（`OPENCODE_SERVER_USERNAME=foo ./mini-oc-gui-serve`）。
2. `--auth-env <PATH>` 指定的文件，或 `$OC_SERVE_AUTH_ENV`，或 `./.oc-serve-auth.env`。

都找不到时，程序会打印明确报错并指出期望的文件路径。

### `opencode serve` 子进程认证（账户凭据）

`opencode serve` **没有 CLI 认证参数**；按官方文档
（<https://opencode.ai/docs/server/#authentication>），它仅通过子进程环境变量
`OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD` 启用 HTTP Basic 认证
（用户名默认为 `opencode`；**纯数字账户 id 合法** — 原样透传，不做格式校验）。
启动独立 serve 或云服务（serve + rathole）时，mini-oc-gui 会把设置面板中的
账户 id + 账户密钥注入为这两个环境变量，使本地 attach
（`OpencodeClient` / `opencode attach -u/-p`）与云端隧道使用同一套凭据。
停止云服务会**同时**拉起 rathole 与 `opencode serve` 的清理。

## HTTP API

| 方法  | 路径                       | 描述                             | 认证 |
|-------|----------------------------|----------------------------------|------|
| GET   | `/health`                  | 存活探测                         | 无   |
| GET   | `/project`                 | 列出 path-list 中的已知项目       | Basic/Session |
| GET   | `/session?directory=...`   | 列出某项目的会话                  | Basic/Session |
| POST  | `/api/session`             | 创建新会话                       | Basic/Session |
| GET   | `/.fs/serv/opencode/{sb_user}/{pctype}/{pcname}/path-list.md` | SilverBullet 兼容文件存储 | Cookie |

## 配置

| 环境变量               | 默认值                         | 说明 |
|------------------------|-------------------------------|-------------|
| `DEFAULT_PORT`         | `9464`                        | （遗留，未使用） |
| `ATTACH_URL`           | `http://127.0.0.1:<oc-port>`  | `opencode attach` 使用的 URL |
| `OC_DEFAULT_DIR`       | `$HOME/.config/opencode`      | 默认回退路径 |
| `SB_URL`               | *（来自账户服务器）*           | （遗留回退）SilverBullet 远程 URL |
| `OC_CONFIG_DIR`        | `$HOME/.config/opencode`      | opencode 配置目录 |
| `OC_CACHE_DIR`         | `$HOME/.cache/opencode`       | opencode 缓存目录 |
| `RATHOLE_BIN`          | `rathole/bin/<os>-<arch>/rathole[.exe]` | rathole 二进制路径（按平台解析） |
| `RATHOLE_CONFIG`       | `rathole/settings/global.toml` | rathole 隧道配置 |
| `OC_OMO_SKIP_VERIFY`   | `0`                           | 跳过 omo 升级校验 |
| `RUST_LOG`             | `info`                        | tracing-subscriber 过滤器 |

### 端口与持久化数据（v3）

端口**不再在本地配置** — 设置面板没有端口区块。启动时（以及保存设置 / 重绑设备后）
应用会调用 `/api/user/info`，从设备列表中**当前绑定设备**条目解析端口：

- `devices[].port` → 系统端口（本应用的 axum 监听端口；启动时固定，
  变更会提示重启）
- `devices[].oc-port` → `opencode serve` 端口（热生效：后续 serve / 云服务启动
  使用新值）

回退策略：无法获取用户信息（离线 / 密钥无效）时使用内置默认值，并假定本地
`DEVICE_NAME` 已绑定；获取成功但无绑定设备时，拒绝启动 serve / 云服务直至绑定
设备。绑定设备缺少 `oc-port`（旧服务端数据）时回退内置默认值并告警。

统一 `.env` 文件**只**持久化 `# --- account login ---` 区段
（`ACCOUNT_ID` / `ACCOUNT_KEY` / `REMOTE_PATH` / `DEVICE_NAME`）。其余一切
（HTTP Basic 凭据、端口、rathole 密钥、sb 密钥）均在每次启动时从
`/api/user/info` 在内存中构建；文件中的陈旧遗留行会在启动时修剪掉。

## 设计指南

从设计文档中提炼的核心原则。扩展本代码库时请遵循。

### 1. 单一事实源

- `path-list.md`（本地）↔ 远程 PUT/GET 是项目索引的唯一权威；按 `path` 键合并。
- `assets/icon.svg` 是唯一的图标源；所有平台产物
  （`.png` / `.ico` / `.icns`、PE 资源、运行时自提取）都在构建期由它派生。
- env 文件位置只经**一个**函数解析：`config::unified_env_path()`
  （`OC_SERVE_AUTH_ENV` → `exe_dir/.env` → `cwd/.env`）。`main.rs` 的 dotenvy
  引导与 `AccountConfig::load()` 必须共用该链 — 此前一个 bug 是 `load()` 用了
  自己的仅 `cwd` 回退，导致写入 `exe_dir/.env` 的账户设置静默加载不到。

### 2. 原子写与并发安全

- `path-list.md` 始终经 tempfile + `fs::rename` 写入，避免文件损坏。
- `RwLock` 保护内存缓存；`fs2` flock 保护文件。
- 子进程按 PID 跟踪；SIGINT/SIGTERM 触发优雅 kill 链。

### 3. 错误必须可见 — 绝不静默成功

- 远程存储不可达时，降级到本地缓存并**告警**；绝不让 UI 阻塞在远程 IO 上。
- 未配置远程客户端的阻塞式推送返回 `Err` 而非默认 `Ok`。fire-and-forget 推送
  经重试通道上报失败。
- 每次用户触发的同步都向状态行写入明确的成功**或**失败消息。静默成功会让
  用户信任从未发生的同步。

### 4. 在每一层门控 UI 状态（纵深防御）

前置条件不满足的交互行（如未配置账户时的"绑定设备"）在**三层**被禁用：

1. 渲染层 — 锁定样式：灰色、无下划线、带说明后缀文本。
2. 交互层 — 不注册 click region，该行不可命中。
3. Handler 层 — 点击处理器复核前置条件，作为最后一道防线。

新增门控动作必须三层齐备；"有锁定样式但仍可点击"的行视为 bug。

### 5. 显式预算 ratatui 盒高度

组件总高 = `2 × 边框 + 2 × 内边距 + 内容行数`。padding 是预算的一部分而非
免费空间：5 行卡片里放 `Padding::uniform(5)` 会使内容区为负，ratatui 把一切
裁掉。内容需要 N 个 `Line` 时，从预算推导卡片高度 — 绝不反向。用
`TestBackend` 绘制并断言内容出现在 buffer 中做验证。

### 6. 纯 Rust 工具链约束（无 C 编译器）

构建必须在不含 binutils / `dlltool` / `cc` 的最小 rustup GNU 工具链上通过。
体现在 `Cargo.toml` 的后果：

- `reqwest` 用 `native-tls`（系统 SChannel / SecureTransport），不用 `rustls`
  （其 `ring` 后端要编译 C 与汇编）。
- 不用 `jsonwebtoken`：基于 `base64` + `serde_json` 手写最小 JWT `exp` 校验器；
  签名校验委托给上游服务器。
- `getrandom` 钉在纯 Rust 回退；`schannel` 钉在 `0.1.27`
  （更新版本在 windows-gnu 上需要 `dlltool`）。
- 剪贴板用 `arboard`（经 `windows-sys` 走 win32 API）。

新增依赖时，检查其在任何 tier-1 target 上都不会拖入 C 构建步骤。

### 7. 图标管线不变量

- `assets/icon.svg` 是唯一源；渲染缩放基准取自 SVG 自身逻辑尺寸
  （`tree.size()`），绝不硬编码常数。硬编码基准（1024）曾在 viewBox 改动
  （32）后静默毁掉所有生成的图标 — 图标缩到画布左上角 1/N，而空 pixmap
  守卫依然放行，因为像素并非*空的*，只是极小。
- `<text>` 元素依赖系统字体：build.rs 必须在解析前调用
  `fontdb_mut().load_system_fonts()` — 空 fontdb 会让 usvg 静默丢弃所有文本。
  宿主机缺失的字体（如 Orbitron）回退到系统 sans-serif，因此文本字标在不同
  机器上的字形度量可能略有差异；对像素敏感的标记优先用几何图形。
- 图标字节额外内嵌进可执行文件（经 `$OUT_DIR` 的 `include_bytes!`，见
  `src/icons.rs`）：每次启动把缺失的图标文件重新释放到 `<exe_dir>/assets/`。
  因此只分发裸可执行文件就够了；已有文件永不覆盖，自定义图标可跨升级存活。

### 8. 优雅的遗留迁移

- 遗留路径迁移只读取一次前命名空间时期的远程文件，合并进新布局
  （按 `path` 去重、sections 取并集、时间戳取 min/max），新路径为空时播种 —
  进程生命周期内幂等。遗留文件留给运维手动清理。
- `.env` 中的陈旧遗留行在启动时修剪，而不是报错退出。

## 应用图标

MOT 标记在构建期由 [`assets/icon.svg`](assets/icon.svg) 生成：
- `target/<profile>/assets/icon.png` — 通用 512×512 PNG（Linux 桌面、文档）
- `target/<profile>/assets/icon.ico` — Windows 多分辨率，经 winresource 嵌入 `.exe`
- `target/<profile>/assets/icon.icns` — macOS 多分辨率

### macOS `.app` 包（自动生成）

`cargo build --release` 还会产出 `target/release/MiniOC.app` — Finder / Dock
开箱即识别其图标。`Contents/MacOS/mini-oc-gui-serve` 是指向同级产物的相对
符号链接（cargo 在 `build.rs` 运行*之后*才链接出二进制，届时无法真拷贝）；
重建可执行文件会自动刷新该包。分发 `.app` 时请把链接解引用为真实拷贝：

```sh
cp -L target/release/MiniOC.app/Contents/MacOS/mini-oc-gui-serve /tmp/exe-copy \
  && cp /tmp/exe-copy target/release/MiniOC.app/Contents/MacOS/mini-oc-gui-serve
# 或归档时保留链接： ditto -c -k --keepParent target/release/MiniOC.app MiniOC.zip
```

更换图标只需编辑 `assets/icon.svg` 并重新构建 — 所有平台产物从这个单一源
重新生成，内容哈希会强制 rustc 把新字节重新嵌入二进制。

## 许可证

MIT
