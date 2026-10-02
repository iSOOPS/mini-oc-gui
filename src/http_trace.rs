//! 外部 HTTP 请求日志 —— 把所有出站请求的 URL / 请求体 / 响应状态 /
//! 响应体统一打印到 tracing 日志（TUI 模式进日志面板，`--no-tui` 模式
//! 走 stderr）。
//!
//! 接入点覆盖了本程序**全部**出站 HTTP 请求：
//! - [`crate::account::fetch_user_info`] / [`crate::account::bind_device`]
//!   （账户中心 `/api/user/info`、`/api/device-bind`）
//! - [`crate::storage::remote::RemoteClient`] 的 `send_req` / `do_login`
//!   （SilverBullet `/.fs/*` 文件同步与 `/.auth` 登录）
//! - [`crate::attach::OpencodeClient`]（opencode serve HTTP API）
//!
//! 请求体 / 响应体超过 [`EXCERPT_CHARS`] 字符时截断 —— path-list.md 约
//! 8 KB，全量打印会刷爆 TUI 日志面板的 500 行环形缓冲
//! （[`crate::ui::log::LogBuffer`]）。

/// 请求体 / 响应体日志截断长度（按字符计）。
const EXCERPT_CHARS: usize = 800;

/// 截断请求体 / 响应体：保留前 [`EXCERPT_CHARS`] 个字符，超长时追加
/// 总长度提示。
fn excerpt(body: &str) -> String {
    let mut s: String = body.chars().take(EXCERPT_CHARS).collect();
    if s.chars().count() < body.chars().count() {
        s.push_str(&format!("…(共 {} 字符，已截断)", body.chars().count()));
    }
    s
}

/// 打印一条出站请求：方法 + URL +（可选）请求体。
///
/// `body` 为 `None` 或空串时只打印方法与 URL。
pub fn log_request(method: &str, url: &str, body: Option<&str>) {
    match body {
        Some(b) if !b.is_empty() => {
            tracing::info!("[外部请求→] {method} {url} body={}", excerpt(b));
        }
        _ => tracing::info!("[外部请求→] {method} {url}"),
    }
}

/// 打印一条出站响应：方法 + URL + HTTP 状态码 + 响应体（截断）。
///
/// 网络层失败（从未拿到 HTTP 响应）用 [`log_response_error`]。
pub fn log_response(method: &str, url: &str, status: u16, body: &str) {
    tracing::info!(
        "[外部请求←] {method} {url} → HTTP {status} body={}",
        excerpt(body)
    );
}

/// 打印一条出站请求的网络层失败（连接拒绝 / 超时等，无 HTTP 状态码）。
pub fn log_response_error(method: &str, url: &str, err: &str) {
    tracing::warn!("[外部请求←] {method} {url} → 网络错误: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_keeps_short_body_intact() {
        assert_eq!(excerpt("hello"), "hello");
        // 空串保持为空。
        assert_eq!(excerpt(""), "");
    }

    #[test]
    fn excerpt_truncates_long_body_with_hint() {
        let long = "x".repeat(EXCERPT_CHARS + 10);
        let out = excerpt(&long);
        assert!(out.starts_with(&"x".repeat(EXCERPT_CHARS)));
        assert!(out.contains("已截断"));
        assert!(out.contains(&format!("共 {} 字符", EXCERPT_CHARS + 10)));
    }
}
