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

use std::fs;
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

    // 触发条件:监听源端 rathole/ 下所有相关文件
    println!("cargo:rerun-if-changed={}", src.join("bin").display());
    println!("cargo:rerun-if-changed={}", src.join("settings").display());

    // 任何 IO 失败立即 panic —— build.rs 必须明确报错
    copy_bundle(&src, &dst);

    // 把"产物目录已就绪"信息告知 cargo cache 系统
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
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