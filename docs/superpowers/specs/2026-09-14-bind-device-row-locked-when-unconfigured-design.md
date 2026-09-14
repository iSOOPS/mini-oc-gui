# 首启时锁定"绑定设备"行 — 设计

> **Status**: draft
> **Date**: 2026-09-14
> **Author**: brainstorm + Sisyphus
> **Scope**: `src/ui/app.rs` — `build_settings_lines` / `register_settings_click_regions` / `click_at` 中 `ClickTarget::SettingsBindDevice` 分支 + 测试模块

## 1. 背景与目标

TUI 设置面板的"绑定设备"行（`build_settings_lines` 第 5 行）目前**无论账户是否配置完成都呈现可点击视觉**（青色 + 下划线 + "▶ 点击重新绑定"），且**无条件注册 click region**。未配置时点击才会被 handler 检查拦截并提示状态栏 — 用户必须先点错才能感知到不可用。

**目标**：当 `AccountConfig::is_configured()` 返回 `false`（即 ACCOUNT_ID / ACCOUNT_KEY / REMOTE_PATH 任意字段为空）时，该行必须**完全不可交互**——首次启动自然满足此条件，但同样适用于中途环境变量 / env 文件被清空的任何时刻：

- 鼠标点击无响应（click region 不注册）
- 视觉无任何可点击暗示（灰显 + 无下划线）
- 文案明确说明前置条件
- `submit_settings` 成功后立即解锁

## 2. 设计决策摘要

| 决策点              | 选择                                                  |
| ------------------- | ----------------------------------------------------- |
| 锁定严格度          | 完全锁定（鼠标 + 键盘都不能触发）                       |
| 配置判定字段        | 复用 `AccountConfig::is_configured()`（账号 + 密钥 + 远程路径三字段都非空） |
| 锁定状态视觉        | 灰色（`Color::DarkGray`）+ 无 `UNDERLINED` + "▶ 未配置账户,需先填写 账户ID/密钥" 后缀 |
| 描述行（第 6 行）   | 不变（仍描述 `DEVICE_NAME` 用途与点击行为）              |
| 实现方式            | 在 `build_settings_lines` / `register_settings_click_regions` 中直接 `self.account_config.read().map(|a| a.is_configured()).unwrap_or(false)`，**不引入新字段** |

## 3. 实现细节

### 3.1 `build_settings_lines` — 渲染样式与文字（app.rs:3808）

在 `bound_device_clickable_style` 计算前（约 line 3847）插入配置状态读取：

```rust
let bind_unlocked = self
    .account_config
    .read()
    .map(|a| a.is_configured())
    .unwrap_or(false);
```

按 `bind_unlocked` 分两路生成 `(text, style)`：

- **解锁态**：
  - 文字：`format!("  绑定设备: {bound_device_name}  ▶ 点击重新绑定")`，未绑定名时为 `"  绑定设备: (未绑定)  ▶ 点击选择设备"`（既有逻辑保留）
  - 样式：`Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)`（既有 `bound_device_clickable_style`）

- **锁定态**：
  - 文字：`"  绑定设备: —  ▶ 未配置账户,需先填写 账户ID/密钥"`（"绑定设备"前缀保留，便于用户识别该行）
  - 样式：`Style::default().fg(Color::DarkGray)`，**不含** `UNDERLINED`（与 `desc_style` 视觉一致）

将 `Line::from(Span::styled(bound_device_display, bound_device_clickable_style))`（当前在 `vec![...]` 内）替换为 `Line::from(Span::styled(bind_device_text, bind_device_style))`，其中 `bind_device_text` / `bind_device_style` 由 §3.1 的两路分支计算得出。

### 3.2 `register_settings_click_regions` — 阻止 click region 注册（app.rs:3934）

在 `const BIND_DEVICE_LINE_IDX: u16 = 5;` 之前插入同样的 `bind_unlocked` 计算（约 line 3966）。

当 `!bind_unlocked` 时，**整个 `if let Some(screen_row) = BIND_DEVICE_LINE_IDX.checked_sub(scroll_offset) { ... }` 块不执行** —— 不向 `click_regions` 推入 `SettingsBindDevice` region。

`find_target` 找不到命中 → `click_at` 走 `dismiss_popup` 分支 → `first_setup_required=true` 时被 `SettingsOutsideAction::FirstSetupBlockOutside` 拦截 → **完全静默**，无 status_message 输出。

### 3.3 `click_at` 中 `SettingsBindDevice` 分支 — 防御纵深（app.rs:1252）

**保留**现有 handler 中 `if !cfg_snapshot.is_configured()` 的检查 + status_message 提示。**仅新增一行注释**，明确这是"防御纵深"：

```
// 防御纵深：正常路径下 register_settings_click_regions 已按 bind_unlocked
// 阻止该 region 注册；此处仍保留检查，避免未来 register 逻辑被改坏时
// 旧实现重新出现。
```

不改 handler 行为、不删除 status_message 提示。

### 3.4 测试覆盖

在 `src/ui/app.rs` 测试模块新增以下单元测试（沿用 `TestBackend` 模式，参考 `clicking_bound_device_row_closes_settings_and_does_not_eagerly_pop_empty_picker`）：

| 测试名                                                          | 断言                                                                                                  |
| --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------- |
| `bind_device_row_unregistered_when_account_unconfigured`        | `account_config` 三字段为空 → `find_target(row 5) == None`                                            |
| `bind_device_row_unlocked_after_account_configured`             | `account_config.write()` 三字段非空 → `find_target(row 5) == Some(SettingsBindDevice)`                |
| `bind_device_row_renders_grey_when_locked`                      | 锁定态 `build_settings_lines()[5]` 文本含"未配置账户"前缀、style 含 `DarkGray`、**不含 `UNDERLINED`**     |
| `bind_device_row_renders_clickable_when_unlocked`               | 解锁态文本含"点击重新绑定"或"点击选择设备"、style 含 `Cyan` 与 `UNDERLINED`                              |
| `locked_bind_device_row_click_is_silent_noop`                   | 锁定态 `click_at(row 5)` → `device_picker == None`、`device_picker_trigger.lock() == None`、无 status_message |

## 4. 风险与权衡

**风险 1：RwLock 跨 await**

`is_configured()` 只是 `.read()` 后立即 `bool` 转换，无 await；与现有 `open_settings`（app.rs:1699）模式一致。**安全**。

**风险 2：handler 层的 status_message 变成死代码**

register 层拦截后 handler 分支在正常路径不再触发；保留作为防御纵深，未来若 register 逻辑被改坏时仍能拦截。

**风险 3：滚动出屏时该行已被既有 `screen_row < content_h` 跳过**

不影响。锁定 / 解锁两态都遵守同一滚动规则。

**YAGNI**：本设计不引入新字段、不重构 `TuiApp`、不新增抽象。**净增代码量约 100 行**（含 5 个新测试用例）。

## 5. 不在范围

- 键盘焦点循环（"绑定设备"行本来就不进 `SETTINGS_FIELDS`，不参与 Tab/↑/↓）
- 描述行（第 6 行）文案（用户确认不变）
- 其他设置面板行为
- `--no-tui` 模式（已有 `main.rs:122-127` 早期 bail，不进入 TUI）

## 6. 验收标准

1. 首启启动（`.oc-serve-auth.env` 不存在或三字段缺失），TUI 弹出设置面板后，"绑定设备"行**视觉灰显 + 无下划线**、文案含"未配置账户,需先填写 账户ID/密钥"
2. 锁定态鼠标移动到该行：**无 hover 高亮**（行不进 `SETTINGS_FIELDS`）
3. 锁定态鼠标点击该行：完全无响应，不弹 status_message、不弹设备选择弹框
4. 用户填写完三字段 + Enter 保存 → 重开设置面板时该行**变回青色 + 下划线**、"▶ 点击重新绑定"或"▶ 点击选择设备"
5. 解锁态鼠标点击该行：行为与现有逻辑一致（关闭设置弹框 + 后台 fetch + 弹设备选择弹框）
6. `cargo test` 全部通过，含新增 5 个测试用例

## 7. 实现计划

进入 `writing-plans` skill 拆分实现步骤。
