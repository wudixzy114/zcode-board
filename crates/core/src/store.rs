//! 本地持久化：账户、凭证与设置。
//!
//! 单机小工具规模，直接用 JSON 文件 + 原子替换写入，不引入数据库依赖。
//! 速率采样历史单独放 `history.json`（紧凑 JSON、节流写入），
//! 不和 state.json 混在一起：后者每次刷新都整体重写，前者体积大一个量级。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zcode_model::{AccountRecord, KeyRecord, RateSample, Settings};

use crate::util::new_id;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedState {
    #[serde(default)]
    pub keys: Vec<KeyRecord>,
    #[serde(default)]
    pub accounts: Vec<AccountRecord>,
    #[serde(default)]
    pub settings: Settings,
}

impl PersistedState {
    /// 载入时的一次性自愈迁移。
    ///
    /// 历史版本（JWT / OAuth / config.json / 账户凭证那几轮）写进 state.json 的
    /// 数据，解析逻辑已经整体移除，但它们会一直躺在文件里，每次打开都会在视图里
    /// 生成永远查不到额度的死条目。这里在载入时统一清掉：
    ///
    /// 1. 删除非 API key 来源的残留（`group_hint` 属于已废弃的来源）；
    /// 2. 同一 secret 只保留最早的一条（老版本并发/重复导入会写出重复项）；
    /// 3. 清理指向不存在账户的归属；
    /// 4. 删除没有任何 key 的空账户（早期归组逻辑迁来迁去留下的壳）；
    /// 5. 同一 `group_tag` 的账户只保留最早的一个。
    fn sanitize(&mut self) {
        // 1. 已废弃的来源前缀（discover.rs 现在只产出 zcode-provider:）
        const LEGACY_PREFIXES: &[&str] = &[
            "zcode-jwt",
            "zcode-oauth:",
            "zcode-config:",
            "zcode-account:",
        ];
        self.keys.retain(|k| match &k.group_hint {
            Some(g) => {
                g.starts_with("zcode-provider:")
                    || !LEGACY_PREFIXES.iter().any(|p| g.starts_with(p))
            }
            None => true,
        });

        // 2. 按 secret 去重，保留最早写入的一条
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        self.keys.retain(|k| seen.insert(k.secret.clone()));

        // 3. 清理指向不存在账户的归属，避免脏数据导致 key 在视图里消失
        let account_ids: std::collections::HashSet<String> =
            self.accounts.iter().map(|a| a.id.clone()).collect();
        for key in self.keys.iter_mut() {
            if let Some(aid) = &key.account_id
                && !account_ids.contains(aid)
            {
                key.account_id = None;
                key.account_pinned = false;
            }
        }

        // 4. 删除没有任何 key 的空账户
        let used: std::collections::HashSet<String> = self
            .keys
            .iter()
            .filter_map(|k| k.account_id.clone())
            .collect();
        self.accounts.retain(|a| used.contains(&a.id));

        // 5. 同一 group_tag 只保留最早的账户
        let mut seen_tags: std::collections::HashSet<String> = std::collections::HashSet::new();
        self.accounts.retain(|a| match &a.group_tag {
            Some(tag) => seen_tags.insert(tag.clone()),
            None => true,
        });
    }
}

pub struct Store {
    path: PathBuf,
    history_path: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("state.json"),
            history_path: data_dir.join("history.json"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 载入状态；文件不存在或解析失败时返回空状态（并把损坏文件改名保留）。
    pub async fn load(&self) -> PersistedState {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(text) => match serde_json::from_str::<PersistedState>(&text) {
                Ok(mut state) => {
                    let before = (state.keys.len(), state.accounts.len());
                    state.sanitize();
                    if (state.keys.len(), state.accounts.len()) != before {
                        // 迁移真的动了数据：立刻落库，避免每次打开都重跑一遍。
                        if let Err(err) = self.save(&state).await {
                            tracing::warn!(error = %err, "状态迁移落库失败");
                        }
                    }
                    state
                }
                Err(err) => {
                    tracing::warn!(error = %err, path = %self.path.display(), "状态文件解析失败，已备份并重置");
                    self.backup_corrupted().await;
                    PersistedState::default()
                }
            },
            Err(_) => PersistedState::default(),
        }
    }

    async fn backup_corrupted(&self) {
        let backup = self.path.with_extension("json.bak");
        let _ = tokio::fs::rename(&self.path, &backup).await;
    }

    /// 载入速率采样历史；文件不存在或损坏时返回空（历史丢了只影响图表，
    /// 不值得为此重置其他状态，也不用备份）。
    pub async fn load_history(&self) -> History {
        let Ok(text) = tokio::fs::read_to_string(&self.history_path).await else {
            return History::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|err| {
            tracing::warn!(
                error = %err,
                path = %self.history_path.display(),
                "速率历史文件解析失败，已重置"
            );
            History::default()
        })
    }

    /// 历史文件用紧凑 JSON：条目多（每序列最多 1000 采样），pretty 的
    /// 体积翻倍纯属浪费；同样走临时文件 + 替换的原子写。
    pub async fn save_history(&self, history: &History) -> Result<(), std::io::Error> {
        if let Some(parent) = self.history_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string(history).unwrap_or_else(|_| "{}".to_string());
        let tmp = self.history_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &text).await?;
        if tokio::fs::rename(&tmp, &self.history_path).await.is_ok() {
            return Ok(());
        }
        // 与 state.json 同理：Windows 上 rename 可能被占用挡住，退化为覆盖写。
        tokio::fs::write(&self.history_path, &text).await?;
        Ok(())
    }

    /// 原子写入（先写临时文件再替换），避免写到一半断电导致状态损坏。
    pub async fn save(&self, state: &PersistedState) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, text).await?;
        if tokio::fs::rename(&tmp, &self.path).await.is_ok() {
            return Ok(());
        }
        // Windows 上目标被占用时 rename 可能失败，退化为直接覆盖写；
        // 只要覆盖写成功，状态就是完好的，不能把上一步的 rename 错误报出去。
        let text = serde_json::to_string_pretty(state).unwrap_or_else(|_| "{}".to_string());
        tokio::fs::write(&self.path, text).await?;
        Ok(())
    }
}

/// 生成一个新的账户 id。
pub fn new_account_id() -> String {
    new_id('a')
}

/// 生成一个新的凭证 id。
pub fn new_key_id() -> String {
    new_id('k')
}

// ─────────────────────────── 速率采样历史 ───────────────────────────

/// 速率采样历史：key id →（窗口标识 → 采样序列）。
///
/// 按 key 而不是按账户存：key 移动/合并账户时历史跟着 key 走，
/// 账户合并后速率曲线不断档。窗口标识复用聚合键（如 `TOKENS_LIMIT:3`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct History {
    #[serde(default)]
    pub series: HashMap<String, HashMap<String, Vec<RateSample>>>,
}

impl History {
    /// 载入后自愈：丢掉已不存在的 key 的序列、序列内乱序/超龄的采样。
    /// 返回是否有改动（有则调用方落库一次）。
    pub fn sanitize(
        &mut self,
        live_key_ids: &std::collections::HashSet<&str>,
        now_ms: i64,
    ) -> bool {
        let keys_before = self.series.len();
        let samples_before = self.total_samples();
        self.series
            .retain(|key_id, _| live_key_ids.contains(key_id.as_str()));
        for windows in self.series.values_mut() {
            for series in windows.values_mut() {
                // 手改 / 异常写入可能乱序：排序后按时间去重。
                series.sort_by_key(|s| s.t_ms);
                series.dedup_by_key(|s| s.t_ms);
                let floor = now_ms - crate::rate::RETAIN_MS;
                series.retain(|s| s.t_ms >= floor);
                let truncated = series
                    .len()
                    .saturating_sub(crate::rate::MAX_SAMPLES_PER_SERIES);
                if truncated > 0 {
                    series.drain(0..truncated);
                }
            }
            windows.retain(|_, s| !s.is_empty());
        }
        self.series.retain(|_, w| !w.is_empty());
        self.total_samples() != samples_before || self.series.len() != keys_before
    }

    fn total_samples(&self) -> usize {
        self.series
            .values()
            .flat_map(|w| w.values())
            .map(|s| s.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_round_trip() {
        let dir = std::env::temp_dir().join(format!("zcode-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(&dir);

        let mut history = History::default();
        history.series.insert(
            "k1".to_string(),
            HashMap::from([(
                "TOKENS_LIMIT:3".to_string(),
                vec![
                    RateSample {
                        t_ms: 1_000,
                        used: 10.0,
                    },
                    RateSample {
                        t_ms: 61_000,
                        used: 20.0,
                    },
                ],
            )]),
        );
        store.save_history(&history).await.expect("保存历史");

        let loaded = store.load_history().await;
        assert_eq!(loaded.series["k1"]["TOKENS_LIMIT:3"].len(), 2);
        assert_eq!(loaded.series["k1"]["TOKENS_LIMIT:3"][1].used, 20.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn history_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("zcode-store-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::new(&dir);
        assert!(store.load_history().await.series.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_sanitize_drops_dead_keys_and_stale_samples() {
        let now = 100 * 24 * 3_600_000_i64;
        let mut history = History::default();
        history.series.insert(
            "dead".to_string(),
            HashMap::from([(
                "TOKENS_LIMIT:3".to_string(),
                vec![RateSample {
                    t_ms: now,
                    used: 1.0,
                }],
            )]),
        );
        history.series.insert(
            "live".to_string(),
            HashMap::from([(
                "TIME_LIMIT:5".to_string(),
                vec![
                    RateSample {
                        t_ms: now - 8 * 24 * 3_600_000,
                        used: 5.0,
                    }, // 超龄
                    RateSample {
                        t_ms: now - 1000,
                        used: 6.0,
                    },
                    RateSample {
                        t_ms: now - 1000,
                        used: 7.0,
                    }, // 重复时间戳
                    RateSample {
                        t_ms: now,
                        used: 8.0,
                    },
                ],
            )]),
        );
        history.sanitize(&["live"].into_iter().collect(), now);
        assert!(
            !history.series.contains_key("dead"),
            "死 key 的序列应被清掉"
        );
        let s = &history.series["live"]["TIME_LIMIT:5"];
        assert_eq!(s.len(), 2, "超龄与重复时间戳的采样应被清掉");
        assert_eq!(s[1].used, 8.0);
    }
}
