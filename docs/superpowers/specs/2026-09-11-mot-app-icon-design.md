# MOT 应用图标设计规格

**项目**：`mini-oc-gui-serve` (Rust TUI + Axum)
**作者**：Sisyphus
**日期**：2026-09-11
**状态**：待评审

---

## 1. 目标

为 `mini-oc-gui-serve` 程序交付一套跨平台图标资产，文字内容为 **MOT**，并在 Windows / macOS / Linux 三个目标平台的发布产物中可见。

### 1.1 成功标准

- [ ] Windows: `target/release/mini-oc-gui-serve.exe` 在资源管理器、Dock、任务栏属性页显示 MOT 图标
- [ ] macOS: `icon.icns` 含 16/32/64/128/256/512/1024 多分辨率，可被 `iconutil -l` 完整列出
- [ ] Linux: `icon.png` (512x512) 可供桌面启动器 `.desktop` 文件引用
- [ ] 所有图标源唯一可信：一份 SVG → 自动派生所有平台产物
- [ ] 不引入额外的开发者本地 CLI 工具依赖（无 `magick` / `iconutil` 强依赖）
- [ ] 不破坏现有 build.rs 的 rathole bundle 流水线
- [ ] 增量构建：仅当 `assets/icon.svg` 变化时重生成图标

## 2. 设计输入（已与用户确认）

| 维度       | 决定                                                          |
| ---------- | ------------------------------------------------------------- |
| 交付形式   | 多平台图标包：Win `.ico` + macOS `.icns` + 通用 `.png`         |
| 视觉风格   | 极简几何风                                                    |
| 颜色基调   | 深色背景 `#1A1A2E` + 高对比白字 `#FFFFFF` + 靛蓝高亮 `#4F46E5` |
| O 字处理   | 高亮空心圆环（独立图形焦点）                                  |
| 文本内容   | "MOT" 三个字母                                                |

## 3. 架构

### 3.1 数据流

```
┌──────────────────────────────────────────────────────────┐
│  assets/icon.svg   ← 唯一可信源 (single source of truth)  │
│  viewBox 0 0 1024 1024                                     │
│  - 背景: <rect rx=180 fill=#1A1A2E/>                       │
│  - M:    <path fill=#FFFFFF/>                              │
│  - O:    <circle stroke=#4F46E5 fill=none stroke-width=88/>│
│  - T:    <path fill=#FFFFFF/>                              │
│  全部用 <path>/<circle> 几何,不依赖外部字体                │
└─────────────┬────────────────────────────────────────────┘
              │ build.rs 读取
              ▼
┌──────────────────────────────────────────────────────────┐
│  build.rs 栅格化流水线                                    │
│  1. usvg::Tree::from_data 解析 SVG                         │
│  2. resvg 渲染到 RGBA pixmap,目标尺寸列表:                  │
│     [16, 24, 32, 48, 64, 128, 256, 512, 1024]            │
│  3. png crate 编码为 PNG bytes                              │
│  4. ico crate 打包 → assets/icon.ico (含全部尺寸)          │
│  5. icns crate 打包 → assets/icon.icns (含全部尺寸)        │
│  6. 拷贝 512x512 PNG → assets/icon.png                     │
│  产出写入 <profile_dir>/assets/                            │
└─────────────┬────────────────────────────────────────────┘
              │
              ▼
┌──────────────────────────────────────────────────────────┐
│  二进制嵌入                                                │
│  - Windows: winresource crate 烧 icon.ico 进 .exe PE      │
│  - macOS:   .icns 文件交付,文档说明 .app bundle 集成方式   │
│  - Linux:   .desktop 文件引用 icon.png                     │
│  - TUI:     不嵌入二进制,可选在封面渲染 (本期不实现)        │
└──────────────────────────────────────────────────────────┘
```

### 3.2 选型理由（路径 1）

候选三方案对比：

| 路径 | 描述                                           | 取舍                                                    |
| ---- | ---------------------------------------------- | ------------------------------------------------------- |
| 1    | SVG 源 + build.rs 栅格化（采纳）               | 可评审、可同步、零本地 CLI 依赖；构建期 +3 个 crate       |
| 2    | 预生成静态 PNG + build.rs 打包                 | 改图标需本地重出图，跨平台协作差                          |
| 3    | 仅 PNG + 外部 CLI 后处理                        | 与项目"自包含 release 产物"哲学冲突，CI 跨平台构建复杂     |

采纳路径 1 的核心理由：
1. SVG diff 在 PR 里直观可审
2. 改一处同步全平台
3. 不破坏现有 build.rs 的"产物自包含"承诺

## 4. SVG 设计规格（精确坐标）

```
viewBox="0 0 1024 1024"

背景:
  <rect x="0" y="0" width="1024" height="1024" rx="180" ry="180" fill="#1A1A2E"/>

M 字母 (左侧):
  填充白色 #FFFFFF,几何对称 M (左 V + 右 V,共 4 段折线)
  区域: x ∈ [120, 400], y ∈ [280, 744]

O 字母 (中央高亮圆环):
  cx=512 cy=512 r=232 (外径)
  描边宽度 88 (即内径 r=144)
  fill="none" stroke="#4F46E5" stroke-width="88"
  stroke-linecap="round" (确保 16x16 缩略图描边不消失)

T 字母 (右侧):
  填充白色 #FFFFFF,几何 T (顶部横杠 + 中央竖杠)
  区域: x ∈ [624, 904], y ∈ [280, 744]

视觉比例:
  - 三个字母等高 464 (744 - 280)
  - 字母水平间距均匀
  - 留白 20% 内边距 (204.8px)
```

字体策略：**全部 `<path>`/`<circle>` 几何化** —— 不使用 `<text>` 元素，从根本上消除 resvg 字体回退问题。

## 5. 文件清单（新增 / 修改）

### 5.1 新增文件

| 路径               | 类型     | 说明                                                                  |
| ------------------ | -------- | --------------------------------------------------------------------- |
| `assets/icon.svg`    | 源文件   | 唯一可信源 SVG (入版本控制)                                            |
| `assets/icon.png`    | 产物     | 512x512 PNG (build.rs 生成,gitignore)                                  |
| `assets/icon.ico`    | 产物     | Windows 多分辨率 ICO (build.rs 生成,gitignore)                          |
| `assets/icon.icns`   | 产物     | macOS 多分辨率 ICNS (build.rs 生成,gitignore)                            |

### 5.2 修改文件

| 路径          | 改动                                                                                                                       |
| ------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `build.rs`     | 新增 `generate_icons()` 函数 + `embed_windows_icon()`（条件编译）；现有 `copy_bundle()` 保留不动                                |
| `Cargo.toml`   | 新增 `[build-dependencies]`：`usvg`、`resvg`、`png`、`ico`、`icns`；新增 `[target.'cfg(windows)'.build-dependencies]`：`winresource` |
| `.gitignore`   | 新增 `assets/icon.png`、`assets/icon.ico`、`assets/icon.icns`                                                                  |
| `README.md`    | Features 段补一句"图标已嵌入二进制 (SVG 源在 assets/icon.svg)"                                                                 |
| `docs/`         | 本 spec 文件                                                                                                                |

### 5.3 不在本期范围（YAGNI）

- ❌ TUI 启动封面渲染 PNG（ratatui 全文本 UI，引入图像库不划算）
- ❌ `.app` bundle 自动构建（需要 `cargo-bundle` 集成，单独 spec 评估）
- ❌ 不同分辨率下的人工优化（粗略测试通过即可）

## 6. build.rs 流水线设计

### 6.1 函数划分

```rust
// 入口（main 函数中追加）
fn main() {
    // ... 现有 rathole bundle 逻辑 ...
    if let Err(e) = generate_icons(&manifest_dir, &profile_dir) {
        eprintln!("cargo:warning=icon generation failed: {e}");
        // 不 panic:允许图标缺失的降级构建(类比 rathole bundle 容错策略)
    }
    #[cfg(windows)]
    embed_windows_icon(&manifest_dir, &profile_dir);
}

// 1. SVG → 多分辨率 PNG
fn render_svg_to_pngs(svg_path: &Path) -> Result<HashMap<u32, Vec<u8>>>;

// 2. 打包 .ico (Windows)
fn pack_ico(pngs: &HashMap<u32, Vec<u8>>, out: &Path) -> Result<()>;

// 3. 打包 .icns (macOS)
fn pack_icns(pngs: &HashMap<u32, Vec<u8>>, out: &Path) -> Result<()>;

// 4. 拷贝 512x512 作为 icon.png
fn write_icon_png(pngs: &HashMap<u32, Vec<u8>>, out: &Path) -> Result<()>;

// 5. 烧 Windows PE 资源
#[cfg(windows)]
fn embed_windows_icon(profile_dir: &Path);
```

### 6.2 增量构建策略

```rust
// 监听 SVG 变化,触发 rerun
println!("cargo:rerun-if-changed={}", svg_path.display());

// 监听 PNG/ICO/ICNS 产物变化,避免中间产物缓存错乱
println!("cargo:rerun-if-changed={}", profile_dir.join("assets").display());
```

### 6.3 错误容忍

参考现有 rathole bundle 的容错策略：图标生成失败 → `eprintln!("cargo:warning=...")` + 返回 `Ok(())`，不阻塞构建。这保证：
- 没有 SVG 时不会编译失败（首次 checkout 旧 commit）
- resvg/ico/icns 在某个平台编译失败时降级（虽然概率极低）

## 7. 嵌入二进制策略

| 平台       | 方式                                                                                                                  |
| ---------- | --------------------------------------------------------------------------------------------------------------------- |
| **Windows** | `winresource::WindowsResource::new().set_icon(ico_path).compile()?` 在 build.rs 里执行                                |
| **macOS**   | `.icns` 文件交付；README 文档说明如何用 `iconutil` 集成到 `.app` bundle 中；不自动嵌入裸 Mach-O                       |
| **Linux**   | 不嵌入；交付 `icon.png` + README 说明如何在 `.desktop` 文件中引用                                                     |
| **TUI**    | 不嵌入二进制；本期不做封面 logo                                                                                       |

## 8. 测试与验证

### 8.1 自动化验证（cargo test）

本期不引入新单元测试。验证以人工抽查为主，因为图标正确性难以自动化断言。

### 8.2 人工验证清单

| 验证项                            | 方法                                                                                            |
| --------------------------------- | ----------------------------------------------------------------------------------------------- |
| SVG → PNG 栅格化正确              | `cargo build` 后打开 `target/<profile>/assets/icon.png`，确认 MOT 字母清晰、O 圆环完整              |
| `.ico` 多分辨率齐全               | Windows VM `cargo build --release`，查看 .exe 属性页"详细信息"图标尺寸                            |
| `.icns` 多分辨率齐全              | macOS `iconutil -l assets/icon.icns` 列出 `ic07/ic08/ic09/ic10`                                  |
| Windows PE 资源嵌入生效           | Windows VM 拖动 .exe 看任务栏/Dock 图标显示正常                                                  |
| macOS iconutil 集成正常           | macOS `cp icon.icns Foo.app/Contents/Resources/` + `Info.plist` `CFBundleIconFile` 后启动 Foo.app |
| 增量构建                          | 修改 `assets/icon.svg` 后 `cargo build` 应重跑 build.rs；不改应走缓存                              |
| 现有 rathole bundle 不回归        | `cargo build --release` 后 `target/release/rathole/` 目录结构与改动前一致                          |

### 8.3 视觉验证

由于本期不引入 Playwright 等截图工具，视觉验证由用户在 GitHub PR review 时人工确认：
- `assets/icon.svg` 在 PR diff 里可见
- 实际 PNG/ICO/ICNS 截图附在 PR 描述

## 9. 依赖清单

### 9.1 Cargo.toml 新增

```toml
[build-dependencies]
usvg = "0.45"
resvg = "0.45"
png = "0.17"
ico = "0.3"
icns = "0.3"

[target.'cfg(windows)'.build-dependencies]
winresource = "0.1"
```

### 9.2 体积影响

- 这些 crate 只在 `build.rs` 用，不进 release 二进制
- 在 `strip = true` + `lto = "thin"` + `panic = "abort"` 现有 profile 下，二进制体积零增量
- 构建期增量：首次构建 +3 crate 编译 ~10 秒；增量构建仅当 SVG 改时 ~0.5 秒

### 9.3 版本号约定

`9.1` 中列出的版本号（`0.45` / `0.17` / `0.3` / `0.1`）为本期编写时点的最新稳定版。
实施阶段（writing-plans → build.rs）应在动手前再核对一次 crates.io，如已有更高 stable，应改用最新版。
写作本文档时未做精确核对，避免锁定过期版本号。

## 10. macOS 裸二进制图标限制（重要）

`mini-oc-gui-serve` 当前 `cargo build --release` 产出的是裸 Mach-O 可执行文件，
**没有 `.app` bundle**，因此 `icon.icns` 不会自动应用到 Dock / Finder 显示。

正确生效路径（README 中需要说明）：

```sh
# 1. 构建 release 二进制
cargo build --release

# 2. 手动创建最小 .app bundle 结构
mkdir -p MiniOC.app/Contents/{MacOS,Resources}
cp target/release/mini-oc-gui-serve MiniOC.app/Contents/MacOS/
cp target/release/assets/icon.icns MiniOC.app/Contents/Resources/

# 3. 写入 Info.plist (CFBundleIconFile = icon.icns)
cat > MiniOC.app/Contents/Info.plist <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>mini-oc-gui-serve</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleIdentifier</key><string>local.mini-oc-gui-serve</string>
  <key>CFBundleName</key><string>mini-oc-gui-serve</string>
</dict>
</plist>
EOF

# 4. 后续集成 .app 构建（cargo-bundle / 独立脚本）留给未来 spec
```

后续若引入 `cargo-bundle` 自动生成 `.app`，应另起一份 spec。

## 10.5 版本控制策略

| 路径                                  | 入版本控制？ | 理由                                                                       |
| ------------------------------------- | ------------ | -------------------------------------------------------------------------- |
| `assets/icon.svg`                      | ✅ 入         | 唯一可信源，需要 review 和历史追溯                                           |
| `assets/icon.png`                      | ❌ 忽略       | build.rs 产物，从 SVG 派生，零信息增量                                       |
| `assets/icon.ico`                      | ❌ 忽略       | 同上                                                                       |
| `assets/icon.icns`                     | ❌ 忽略       | 同上                                                                       |
| `docs/superpowers/specs/*.md`          | ✅ 入         | 设计决策需要 review + 历史追溯                                              |
| `docs/superpowers/plans/*.md`          | ✅ 入         | 实施计划同上                                                               |

`.gitignore` 改动：追加 `assets/icon.png`、`assets/icon.ico`、`assets/icon.icns`。
**不** 追加 `docs/` —— 现有项目里 docs/ 没有被忽略，整个目录都入版本控制。

## 11. 风险与回滚

| 风险                                | 缓解                                                                                  |
| ----------------------------------- | ------------------------------------------------------------------------------------- |
| `resvg` 0.45 在 macOS aarch64 编译失败 | 用稳定版 0.45（已发布）。若失败回退路径 2（预生成静态资源）                            |
| `winresource` 与现有 build.rs 冲突    | `WindowsResource::new()` 是独立 API，与 `copy_bundle()` 函数并列、互不影响             |
| SVG 字体回退                        | 设计已**完全用 path** 不用 `<text>`，从根本上消除字体依赖                              |
| 图标改一次全平台重 build            | 接受。SVG diff 评审成本远低于手动改 9 个 PNG 文件                                      |
| 现有用户的 release 二进制没有图标    | 无影响：旧二进制该工作还工作；新构建带图标是纯增量                                      |

## 12. 实施拆分（指引，不属 spec 范围）

按 writing-plans skill 产出的实施计划执行。预期阶段：

1. **阶段 1**：手写 `assets/icon.svg`，肉眼复核设计
2. **阶段 2**：扩展 `Cargo.toml` 新增 build-dependencies
3. **阶段 3**：扩展 `build.rs` 实现 `generate_icons()` 流水线
4. **阶段 4**：扩展 `build.rs` 实现 `embed_windows_icon()` (条件编译)
5. **阶段 5**：更新 `.gitignore`、`README.md`
6. **阶段 6**：本地 `cargo build --release` 跑通，附 PNG 截图到 PR

## 12. 评审要求

- [ ] 用户已确认视觉方向（极简几何 / 深底白字 / O 高亮环）
- [ ] 用户已确认交付范围（多平台包 + Windows PE 嵌入）
- [ ] 用户已确认实施路径（路径 1：SVG + build.rs）
- [ ] 用户已确认本期不做：TUI 封面 logo、`.app` bundle 自动构建
