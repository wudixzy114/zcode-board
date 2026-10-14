//! 从本机安装的 ZCode 中读取 API Key。
//!
//! 只读一个文件：`~/.zcode/v2/provider_config.json`。
//!
//! 这里面的 `config.providerConfigRules.providerRules[]` 就是 zcode 里配的各个
//! BigModel provider，每项含 `providerName` 与 `config.access.apiKey`（明文）。
//! 这些 key 才能查到 coding plan 额度——zcode 界面本身只显示 token plan，
//! 不显示这些 key 的额度，所以需要这个工具补上。
//!
//! 其他文件一概不碰：
//!
//! - `credentials.json` 的值全是 `enc:v1:` 密文，且多为已过期的 JWT/OAuth；
//! - `config.json` 的 provider key 与 provider_config.json 重复，且多为过期 token。
//!
//! 两者都只会产生「异常 / 无 plan」的噪音，所以直接不读。
//!
//! 只做**只读**操作，不写入 zcode 的任何文件。

use std::path::PathBuf;

use serde_json::Value;

/// 一条扫描到的 API Key。
#[derive(Debug, Clone)]
pub struct DiscoveredKey {
    /// 展示名，例如「BigModel API 2」。
    pub label: String,
    pub secret: String,
    /// 归属线索：同一个 provider 一个 key，天然独立成组。
    pub group: String,
}

/// 扫描结果。
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    pub keys: Vec<DiscoveredKey>,
    /// 扫描过的位置（含不存在的），用于界面说明。
    pub scanned_paths: Vec<String>,
    /// 非致命错误（文件解析失败等）。
    pub warnings: Vec<String>,
}

impl Discovery {
    /// 去掉重复 secret，保留先出现的。
    pub fn dedup(mut self) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.keys.retain(|k| seen.insert(k.secret.clone()));
        self
    }

    /// 摘要，例如「5 个 API Key」。
    pub fn summary(&self) -> String {
        format!("{} 个 API Key", self.keys.len())
    }
}

/// zcode 数据根目录，优先 `ZCODE_HOME`，其次 `~/.zcode`。
fn zcode_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ZCODE_HOME") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let home = home_dir()?;
    let p = home.join(".zcode");
    p.is_dir().then_some(p)
}

/// 跨平台取主目录（不引入 `dirs` crate）。
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// 主扫描入口。
pub fn scan() -> Discovery {
    let mut out = Discovery::default();

    let Some(root) = zcode_home() else {
        out.scanned_paths.push("~/.zcode（不存在）".to_string());
        return out;
    };

    let pc_path = root.join("v2").join("provider_config.json");
    out.scanned_paths.push(rel_display(&pc_path));

    match std::fs::read_to_string(&pc_path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(json) => scan_provider_config(&json, &mut out),
            Err(err) => out
                .warnings
                .push(format!("provider_config.json 解析失败: {err}")),
        },
        Err(err) => {
            out.warnings
                .push(format!("provider_config.json 不可读: {err}"));
        }
    }

    out.dedup()
}

/// 解析 `provider_config.json` 的 providerRules。
///
/// 结构：`config.providerConfigRules.providerRules[]`
///       每项含 `providerId` / `providerName` / `config.access.apiKey`。
fn scan_provider_config(json: &Value, out: &mut Discovery) {
    let Some(rules) = json
        .get("config")
        .and_then(|c| c.get("providerConfigRules"))
        .and_then(|r| r.get("providerRules"))
        .and_then(|v| v.as_array())
    else {
        out.warnings
            .push("provider_config.json 里没有 providerConfigRules".to_string());
        return;
    };

    for rule in rules {
        let Some(access) = rule.get("config").and_then(|c| c.get("access")) else {
            continue;
        };
        // 只取 api-key 类型的 access
        if !matches!(access.get("type").and_then(|v| v.as_str()), Some("api-key")) {
            continue;
        }
        let Some(api_key) = access.get("apiKey").and_then(|v| v.as_str()) else {
            continue;
        };
        let api_key = api_key.trim();
        if api_key.is_empty() {
            continue;
        }

        let provider_id = rule
            .get("providerId")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let name = rule
            .get("providerName")
            .and_then(|v| v.as_str())
            .unwrap_or(provider_id);

        out.keys.push(DiscoveredKey {
            label: name.to_string(),
            secret: api_key.to_string(),
            // 每个 provider 是独立配置项，一个 key 不会出现在两个 provider 下，
            // 所以按 provider 分组天然是「一组一个 key」；同账户由 twin 检测识别后手动合并。
            group: format!("zcode-provider:{provider_id}"),
        });
    }
}

/// 把绝对路径压成 `~/.zcode/...` 便于展示。
fn rel_display(path: &std::path::Path) -> String {
    let display = path.display().to_string();
    if let Some(home) = home_dir() {
        let home_prefix = home.display().to_string();
        if display.starts_with(&home_prefix) {
            return display.replacen(&home_prefix, "~", 1);
        }
    }
    display
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_rules_into_keys() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "config": {
                    "providerConfigRules": {
                        "providerRules": [
                            {"providerId":"bigmodel-standard-api","providerName":"BigModel API",
                             "config":{"access":{"type":"api-key","apiKey":"aaaa.bbbb"}}},
                            {"providerId":"bigmodel-standard-api-2","providerName":"BigModel API 2",
                             "config":{"access":{"type":"api-key","apiKey":"cccc.dddd"}}}
                        ]
                    }
                }
            }"#,
        )
        .unwrap();

        let mut out = Discovery::default();
        scan_provider_config(&json, &mut out);

        assert_eq!(out.keys.len(), 2);
        assert_eq!(out.keys[0].label, "BigModel API");
        assert_eq!(out.keys[0].secret, "aaaa.bbbb");
        assert_eq!(out.keys[0].group, "zcode-provider:bigmodel-standard-api");
        assert_eq!(out.keys[1].label, "BigModel API 2");
    }

    #[test]
    fn skips_non_api_key_and_empty() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{
                "config": {"providerConfigRules": {"providerRules": [
                    {"providerId":"p1","providerName":"P1",
                     "config":{"access":{"type":"oauth","token":"x"}}},
                    {"providerId":"p2","providerName":"P2",
                     "config":{"access":{"type":"api-key","apiKey":"   "}}},
                    {"providerId":"p3","providerName":"P3",
                     "config":{"access":{"type":"api-key","apiKey":"real.key"}}}
                ]}}
            }"#,
        )
        .unwrap();

        let mut out = Discovery::default();
        scan_provider_config(&json, &mut out);

        assert_eq!(out.keys.len(), 1, "只应保留有效 api-key");
        assert_eq!(out.keys[0].secret, "real.key");
    }

    #[test]
    fn dedup_removes_repeated_secrets() {
        let out = Discovery {
            keys: vec![
                DiscoveredKey {
                    label: "a".into(),
                    secret: "SAME".into(),
                    group: "g1".into(),
                },
                DiscoveredKey {
                    label: "b".into(),
                    secret: "SAME".into(),
                    group: "g2".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(out.dedup().keys.len(), 1);
    }
}
