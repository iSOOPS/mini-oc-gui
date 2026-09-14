# 修复 OC 项目子页面卡片渲染与远程存储同步 — 设计

> **Status**: draft
> **Date**: 2026-09-14
> **Author**: brainstorm + Sisyphus
> **Scope**: `src/ui/app.rs`（卡片 padding + sub_page 缓存刷新）+ `src/storage/sync.rs`（push 错误可见性）+ `src/ui/app.rs` 状态栏消息

## 1. 背景与目标

TUI 进入「OC 项目」子页面后存在两个用户可见的 bug：

1. **Bug 1：卡片框存在但内容不显示**
   - 现象：进入 OC 项目后，「项目」「会话」「选择系统路径 / 手动输入路径」等卡片框存在，但卡片内文字（项目名、路径、状态、按钮等）不显示
   - 根因：`src/ui/app.rs:4710` 的 `Padding::uniform(5)` 上下左右各加 5 格 padding，而 `card_h = 5u16`（line 4684）总高仅 5 行。扣除上下边框各 1 行后，可用内容区仅 3 行；再扣 top/bottom padding 各 5 行后内容区为负，ratatui 渲染时内容被裁掉
   - 影响范围：所有走 `render_card_stack` 的子页面 — `SubPage::Projects`、`SubPage::Sessions`、`SubPage::NewPathChoice`（共 3 个变体）。`SubPage::ManualPath` 走单独的 `Paragraph` 渲染不受影响

2. **Bug 2：新建项目不触发远程存储**
   - 现象：用户选完路径后退出，远程存储里查不到该条目
   - 现状：代码已实现 `upsert_path` + `create_remote_path` 调用，但**推送失败被静默吞掉**：
     - `upsert_path` → `persist(..., false)` → `async_push`（fire-and-forget，无 retry，无错误返回，line 579）
     - `create_remote_path` → `push_blocking`，但 line 508-518 在没有 remote client 时直接 `return Ok(())` 静默成功
     - `confirm_manual_path` 已经在调用 `create_remote_path` 并把 Err 写到 status_message，但**成功时无任何反馈**，用户不知道是否真的同步成功
   - 影响范围：`choose_folder_flow`（系统路径选择）、`confirm_manual_path`（手动输入路径）

**目标**：
- Bug 1：让 3 个 `render_card_stack` 子页面的卡片内容**正确显示**（padding 合理、内容可见）
- Bug 2：选完路径后**确保远端存储收到条目**（失败报错、成功也提示）；同时让用户退出子页面回到 Projects 列表时看到新增条目

## 2. 设计决策摘要

| 决策点                  | 选择                                                                                       |
| ----------------------- | ------------------------------------------------------------------------------------------ |
| Bug 1 padding 修复策略  | `Padding::uniform(1)`（上下左右各 1 格）+ `card_h` 保持 5（最小可用高度）                          |
| Bug 1 `card_h` 调整     | 不改，保持 5 行（项目卡 / 会话卡内容只有 3 行 Line 渲染）                                          |
| Bug 2 推送策略          | **统一改用 `create_remote_path`**（已存在），从 `confirm_manual_path` / `choose_folder_flow` 中移除冗余的 `upsert_path` 预先调用 |
| Bug 2 错误可见性        | `push_blocking` 在没有 remote 时返回 `AppError::Internal("远程存储未配置")`，不再静默成功         |
| Bug 2 成功可见性        | `confirm_manual_path` / `choose_folder_flow` 在 `create_remote_path` 成功时写 status_message：`"✅ 已同步到远端: <path>"` |
| Bug 2 缓存刷新          | **不需要显式刷新** — `pop_sub_page`（line 3060）从 Sessions/ManualPath/NewPathChoice 返回时已经调 `enter_projects()` 重新从远端拉取最新 |
| 改动文件范围            | `src/ui/app.rs`（多处）+ `src/storage/sync.rs`（push_blocking 错误处理）                         |
| 新增测试                | 至少 2 个：① padding 修复后卡片渲染可见 ② push_blocking 无 remote 时返回 Err                       |

## 3. 实现细节

### 3.1 Bug 1：修复 `Padding::uniform(5)` 为 `Padding::uniform(1)`（src/ui/app.rs:4710）

```rust
// 修改前
.padding(Padding::uniform(5));
// 修改后
.padding(Padding::uniform(1));
```

**理由**：5 行卡片 = 1(top border) + 5(top padding) + N(content) + 5(bottom padding) + 1(bottom border)。`Padding::uniform(5)` 让 N = 5 - 12 = -7，内容被裁掉。改 1 后 N = 5 - 4 = 1 行可用 — 但项目卡 / 会话卡内容是 3 行 `Vec<Line>`。所以**必须同时调整 `card_h`** 才能放下 3 行 Line。

```rust
// 修改 card_h：5 → 7
let card_h = 7u16;
```

新计算：1(border) + 1(top padding) + 3(content) + 1(bottom padding) + 1(border) = 7 行。`padding(Padding::uniform(1))` 下 ratatui 会自动从 `card_area` 扣 padding，3 行 Line 完整显示。

**`visible_count` 计算调整**（line 4686）：
```rust
let visible_count = (visible_h / card_h).max(1) as usize;
```
原本 `visible_h` 是 area 高度减 1（title 占 1 行），除以 `card_h`。`card_h` 从 5 改 7 后，每屏可见卡片数会减少 — 这是符合预期的（小卡片装不下 3 行内容）。

### 3.2 Bug 2：移除 `upsert_path` 冗余 + 增强 `push_blocking` 错误可见性 + 状态栏反馈

#### 3.2.1 `confirm_manual_path`（src/ui/app.rs:2860-2885）

```rust
async fn confirm_manual_path(&mut self, input: String) {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        let default_dir = std::env::var("OC_DEFAULT_DIR")
            .unwrap_or_else(|_| "/Users/samuel/.config/opencode".to_string());
        // 默认目录也走远程同步路径
        match self.create_remote_path_and_refresh(&default_dir).await {
            Ok(()) => self.enter_sessions(default_dir).await,
            Err(e) => {
                *self.status_message.lock().unwrap() =
                    format!("⚠️ 远端同步失败：{e}");
                // 不进入 sessions（避免用户对着空列表困惑）
            }
        }
        return;
    }
    match PathValidator::validate(trimmed) {
        Ok(path) => {
            match self.create_remote_path_and_refresh(&path).await {
                Ok(()) => self.enter_sessions(path).await,
                Err(e) => {
                    *self.status_message.lock().unwrap() =
                        format!("⚠️ 远端同步失败：{e}");
                    // 同上：远端失败不进入 sessions
                }
            }
        }
        Err(e) => {
            if let Some(SubPage::ManualPath { error, .. }) = &mut self.sub_page {
                *error = Some(e.to_string());
            }
        }
    }
}
```

#### 3.2.2 `choose_folder_flow`（src/ui/app.rs:3216-3231）

```rust
async fn choose_folder_flow(&mut self) {
    match choose_folder().await {
        Ok(path) => {
            match self.create_remote_path_and_refresh(&path).await {
                Ok(()) => self.enter_sessions(path).await,
                Err(e) => {
                    *self.status_message.lock().unwrap() =
                        format!("⚠️ 远端同步失败：{e}");
                }
            }
        }
        Err(e) => {
            *self.status_message.lock().unwrap() = format!("⚠️ {e}");
        }
    }
}
```

#### 3.2.3 新增 `create_remote_path_and_refresh` 辅助方法（src/ui/app.rs）

```rust
/// 选完新路径后调用：同步推远端 + 状态栏反馈。
///
/// 成功 → 返回 Ok(())，调用方决定后续（enter_sessions 等）
/// 失败 → 返回 Err(AppError)，调用方决定错误展示
///
/// 设计要点：
/// - 替代 `upsert_path + create_remote_path` 双调用 —— `create_remote_path`
///   已经幂等包含 upsert 逻辑（sync.rs:280-288）
/// - 同步阻塞推送，错误立即可见
/// - 成功后写成功状态栏；失败留给调用方写（避免重复）
/// - **不**主动刷新 Projects 缓存：`pop_sub_page`（line 3060）从
///   Sessions/ManualPath/NewPathChoice 返回时已调 `enter_projects()` 重新
///   从远端拉取最新数据
async fn create_remote_path_and_refresh(&mut self, path: &str) -> Result<(), AppError> {
    self.store.create_remote_path(path).await?;
    *self.status_message.lock().unwrap() =
        format!("✅ 已同步到远端：{path}");
    Ok(())
}
```

#### 3.2.4 （已删除）`refresh_projects_list`

经自审发现：**不需要**新增此方法。`pop_sub_page`（line 3060-3070）从 Sessions / ManualPath / NewPathChoice 返回时已经调 `enter_projects()` 重新从远端拉取，所以用户 Esc 返回 Projects 列表时会看到最新数据。Spec 初稿误以为需要显式刷新，自审时发现 `enter_projects` 已经覆盖此场景。

#### 3.2.5 `push_blocking` 修复（src/storage/sync.rs:507-518）

```rust
async fn push_blocking(&self, entries: Vec<PathEntry>) -> Result<(), AppError> {
    let Some(remote_arc) = self.remote.read().await.clone() else {
        // 修复：之前静默 return Ok(()),现在返回明确错误让 TUI 能展示。
        return Err(AppError::Internal(
            "远程存储未配置（需要先完成账户登录 + fetch_user_info）".to_string(),
        ));
    };
    // ... 其余不变
}
```

**影响**：`confirm_manual_path` / `choose_folder_flow` 已经 `if let Err(e)` 捕获，错误会被写到 status_message。

## 4. 测试覆盖

| 测试名                                                  | 断言                                                                                                              |
| ------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `render_card_stack_shows_card_content`                   | `Padding::uniform(1)` + `card_h = 7` 后，用 TestBackend 渲染 `SubPage::Projects` 含 1 个项目，确认项目名 Line 出现在屏幕 buffer 中 |
| `push_blocking_without_remote_returns_error`            | 在没有 remote client 的 store 上调 `create_remote_path` → 返回 `Err(AppError::Internal)` 而不是 `Ok(())`               |
| `create_remote_path_and_refresh_writes_status_on_success` | mock remote client → 调辅助方法 → 验证 status_message 含 `"✅ 已同步到远端"`                                          |
| `confirm_manual_path_enters_sessions_on_remote_success`  | mock remote → 调 `confirm_manual_path("valid path")` → 验证 sub_page = `Some(SubPage::Sessions { ... })`              |
| `confirm_manual_path_keeps_manual_form_on_remote_failure` | 无 remote → 调 `confirm_manual_path("valid path")` → 验证 sub_page = `Some(SubPage::ManualPath { ..., error: Some(...) })`，不进入 sessions |
| `choose_folder_flow_with_remote_enters_sessions`         | mock remote + mock `choose_folder` → 调 `choose_folder_flow` → 验证 sub_page = `Sessions { ... }` + status 含 "已同步" |

测试用 `mockall`（已在 dev-dependencies）mock RemoteClient 与 choose_folder 调用。

## 5. 不在范围

- 修改变 `Padding::horizontal` 或其他已正常工作的 padding 调用（line 4451, 4531）
- 改动 `enter_projects` / `fetch_remote_projects_only` 的核心逻辑（仅供 `refresh_projects_list` 复用）
- 改动 `SubPage::ManualPath` 的渲染（line 4628-4662，已正常工作）
- 调整 `card_h` 之外的其他布局参数（title_area / visible_h）

## 6. 验收标准

### Bug 1
1. 启动 mini-oc-gui-serve，进入 OC 项目 → 项目列表卡片**显示项目名 + sections 数 + 路径 + 最后打开时间**
2. 选择某个项目 → Sessions 卡片**显示 session id + title + Enter/T/D 操作提示**
3. 在项目列表选「➕ 新建 path」→ NewPathChoice 卡片**显示「🖥 系统路径选择」+ 「⌨️ 手动输入路径」两行选项**
4. `cargo test --lib` 全量通过，原有 184 + 新增 1 = 至少 185 PASS

### Bug 2
5. 选完路径（系统 / 手动）后**状态栏立即显示「✅ 已同步到远端：<path>」**
6. 远端存储（如 curl GET `<remote>/serv/opencode/{user_id}/{pctype}/{device_name}/path-list`）**包含新加的路径条目**（sections=[]）
7. 远程未配置时（账户未登录），选完路径**状态栏显示「⚠️ 远端同步失败：远程存储未配置...」**，不进入 sessions
8. 选完路径后进入 Sessions、按 Esc 返回 Projects 列表 — **新加的路径出现在列表中**（由 `pop_sub_page` 触发 `enter_projects` 重新拉远端保证）

### 整体
9. `cargo test --lib` 全量通过，新增测试 + 既有 184 全部通过
10. `cargo build --release` 无 warning

## 7. 实现计划

进入 `writing-plans` skill 拆分实现步骤（预计 3-4 个 task）。
