//! zcode（Z.ai / GLM Coding Plan）看板的共享数据模型。
//!
//! 该 crate 被服务端（`zcode-core` / `zcode-server`）与 WASM 前端（`zcode-web`）共同依赖，
//! 因此**只允许依赖 wasm 安全的纯 Rust crate**（serde / serde_json）。

use serde::{Deserialize, Serialize};

// ────────────────────────────── 基础枚举 ──────────────────────────────

/// 单个凭证（API Key）的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    /// 从未探测过。
    #[default]
    Idle,
    /// 探测成功，拿到了额度数据。
    Ok,
    /// 上游明确告知该账号没有 coding plan（中性状态，非故障）。
    NoPlan,
    /// 探测失败（网络 / 鉴权 / 解析）。
    Error,
    /// 被上游限流，冷却中。
    Cooling,
    /// 手动禁用。
    Disabled,
}

impl KeyStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "待探测",
            Self::Ok => "正常",
            Self::NoPlan => "无 coding plan",
            Self::Error => "异常",
            Self::Cooling => "限流冷却",
            Self::Disabled => "已禁用",
        }
    }

    /// 用于前端徽章配色的语义分类。
    pub fn tone(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            // 无 plan / 待探测 / 已停用都是中性灰
            Self::NoPlan | Self::Idle | Self::Disabled => "muted",
            Self::Cooling => "warn",
            Self::Error => "bad",
        }
    }
}

// ────────────────────────────── 持久化记录 ──────────────────────────────

/// 一条凭证记录（持久化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRecord {
    pub id: String,
    /// 用户自定义备注名。
    pub label: String,
    /// 凭证原文（智谱 API Key）。本地单机工具，明文存放在数据目录。
    pub secret: String,
    pub enabled: bool,
    /// 归属账户 id；由归组逻辑写入，也可被手动合并覆盖。
    pub account_id: Option<String>,
    /// 本地 zcode 扫描导入时的归组线索（zcode 账户 ID / provider id）。
    #[serde(default)]
    pub group_hint: Option<String>,
    /// 用户是否手动固定了归属（为 true 时自动归组不再改写）。
    pub account_pinned: bool,
    pub created_at_ms: i64,
}

/// 账户记录（持久化）。同一账户下的多个 key 归到一起。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: String,
    /// 展示名；为空时用首个 key 的备注名兜底。
    pub name: Option<String>,
    /// 归组标签，例如 `zcode-provider:bigmodel-standard-api-2`。
    pub group_tag: Option<String>,
    /// 上一次已经提醒过的重置时间戳，避免重复提醒。
    pub last_notified_reset_ms: Option<i64>,
    /// 该账户静音（不参与桌面 / 页面提醒）。
    #[serde(default)]
    pub notify_muted: bool,
    pub created_at_ms: i64,
}

// ────────────────────────────── 配额快照 ──────────────────────────────

/// 上游 `data.limits[]` 中的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaLimit {
    /// 例如 `TOKENS_LIMIT` / `TIME_LIMIT`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 已使用百分比（0–100，上游可能给 0–1 之外的形态，读取时已归一）。
    pub percentage: Option<f64>,
    /// 窗口单位编码，如 3 / 5 / 6。
    pub unit: Option<u32>,
    /// 窗口序号（同一 type 下可能有多个窗口）。
    pub number: Option<u32>,
    /// 下次重置时间（毫秒时间戳）。
    #[serde(rename = "nextResetTime")]
    pub next_reset_time_ms: Option<i64>,
    /// 总额度（`TIME_LIMIT` 会给出，如 4000 次）。
    pub usage: Option<f64>,
    /// 已用量。
    pub current_value: Option<f64>,
    /// 剩余量。
    pub remaining: Option<f64>,
    /// 按模型的用量明细。
    pub usage_details: Vec<UsageDetail>,
    /// 部分上游会带模型名。
    pub model: Option<String>,
    pub used: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
}

/// 单模型的用量明细。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageDetail {
    pub model_code: String,
    pub usage: f64,
}

/// 单个 key 的一次探测结果。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QuotaSnapshot {
    /// 是否成功拿到额度数据。
    pub ok: bool,
    /// 上游明确告知「该账号没有 coding plan」，这不是故障，只是没额度可看。
    #[serde(default)]
    pub no_plan: bool,
    pub error: Option<String>,
    pub limits: Vec<QuotaLimit>,
    pub fetched_at_ms: i64,
    /// 套餐档位，如 `max`。
    pub level: Option<String>,
    /// 原始响应体（诊断用，仅保留最近一次）。
    pub raw: Option<serde_json::Value>,
}

/// key 的运行时状态（不持久化，由内存状态机持有）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeyRuntime {
    pub status: KeyStatus,
    pub quota: Option<QuotaSnapshot>,
    pub last_error: Option<String>,
    pub last_checked_ms: Option<i64>,
    pub cooling_until_ms: Option<i64>,
    pub fail_streak: u32,
}

// ────────────────────────────── 前端视图 ──────────────────────────────

/// 配额窗口视图：一个账户下所有 key 的同类型窗口聚合后的展示单元。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowView {
    /// 稳定标识，如 `TOKENS_LIMIT:3`。
    pub key: String,
    /// 人类可读窗口名，如「5 小时窗口」。
    pub label: String,
    pub kind: String,
    /// 已使用百分比（取该账户下所有 key 的最大值，代表最紧的窗口）。
    pub used_percent: f64,
    /// 已用量（若上游给出）。
    pub used: Option<f64>,
    /// 总量（若上游给出）。
    pub total: Option<f64>,
    /// 剩余量（若上游给出）。
    pub remaining: Option<f64>,
    /// 最近一次重置时间（毫秒）。
    pub next_reset_ms: Option<i64>,
    /// 窗口总时长（毫秒），用于绘制时间进度；无法推断时为 None。
    pub window_ms: Option<i64>,
    /// 参与聚合的 key 数。
    pub key_count: usize,
    /// 各 key 的百分比明细（用于展开查看）。
    pub per_key: Vec<WindowKeyPercent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowKeyPercent {
    pub key_id: String,
    pub key_label: String,
    pub used_percent: f64,
}

/// 前端展示的单个 key。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyView {
    pub id: String,
    pub label: String,
    /// 脱敏后的凭证。
    pub masked: String,
    /// 凭证明文（仅本机工具使用，界面用来做「已导入 / 已失效」比对）。
    pub secret: String,
    pub enabled: bool,
    pub status: KeyStatus,
    pub status_label: String,
    pub last_error: Option<String>,
    pub last_checked_ms: Option<i64>,
    pub quota: Option<QuotaSnapshot>,
    pub account_id: String,
    pub account_pinned: bool,
    /// 本地 zcode 扫描导入时的归组线索（手动粘贴的 key 为 None）。
    /// 界面据此区分「本机同步来的 key」和「手动添加的 key」，
    /// 一键清理只动前者。
    pub source_group: Option<String>,
}

/// 前端展示的账户（聚合视图）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
    pub id: String,
    pub name: String,
    /// 该账户下的所有 key。
    pub keys: Vec<KeyView>,
    /// 聚合后的配额窗口。
    pub windows: Vec<WindowView>,
    /// 该账户最近一次「配额刷新」时间（毫秒）。
    pub next_reset_ms: Option<i64>,
    /// 账户整体状态（取最差的一个 key）。
    pub status: KeyStatus,
    pub status_label: String,
    pub last_checked_ms: Option<i64>,
    pub last_error: Option<String>,
    /// 该账户是否静音提醒。
    pub notify_muted: bool,
    /// 疑似同一账户的分组标记。
    ///
    /// 不同账户的配额快照完全一致（窗口类型、用量、百分比、重置时间都相同）时，
    /// 极可能是同一账号的多个 key。据此给出合并建议。
    pub twin_group: Option<String>,
}

/// 一条系统事件（提醒 / 错误），用于页面右上角消息中心。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventItem {
    pub id: u64,
    pub at_ms: i64,
    pub level: String, // info | warn | bad
    pub title: String,
    pub body: String,
}

/// 全局设置（持久化 + 前端可编辑）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 自动刷新间隔（秒）。0 表示关闭自动刷新。
    pub refresh_interval_secs: u64,
    /// 并发探测上限。
    pub concurrency: usize,
    /// 单次请求超时（秒）。
    pub request_timeout_secs: u64,
    /// 限流冷却时长（秒）。
    pub cooling_secs: u64,
    /// 是否启用「下一次刷新时间」提醒。
    pub notify_enabled: bool,
    /// 提前多少分钟提醒。
    pub notify_lead_minutes: u32,
    /// 是否发送桌面系统通知。
    pub notify_desktop: bool,
    /// 额度查询端点。
    pub quota_endpoint: String,
    /// 附带的自定义请求头（每行 `Header-Name: value`），用于调试或兼容上游变更。
    pub extra_headers: String,
    /// 开机自动启动（写入当前用户 Run 注册表项）。
    #[serde(default = "default_true")]
    pub start_with_windows: bool,
    /// 点关闭按钮时最小化到托盘而不是退出。
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
}

/// 新功能的默认值：默认开启（老配置文件没有这两个字段时也希望是开的）。
fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 60,
            concurrency: 6,
            request_timeout_secs: 15,
            cooling_secs: 300,
            notify_enabled: true,
            notify_lead_minutes: 5,
            notify_desktop: true,
            quota_endpoint: "https://api.z.ai/api/monitor/usage/quota/limit".to_string(),
            extra_headers: String::new(),
            start_with_windows: true,
            minimize_to_tray: true,
        }
    }
}

/// 前端拿到的完整状态快照。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppState {
    /// 服务端当前时间（毫秒），前端据此计算倒计时。
    pub server_time_ms: i64,
    pub accounts: Vec<AccountView>,
    pub settings: Settings,
    /// 下一次自动刷新的时间戳（毫秒）。
    pub next_auto_refresh_ms: Option<i64>,
    /// 正在刷新中的 key 数。
    pub refreshing: usize,
    pub events: Vec<EventItem>,
}

// ────────────────────────────── 请求 / 响应 DTO ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddKeyRequest {
    /// 可一次添加多行，每行一个凭证。
    pub secrets: String,
    /// 可选备注名（单行添加时生效）。
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateKeyRequest {
    pub label: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveKeyRequest {
    /// 目标账户 id；为空表示「解绑，重新自动归组」。
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameAccountRequest {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResponse {
    pub scheduled: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiOk<T> {
    pub data: T,
}

/// 通用错误响应体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

// ────────────────────────────── 工具函数 ──────────────────────────────

/// 凭证脱敏：保留前 6 与后 4 位。
pub fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 14 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 把上游窗口单位编码映射为人类可读名与展示顺序。
pub fn window_meta(kind: &str, unit: Option<u32>) -> (String, String, u32) {
    // key / label / sort
    let unit_key = unit
        .map(|u| u.to_string())
        .unwrap_or_else(|| "-".to_string());
    let key = format!("{kind}:{unit_key}");
    let label = match (kind, unit) {
        ("TOKENS_LIMIT", Some(3)) => "5 小时 Token 窗口".to_string(),
        ("TOKENS_LIMIT", Some(6)) => "周 Token 窗口".to_string(),
        ("TOKENS_LIMIT", Some(u)) => format!("Token 窗口 · unit {u}"),
        ("TOKENS_LIMIT", None) => "Token 窗口".to_string(),
        // unit 5 的 TIME_LIMIT 实测约 29 天重置一次，即月度窗口
        ("TIME_LIMIT", Some(5)) => "月度调用窗口".to_string(),
        ("TIME_LIMIT", Some(3)) => "5 小时调用窗口".to_string(),
        ("TIME_LIMIT", Some(6)) => "周调用窗口".to_string(),
        ("TIME_LIMIT", Some(u)) => format!("调用窗口 · unit {u}"),
        ("TIME_LIMIT", None) => "调用窗口".to_string(),
        (other, Some(u)) => format!("{other} · unit {u}"),
        (other, None) => other.to_string(),
    };
    let sort = match (kind, unit) {
        ("TOKENS_LIMIT", Some(3)) => 0,
        ("TOKENS_LIMIT", Some(6)) => 1,
        ("TIME_LIMIT", Some(3)) => 2,
        ("TIME_LIMIT", Some(6)) => 3,
        (_, Some(u)) => 10 + u,
        (_, None) => 99,
    };
    (key, label, sort)
}

/// 推断窗口总时长（毫秒）。unit 3 → 5 小时，unit 6 → 7 天。
pub fn window_duration_ms(_kind: &str, unit: Option<u32>) -> Option<i64> {
    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;
    match unit {
        Some(3) => Some(5 * HOUR),
        Some(6) => Some(7 * DAY),
        _ => None,
    }
}

/// 上游百分比可能是 0–100 或 0–1，统一归一到 0–100。
pub fn normalize_percent(raw: f64) -> f64 {
    if raw > 0.0 && raw <= 1.0 {
        raw * 100.0
    } else {
        raw
    }
    .clamp(0.0, 100.0)
}

/// 把毫秒时间戳格式化成 `HH:MM:SS` 的剩余时长；已过期返回 `00:00:00`。
pub fn format_countdown(delta_ms: i64) -> String {
    let total = (delta_ms / 1000).max(0);
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_short_secret() {
        assert_eq!(mask_secret("abc"), "***");
    }

    #[test]
    fn mask_long_secret() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        let m = mask_secret(s);
        assert!(m.starts_with("abcdef"));
        assert!(m.ends_with("wxyz"));
    }

    #[test]
    fn percent_normalization() {
        assert_eq!(normalize_percent(0.42), 42.0);
        assert_eq!(normalize_percent(42.0), 42.0);
        assert_eq!(normalize_percent(120.0), 100.0);
    }

    #[test]
    fn countdown_format() {
        assert_eq!(format_countdown(3_661_000), "01:01:01");
        assert_eq!(format_countdown(-5), "00:00:00");
    }

    #[test]
    fn settings_tray_and_autostart_default_on() {
        // 老版本配置文件没有这两个字段：反序列化时应默认开启，
        // 升级用户无感获得托盘常驻 + 开机启动。
        let json = r#"{
            "refresh_interval_secs": 60,
            "concurrency": 4,
            "request_timeout_secs": 5,
            "cooling_secs": 60,
            "notify_enabled": false,
            "notify_lead_minutes": 10,
            "notify_desktop": false,
            "quota_endpoint": "https://example.invalid/quota",
            "extra_headers": ""
        }"#;
        let settings: Settings = serde_json::from_str(json).expect("解析旧配置");
        assert!(settings.start_with_windows, "开机启动默认应开启");
        assert!(settings.minimize_to_tray, "最小化到托盘默认应开启");

        // 新裝默认值同样开启。
        assert!(Settings::default().start_with_windows);
        assert!(Settings::default().minimize_to_tray);
    }
}
