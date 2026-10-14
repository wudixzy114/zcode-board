//! 时间与 id 之类的小工具。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前时间的毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成一个短 id。形如 `k18f3a9b2c01-7`，保证同进程内唯一。
pub fn new_id(prefix: char) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}{:x}-{}", now_ms(), n)
}

/// 把 `Option<String>` 去掉首尾空白，空串视为 None。
pub fn trim_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}
