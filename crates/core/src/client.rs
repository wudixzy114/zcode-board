//! z.ai / ZCode 上游客户端：额度探测与账户身份探测。

use serde::Deserialize;
use serde_json::Value;
use zcode_model::{KeyRecord, QuotaLimit, QuotaSnapshot, Settings};

use crate::util::now_ms;

/// 探测失败的细分类别，供上层决定「冷却 / 失效 / 重试」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// 网络层失败，可重试。
    Network,
    /// 401 / 403，凭证可能失效。
    Unauthorized,
    /// 429，上游限流，需要冷却。
    RateLimited,
    /// 账号没有 coding plan —— 明确的业务状态，不是故障。
    NoPlan,
    /// 其他服务端错误。
    Server(u16),
    /// 响应结构无法解析。
    Parse,
}

impl FailureKind {
    pub fn message(self) -> String {
        match self {
            Self::Network => "网络请求失败".to_string(),
            Self::Unauthorized => "鉴权失败（401/403），凭证可能已失效".to_string(),
            Self::RateLimited => "上游限流（429），进入冷却".to_string(),
            Self::NoPlan => "该账号没有 coding plan 额度".to_string(),
            Self::Server(code) => format!("上游返回 HTTP {code}"),
            Self::Parse => "响应结构无法解析".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct FetchOutcome {
    pub snapshot: QuotaSnapshot,
    pub failure: Option<FailureKind>,
}

/// 上游 `quota/limit` 的原始响应。
#[derive(Debug, Deserialize)]
struct QuotaResponse {
    success: Option<bool>,
    msg: Option<String>,
    data: Option<Value>,
}

/// 通用的 ZCode 客户端头部，尽量贴近官方客户端。
fn zcode_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_string(),
            "ZCode/3.1.0 (zcode-quota-board)".to_string(),
        ),
        ("X-ZCode-App-Version".to_string(), "3.1.0".to_string()),
        ("X-ZCode-Agent".to_string(), "glm".to_string()),
        (
            "HTTP-Referer".to_string(),
            "https://zcode.z.ai/".to_string(),
        ),
        ("Accept".to_string(), "application/json".to_string()),
    ]
}

/// 解析设置里的自定义请求头，每行 `Header-Name: value`。
fn parse_extra_headers(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            line.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, _)| !k.is_empty())
        })
        .collect()
}

/// 组装一次额度探测的鉴权头。
///
/// API Key 两种写法上游都可能接受，返回多组候选依次尝试。
/// 组装鉴权头候选。智谱两种写法上游都可能接受，逐个尝试。
fn auth_candidates(secret: &str) -> Vec<Vec<(String, String)>> {
    vec![
        vec![("Authorization".to_string(), format!("Bearer {secret}"))],
        vec![("x-api-key".to_string(), secret.to_string())],
    ]
}

/// 探测单个 key 的额度。
pub async fn fetch_quota(
    client: &reqwest::Client,
    settings: &Settings,
    key: &KeyRecord,
) -> FetchOutcome {
    let secret = key.secret.trim();
    if secret.is_empty() {
        return FetchOutcome {
            snapshot: QuotaSnapshot {
                ok: false,
                error: Some("凭证为空".to_string()),
                fetched_at_ms: now_ms(),
                ..Default::default()
            },
            failure: None,
        };
    }

    let mut last_error = None::<String>;
    let mut last_failure = None::<FailureKind>;

    for auth in auth_candidates(secret) {
        let mut req = client.get(settings.quota_endpoint.trim());
        for (k, v) in zcode_headers() {
            req = req.header(k, v);
        }
        for (k, v) in parse_extra_headers(&settings.extra_headers) {
            req = req.header(k, v);
        }
        for (k, v) in auth {
            req = req.header(k, v);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();

                if status == 429 {
                    return FetchOutcome {
                        snapshot: QuotaSnapshot {
                            ok: false,
                            error: Some(FailureKind::RateLimited.message()),
                            fetched_at_ms: now_ms(),
                            raw: Some(Value::String(body)),
                            ..Default::default()
                        },
                        failure: Some(FailureKind::RateLimited),
                    };
                }
                if status == 401 || status == 403 {
                    // 换一种鉴权写法再试（仅 API Key 有两种候选）。
                    last_error = Some(FailureKind::Unauthorized.message());
                    last_failure = Some(FailureKind::Unauthorized);
                    continue;
                }
                if !resp_status_ok(status) {
                    return FetchOutcome {
                        snapshot: QuotaSnapshot {
                            ok: false,
                            error: Some(FailureKind::Server(status).message()),
                            fetched_at_ms: now_ms(),
                            raw: Some(Value::String(body)),
                            ..Default::default()
                        },
                        failure: Some(FailureKind::Server(status)),
                    };
                }

                return parse_quota_body(&body);
            }
            Err(err) => {
                last_error = Some(format!("{}（{}）", FailureKind::Network.message(), err));
                last_failure = Some(FailureKind::Network);
            }
        }
    }

    FetchOutcome {
        snapshot: QuotaSnapshot {
            ok: false,
            error: last_error.or(Some("未知错误".to_string())),
            fetched_at_ms: now_ms(),
            ..Default::default()
        },
        failure: last_failure,
    }
}

fn resp_status_ok(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 解析额度响应体。
fn parse_quota_body(body: &str) -> FetchOutcome {
    let value: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            return FetchOutcome {
                snapshot: QuotaSnapshot {
                    ok: false,
                    error: Some(FailureKind::Parse.message()),
                    fetched_at_ms: now_ms(),
                    raw: Some(Value::String(body.to_string())),
                    ..Default::default()
                },
                failure: Some(FailureKind::Parse),
            };
        }
    };

    let parsed: QuotaResponse = match serde_json::from_value(value.clone()) {
        Ok(v) => v,
        Err(_) => {
            return FetchOutcome {
                snapshot: QuotaSnapshot {
                    ok: false,
                    error: Some(FailureKind::Parse.message()),
                    fetched_at_ms: now_ms(),
                    raw: Some(value),
                    ..Default::default()
                },
                failure: Some(FailureKind::Parse),
            };
        }
    };

    if parsed.success == Some(false) {
        // 「当前用户不存在coding plan」是一个明确的业务状态：这个账号没有 coding plan，
        // 不是探测故障。单独标记，界面据此显示中性提示而不是红色报错。
        let msg = parsed.msg.unwrap_or_else(|| "上游返回失败".to_string());
        let no_plan = msg.contains("coding plan") || msg.contains("不存在");
        return FetchOutcome {
            snapshot: QuotaSnapshot {
                ok: false,
                no_plan,
                error: Some(msg),
                fetched_at_ms: now_ms(),
                raw: Some(value),
                ..Default::default()
            },
            // 无 plan 不视为可重试故障
            failure: no_plan.then_some(FailureKind::NoPlan),
        };
    }

    // data 可能直接是对象，也可能被再包一层 data。
    let data = parsed
        .data
        .as_ref()
        .and_then(|d| {
            d.get("data")
                .and_then(|v| v.as_object())
                .map(|_| d.get("data").unwrap())
        })
        .or(parsed.data.as_ref());

    let limits = data
        .and_then(|d| d.get("limits"))
        .and_then(|v| v.as_array());
    let limits = match limits {
        Some(arr) => arr
            .iter()
            .filter_map(|item| {
                let kind = item.get("type")?.as_str()?.to_string();
                let next_reset_time_ms = item
                    .get("nextResetTime")
                    .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|i| i as f64)))
                    .map(|f| f.round() as i64);
                let usage_details = item
                    .get("usageDetails")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|d| {
                                Some(zcode_model::UsageDetail {
                                    model_code: d.get("modelCode")?.as_str()?.to_string(),
                                    usage: d.get("usage")?.as_f64()?,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                Some(QuotaLimit {
                    kind,
                    percentage: item.get("percentage").and_then(|v| v.as_f64()),
                    unit: item
                        .get("unit")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                        .map(|u| u as u32),
                    number: item
                        .get("number")
                        .and_then(|v| v.as_u64().or_else(|| v.as_i64().map(|i| i as u64)))
                        .map(|n| n as u32),
                    next_reset_time_ms,
                    usage: item.get("usage").and_then(|v| v.as_f64()),
                    current_value: item.get("currentValue").and_then(|v| v.as_f64()),
                    remaining: item.get("remaining").and_then(|v| v.as_f64()),
                    usage_details,
                    model: item
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    used: item.get("used").and_then(|v| v.as_f64()),
                    limit: item.get("limit").and_then(|v| v.as_f64()),
                })
            })
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };

    // limits 为空但响应成功：账号没有可看的额度窗口，同样归入「无 plan」。
    if limits.is_empty() {
        return FetchOutcome {
            snapshot: QuotaSnapshot {
                ok: false,
                no_plan: true,
                error: Some("该账号没有可用的额度窗口".to_string()),
                fetched_at_ms: now_ms(),
                raw: Some(value),
                ..Default::default()
            },
            failure: Some(FailureKind::NoPlan),
        };
    }

    let level = data
        .and_then(|d| d.get("level"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    FetchOutcome {
        snapshot: QuotaSnapshot {
            ok: true,
            no_plan: false,
            error: None,
            limits,
            fetched_at_ms: now_ms(),
            level,
            raw: Some(value),
        },
        failure: None,
    }
}
