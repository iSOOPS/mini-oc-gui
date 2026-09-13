//! Build script: 把 `rathole/` bundle 复制到 `target/<profile>/rathole/`。
//!
//! 这样 `cargo build --release` 后,产物自包含:
//! ```text
//! target/release/
//! ├── mini-oc-gui-serve          # 可执行程序
//! ├── path-list-actor             # 可执行程序
//! └── rathole/                    # <- 本脚本生成
//!     ├── bin/<os>/rathole        #   当前平台的 rathole 二进制
//!     └── settings/                #   rathole 配置目录
//! ```
//!
//! 运行时 `src/serve/rathole.rs::default_bin` 会先查可执行文件同目录的
//! `rathole/bin/<os>/rathole`,命中即用 — 这样发布物完全脱离源码树。
//!
//! # 为什么放 `target/<profile>/` 而不是 `$OUT_DIR`?
//! `$OUT_DIR` 是 `target/<profile>/build/<crate>-<hash>/out`,其内容每次
//! cargo 都会按哈希改路径重建,而最终交付产物位于 `target/<profile>/`,
//! 所以这里把 bundle 放到 OUT_DIR 的祖父级 — 同一目录。
//!
//! # 何时重新复制?
//! 通过 `cargo:rerun-if-changed` 监听源端 `rathole/bin/` 与
//! `rathole/settings/` 下的文件变化;只在源端变动时执行复制,避免
//! 每次增量编译都白白 fs copy 大文件。

use std::collections::BTreeMap;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// 源端 rathole bundle 根目录(<CARGO_MANIFEST_DIR>/rathole)。
fn src_rathole_root(manifest_dir: &Path) -> PathBuf {
    manifest_dir.join("rathole")
}

/// 目标 bundle 根目录(`target/<profile>/rathole/`)。
///
/// OUT_DIR 形如 `<target_dir>/<profile>/build/<pkg>-<hash>/out`,
/// 其父级的父级就是 `<target_dir>/<profile>/`。
fn dst_rathole_root(out_dir: &Path) -> PathBuf {
    // <target>/<profile>/build/<pkg>-<hash>/out
    //                ^^^^^ 这层上去两级
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR should be at least 3 levels deep under <target>/<profile>");
    profile_dir.join("rathole")
}

/// 当前构建目标的 rathole bundle 子目录(`<os>-<arch>`)。
///
/// 按 Rust 目标三元组对齐 — 同时区分 OS 和 CPU 架构,使同一平台下
/// 不同 ABI/arch 可以并存多个变体(例如以后加入 `linux-x86_64-gnu`、
/// `macos-x86_64` 等)。
fn bin_subdir() -> &'static str {
    // `cfg!(...)` 是 bool,无法直接进 match guard 的字符串比较,
    // 用常量 + 嵌套 if 表达同样的 `<os>-<arch>` 选择。
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        _ => "unknown",
    }
}

/// 当前构建目标的 rathole 二进制文件名。
fn bin_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "rathole.exe"
    } else {
        "rathole"
    }
}

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo"),
    );
    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").expect("OUT_DIR set by cargo for build scripts"),
    );

    let src = src_rathole_root(&manifest_dir);
    let dst = dst_rathole_root(&out_dir);

    // profile_dir = target/<profile>/ (OUT_DIR 的祖父级)
    let profile_dir = dst
        .parent()
        .expect("dst should have a parent (target/<profile>/)");

    // 触发条件:监听源端 rathole/ 下所有相关文件
    println!("cargo:rerun-if-changed={}", src.join("bin").display());
    println!("cargo:rerun-if-changed={}", src.join("settings").display());

    // 任何 IO 失败立即 panic —— build.rs 必须明确报错
    copy_bundle(&src, &dst);

    // 把"产物目录已就绪"信息告知 cargo cache 系统
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");

    // MOT icon pipeline (SVG -> PNG/ICO/ICNS)
    if let Err(e) = generate_icons(&manifest_dir, &profile_dir) {
        eprintln!("cargo:warning=icon pipeline: {e}");
    }

    // Windows: embed icon.ico into the final .exe
    #[cfg(windows)]
    embed_windows_icon(&profile_dir);
}

/// 把 `src/rathole/` 整个 bundle(只挑当前平台的 binary + 全部 settings)
/// 复制到 `dst/rathole/`。
fn copy_bundle(src: &Path, dst: &Path) {
    if !src.is_dir() {
        // 没有 bundle 目录:跳过 —— release 产物将沿用源码 CWD/向上搜索策略。
        // 不 panic:允许开发者在没有 rathole bundle 的环境下构建。
        eprintln!(
            "cargo:warning=源端 rathole bundle 不存在 ({});跳过复制。运行时将沿用源码搜索。",
            src.display()
        );
        return;
    }

    fs::create_dir_all(dst).expect("create dst rathole/ root");

    // 1. 当前平台的 binary
    let bin_name = bin_filename();
    let sub = bin_subdir();
    let src_bin = src.join("bin").join(sub).join(bin_name);
    if src_bin.is_file() {
        let dst_bin = dst.join("bin").join(sub).join(bin_name);
        fs::create_dir_all(dst_bin.parent().unwrap()).expect("create dst bin/<os>/");
        copy_with_mode(&src_bin, &dst_bin);
        eprintln!(
            "cargo:warning=bundled rathole: {} -> {}",
            src_bin.display(),
            dst_bin.display()
        );
    } else {
        eprintln!(
            "cargo:warning=当前平台 ({sub}) 的 rathole binary 不存在: {};跳过。",
            src_bin.display()
        );
    }

    // 2. settings/(完整目录;运行时 TUI 设置面板会就地更新 global.toml)
    let src_settings = src.join("settings");
    if src_settings.is_dir() {
        let dst_settings = dst.join("settings");
        fs::create_dir_all(&dst_settings).expect("create dst settings/");
        copy_dir(&src_settings, &dst_settings);
        eprintln!(
            "cargo:warning=bundled rathole settings: {} -> {}",
            src_settings.display(),
            dst_settings.display()
        );
    }
}

/// 复制单个文件并保留 Unix 文件模式(可执行位)。
#[cfg(unix)]
fn copy_with_mode(src: &Path, dst: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::copy(src, dst).expect("copy file");
    let mode = fs::metadata(src)
        .expect("stat src")
        .permissions()
        .mode();
    fs::set_permissions(dst, fs::Permissions::from_mode(mode)).expect("chmod");
}

#[cfg(not(unix))]
fn copy_with_mode(src: &Path, dst: &Path) {
    fs::copy(src, dst).expect("copy file");
}

/// 递归复制整个目录(覆盖写),跳过 macOS 元数据等无用文件。
fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create dst dir");
    for entry in fs::read_dir(src).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // 跳过 macOS `.DS_Store` / Windows `Thumbs.db` / `desktop.ini` 等平台垃圾
        if matches!(name_str.as_ref(), ".DS_Store" | "Thumbs.db" | "desktop.ini") {
            continue;
        }
        let src_child = entry.path();
        let dst_child = dst.join(&name);
        let ft = entry.file_type().expect("file_type");
        if ft.is_dir() {
            copy_dir(&src_child, &dst_child);
        } else if ft.is_file() {
            copy_with_mode(&src_child, &dst_child);
        }
    }
}

// ============================================================================
// Icon generation pipeline (MOT)
// Reads assets/icon.svg and produces:
//   <profile_dir>/assets/icon.png  (512x512)
//   <profile_dir>/assets/icon.ico  (Windows multi-res)
//   <profile_dir>/assets/icon.icns (macOS multi-res)
// On Windows, also embeds the .ico into the final .exe via winresource.
// Failures are non-fatal: emit cargo:warning and return Ok so a missing SVG
// or transient renderer error never breaks the build.
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

/// Render SVG → BTreeMap<size, png_bytes> for all ICON_SIZES.
fn render_svg_to_pngs(svg_path: &Path) -> anyhow::Result<BTreeMap<u32, Vec<u8>>> {
    let svg_data = std::fs::read(svg_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", svg_path.display()))?;
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(&svg_data, &opts)
        .map_err(|e| anyhow::anyhow!("usvg parse: {e}"))?;

    let mut out = BTreeMap::new();
    for &size in ICON_SIZES {
        // resvg expects usvg::Size (logical size), then we apply a scale transform.
        let pixmap_size = resvg::tiny_skia::IntSize::from_wh(size, size)
            .ok_or_else(|| anyhow::anyhow!("invalid pixmap size {size}"))?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(pixmap_size.width(), pixmap_size.height())
            .ok_or_else(|| anyhow::anyhow!("pixmap alloc for {size} failed"))?;
        // Scale: SVG is 1024 logical units; we want the rendered output at `size` physical px.
        let scale = size as f32 / 1024.0;
        let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        if pixmap.data().iter().all(|&p| p == 0) {
            return Err(anyhow::anyhow!("resvg render produced empty pixmap for size {size}"));
        }
        let mut buf: Vec<u8> = Vec::new();
        {
            let cursor = std::io::Cursor::new(&mut buf);
            let mut encoder = png::Encoder::new(cursor, size, size);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header()
                .map_err(|e| anyhow::anyhow!("png write_header {size}: {e}"))?;
            writer.write_image_data(pixmap.data())
                .map_err(|e| anyhow::anyhow!("png write_image {size}: {e}"))?;
        }
        out.insert(size, buf);
    }
    Ok(out)
}

/// Write icon.png (512x512) at <profile_dir>/assets/icon.png.
fn write_icon_png(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let bytes = pngs.get(&512)
        .ok_or_else(|| anyhow::anyhow!("missing 512px png"))?;
    std::fs::write(assets_dir.join(PROFILE_ICON_PNG), bytes)
        .map_err(|e| anyhow::anyhow!("write icon.png: {e}"))?;
    Ok(())
}

/// Pack icon.ico at <profile_dir>/assets/icon.ico with sizes <=256.
fn pack_ico(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    // ICO format only supports up to 256x256; pick standard sizes.
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

/// Pack icon.icns at <profile_dir>/assets/icon.icns.
fn pack_icns(pngs: &BTreeMap<u32, Vec<u8>>, assets_dir: &Path) -> anyhow::Result<()> {
    let mut family = icns::IconFamily::new();
    // Use from_pixel_size_and_density to find the right icns::IconType for each size.
    // For 1024px (ic10), density=2 since the base type is 512@2x = 1024.
    let pairs: &[(u32, u32, u32)] = &[
        // (width, height, density)
        (16, 16, 1),
        (32, 32, 1),
        (64, 64, 1),
        (128, 128, 1),
        (256, 256, 1),
        (512, 512, 1),
        (1024, 1024, 2), // ic10 = 512@2x
    ];
    for &(w, h, d) in pairs {
        let bytes = pngs.get(&w)
            .ok_or_else(|| anyhow::anyhow!("missing {w}px png"))?;
        let icon_type = icns::IconType::from_pixel_size_and_density(w, h, d)
            .ok_or_else(|| anyhow::anyhow!("no icns type for {w}x{h}@{d}"))?;
        let image = icns::Image::read_png(std::io::Cursor::new(bytes.as_slice()))
            .map_err(|e| anyhow::anyhow!("icns Image::read_png {w}x{h}: {e}"))?;
        family.add_icon_with_type(&image, icon_type)
            .map_err(|e| anyhow::anyhow!("icns add {w}x{h}: {e}"))?;
    }
    let file = std::fs::File::create(assets_dir.join(PROFILE_ICON_ICNS))
        .map_err(|e| anyhow::anyhow!("create icon.icns: {e}"))?;
    family.write(BufWriter::new(file))
        .map_err(|e| anyhow::anyhow!("icns::write: {e}"))?;
    Ok(())
}

/// Top-level orchestrator. Errors degrade to cargo:warning, never panic.
fn generate_icons(manifest_dir: &Path, profile_dir: &Path) -> anyhow::Result<()> {
    let svg_path = icon_src_path(manifest_dir);
    println!("cargo:rerun-if-changed={}", svg_path.display());
    println!("cargo:rerun-if-changed={}", manifest_dir.join("assets").display());

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
    let ico_str = match ico.to_str() {
        Some(s) => s,
        None => {
            eprintln!("cargo:warning=icon.ico path is not valid UTF-8");
            return;
        }
    };
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_str);
    if let Err(e) = res.compile() {
        eprintln!("cargo:warning=winresource compile failed: {e}");
    }
}