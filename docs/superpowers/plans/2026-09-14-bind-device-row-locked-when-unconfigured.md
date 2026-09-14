# 首启时锁定"绑定设备"行 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 当 `AccountConfig::is_configured()` 为 false 时，TUI 设置面板的"绑定设备"行必须**完全不可交互**（鼠标 click region 不注册 + 视觉灰显无下划线），`submit_settings` 成功后立即解锁。

**Architecture:** 在 `src/ui/app.rs` 中复用 `AccountConfig::is_configured()`（已有函数，无需新增），在 `build_settings_lines` 与 `register_settings_click_regions` 两处按配置状态分支；`click_at` handler 保留原 `is_configured()` 检查并加一行注释作为防御纵深。**不引入新字段**（与现有 `first_setup_required`/`open_settings` 的 `RwLock` 读取模式一致）。

**Tech Stack:** Rust 1.75+, ratatui 0.28, std::sync::RwLock, TestBackend (单元测试)

**Spec:** `docs/superpowers/specs/2026-09-14-bind-device-row-locked-when-unconfigured-design.md`

---

## File Structure

| 文件 | 改动类型 | 责任 |
| --- | --- | --- |
| `src/ui/app.rs` | Modify — 3 处生产代码 + 1 处注释 + 5 个新测试 | "绑定设备"行的渲染样式、click region 注册、handler 防御纵深注释、单元测试 |

**不新增文件、不重构模块**。app.rs 已 6927 行，新增内容集中在 ~3808-3990（生产代码）与测试模块尾部（5 个新用例）。

---

## Task 1: 锁定态 click region 阻止注册（`register_settings_click_regions`）

**Files:**
- Modify: `src/ui/app.rs:3934-3999`（`register_settings_click_regions`）
- Test: `src/ui/app.rs` 测试模块（在 `mod tests { ... }` 末尾添加）

### Step 1.1: 写失败测试 — 锁定态 `find_target` 在第 5 行坐标返回 `None`

在 `src/ui/app.rs` 的 `mod tests` 模块末尾添加：

```rust
/// 锁定态：`account_config` 三字段缺失时，第 5 行（绑定设备）坐标不应
/// 注册任何 click region —— `find_target` 必须返回 None。
/// 锁定态的渲染样式在 Task 3 覆盖；本测试只验证"点击不到"。
#[test]
fn bind_device_row_unregistered_when_account_unconfigured() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = TuiApp::test_stub();
    // 首启标志关闭（让 click_at 走 dismiss_popup 分支，不影响本测试断言）
    app.first_setup_required = false;
    // 默认 account_config 三字段就是空（test_stub 不填充），无需显式清空
    app.input_mode = InputMode::SettingsAccountId;
    terminal.draw(|frame| app.render_settings_popup(frame)).expect("draw");
    let rect = app.last_settings_popup_rect.expect("popup rect");
    let bind_y = rect.y + 1 + 5; // FIELD_LINE_IDX[绑定设备] = 5
    let bind_x = rect.x + 4;
    let target = app.find_target(bind_x, bind_y);
    assert!(
        target.is_none(),
        "锁定态不应注册 SettingsBindDevice region，但 find_target 返回了 {target:?}"
    );
}
```

### Step 1.2: 运行测试，验证失败

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
cargo test --lib ui::app::tests::bind_device_row_unregistered_when_account_unconfigured 2>&1 | tail -20
```

**Expected:** FAIL — `target.is_none()` 不成立（现状无条件注册）。

### Step 1.3: 实现 — 在 `register_settings_click_regions` 中加 `bind_unlocked` 守卫

修改 `src/ui/app.rs` 的 `register_settings_click_regions` 函数，在 `const BIND_DEVICE_LINE_IDX: u16 = 5;` 这一行**之前**插入：

```rust
        // 「绑定设备」行：未配置账户时**不**注册 click region —— 鼠标
        // 点不到、点击静默无响应。解锁条件与 build_settings_lines 一致：
        // 复用 AccountConfig::is_configured() 三字段判定（账号+密钥+远程路径）。
        let bind_unlocked = self
            .account_config
            .read()
            .map(|a| a.is_configured())
            .unwrap_or(false);
```

然后将紧随其后的整段（包含 `if let Some(screen_row) = ... { ... if screen_row < content_h { ... push(...) } }`）包到 `if bind_unlocked { ... }` 中。最终该段应近似为：

```rust
        if bind_unlocked {
            if let Some(screen_row) = BIND_DEVICE_LINE_IDX.checked_sub(scroll_offset) {
                if screen_row < content_h {
                    let target_y = rect.y + 1 + screen_row;
                    self.click_regions.push(ClickRegion {
                        rect: Rect::new(rect.x + 1, target_y, rect.width.saturating_sub(2), 1),
                        target: ClickTarget::SettingsBindDevice,
                    });
                }
            }
        }
```

### Step 1.4: 重新运行测试，验证通过

```bash
cargo test --lib ui::app::tests::bind_device_row_unregistered_when_account_unconfigured 2>&1 | tail -10
```

**Expected:** PASS

### Step 1.5: 提交

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
cargo build --lib 2>&1 | tail -5  # 确保不破坏现有编译
cargo test --lib ui::app::tests::bind_device_row_unregistered_when_account_unconfigured 2>&1 | tail -3
git add src/ui/app.rs
git commit -m "feat(ui): 锁定态不注册绑定设备 click region

未配置账户时（AccountConfig::is_configured() == false），
register_settings_click_regions 跳过第 5 行（绑定设备）的
ClickRegion 推送 —— 鼠标点不到该行。

配合后续 commit（build_settings_lines 视觉锁定）形成完整锁定体验。"
```

---

## Task 2: 解锁态仍正常注册 click region（回归保护）

**Files:**
- Test: `src/ui/app.rs` 测试模块

### Step 2.1: 写失败测试 — 已配置账户时 `find_target` 在第 5 行坐标返回 `SettingsBindDevice`

```rust
/// 解锁态：`account_config` 三字段非空时，第 5 行（绑定设备）坐标必须
/// 注册 `SettingsBindDevice` click region —— 与既有
/// `clicking_bound_device_row_closes_settings_and_does_not_eagerly_pop_empty_picker`
/// 测试的"已配置"前置条件一致，但本测试聚焦 click region 注册本身。
#[test]
fn bind_device_row_unlocked_after_account_configured() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = TuiApp::test_stub();
    app.first_setup_required = false;
    {
        let mut guard = app.account_config.write().unwrap_or_else(|e| e.into_inner());
        guard.account_id = "u-1".to_string();
        guard.account_key = "k-abcdef".to_string();
        guard.remote_path = "https://oc.isoops.com".to_string();
    }
    app.input_mode = InputMode::SettingsAccountId;
    terminal.draw(|frame| app.render_settings_popup(frame)).expect("draw");
    let rect = app.last_settings_popup_rect.expect("popup rect");
    let bind_y = rect.y + 1 + 5;
    let bind_x = rect.x + 4;
    match app.find_target(bind_x, bind_y) {
        Some(ClickTarget::SettingsBindDevice) => {}
        other => panic!("expected SettingsBindDevice, got {other:?}"),
    }
}
```

### Step 2.2: 运行测试，验证通过（Task 1 已让 `find_target` 在 `bind_unlocked` 时返回该 target）

```bash
cargo test --lib ui::app::tests::bind_device_row_unlocked_after_account_configured 2>&1 | tail -10
```

**Expected:** PASS

### Step 2.3: 提交

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
git add src/ui/app.rs
git commit -m "test(ui): 解锁态绑定设备 click region 注册回归测试

三字段配置齐全时，第 5 行必须注册 SettingsBindDevice region。
与 Task 1 的锁定态测试形成对偶，确保 if 守卫方向正确。"
```

---

## Task 3: 锁定态渲染灰显 + 无下划线 + 原因提示（`build_settings_lines`）

**Files:**
- Modify: `src/ui/app.rs:3808-3918`（`build_settings_lines`）
- Test: `src/ui/app.rs` 测试模块

### Step 3.1: 写失败测试 — 锁定态 `build_settings_lines()[5]` 文本与样式

```rust
/// 锁定态：`build_settings_lines` 第 5 行（绑定设备）必须：
/// - 文本含"未配置账户"前缀；
/// - 文本不含"点击重新绑定"或"点击选择设备"（避免误导）；
/// - style 颜色为 `Color::DarkGray`；
/// - style **不含** `Modifier::UNDERLINED`。
#[test]
fn bind_device_row_renders_grey_when_locked() {
    let mut app = TuiApp::test_stub();
    // 默认 account_config 三字段就是空（test_stub 不填充），无需显式清空
    let line = &app.build_settings_lines()[5];
    let text: String = line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("未配置账户"),
        "锁定态绑定设备行应说明前置条件: {text}"
    );
    assert!(
        !text.contains("点击重新绑定") && !text.contains("点击选择设备"),
        "锁定态不应包含可点击暗示: {text}"
    );
    // 单 Span（我们把整行塞到一个 Span::styled 里）
    assert_eq!(line.spans.len(), 1, "锁定态绑定设备行应为单一 Span");
    let style = line.spans[0].style;
    assert_eq!(
        style.fg,
        Some(Color::DarkGray),
        "锁定态应为 DarkGray，实际: {:?}",
        style.fg
    );
    assert!(
        !style.add_modifier.contains(Modifier::UNDERLINED),
        "锁定态**不应**有 UNDERLINED（视觉无下划线）"
    );
}

/// 解锁态：绑定设备行恢复 cyan + 下划线 + 点击提示。
#[test]
fn bind_device_row_renders_clickable_when_unlocked() {
    let mut app = TuiApp::test_stub();
    {
        let mut guard = app.account_config.write().unwrap_or_else(|e| e.into_inner());
        guard.account_id = "u-1".to_string();
        guard.account_key = "k-abcdef".to_string();
        guard.remote_path = "https://oc.isoops.com".to_string();
    }
    let line = &app.build_settings_lines()[5];
    let text: String = line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("点击选择设备") || text.contains("点击重新绑定"),
        "解锁态应含点击提示: {text}"
    );
    let style = line.spans[0].style;
    assert_eq!(style.fg, Some(Color::Cyan), "解锁态应为 Cyan");
    assert!(
        style.add_modifier.contains(Modifier::UNDERLINED),
        "解锁态应有 UNDERLINED"
    );
}
```

### Step 3.2: 运行测试，验证锁定态测试失败

```bash
cargo test --lib ui::app::tests::bind_device_row_renders_grey_when_locked 2>&1 | tail -20
```

**Expected:** FAIL（现状是 cyan + "点击选择设备"）。

### Step 3.3: 实现 — `build_settings_lines` 按 `bind_unlocked` 分支渲染

修改 `src/ui/app.rs` 的 `build_settings_lines`：

1. 在 `bound_device_clickable_style` 计算前（约 line 3847 之前），插入 `bind_unlocked` 计算（同 Task 1 Step 1.3 的形式）。
2. 将原本的 `let bound_device_display = ...` 与 `let bound_device_clickable_style = ...` 两段改为按 `bind_unlocked` 分支：

```rust
        let bind_device_text = if bind_unlocked {
            if bound_device_name.is_empty() {
                "  绑定设备: (未绑定)  ▶ 点击选择设备".to_string()
            } else {
                format!("  绑定设备: {bound_device_name}  ▶ 点击重新绑定")
            }
        } else {
            "  绑定设备: —  ▶ 未配置账户,需先填写 账户ID/密钥".to_string()
        };
        let bind_device_style = if bind_unlocked {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(Color::DarkGray)
        };
```

3. 在 `vec![...]` 中（约 line 3870）将：
   ```rust
   Line::from(Span::styled(bound_device_display, bound_device_clickable_style)),
   ```
   替换为：
   ```rust
   Line::from(Span::styled(bind_device_text, bind_device_style)),
   ```

### Step 3.4: 重新运行，验证锁定 + 解锁两测试均通过

```bash
cargo test --lib ui::app::tests::bind_device_row_renders 2>&1 | tail -10
```

**Expected:** PASS（两个测试都过）

### Step 3.5: 提交

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
git add src/ui/app.rs
git commit -m "feat(ui): 锁定态绑定设备行渲染为灰显+无下划线+原因提示

未配置账户时第 5 行渲染：
- 颜色 DarkGray（与描述行 desc_style 视觉一致）
- 无 UNDERLINED 修饰
- 文本：\"  绑定设备: —  ▶ 未配置账户,需先填写 账户ID/密钥\"

配合 Task 1（不注册 click region）形成完整锁定体验。"
```

---

## Task 4: 端到端 — 锁定态点击完全静默无响应

**Files:**
- Test: `src/ui/app.rs` 测试模块

### Step 4.1: 写测试 — 锁定态 `click_at` 不弹 status_message / 不 spawn fetch / device_picker 仍 None

```rust
/// 端到端：锁定态点击第 5 行应**完全静默**——不弹 status_message，
/// 不发起 fetch，不弹出设备选择弹框，不修改 trigger 槽。
///
/// 这是"完全锁定"语义的最强回归测试：即使未来 register / render 逻辑
/// 回归（如忘记加守卫），仍能在此处抓出"点击产生了副作用"。
#[tokio::test]
async fn locked_bind_device_row_click_is_silent_noop() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = TuiApp::test_stub();
    // 默认三字段就是空（test_stub）。first_setup_required 由 test_stub 设默认。
    app.input_mode = InputMode::SettingsAccountId;
    terminal.draw(|frame| app.render_settings_popup(frame)).expect("draw");
    let rect = app.last_settings_popup_rect.expect("popup rect");
    let bind_y = rect.y + 1 + 5;
    let bind_x = rect.x + 4;

    // 锁定态：find_target 应返回 None（由 Task 1 保证）
    assert!(
        app.find_target(bind_x, bind_y).is_none(),
        "前置：锁定态不应注册该 region"
    );

    // 直接调用 click_at —— 即便坐标命中不到 region，也不应有任何副作用
    app.click_at(bind_x, bind_y).await;

    // 不应弹出设备选择弹框
    assert!(app.device_picker.is_none(), "device_picker 应仍为 None");
    // 不应写入 trigger 槽
    assert!(
        app.device_picker_trigger
            .lock()
            .map(|g| g.is_none())
            .unwrap_or(true),
        "trigger 槽应仍为 None"
    );
    // status_message 不应是绑定设备相关的提示
    let status = app.status_message.lock().unwrap().clone();
    assert!(
        !status.contains("绑定设备") && !status.contains("拉取设备清单"),
        "锁定态点击不应产生绑定设备相关状态栏消息，实际: {status}"
    );
}
```

### Step 4.2: 运行测试，验证通过

```bash
cargo test --lib ui::app::tests::locked_bind_device_row_click_is_silent_noop 2>&1 | tail -10
```

**Expected:** PASS（Task 1 已阻止 region 注册 + click_at 在锁定态会走 FirstSetupBlockOutside）

### Step 4.3: 提交

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
git add src/ui/app.rs
git commit -m "test(ui): 锁定态绑定设备点击完全静默端到端测试

验证点击后：
- device_picker 仍为 None
- trigger 槽仍为 None
- status_message 不含绑定设备相关提示

回归保护：任何后续修改若让锁定态点击产生副作用都会被捕获。"
```

---

## Task 5: handler 层防御纵深注释

**Files:**
- Modify: `src/ui/app.rs:1252-1274`（`click_at` 中 `ClickTarget::SettingsBindDevice` 分支）

### Step 5.1: 修改 — 在 `if !cfg_snapshot.is_configured()` 检查之上加一行注释

在 `src/ui/app.rs` line 1253 上方插入：

```rust
                // 防御纵深：register_settings_click_regions 已按
                // bind_unlocked 阻止该 region 注册，正常路径下此分支
                // 不会被触发；此处仍保留 is_configured() 检查，避免
                // 未来 register 逻辑被改坏时旧实现（点击弹 status_message）
                // 重新出现。
```

**注意**：注释必须放在已有注释块（"「绑定设备」只读行被点击 —— 关闭设置弹框,触发设备选择。"）**之前**或**之后**一行，不要修改现有逻辑、不要修改 status_message 文案。

### Step 5.2: 验证不破坏现有测试

```bash
cargo test --lib ui::app::tests::clicking_bound_device_row_closes_settings_and_does_not_eagerly_pop_empty_picker 2>&1 | tail -10
```

**Expected:** PASS（handler 行为未变，只是注释）

### Step 5.3: 提交

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
git add src/ui/app.rs
git commit -m "docs(ui): handler 防御纵深注释

click_at 中 ClickTarget::SettingsBindDevice 分支的 is_configured() 检查
作为防御纵深保留，并加注释说明意图，避免未来维护者误删。"
```

---

## Task 6: 完整回归 + 收尾

**Files:**
- Test: 全部 src/ui/app.rs 测试 + 整个 crate

### Step 6.1: 跑完整测试套件，确保无回归

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
cargo build --release 2>&1 | tail -5
cargo test --lib 2>&1 | tail -20
```

**Expected:**
- `cargo build --release` 成功（无 warning）
- 所有现有测试 + 5 个新测试通过
- 重点关注：`build_settings_lines_has_expected_row_count_for_popup_geometry`、`build_settings_lines_documents_every_field`、`build_settings_lines_keyword_alignment` 等与第 5 行内容相关的回归测试

### Step 6.2: 手动对照 spec §6 验收标准

- [ ] 首启启动后 "绑定设备"行视觉灰显 + 无下划线 + 文案含"未配置账户"
- [ ] 锁定态鼠标移动到该行无 hover 高亮
- [ ] 锁定态鼠标点击该行完全无响应
- [ ] submit_settings 后重开面板时该行变回青色 + 下划线
- [ ] 解锁态点击行为与现有逻辑一致（关闭面板 + 后台 fetch + 弹设备选择）

### Step 6.3: 最终提交（如有遗漏）

```bash
cd /Users/samuel/Documents/GitForAi/mini-oc-gui
git status
# 若有未提交：
git add -A
git commit -m "chore: 完整回归通过"
```

---

## Self-Review（against spec）

**1. Spec coverage:**
- §3.1 build_settings_lines 渲染分支 → Task 3 ✅
- §3.2 register_settings_click_regions 守卫 → Task 1 ✅
- §3.3 handler 防御纵深注释 → Task 5 ✅
- §3.4 测试覆盖（5 个用例） → Task 1 / Task 2 / Task 3 / Task 4 ✅（共 5 个新测试：unregistered / unlocked / grey / clickable / silent_noop）
- §6 验收标准 → Task 6 ✅

**2. Placeholder scan:**
- 无 "TBD" / "TODO" / "implement later"
- 所有代码片段都是完整可编译的 Rust
- 所有命令都有精确的 path 和 expected output

**3. Type consistency:**
- `bind_unlocked` 在 Task 1 / Task 3 都用同一形式：`self.account_config.read().map(|a| a.is_configured()).unwrap_or(false)`
- `find_target(bind_x, bind_y)` / `click_at(bind_x, bind_y)` 签名与既有测试一致
- `ClickTarget::SettingsBindDevice` / `ClickTarget::SettingsField` 命名一致

**4. Order dependencies:**
- Task 1 必须先于 Task 2（Task 2 验证 Task 1 没破坏解锁路径）
- Task 1 / Task 3 都可在 Task 4 之前完成
- Task 5 独立（仅注释）
- Task 6 收尾

---

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-09-14-bind-device-row-locked-when-unconfigured.md`. Two execution options:

**1. Subagent-Driven (recommended)** — 我每个任务派遣一个新 subagent，任务间 review，快速迭代

**2. Inline Execution** — 在本会话内用 executing-plans 执行，批量执行带检查点

请选择执行方式。
