//! 本地持久化：账户、凭证与设置。
//!
//! 单机小工具规模，直接用 JSON 文件 + 原子替换写入，不引入数据库依赖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zcode_model::{AccountRecord, KeyRecord, Settings};

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
}

impl Store {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("state.json"),
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
