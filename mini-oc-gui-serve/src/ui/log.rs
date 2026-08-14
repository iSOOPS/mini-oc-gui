//! 共享日志缓冲区：tracing 写入、TUI 日志面板渲染。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// 内存中保留的最大日志行数。
const MAX_LOG_LINES: usize = 500;

/// 线程安全的环形日志缓冲区。
///
/// 同时实现 [`std::io::Write`] 与 [`MakeWriter`]，可直接作为
/// `tracing_subscriber::fmt` 的 writer，把日志收集到内存供 TUI 渲染。
#[derive(Clone)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// 追加一行，超出容量时丢弃最旧的行。
    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        lines.push_back(line);
        while lines.len() > MAX_LOG_LINES {
            lines.pop_front();
        }
    }

    /// 返回最近 `n` 行（按时间顺序，最旧的在前）。
    #[must_use]
    pub fn tail(&self, n: usize) -> Vec<String> {
        let lines = self.lines.lock().unwrap();
        lines.iter().rev().take(n).rev().cloned().collect()
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        for line in text.lines() {
            self.push(line.to_string());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
