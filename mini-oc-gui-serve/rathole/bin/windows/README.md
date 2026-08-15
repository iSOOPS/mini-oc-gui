# Windows 版 rathole 二进制占位

此目录用于存放 **Windows 版** `rathole.exe`，供 `mini-oc-gui-serve` 在 Windows 平台启动内网穿透隧道时使用。

## 当前状态

**尚未放置 `rathole.exe`。** 当前仓库只携带了 macOS (aarch64-apple-darwin) 版二进制，位于 `../macos/rathole`。

## 获取方式

`rathole` 依赖 `tokio-native-tls`（原生 OpenSSL），无法在 macOS 上直接交叉编译到 Windows。请在 Windows 机器或 CI 上自行构建：

```powershell
# 在 rathole 源码目录（rathole-main，v0.4.7）下：
cargo build --release
# 产物：target/release/rathole.exe
```

构建完成后，将 `rathole.exe` 复制到此目录（`rathole/bin/windows/rathole.exe`）。

## 路径约定

`serve` 代码通过 `cfg(target_os)` 自动选择平台二进制：

| 平台 | 二进制相对路径（相对 mini-oc-gui-serve 运行目录） |
|------|------|
| macOS / Linux | `rathole/bin/macos/rathole` |
| Windows | `rathole/bin/windows/rathole.exe` |

也可通过环境变量 `RATHOLE_BIN` 覆盖默认路径。
