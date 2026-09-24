//! 内嵌应用图标(MOT):编译期由 build.rs 从 `assets/icon.svg` 生成,经
//! `$OUT_DIR/assets/` 以 `include_bytes!` 打进二进制 —— 单文件分发也不丢
//! 图标。启动时把缺失的图标释放到可执行文件同目录的 `assets/` 下,
//! 与 rathole bundle 的 exe-adjacent 布局保持一致。
//!
//! 生成管线降级(SVG 缺失/渲染失败)时,build.rs 会写空占位文件,
//! `include_bytes!` 嵌入的是空字节 —— 释放逻辑跳过空字节,行为退化为
//! 「无内嵌图标」,不影响启动。

use std::path::{Path, PathBuf};

/// 内嵌图标清单:(`assets/` 下文件名, 字节)。
pub const EMBEDDED_ICONS: &[(&str, &[u8])] = &[
    ("icon.png", include_bytes!(concat!(env!("OUT_DIR"), "/assets/icon.png"))),
    ("icon.ico", include_bytes!(concat!(env!("OUT_DIR"), "/assets/icon.ico"))),
    ("icon.icns", include_bytes!(concat!(env!("OUT_DIR"), "/assets/icon.icns"))),
];

/// build.rs 传入的图标内容 hash。OUT_DIR 路径稳定,没有它 rustc 会在图标
/// 字节变化时继续用缓存的旧字节;此常量仅在日志中输出以建立编译依赖。
pub const ICON_HASH: &str = env!("MINI_OC_GUI_ICON_HASH");

/// 把 `icons` 释放到 `dir`:只为「字节非空且目标不存在」的条目建文件。
/// 已存在的文件不动(允许用户自定义图标);返回实际写出的路径。
///
/// # Errors
/// 目录创建或文件写入失败时返回 `std::io::Error`。
pub fn export_icons_to(dir: &Path, icons: &[(&str, &[u8])]) -> std::io::Result<Vec<PathBuf>> {
    let pending: Vec<(&str, &[u8])> = icons
        .iter()
        .copied()
        .filter(|&(_, bytes)| !bytes.is_empty())
        .filter(|&(name, _)| !dir.join(name).exists())
        .collect();
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    std::fs::create_dir_all(dir)?;
    pending
        .iter()
        .map(|&(name, bytes)| {
            let dst = dir.join(name);
            std::fs::write(&dst, bytes).map(|()| dst)
        })
        .collect()
}

/// 释放 [`EMBEDDED_ICONS`] 到可执行文件同目录的 `assets/`。
/// `current_exe()` 不可用(部分测试环境)时静默跳过。
///
/// # Errors
/// 同 [`export_icons_to`]。
pub fn export_default() -> std::io::Result<Vec<PathBuf>> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let Some(exe_dir) = exe_dir else {
        return Ok(Vec::new());
    };
    let written = export_icons_to(&exe_dir.join("assets"), EMBEDDED_ICONS)?;
    tracing::debug!(hash = ICON_HASH, written = written.len(), "embedded icons ensured");
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_icons() -> Vec<(&'static str, &'static [u8])> {
        vec![("icon.png", b"PNG-bytes"), ("icon.icns", b"ICNS-bytes")]
    }

    #[test]
    fn export_icons_to_creates_missing_files_when_dir_empty() {
        // Given: 空 tempdir 与两枚非空图标
        let dir = tempfile::tempdir().expect("tempdir");
        let icons = fixture_icons();

        // When
        let written = export_icons_to(dir.path(), &icons).expect("export ok");

        // Then: 两个文件按原始字节写出
        assert_eq!(written.len(), 2);
        assert_eq!(fs::read(dir.path().join("icon.png")).unwrap(), b"PNG-bytes");
        assert_eq!(fs::read(dir.path().join("icon.icns")).unwrap(), b"ICNS-bytes");
    }

    #[test]
    fn export_icons_to_keeps_existing_files_when_already_present() {
        // Given: icon.png 已存在(用户自定义的 sentinel 字节)
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("icon.png"), b"custom-icon").unwrap();
        let icons = fixture_icons();

        // When
        let written = export_icons_to(dir.path(), &icons).expect("export ok");

        // Then: 只补写缺失的 icns,已有 png 原样保留
        assert_eq!(written, vec![dir.path().join("icon.icns")]);
        assert_eq!(fs::read(dir.path().join("icon.png")).unwrap(), b"custom-icon");
    }

    #[test]
    fn export_icons_to_skips_empty_placeholder_bytes_when_pipeline_degraded() {
        // Given: 生成管线降级,嵌入字节为空占位
        let dir = tempfile::tempdir().expect("tempdir");
        let empty: &[u8] = b"";
        let icons = vec![("icon.ico", empty), ("icon.png", b"PNG-bytes".as_slice())];

        // When
        let written = export_icons_to(dir.path(), &icons).expect("export ok");

        // Then: 空占位不落盘,不创建空文件
        assert_eq!(written, vec![dir.path().join("icon.png")]);
        assert!(!dir.path().join("icon.ico").exists());
    }
}
