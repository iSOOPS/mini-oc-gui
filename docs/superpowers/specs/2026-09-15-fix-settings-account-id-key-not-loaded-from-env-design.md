# 修复设置面板账户 ID / 密钥无法从 `.env` 读取

## 摘要

mini-oc-gui-serve 启动时 `.env` 文件已写入 `ACCOUNT_ID` / `ACCOUNT_KEY`，但 TUI 设置面板打开后"账户 ID"与"密钥"两行均显示为空。根因是 `AccountConfig::load()` 内部的 `.env` 路径解析与 main.rs 的 dotenvy 路径解析**默认位置不一致**：dotenvy 走 `exe_dir/.env`，`AccountConfig::load()` 只走 `cwd/.env`。

## 背景

### 当前行为（bug）

- 用户启动 `./mini-oc-gui-serve`
- main.rs 通过 `dotenvy::from_filename_override(&unified_env_path)` 注入 `.env` 到进程环境
- main.rs 调用 `AccountConfig::load()` 期望账户 ID/密钥已加载
- TuiApp 构造完成后，用户从主菜单进入"设置"面板
- 面板里"账户 ID"显示为空、"密钥"显示为空（首启未配置时是预期；已配置但有 `.env` 时是 bug）

### 期望行为

`.env`（exe_dir 或 cwd 或显式指定）中已有的 `ACCOUNT_ID` / `ACCOUNT_KEY` 在设置面板打开时**直接显示**：
- 账户 ID 显示明文
- 密钥以 `*` 掩码形式显示，长度 = 已保存密钥字符数（已有逻辑）

### 已排除

- 渲染层（`build_settings_lines` / `render_account_key_line`）正确：直接读 `self.account_id_input` / `self.account_key_input`。
- `open_settings()` 回填逻辑正确：从 `self.account_config.read()` 克隆到 buffer。
- `submit_settings` 写回逻辑不影响首次加载。
- bug 在**加载阶段**：进程环境里的 `ACCOUNT_ID` / `ACCOUNT_KEY` 为空字符串。

## 根因

路径解析在两个位置各写一份，行为分裂：

| 阶段 | 代码 | 路径优先级 |
| --- | --- | --- |
| ① dotenvy 注入（main.rs:125） | `dotenvy::from_filename_override(&unified_env_path)` | `OC_SERVE_AUTH_ENV` → **exe_dir/.env** → cwd/.env |
| ② `AccountConfig::load()` 兜底（account.rs:185-188） | `Self::read_env_file(&path)` | `OC_SERVE_AUTH_ENV` → **cwd/.env**（无 exe_dir 中间级） |

`config::unified_env_path()`（config.rs:123）已是单一来源的实现，但 `AccountConfig::load()` 没有用它。

当 `.env` 仅存在于 exe_dir（程序标准安装位置），② 兜底完全找不到文件 → `account_id` / `account_key` 字段保持空字符串。

## 方案

### 选型

**采用方案 A：让 `AccountConfig::load()` 与 dotenvy 用同一个路径解析。**

- 改动最小（只改 `account.rs` 内 fallback 路径解析）
- `config::unified_env_path()` 已存在并经 dotenvy 验证，零新逻辑
- `AccountConfig` 保留独立可测性（不依赖 main.rs 提前注入）
- 副作用：无

**否决方案 B（删除 fallback 让 dotenvy 单一负责）：**
- 现有 `AccountConfig::load()` 是 `pub fn`，被多处直接调用（测试、单 no-tui 路径等）
- 一旦删除 fallback，这些调用方都依赖 main.rs 已注入；违反"调用方只读所需来源"原则
- 改动面更大，风险更高

## 设计

### 1. Architecture

```
   ┌─────────────────────────────────┐
   │  config::unified_env_path()     │  ← 单一路径解析来源（已存在）
   │  优先级:                        │
   │    1. OC_SERVE_AUTH_ENV         │
   │    2. <exe_dir>/.env            │
   │    3. <cwd>/.env                │
   └──────────────┬──────────────────┘
                  │ 被以下两处共同使用
       ┌──────────┴──────────┐
       ▼                     ▼
  main.rs:125            account.rs:185
  dotenvy 注入           AccountConfig::load()
       │                     │
       ▼                     ▼
  进程 env vars         read_env_file 回填
                  │
                  ▼
         TuiApp.account_config
                  │
                  ▼
         open_settings() 回填 buffer
                  │
                  ▼
         设置面板显示 ✓
```

### 2. Components

#### `config::unified_env_path()`（**不动**，已存在）

config.rs:123 — 已实现正确的三段优先级。保持原样。

#### `account::AccountConfig::load()`（**改一处**）

account.rs:184-202 — 把 fallback 路径解析从内联逻辑替换为调用 `unified_env_path()`：

```rust
// 旧(仅替换 if 块内 path 解析表达式,is_configured() 守卫保留):
if !cfg.is_configured() {
    let path = std::env::var("OC_SERVE_AUTH_ENV")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".env"));
    let from_file = Self::read_env_file(&path);
    // (后续字段回填逻辑保持不变)
}

// 新(只换 if 块内 path 的解析来源,字段回填逻辑不动):
if !cfg.is_configured() {
    let path = crate::config::unified_env_path();
    let from_file = Self::read_env_file(&path);
    // (后续字段回填逻辑保持不变)
}
```

**为什么不是无条件调用 read_env_file:** 保留 `is_configured()` 守卫以兼容"用户已通过进程 env 注入全部 4 个字段（包括 `DEVICE_NAME`）"的场景——避免用陈旧 .env 覆盖更新的进程 env。这与现有 README "env vars take precedence over .oc-serve-auth.env" 语义一致。

**额外约束：** 现有 `is_configured()` 检查的 4 字段是 `account_id` / `account_key` / `remote_path`（**不含** `device_name`）。DEVICE_NAME 走单独回填逻辑（account.rs:199-201）。这保持不变。

#### `auth::AuthConfig::from_env` / `from_env_with_file`（**不动**）

auth/mod.rs:51-109 — main.rs 调用 `from_env_with_file(&cli.auth_env.as_ref().unwrap_or(...))` 显式传路径。main.rs 已在用 `unified_env_path()` 作为 fallback，行为正确。无需改动。

#### `main.rs`（**不动**）

main.rs:90-94 已经在用 `unified_env_path()`。无需改动。

### 3. Data Flow

启动序列（修复后）：

1. `main.rs:125` 调用 `dotenvy::from_filename_override(&unified_env_path)`，把 `.env` 内容注入进程环境
   - **场景 A**：`.env` 在 exe_dir → dotenvy 成功注入 `ACCOUNT_ID=xxx ACCOUNT_KEY=xxx`
   - **场景 B**：`.env` 在 cwd（开发模式）→ dotenvy 成功注入
   - **场景 C**：`.env` 不存在 → dotenvy 静默失败，进程环境全空
2. `main.rs:128` 调用 `AccountConfig::load()`
   - 场景 A/B：dotenvy 注入后进程环境三字段（`ACCOUNT_ID`/`ACCOUNT_KEY`/`REMOTE_PATH`）均非空 → `cfg.is_configured()=true` → 跳过 if 块，不进 fallback（**保留进程 env 优先级，与"env vars take precedence over .env"语义一致**）
   - 场景 C：进程 env 全空 → `is_configured()=false` → 调用 `unified_env_path()` → 调用 `read_env_file(&path)` → 读不到 → 返回空 cfg（**首启合理状态**）
3. main.rs 把 `AccountConfig` 装进 `Arc<RwLock>`，传给 `TuiApp::new()`
4. 用户从主菜单进入"设置"面板，触发 `open_settings()`
5. `open_settings()` 从 `self.account_config.read()` 克隆到 `account_id_input` / `account_key_input`
6. `build_settings_lines()` 渲染：
   - 账户 ID 行：`format!("  账户ID: {}", self.account_id_input)`（明文）
   - 密钥行：`render_account_key_line(&self.account_key_input)`（掩码）

### 4. Error Handling

| 场景 | 行为 |
| --- | --- |
| `.env` 不存在 | `AccountConfig::default()` + `remote_path` 填默认值；TUI 面板显示空，进入"首启填写"流程（既有行为） |
| `.env` 存在但缺 `ACCOUNT_ID` | `account_id` 为空，**显示空**；`submit_settings` 校验时报"❌ 必须填写 账户ID"（既有行为） |
| `.env` 存在但缺 `ACCOUNT_KEY` | 同上，校验时报"❌ 必须填写 密钥" |
| `.env` 存在但值含非法字符 / 引号未闭合 | `read_env_file` 已做 `trim_matches('"')` 处理（account.rs:289-294）；破坏格式沿用既有容错 |
| `OC_SERVE_AUTH_ENV` 指向不存在的路径 | `unified_env_path()` 返回该路径，`read_env_file` 走文件缺失分支返回空 cfg（同首启） |
| `current_exe()` 失败（如 sandbox） | `unified_env_path()` 走最后兜底 cwd/.env（既有行为） |

无新错误类型，无错误传播路径变更。

### 5. Testing

#### 单元测试（新增 / 调整）

**新增 3 个 path 解析优先级测试**（在 `account.rs` 模块测试里）：

1. `account_load_prefers_oc_serve_auth_env_over_exe_dir`：设置 `OC_SERVE_AUTH_ENV` 指向 temp 路径 → 读该路径，不读 exe_dir
2. `account_load_falls_back_to_exe_dir_when_no_env_var`：**核心回归测试** — 把 temp 路径的 `.env` 放到当前进程 exe_dir（模拟用户安装场景）→ 验证读得到；旧实现下此测试必失败
3. `account_load_falls_back_to_cwd_when_no_exe_dir`：`current_exe()` 可用但 parent 不可用场景 → 验证走 cwd

**现有测试保持：**

- `read_env_file_parses_keys_and_trims_quotes` 等（不依赖路径解析）
- `read_env_file_missing_file_returns_default_remote_path`（首启场景）

**注：** `AccountConfig::load()` 内部直接调 `std::env::current_exe()`，需要 mock。最小化改动方案：把 `unified_env_path()` 抽成参数，让 `load()` 内部用 `crate::config::unified_env_path()` 而非 `current_exe()` —— **不需 mock**，因为 `unified_env_path()` 已经独立可测（已有测试）。`AccountConfig::load()` 的新测试直接覆盖三种优先级即可。

#### 端到端测试

- 在临时 exe_dir（用 `std::env::current_exe()` 配合 `dirs` 或 symlink）写入 `.env`，启动 mini-oc-gui-serve，断言 `open_settings()` 回填非空
- 或者：手动验证步骤（脚本化）— `cargo run` + 模拟键盘事件进入设置面板 + 截图比对

考虑到这是 ratatui TUI + Rust binary 的端到端测试成本，**以单元测试为主**，端到端由开发者手动验证一次即可（修复路径很短，单测覆盖完整）。

### 6. Migration / Backward Compatibility

无破坏性变更：

- 现有用户：`OC_SERVE_AUTH_ENV` 显式指定 → 行为不变（仍走 env var 路径）
- 现有用户：`.env` 在 cwd（旧习惯）→ 行为不变（兜底仍走 cwd）
- **修复受益用户**：`.env` 在 exe_dir（新标准位置）→ 此前找不到，现在找到

无 schema 变更、无配置文件格式变更、无 CLI flag 变更。

## 文件改动清单

| 文件 | 改动 | 行数 |
| --- | --- | --- |
| `src/account.rs` | `AccountConfig::load()` fallback 路径解析替换为 `unified_env_path()` 调用 | -5/+3 |
| `src/account.rs` | 新增 3 个测试（path 优先级） | +60~80 |

总计：~70 行改动，集中在 `account.rs` 一个文件。

## 不在范围

- 加密存储密钥（已有掩码显示，但仍是明文）
- 设置面板增加"路径选择"按钮
- `--auth-env` CLI flag（已存在，由 `unified_env_path()` 间接覆盖）
- 迁移 `.env` 路径（main.rs 已有一处迁移逻辑，处理 cwd→exe_dir 的旧 .env 迁移；本 spec 不动）
- TUI 渲染层改进（如"点击行展开密钥显示"）
- 远程同步路径的路径解析（`storage::remote` 模块，不在本 spec 范围）