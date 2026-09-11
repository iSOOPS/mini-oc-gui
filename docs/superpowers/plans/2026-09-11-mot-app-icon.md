# MOT 应用图标实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 `mini-oc-gui-serve` 程序交付一套跨平台图标资产，文字内容为 "MOT"，并在 Windows / macOS / Linux 三个目标平台的发布产物中可见。

**Architecture:** 单一 SVG 源 (`assets/icon.svg`) 作为唯一可信源，由 `build.rs` 在编译期用 `usvg` + `resvg` 解析栅格化成多分辨率 PNG，再用 `ico` / `icns` crate 打包成 Windows `.ico` 和 macOS `.icns`。Windows 通过 `winresource` 把 `.ico` 烧进 `.exe` PE 资源；macOS / Linux 交付图标文件本身，由运行时或桌面启动器引用。

**Tech Stack:** Rust `build.rs`、`usvg` 0.48.1、`resvg` 0.48.1、`png` 0.18.1、`ico` 0.5.0、`icns` 0.4.0、`winresource` 0.1.31 (Windows only)。

**Spec 引用:** `docs/superpowers/specs/2026-09-11-mot-app-icon-design.md`

---

## 文件结构总览

### 新增
- `assets/icon.svg` —— 唯一可信源 SVG (1024×1024 viewBox)
- `assets/icon.png` —— build.rs 产物 (512×512), gitignore
- `assets/icon.ico` —— build.rs 产物 (Windows 多分辨率), gitignore
- `assets/icon.icns` —— build.rs 产物 (macOS 多分辨率), gitignore

### 修改
- `build.rs` —— 新增图标生成流水线 + Windows 嵌入 (条件编译)
- `Cargo.toml` —— 新增 `[build-dependencies]`
- `.gitignore` —— 忽略产物 3 项
- `README.md` —— Features 段补一句说明图标已嵌入

### 不动
- `src/`、`rathole/`、现有 main.rs 逻辑、TUI 业务代码

---

## Task 1: 创建 `assets/icon.svg` 唯一可信源

**Files:**
- Create: `assets/icon.svg`

- [ ] **Step 1.1: 创建 `assets/` 目录**

```bash
mkdir -p assets
```

- [ ] **Step 1.2: 写入 SVG 文件**

写入完整 SVG 内容到 `assets/icon.svg`：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg"
     viewBox="0 0 1024 1024"
     width="1024" height="1024"
     shape-rendering="geometricPrecision">
  <!-- 背景: 深色圆角矩形 -->
  <rect x="0" y="0" width="1024" height="1024" rx="180" ry="180" fill="#1A1A2E"/>

  <!-- M 字母 (左侧) -->
  <path d="M 140 720
           L 140 304
           L 260 304
           L 360 540
           L 460 304
           L 580 304
           L 580 720
           L 480 720
           L 480 460
           L 400 660
           L 320 660
           L 240 460
           L 240 720 Z"
        fill="#FFFFFF"/>

  <!-- O 字母 (中央高亮空心圆环) -->
  <circle cx="512" cy="512" r="188"
          fill="none"
          stroke="#4F46E5"
          stroke-width="88"
          stroke-linecap="round"/>

  <!-- T 字母 (右侧) -->
  <path d="M 624 304
           L 904 304
           L 904 404
           L 814 404
           L 814 720
           L 714 720
           L 714 404
           L 624 404 Z"
        fill="#FFFFFF"/>
</svg>
```

- [ ] **Step 1.3: 肉眼复核设计**

用浏览器或 SVG viewer 打开 `assets/icon.svg`（macOS：`open assets/icon.svg`），确认：
- 深底色 + 三个白色几何字母 + 中央靛蓝圆环
- 三个字母水平等距分布
- 圆环与 M、T 字母不重叠

- [ ] **Step 1.4: Commit**

```bash
git add assets/icon.svg
git commit -m "feat(assets): add MOT app icon SVG source

Single source of truth for the application icon.
Geometric path-only design, no text/font dependency:
- background: #1A1A2E rounded square
- M/T: #FFFFFF geometric paths
- O: #4F46E5 stroked ring (visual focus)"
```

---

## Task 2: 在 `Cargo.toml` 新增 build-dependencies

**Files:**
- Modify: `Cargo.toml` (在文件末尾的 `[profile.*]` 段前插入 `[build-dependencies]`)

- [ ] **Step 2.1: 在 `[dev-dependencies]` 之前插入 `[build-dependencies]` 段**

打开 `Cargo.toml`，在 `[dev-dependencies]` 这一行前插入：

```toml
# Build-time icon generation (SVG -> PNG -> ICO/ICNS).
# These crates are only used in build.rs and do not enter the release binary.
usvg = "0.48.1"
resvg = "0.48.1"
png = "0.18.1"
ico = "0.5.0"
icns = "0.4.0"
```

- [ ] **Step 2.2: 在文件末尾添加 Windows-only 的 winresource**

在 `Cargo.toml` 末尾（`[profile.release]` 之后或之前任意位置）追加：

```toml
[target.'cfg(windows)'.build-dependencies]
winresource = "0.1.31"
```

- [ ] **Step 2.3: 验证 `cargo build` 仍可启动 build.rs**

```bash
cargo build --bin mini-oc-gui-serve 2>&1 | head -30
```

预期：build.rs 会先编译新依赖（首次 ~10s），之后再次 build 时直接成功。
如果失败，应回退到 Task 2.1 把版本号换成 `usvg = "*"` 这种更宽松的版本要求再试一次。

- [ ] **Step 2.4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add usvg/resvg/png/ico/icns build-deps for icon pipeline

Build-time only; does not affect release binary size."
```

---

## Task 3: 在 `build.rs` 接入 SVG → 多分辨率 PNG 渲染

**Files:**
- Modify: `build.rs`

- [ ] **Step 3.1: 在 `build.rs` 顶部追加 imports**

在 `build.rs` 第 26 行（`use std::path::{Path, PathBuf};` 之后）追加：

```rust
use std::collections::BTreeMap;
use std::io::BufWriter;
```

并在文件末尾追加以下三个函数（不要修改现有 `main`、`copy_bundle`、`copy_dir`、`copy_with_mode`、函数):

```rust
// ============================================================================
// Icon generation pipeline (MOT)
// Reads assets/icon.svg and produces:
//   <profile_dir>/assets/icon.png  (512x512)
//   <profile_dir>/assets/icon.ico  (Windows multi-res)
//   <profile_dir>/assets/icon.icns (macOS multi-res)
// On Windows, also embeds the .ico into the final .exe via winresource.
// ============================================================================

const ICON_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256, 512, 1024];
const PROFILE_ICON_PNG: &str = "icon.png";
const PROFILE_ICON_ICO: &str = "icon.ico";
const PROFILE_ICON_ICNS: &str = "icon.icns";

fn icon_src_path(manifest_dir: &Path) -> PathBuf {
    manifest_dir.join("assets").join("icon.svg")
}

fn icon_assets_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("assets")
}

/// Render SVG → BTreeMap<size, png_bytes>.
fn render_svg_to_pngs(svg_path: &Path) -> anyhow::Result<BTreeMap<u32, Vec<u8>>> {
    let svg_data = std::fs::read(svg_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", svg_path.display()))?;
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(&svg_data, &opts)
        .map_err(|e| anyhow::anyhow!("usvg parse: {e}"))?;

    let mut out = BTreeMap::new();
    for &size in ICON_SIZES {
        let scale = size as f32 / 1024.0;
        let pixmap_size = usvg::Size::new(size as f32, size as f32)
            .to_screen_size(scale, 1.0)
            .ok_or_else(|| anyhow::anyhow!("size {size} overflow"))?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height())
            .ok_or_else(|| anyhow::anyhow!("pixmap alloc for {size}"))?;
        let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut encoder = png::Encoder::new(cursor, size, size);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()
                .map_err(|e| anyhow::anyhow!("png write_header {size}: {e}"))?;
            writer.write_image(pixmap.data(), size as usize, size as usize)
                .map_err(|e| anyhow::anyhow!("png write_image {size}: {e}"))?;
        }
        out.insert(size, buf);
    }
    Ok(out)
}

/// Generate icon.png (512x512) at <profile_dir>/assets/icon.png.
fn write_icon_png(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let bytes = pngs.get(&512)
        .ok_or_else(|| anyhow::anyhow!("missing 512px png"))?;
    std::fs::write(assets_dir.join(PROFILE_ICON_PNG), bytes)
        .map_err(|e| anyhow::anyhow!("write icon.png: {e}"))?;
    Ok(())
}

/// Generate icon.ico at <profile_dir>/assets/icon.ico.
fn pack_ico(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    // ICO 格式对 <=256 大小按 0/16/32/48/64/128/256 编码
    let sizes_for_ico: &[u32] = &[16, 24, 32, 48, 64, 128, 256];
    for &size in sizes_for_ico {
        let bytes = pngs.get(&size)
            .ok_or_else(|| anyhow::anyhow!("missing {size}px png"))?;
        let image = ico::IconImage::read_png(bytes.as_slice())
            .map_err(|e| anyhow::anyhow!("ico::read_png {size}: {e}"))?;
        dir.add_entry(ico::IconDirEntry::encode(&image)
            .map_err(|e| anyhow::anyhow!("ico::encode {size}: {e}"))?);
    }
    let file = std::fs::File::create(assets_dir.join(PROFILE_ICON_ICO))
        .map_err(|e| anyhow::anyhow!("create icon.ico: {e}"))?;
    dir.write(BufWriter::new(file))
        .map_err(|e| anyhow::anyhow!("ico::write: {e}"))?;
    Ok(())
}

/// Generate icon.icns at <profile_dir>/assets/icon.icns.
fn pack_icns(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let mut family = icns::IconFamily::new();
    // ICNS type IDs: icp4=16, icp5=32, ic07=128, ic08=256, ic09=512, ic10=1024
    let pairs: &[(u32, icns::IconType)] = &[
        (16, icns::IconType::Icp4),
        (32, icns::IconType::Icp5),
        (64, icns::IconType::Ic06),
        (128, icns::IconType::Ic07),
        (256, icns::IconType::Ic08),
        (512, icns::IconType::Ic09),
        (1024, icns::IconType::Ic10),
    ];
    for &(size, icns_type) in pairs {
        let bytes = pngs.get(&size)
            .ok_or_else(|| anyhow::anyhow!("missing {size}px png"))?;
        family.add_icon_with_data(icns_type, bytes)
            .map_err(|e| anyhow::anyhow!("icns add {size}: {e}"))?;
    }
    let file = std::fs::File::create(assets_dir.join(PROFILE_ICON_ICNS))
        .map_err(|e| anyhow::anyhow!("create icon.icns: {e}"))?;
    family.write(BufWriter::new(file))
        .map_err(|e| anyhow::anyhow!("icns::write: {e}"))?;
    Ok(())
}

/// Top-level: orchestrates icon generation. Called from main().
/// Errors are non-fatal (cargo:warning + return Ok) so builds don't break
/// when SVG is missing or a renderer crate fails on some platform.
fn generate_icons(manifest_dir: &Path, profile_dir: &Path) -> anyhow::Result<()> {
    let svg_path = icon_src_path(manifest_dir);
    println!("cargo:rerun-if-changed={}", svg_path.display());
    println!("cargo:rerun-if-changed={}", profile_dir.join("assets").display());

    if !svg_path.is_file() {
        eprintln!(
            "cargo:warning=icon svg not found at {}; skipping icon generation",
            svg_path.display()
        );
        return Ok(());
    }

    let pngs = match render_svg_to_pngs(&svg_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("cargo:warning=icon svg render failed: {e}");
            return Ok(());
        }
    };

    let assets_dir = icon_assets_dir(profile_dir);
    std::fs::create_dir_all(&assets_dir)
        .map_err(|e| anyhow::anyhow!("mkdir {}: {e}", assets_dir.display()))?;

    if let Err(e) = write_icon_png(&pngs, &assets_dir) {
        eprintln!("cargo:warning=write icon.png failed: {e}");
    }
    if let Err(e) = pack_ico(&pngs, &assets_dir) {
        eprintln!("cargo:warning=pack icon.ico failed: {e}");
    }
    if let Err(e) = pack_icns(&pngs, &assets_dir) {
        eprintln!("cargo:warning=pack icon.icns failed: {e}");
    }

    Ok(())
}

#[cfg(windows)]
fn embed_windows_icon(profile_dir: &Path) {
    let ico = profile_dir.join("assets").join(PROFILE_ICON_ICO);
    if !ico.is_file() {
        eprintln!(
            "cargo:warning=icon.ico missing at {}; skipping winresource embed",
            ico.display()
        );
        return;
    }
    let mut res = winresource::WindowsResource::new();
    if let Err(e) = res.set_icon_path(ico.to_str().unwrap_or("")) {
        eprintln!("cargo:warning=winresource set_icon_path failed: {e}");
        return;
    }
    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=winresource compile failed: {e}");
    }
}
```

- [ ] **Step 3.2: 在 `main()` 末尾追加图标生成调用**

修改 `build.rs` 第 78-98 行的 `main()` 函数，在 `copy_bundle(&src, &dst);` 这一行**之后**追加：

```rust
    // 把"产物目录已就绪"信息告知 cargo cache 系统
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    // 4. MOT icon pipeline (SVG -> PNG/ICO/ICNS)
    generate_icons(&manifest_dir, &profile_dir)
        .unwrap_or_else(|e| eprintln!("cargo:warning=icon pipeline: {e}"));

    // 5. Windows: embed icon.ico into the final .exe
    #[cfg(windows)]
    embed_windows_icon(&profile_dir);
```

注意保留第 97 行原有的 `println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");`，新追加内容放在它后面。

- [ ] **Step 3.3: 本地构建并验证图标产物**

```bash
cargo build --bin mini-oc-gui-serve 2>&1 | tail -20
ls -la target/debug/assets/
```

预期：
- 编译无错误（首次需编译 5 个新 build-deps，~10s）
- `target/debug/assets/` 下存在 `icon.png`、`icon.ico`、`icon.icns`

- [ ] **Step 3.4: 用 ImageMagick 或 macOS Preview 验证 PNG**

macOS:
```bash
qlmanage -t -s 512 -o /tmp target/debug/assets/icon.png
```
Linux:
```bash
file target/debug/assets/icon.png target/debug/assets/icon.ico target/debug/assets/icon.icns
```
预期：PNG 报告 `PNG image data, 512 x 512, 8-bit/color RGBA`。

- [ ] **Step 3.5: Commit**

```bash
git add build.rs
git commit -m "build: implement SVG -> PNG/ICO/ICNS pipeline + Windows embed

generate_icons() reads assets/icon.svg, rasterizes 9 sizes via usvg/resvg,
encodes PNG, packs ICO via ico crate, ICNS via icns crate.
On Windows, embed_windows_icon() burns icon.ico into the .exe via winresource.
Failures degrade gracefully via cargo:warning, never block the build."
```

---

## Task 4: 验证 Windows 嵌入生效（仅在 Windows 主机执行）

**Files:** (无源码修改，仅验证)

- [ ] **Step 4.1: 在 Windows 上重新构建 release**

```powershell
cargo build --release
```

- [ ] **Step 4.2: 检查 .exe 是否带图标**

```powershell
# 用 PowerShell 检查资源
$exe = "target\release\mini-oc-gui-serve.exe"
[System.Drawing.Icon]::ExtractAssociatedIcon($exe)
```

或在资源管理器右键 .exe → 属性 → 详细信息，应该能看到 MOT 图标。

- [ ] **Step 4.3: 如果 Step 4.2 失败**

回退方案：
1. 确认 `target\release\assets\icon.ico` 存在
2. 重新运行 `cargo clean -p mini-oc-gui-serve` 然后重 build
3. 检查 build.rs 输出是否有 `cargo:warning=winresource`

---

## Task 5: 更新 `.gitignore`

**Files:**
- Modify: `.gitignore`

- [ ] **Step 5.1: 追加 build 产物忽略规则**

在 `.gitignore` 末尾追加：

```gitignore

# MOT icon build artifacts (regenerated from assets/icon.svg by build.rs)
assets/icon.png
assets/icon.ico
assets/icon.icns
```

- [ ] **Step 5.2: Commit**

```bash
git add .gitignore
git commit -m "chore: gitignore icon build artifacts

icon.png / icon.ico / icon.icns are generated by build.rs from assets/icon.svg."
```

---

## Task 6: 更新 `README.md`

**Files:**
- Modify: `README.md`

- [ ] **Step 6.1: 在 Features 段落补充图标说明**

定位到 `README.md` 第 5-15 行的 Features 段（在 `- 📡 Syncs \`path-list.md\`` 之后），追加：

```markdown
- 🎨 App icon: 3-letter "MOT" mark, deep-navy background with indigo accent ring; auto-baked into Windows PE resources and shipped as macOS `.icns` / Linux `.png`. SVG source in `assets/icon.svg`.
```

- [ ] **Step 6.2: 在 README 末尾"License"段前追加 macOS .app bundle 集成说明**

定位到 README 第 119 行附近（在 `## License` 之前），追加新章节：

````markdown
## Application icon

The MOT mark is generated at build time from [`assets/icon.svg`](assets/icon.svg) into:
- `target/<profile>/assets/icon.png` — generic 512×512 PNG (Linux desktop, docs)
- `target/<profile>/assets/icon.ico` — Windows multi-resolution, embedded into `.exe` via `winresource`
- `target/<profile>/assets/icon.icns` — macOS multi-resolution

### macOS `.app` bundle integration

`cargo build --release` produces a bare Mach-O binary; to make Finder / Dock
honor the icon, wrap it in a minimal `.app`:

```sh
cargo build --release
mkdir -p MiniOC.app/Contents/{MacOS,Resources}
cp target/release/mini-oc-gui-serve MiniOC.app/Contents/MacOS/
cp target/release/assets/icon.icns MiniOC.app/Contents/Resources/

cat > MiniOC.app/Contents/Info.plist <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>mini-oc-gui-serve</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundleIdentifier</key><string>local.mini-oc-gui-serve</string>
  <key>CFBundleName</key><string>mini-oc-gui-serve</string>
</dict>
</plist>
EOF
```

To change the icon, edit `assets/icon.svg` and rebuild — all platform artifacts regenerate from this single source.

````

- [ ] **Step 6.3: Commit**

```bash
git add README.md
git commit -m "docs(readme): document MOT app icon + macOS .app integration"
```

---

## Task 7: 全链路验证

**Files:** (无源码修改)

- [ ] **Step 7.1: 完整 release 构建**

```bash
cargo build --release 2>&1 | tee /tmp/build.log
tail -20 /tmp/build.log
```

预期：无 error；输出含 `target/release/assets/{icon.png,icon.ico,icon.icns}` 三个文件。

- [ ] **Step 7.2: 三个产物 file 类型校验**

```bash
file target/release/assets/icon.png target/release/assets/icon.ico target/release/assets/icon.icns
```

预期：
- `icon.png`: PNG image data, 512 x 512, 8-bit/color RGBA
- `icon.ico`: MS Windows icon resource
- `icon.icns`: Mac OS X icon image (icns)

- [ ] **Step 7.3: macOS 上 icns 多分辨率校验**

```bash
iconutil -l target/release/assets/icon.icns
```

预期：列出 `ic07` (128)、`ic08` (256)、`ic09` (512)、`ic10` (1024) 等条目。

- [ ] **Step 7.4: 现有 rathole bundle 不回归**

```bash
ls target/release/rathole/bin/macos-aarch64/
ls target/release/rathole/settings/
```

预期：rathole 二进制和 settings 目录存在且完整。

- [ ] **Step 7.5: 增量构建验证**

```bash
# 不改 SVG,cargo 应直接复用 build.rs 缓存
touch build.rs
cargo build --release 2>&1 | grep -E "Compiling mini-oc-gui-serve|Finished" | head -5
```

预期：仅 rebuild build.rs，crate 主体不重编译。

- [ ] **Step 7.6: 修改 SVG 触发重建**

```bash
# 临时改一个像素看是否触发重新生成
echo "" >> assets/icon.svg
cargo build --release 2>&1 | tail -20
# 回滚
git checkout -- assets/icon.svg
```

预期：build.rs 重跑，三个图标产物 mtime 更新。

---

## 自查（实施前最后一道关）

- [ ] ✅ spec 1.1 节全部成功标准覆盖：Windows 嵌入 (Task 4)、macOS icns (Task 3.3)、Linux png (Task 3.3)、单一源 (Task 1)、零 CLI 依赖 (Task 3)、不改 rathole 流水线 (Task 7.4)、增量构建 (Task 7.5/7.6)
- [ ] ✅ spec 5 节所有文件清单已分配到 Task
- [ ] ✅ 占位符扫描：无 "TBD" / "TODO" / "implement later"
- [ ] ✅ 类型一致性：`render_svg_to_pngs` 返回 `BTreeMap<u32, Vec<u8>>` 在 Task 3 调用处一致使用；`pack_ico` / `pack_icns` / `write_icon_png` 接受相同签名
- [ ] ✅ 版本号：用 crates.io 实时最新 stable（spec 写时点是更早版本）

## 执行交接

Plan 已写入 `docs/superpowers/plans/2026-09-11-mot-app-icon.md`。

执行选项：

1. **Subagent-Driven (推荐)** — 每个 Task 派一个独立 subagent、阶段间 review、快速迭代
2. **Inline Execution** — 当前会话直接执行，含 checkpoints

请选择执行方式。
