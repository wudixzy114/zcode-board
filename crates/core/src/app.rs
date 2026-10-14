//! 应用状态机：凭证 / 账户 / 运行时状态的管理与聚合视图构建。

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{Notify, RwLock, Semaphore};
use zcode_model::{
    AccountRecord, AccountView, AddKeyRequest, AppState, EventItem, KeyRecord, KeyRuntime,
    KeyStatus, KeyView, MoveKeyRequest, QuotaSnapshot, Settings, UpdateKeyRequest,
    WindowKeyPercent, WindowView, mask_secret, normalize_percent, window_duration_ms, window_meta,
};

use crate::client::{self, FailureKind};
use crate::store::{PersistedState, Store, new_account_id, new_key_id};
use crate::util::{now_ms, trim_opt};

/// 事件环形缓冲上限。
const MAX_EVENTS: usize = 60;

#[derive(Debug, Default)]
struct Inner {
    keys: Vec<KeyRecord>,
    accounts: Vec<AccountRecord>,
    runtime: HashMap<String, KeyRuntime>,
    settings: Settings,
    next_auto_refresh_ms: Option<i64>,
    refreshing: HashSet<String>,
    events: VecDeque<EventItem>,
    event_seq: u64,
}

/// 桌面通知出口：`(标题, 正文)`。由界面层注册（Windows 上走托盘气泡，
/// 归属本应用且带系统提示音）；未注册时退回 notify-rust 直接发。
pub type DesktopNotifier = Arc<dyn Fn(String, String) + Send + Sync>;

/// 应用管理器。所有字段都是 `Arc`，可廉价克隆进后台任务。
#[derive(Clone)]
pub struct Manager {
    inner: Arc<RwLock<Inner>>,
    http: reqwest::Client,
    store: Arc<Store>,
    /// 设置变更时唤醒调度循环。
    pub settings_changed: Arc<Notify>,
    /// 手动触发一次全量刷新。
    pub refresh_requested: Arc<Notify>,
    /// 界面注册的桌面通知出口（托盘气泡）。
    desktop_notifier: Arc<std::sync::RwLock<Option<DesktopNotifier>>>,
}

impl Manager {
    pub async fn new(data_dir: &Path) -> Self {
        let store = Arc::new(Store::new(data_dir));
        let persisted = store.load().await;
        // 开机启动自愈：设置项与注册表实际状态不一致时，以设置项为准补齐。
        // 覆盖三种情况——首次运行（默认开启但注册表还没写过）、用户手动在
        // 注册表里删过值、exe 挪过位置导致 Run 值指向旧路径（is_enabled
        // 视为未开启，这里用当前路径重写）。否则界面里勾选状态是假的，
        // 要手动取消再勾选一次才能真正写进去。
        if crate::autostart::is_enabled() != persisted.settings.start_with_windows {
            crate::autostart::set_enabled(persisted.settings.start_with_windows);
        }
        let mut runtime = HashMap::new();
        for key in &persisted.keys {
            runtime.insert(
                key.id.clone(),
                KeyRuntime {
                    status: if key.enabled {
                        KeyStatus::Idle
                    } else {
                        KeyStatus::Disabled
                    },
                    ..Default::default()
                },
            );
        }

        let inner = Inner {
            keys: persisted.keys,
            accounts: persisted.accounts,
            runtime,
            settings: persisted.settings,
            ..Default::default()
        };

        Self {
            inner: Arc::new(RwLock::new(inner)),
            http: reqwest::Client::builder()
                .user_agent("zcode-board/0.1")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            store,
            settings_changed: Arc::new(Notify::new()),
            refresh_requested: Arc::new(Notify::new()),
            desktop_notifier: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// 注册桌面通知出口（界面层在启动时调用，通常指向托盘气泡）。
    pub fn set_desktop_notifier(&self, f: DesktopNotifier) {
        let mut slot = self
            .desktop_notifier
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *slot = Some(f);
    }

    /// 发一条桌面通知：优先走注册的出口（托盘气泡），否则 notify-rust。
    fn desktop_notify(&self, title: String, body: String) {
        let via = self
            .desktop_notifier
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        tokio::task::spawn_blocking(move || match via {
            Some(f) => f(title, body),
            // 后备路径也把归属与声音修正好：不设 app_id 会借 PowerShell 的
            // AUMID（通知显示成 powershell），不设 sound 则静音。
            None => {
                if let Err(err) = notify_rust::Notification::new()
                    .app_id("zcode-board")
                    .summary(&title)
                    .body(&body)
                    .sound_name("Default")
                    .timeout(notify_rust::Timeout::Milliseconds(10_000))
                    .show()
                {
                    tracing::warn!(error = %err, "桌面通知发送失败");
                }
            }
        });
    }

    pub fn data_path(&self) -> PathBuf {
        self.store.path().to_path_buf()
    }

    // ──────────────────────── 持久化 ────────────────────────

    async fn persist(&self) {
        let snapshot = {
            let guard = self.inner.read().await;
            PersistedState {
                keys: guard.keys.clone(),
                accounts: guard.accounts.clone(),
                settings: guard.settings.clone(),
            }
        };
        if let Err(err) = self.store.save(&snapshot).await {
            tracing::warn!(error = %err, "状态持久化失败");
        }
    }

    async fn push_event(&self, level: &str, title: impl Into<String>, body: impl Into<String>) {
        let mut guard = self.inner.write().await;
        guard.event_seq += 1;
        let id = guard.event_seq;
        guard.events.push_back(EventItem {
            id,
            at_ms: now_ms(),
            level: level.to_string(),
            title: title.into(),
            body: body.into(),
        });
        while guard.events.len() > MAX_EVENTS {
            guard.events.pop_front();
        }
    }

    /// 清空事件日志（界面「清空」按钮）。
    pub async fn clear_events(&self) {
        let mut guard = self.inner.write().await;
        guard.events.clear();
    }

    // ──────────────────────── 凭证 CRUD ────────────────────────

    /// 批量添加凭证。一次可粘贴多行。
    pub async fn add_keys(&self, req: &AddKeyRequest) -> usize {
        let mut added = Vec::new();
        {
            let mut guard = self.inner.write().await;
            for line in req.secrets.lines() {
                let secret = line.trim();
                if secret.is_empty() {
                    continue;
                }
                // 支持 `备注: 凭证` 的写法，方便批量导入时区分。
                let (label, secret) = match secret.split_once(':') {
                    Some((left, right)) if !right.trim().is_empty() && left.len() <= 32 => {
                        (Some(left.trim().to_string()), right.trim())
                    }
                    _ => (None, secret),
                };
                if guard.keys.iter().any(|k| k.secret == secret) {
                    continue; // 去重
                }
                let id = new_key_id();
                guard.runtime.insert(
                    id.clone(),
                    KeyRuntime {
                        status: KeyStatus::Idle,
                        ..Default::default()
                    },
                );
                guard.keys.push(KeyRecord {
                    id: id.clone(),
                    label: trim_opt(req.label.clone().or(label)).unwrap_or_default(),
                    secret: secret.to_string(),
                    enabled: true,
                    account_id: None,
                    group_hint: None,
                    account_pinned: false,
                    created_at_ms: now_ms(),
                });
                added.push(id);
            }
        }

        let count = added.len();
        if count > 0 {
            self.persist().await;
            self.refresh_keys(Some(added)).await;
        }
        count
    }

    /// 从本机 zcode 扫描结果批量导入。
    ///
    /// 与 `add_keys` 的区别：带着 `group_hint`，导入即按 zcode 的账户 ID 归组，
    /// 不依赖网络探测成功。已存在的 secret 自动跳过。
    pub async fn import_discovered(
        &self,
        discovered: &[crate::discover::DiscoveredKey],
    ) -> (usize, usize) {
        let mut added = Vec::new();
        {
            let mut guard = self.inner.write().await;
            for item in discovered {
                let secret = item.secret.trim();
                if secret.is_empty() {
                    continue;
                }
                if guard.keys.iter().any(|k| k.secret == secret) {
                    continue; // 去重
                }
                let id = new_key_id();
                guard.runtime.insert(
                    id.clone(),
                    KeyRuntime {
                        status: KeyStatus::Idle,
                        ..Default::default()
                    },
                );
                guard.keys.push(KeyRecord {
                    id: id.clone(),
                    label: item.label.clone(),
                    secret: secret.to_string(),
                    enabled: true,
                    account_id: None,
                    group_hint: Some(item.group.clone()),
                    account_pinned: false,
                    created_at_ms: now_ms(),
                });
                added.push(id);
            }
        }

        let imported = added.len();
        if imported > 0 {
            self.persist().await;
            self.refresh_keys(Some(added)).await;
        }
        (imported, discovered.len())
    }

    pub async fn remove_key(&self, id: &str) -> bool {
        let removed = {
            let mut guard = self.inner.write().await;
            let before = guard.keys.len();
            guard.keys.retain(|k| k.id != id);
            guard.runtime.remove(id);
            guard.keys.len() != before
        };
        if removed {
            // 账户下没有 key 了就顺手清理空账户。
            let mut guard = self.inner.write().await;
            let used: Vec<String> = guard
                .keys
                .iter()
                .filter_map(|k| k.account_id.clone())
                .collect();
            let used_refs: HashSet<&str> = used.iter().map(String::as_str).collect();
            guard.accounts.retain(|a| used_refs.contains(a.id.as_str()));
            drop(guard);
            self.persist().await;
        }
        removed
    }

    /// 删除整个账户：账户本身 + 它下面的所有 key。返回删掉的 key 数量。
    pub async fn remove_account(&self, id: &str) -> usize {
        let removed = {
            let mut guard = self.inner.write().await;
            let ids: Vec<String> = guard
                .keys
                .iter()
                .filter(|k| k.account_id.as_deref() == Some(id))
                .map(|k| k.id.clone())
                .collect();
            for kid in &ids {
                guard.runtime.remove(kid);
            }
            guard.keys.retain(|k| k.account_id.as_deref() != Some(id));
            guard.accounts.retain(|a| a.id != id);
            ids.len()
        };
        if removed > 0 {
            self.persist().await;
        }
        removed
    }

    /// 一键清理「失效的本机同步 key」：本机 zcode 里已经不在的扫描 key。
    ///
    /// 只清理 `group_hint` 以 `zcode-provider:` 开头的 key（本机同步来的），
    /// 手动粘贴的 key 一律不动。清理后顺手删掉腾空的账户。
    pub async fn cleanup_stale(&self, live_secrets: &HashSet<String>) -> usize {
        let removed = {
            let mut guard = self.inner.write().await;
            let ids: Vec<String> = guard
                .keys
                .iter()
                .filter(|k| {
                    k.group_hint
                        .as_deref()
                        .is_some_and(|g| g.starts_with("zcode-provider:"))
                        && !live_secrets.contains(&k.secret)
                })
                .map(|k| k.id.clone())
                .collect();
            for kid in &ids {
                guard.runtime.remove(kid);
            }
            let before = guard.keys.len();
            guard.keys.retain(|k| !ids.contains(&k.id));
            let removed = before - guard.keys.len();
            if removed > 0 {
                let used: Vec<String> = guard
                    .keys
                    .iter()
                    .filter_map(|k| k.account_id.clone())
                    .collect();
                let used_refs: HashSet<&str> = used.iter().map(String::as_str).collect();
                guard.accounts.retain(|a| used_refs.contains(a.id.as_str()));
            }
            removed
        };
        if removed > 0 {
            self.persist().await;
        }
        removed
    }

    /// 删除所有没有 key 的空账户（刷新归组后可能留下壳）。
    async fn prune_empty_accounts(&self) -> usize {
        let mut guard = self.inner.write().await;
        let used: Vec<String> = guard
            .keys
            .iter()
            .filter_map(|k| k.account_id.clone())
            .collect();
        let used_refs: HashSet<&str> = used.iter().map(String::as_str).collect();
        let before = guard.accounts.len();
        guard.accounts.retain(|a| used_refs.contains(a.id.as_str()));
        before - guard.accounts.len()
    }

    pub async fn update_key(&self, id: &str, req: &UpdateKeyRequest) -> bool {
        let changed = {
            let mut guard = self.inner.write().await;
            let Some(key) = guard.keys.iter_mut().find(|k| k.id == id) else {
                return false;
            };
            if let Some(label) = &req.label {
                key.label = label.trim().to_string();
            }
            if let Some(enabled) = req.enabled {
                key.enabled = enabled;
            }
            true
        };
        if changed {
            let mut guard = self.inner.write().await;
            if let Some(rt) = guard.runtime.get_mut(id)
                && let Some(enabled) = req.enabled
            {
                if !enabled {
                    rt.status = KeyStatus::Disabled;
                } else if rt.status == KeyStatus::Disabled {
                    rt.status = KeyStatus::Idle;
                }
            }
            drop(guard);
            self.persist().await;
        }
        changed
    }

    /// 手动把一个 key 归并到指定账户（或解除固定重新自动归组）。
    pub async fn move_key(&self, id: &str, req: &MoveKeyRequest) -> bool {
        let target_valid = match &req.account_id {
            Some(aid) => self.account_exists(aid).await,
            None => true,
        };
        if !target_valid {
            return false;
        }

        let ok = {
            let mut guard = self.inner.write().await;
            let Some(key) = guard.keys.iter_mut().find(|k| k.id == id) else {
                return false;
            };
            key.account_id = req.account_id.clone();
            key.account_pinned = req.account_id.is_some();
            true
        };
        if ok {
            // 归组后可能留下空账户，顺手清理。
            let used: Vec<String> = {
                let guard = self.inner.read().await;
                guard
                    .keys
                    .iter()
                    .filter_map(|k| k.account_id.clone())
                    .collect()
            };
            let mut guard = self.inner.write().await;
            let used_refs: HashSet<&str> = used.iter().map(String::as_str).collect();
            guard.accounts.retain(|a| used_refs.contains(a.id.as_str()));
            drop(guard);
            self.persist().await;
            self.refresh_keys(Some(vec![id.to_string()])).await;
        }
        ok
    }

    pub async fn rename_account(&self, id: &str, name: &str) -> bool {
        let ok = {
            let mut guard = self.inner.write().await;
            match guard.accounts.iter_mut().find(|a| a.id == id) {
                Some(acc) => {
                    acc.name = trim_opt(Some(name.to_string()));
                    true
                }
                None => false,
            }
        };
        if ok {
            self.persist().await;
        }
        ok
    }

    /// 单个账户静音 / 取消静音提醒。
    pub async fn set_account_muted(&self, id: &str, muted: bool) -> bool {
        let ok = {
            let mut guard = self.inner.write().await;
            match guard.accounts.iter_mut().find(|a| a.id == id) {
                Some(acc) => {
                    acc.notify_muted = muted;
                    true
                }
                None => false,
            }
        };
        if ok {
            self.persist().await;
        }
        ok
    }

    /// 把一组账户合并成一个：所有 key 挂到目标账户下，其余账户删除。
    ///
    /// 用于「疑似同一账户」提示：多个账户的配额快照一致说明它们其实是同一个账号。
    pub async fn merge_accounts(&self, source_ids: &[String], target_id: &str) -> bool {
        let sources: Vec<String> = source_ids
            .iter()
            .filter(|id| id.as_str() != target_id)
            .cloned()
            .collect();
        if sources.is_empty() || !self.account_exists(target_id).await {
            return false;
        }

        let moved = {
            let mut guard = self.inner.write().await;
            for source in &sources {
                // 源账户下的 key 全部转到目标账户，并固定归属避免被重新归组打散。
                for key in guard.keys.iter_mut() {
                    if key.account_id.as_deref() == Some(source.as_str()) {
                        key.account_id = Some(target_id.to_string());
                        key.account_pinned = true;
                    }
                }
            }
            // 目标账户下原有的 key 也一并固定：合并结果应当稳定，
            // 否则下次刷新时自动归组可能又把它们拆回各自的账户。
            for key in guard.keys.iter_mut() {
                if key.account_id.as_deref() == Some(target_id) {
                    key.account_pinned = true;
                }
            }
            // 删除已腾空的源账户
            guard.accounts.retain(|a| !sources.contains(&a.id));
            guard
                .keys
                .iter()
                .filter(|k| k.account_id.as_deref() == Some(target_id))
                .count()
        };

        self.persist().await;
        let ids = self.key_ids_for_account(target_id).await;
        let _ = self.refresh_keys(Some(ids)).await;
        self.nudge_auto_refresh_now().await;
        moved > 0
    }

    pub async fn update_settings(&self, next: Settings) {
        let autostart_changed = {
            let current = self.inner.read().await.settings.clone();
            current.start_with_windows != next.start_with_windows
        };
        if autostart_changed {
            // 设置项即事实：勾上就写 Run 项，取消就删。
            crate::autostart::set_enabled(next.start_with_windows);
        }
        {
            let mut guard = self.inner.write().await;
            guard.settings = next;
        }
        self.persist().await;
        self.settings_changed.notify_waiters();
    }

    pub async fn settings(&self) -> Settings {
        self.inner.read().await.settings.clone()
    }

    // ──────────────────────── 账户归组 ────────────────────────

    /// 依据归组线索找到（或创建）对应账户，返回账户 id。
    fn ensure_account(inner: &mut Inner, key: &KeyRecord, group_hint: Option<&str>) -> String {
        // 用户手动固定过的最优先。
        if key.account_pinned
            && let Some(aid) = &key.account_id
            && inner.accounts.iter().any(|a| a.id == *aid)
        {
            return aid.clone();
        }

        // 本地 zcode 扫描导入的 key：按 provider 归组（一个 provider 一个 key）。
        if let Some(group) = group_hint {
            let fingerprint = format!("zcode:{group}");
            if let Some(existing) = inner
                .accounts
                .iter()
                .find(|a| a.group_tag.as_deref() == Some(fingerprint.as_str()))
            {
                return existing.id.clone();
            }
            let account = AccountRecord {
                id: new_account_id(),
                name: Some(account_name_from_hint(group)),
                group_tag: Some(fingerprint),
                last_notified_reset_ms: None,
                notify_muted: false,
                created_at_ms: now_ms(),
            };
            let aid = account.id.clone();
            inner.accounts.push(account);
            return aid;
        }

        // 独立账户：复用 key 已指向的账户，否则新建一个。
        if let Some(aid) = &key.account_id
            && inner.accounts.iter().any(|a| &a.id == aid)
        {
            return aid.clone();
        }
        let account = AccountRecord {
            id: new_account_id(),
            name: None,
            group_tag: None,
            last_notified_reset_ms: None,
            notify_muted: false,
            created_at_ms: now_ms(),
        };
        let aid = account.id.clone();
        inner.accounts.push(account);
        aid
    }

    // ──────────────────────── 刷新 ────────────────────────

    /// 触发刷新（后台执行，立即返回被调度的数量）。
    pub async fn refresh_keys(&self, ids: Option<Vec<String>>) -> usize {
        let settings = self.settings().await;
        let targets = {
            let guard = self.inner.read().await;
            guard
                .keys
                .iter()
                .filter(|k| match &ids {
                    Some(list) => list.contains(&k.id),
                    None => true,
                })
                .map(|k| k.id.clone())
                .collect::<Vec<_>>()
        };

        let concurrency = settings.concurrency.max(1);
        let count = targets.len();
        {
            let mut guard = self.inner.write().await;
            for id in &targets {
                guard.refreshing.insert(id.clone());
            }
        }

        if count > 0 {
            let manager = self.clone();
            tokio::spawn(async move {
                manager.run_refresh(targets, concurrency).await;
            });
        }
        count
    }

    /// 执行刷新并等待完成（调度器使用）。
    pub async fn run_refresh(&self, ids: Vec<String>, concurrency: usize) {
        let sem = Arc::new(Semaphore::new(concurrency.max(1)));
        let manager = Arc::new(self.clone());
        let mut handles = Vec::with_capacity(ids.len());
        for id in ids {
            let manager = manager.clone();
            let sem = sem.clone();
            handles.push(tokio::spawn(async move {
                let permit = sem.acquire().await;
                let _permit = match permit {
                    Ok(p) => p,
                    Err(_) => return,
                };
                manager.refresh_one(id).await;
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }
        // 归组可能把 key 挪走而留下空账户，这里统一回收，避免视图里出现幽灵卡片。
        if self.prune_empty_accounts().await > 0 {
            self.persist().await;
        }
    }

    async fn refresh_one(self: Arc<Self>, key_id: String) {
        let settings = self.settings().await;
        let key = {
            let guard = self.inner.read().await;
            guard.keys.iter().find(|k| k.id == key_id).cloned()
        };

        let result = match key {
            None => RefreshResult::Skipped,
            Some(ref key) if !key.enabled => RefreshResult::Disabled,
            Some(ref key) => {
                // 冷却期内直接跳过，避免把 429 打得更惨。
                let cooling_until = {
                    let guard = self.inner.read().await;
                    guard
                        .runtime
                        .get(&key_id)
                        .and_then(|r| r.cooling_until_ms)
                        .unwrap_or(0)
                };
                if cooling_until > now_ms() {
                    RefreshResult::Cooling
                } else {
                    let timeout =
                        std::time::Duration::from_secs(settings.request_timeout_secs.max(1));
                    let quota = tokio::time::timeout(
                        timeout,
                        client::fetch_quota(&self.http, &settings, key),
                    )
                    .await;

                    let outcome = match quota {
                        Ok(outcome) => outcome,
                        Err(_) => client::FetchOutcome {
                            snapshot: QuotaSnapshot {
                                ok: false,
                                error: Some("请求超时".to_string()),
                                fetched_at_ms: now_ms(),
                                ..Default::default()
                            },
                            failure: Some(FailureKind::Network),
                        },
                    };

                    // 归属只靠 group_hint（本地扫描时的 provider id）：
                    // 同一 zcode 账号的多个 key 由 twin 检测识别后手动合并，
                    // 不再走 JWT 解码或身份端点探测。
                    RefreshResult::Done {
                        outcome,
                        group_hint: key.group_hint.clone(),
                    }
                }
            }
        };

        self.apply_refresh_result(&key_id, result).await;
    }

    async fn apply_refresh_result(&self, key_id: &str, result: RefreshResult) {
        let mut to_persist = true;
        let mut event: Option<(String, String, String)> = None;

        {
            let mut guard = self.inner.write().await;
            guard.refreshing.remove(key_id);

            let cooling_secs = guard.settings.cooling_secs;

            // 先把运行期字段写进一个局部快照，最后统一落库，避免多处可变借用。
            let Some(rt) = guard.runtime.get_mut(key_id) else {
                return;
            };
            let previous = rt.status;
            let mut new_account_id: Option<String> = None;
            let mut new_error: Option<String> = None;
            let mut new_status = previous;

            match result {
                RefreshResult::Skipped => {
                    to_persist = false;
                }
                RefreshResult::Disabled => {
                    new_status = KeyStatus::Disabled;
                    rt.cooling_until_ms = None;
                }
                RefreshResult::Cooling => {
                    new_status = KeyStatus::Cooling;
                }
                RefreshResult::Done {
                    outcome,
                    group_hint,
                } => {
                    rt.last_checked_ms = Some(outcome.snapshot.fetched_at_ms);
                    rt.quota = Some(outcome.snapshot.clone());

                    if outcome.snapshot.ok {
                        new_status = KeyStatus::Ok;
                        rt.last_error = None;
                        rt.cooling_until_ms = None;
                        rt.fail_streak = 0;
                        new_error = None;
                    } else {
                        let failure = outcome.failure.unwrap_or(FailureKind::Parse);
                        // 无 plan 是明确的中性状态：不进入错误、不重试、不冷却。
                        if failure == FailureKind::NoPlan {
                            new_status = KeyStatus::NoPlan;
                            rt.cooling_until_ms = None;
                            rt.fail_streak = 0;
                        } else if failure == FailureKind::RateLimited {
                            new_status = KeyStatus::Cooling;
                            rt.cooling_until_ms = Some(now_ms() + (cooling_secs * 1000) as i64);
                            rt.fail_streak = rt.fail_streak.saturating_add(1);
                        } else {
                            new_status = KeyStatus::Error;
                            rt.cooling_until_ms = None;
                            rt.fail_streak = rt.fail_streak.saturating_add(1);
                        }
                        new_error = outcome.snapshot.error.clone();
                        rt.last_error = new_error.clone();
                    }

                    // 归组：先取 key 副本，避免与 runtime 的借用冲突。
                    let key_snapshot = guard.keys.iter().find(|k| k.id == key_id).cloned();
                    if let Some(key) = key_snapshot
                        && (!key.account_pinned || key.account_id.is_none())
                    {
                        let aid = Self::ensure_account(&mut guard, &key, group_hint.as_deref());
                        new_account_id = Some(aid);
                    }
                }
            }

            if let Some(aid) = new_account_id
                && let Some(stored) = guard.keys.iter_mut().find(|k| k.id == key_id)
            {
                stored.account_id = Some(aid);
            }
            if let Some(rt) = guard.runtime.get_mut(key_id) {
                rt.status = new_status;
            }

            if new_status == KeyStatus::Error && previous != KeyStatus::Error {
                let label = guard
                    .keys
                    .iter()
                    .find(|k| k.id == key_id)
                    .map(|k| k.label.clone())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| key_id.to_string());
                event = Some((
                    "warn".to_string(),
                    format!("探测失败：{label}"),
                    new_error.unwrap_or_default(),
                ));
            }
        }

        if let Some((level, title, body)) = event {
            self.push_event(&level, title, body).await;
        }
        if to_persist {
            self.persist().await;
        }
    }

    // ──────────────────────── 提醒 ────────────────────────

    /// 检查「下一次刷新时间」提醒，命中则发桌面通知并写入事件。
    pub async fn check_reminders(&self) {
        let (settings, due) = {
            let guard = self.inner.read().await;
            if !guard.settings.notify_enabled {
                return;
            }
            let now = now_ms();
            let lead_ms = guard.settings.notify_lead_minutes as i64 * 60_000;
            let mut due: Vec<(String, String, i64)> = Vec::new();

            for account in &guard.accounts {
                if account.notify_muted {
                    continue;
                }
                let Some(reset_ms) = account_next_reset(&guard, account, now) else {
                    continue;
                };
                // 已进入提前提醒窗口，且尚未提醒过这一轮。
                if reset_ms - now <= lead_ms
                    && reset_ms > now - 60_000
                    && account.last_notified_reset_ms != Some(reset_ms)
                {
                    due.push((
                        account.id.clone(),
                        account_display_name(&guard, account),
                        reset_ms,
                    ));
                }
            }
            (guard.settings.clone(), due)
        };

        let notified = !due.is_empty();
        for (account_id, name, reset_ms) in due {
            {
                let mut guard = self.inner.write().await;
                if let Some(acc) = guard.accounts.iter_mut().find(|a| a.id == account_id) {
                    acc.last_notified_reset_ms = Some(reset_ms);
                }
            }
            let minutes = settings.notify_lead_minutes;
            let title = format!("{name} 的 zcode 额度即将刷新");
            let body = format!("约 {minutes} 分钟后（剩余 {}）额度窗口重置", {
                let left = (reset_ms - now_ms()) / 1000;
                format!("{:02}:{:02}", (left / 60).max(0), (left % 60).max(0))
            });
            self.push_event("info", title.clone(), body.clone()).await;

            if settings.notify_desktop {
                self.desktop_notify(title, body);
            }
        }

        if notified {
            self.persist().await;
        }
    }

    // ──────────────────────── 视图 ────────────────────────

    pub async fn view(&self) -> AppState {
        let guard = self.inner.read().await;
        let now = now_ms();

        let mut accounts: Vec<AccountView> = guard
            .accounts
            .iter()
            // 没有 key 的账户不渲染：正常情况下 load / 删除 / 归组都会回收空账户，
            // 这里再兜底一层，保证视图里不出现「暂无额度数据」的幽灵卡片。
            .filter(|account| {
                guard
                    .keys
                    .iter()
                    .any(|k| k.account_id.as_deref() == Some(account.id.as_str()))
            })
            .map(|account| {
                let keys: Vec<KeyView> = guard
                    .keys
                    .iter()
                    .filter(|k| k.account_id.as_deref() == Some(account.id.as_str()))
                    .map(|k| {
                        let rt = guard.runtime.get(&k.id);
                        let status = rt.map(|r| r.status).unwrap_or(KeyStatus::Idle);
                        KeyView {
                            id: k.id.clone(),
                            label: k.label.clone(),
                            masked: mask_secret(&k.secret),
                            secret: k.secret.clone(),
                            enabled: k.enabled,
                            status,
                            status_label: status.label().to_string(),
                            last_error: rt.and_then(|r| r.last_error.clone()),
                            last_checked_ms: rt.and_then(|r| r.last_checked_ms),
                            quota: rt.and_then(|r| r.quota.clone()),
                            account_id: account.id.clone(),
                            account_pinned: k.account_pinned,
                            source_group: k.group_hint.clone(),
                        }
                    })
                    .collect();

                let windows = aggregate_windows(&keys);
                let next_reset_ms = pick_next_reset(&windows, now);
                let status = worst_status(&keys.iter().map(|k| k.status).collect::<Vec<_>>());
                let last_checked_ms = keys.iter().filter_map(|k| k.last_checked_ms).max();
                let last_error = keys
                    .iter()
                    .find(|k| k.status == KeyStatus::Error)
                    .and_then(|k| k.last_error.clone());

                AccountView {
                    id: account.id.clone(),
                    name: account_display_name(&guard, account),
                    keys,
                    windows,
                    next_reset_ms,
                    status,
                    status_label: status.label().to_string(),
                    last_checked_ms,
                    last_error,
                    notify_muted: account.notify_muted,
                    // 下面统一填
                    twin_group: None,
                }
            })
            .collect();

        // 检测「疑似同一账户」：不同账户的配额窗口完全一致时，
        // 极可能是同一账号的多个 key（例如 zcode 里配的 API 2/3/4/5）。
        assign_twin_groups(&mut accounts);

        // 排序：有下一次刷新时间的靠前，然后按名称稳定排序。
        accounts.sort_by(|a, b| {
            let ka = a.next_reset_ms.unwrap_or(i64::MAX);
            let kb = b.next_reset_ms.unwrap_or(i64::MAX);
            kb.cmp(&ka).then_with(|| a.name.cmp(&b.name))
        });

        AppState {
            server_time_ms: now,
            accounts,
            settings: guard.settings.clone(),
            next_auto_refresh_ms: guard.next_auto_refresh_ms,
            refreshing: guard.refreshing.len(),
            // 时间正序（旧→新），最近 20 条：日志面板 stick_to_bottom 时
            // 自动滚到最新一条。
            events: {
                let start = guard.events.len().saturating_sub(20);
                guard.events.iter().skip(start).cloned().collect()
            },
        }
    }

    pub async fn set_next_auto_refresh(&self, at_ms: Option<i64>) {
        let mut guard = self.inner.write().await;
        guard.next_auto_refresh_ms = at_ms;
    }

    /// 手动刷新后重置自动刷新计时，并唤醒调度循环。
    pub async fn nudge_auto_refresh_now(&self) {
        let interval = self.settings().await.refresh_interval_secs;
        let next = (interval > 0).then(|| now_ms() + (interval as i64 * 1000));
        self.set_next_auto_refresh(next).await;
        self.refresh_requested.notify_waiters();
    }

    /// 发送一条测试桌面通知，用于确认系统通知可用。
    pub async fn test_notify(&self) {
        self.push_event("info", "测试通知", "这是一条来自 zcode 额度看板的测试通知")
            .await;
        self.desktop_notify(
            "zcode 额度看板".to_string(),
            "测试通知：桌面通知已可用。".to_string(),
        );
    }

    /// 返回当前需要刷新的全部 key id（供调度器使用）。
    pub async fn all_key_ids(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        guard.keys.iter().map(|k| k.id.clone()).collect()
    }

    pub async fn key_ids_for_account(&self, account_id: &str) -> Vec<String> {
        let guard = self.inner.read().await;
        guard
            .keys
            .iter()
            .filter(|k| k.account_id.as_deref() == Some(account_id))
            .map(|k| k.id.clone())
            .collect()
    }

    pub async fn key_exists(&self, id: &str) -> bool {
        let guard = self.inner.read().await;
        guard.keys.iter().any(|k| k.id == id)
    }

    pub async fn account_exists(&self, id: &str) -> bool {
        let guard = self.inner.read().await;
        guard.accounts.iter().any(|a| a.id == id)
    }
}

// ──────────────────────── 刷新结果 ────────────────────────

enum RefreshResult {
    Skipped,
    Disabled,
    Cooling,
    Done {
        outcome: client::FetchOutcome,
        group_hint: Option<String>,
    },
}

// ──────────────────────── 视图计算辅助 ────────────────────────

/// 把一个账户下所有 key 的配额窗口聚合起来。
fn aggregate_windows(keys: &[KeyView]) -> Vec<WindowView> {
    struct Agg {
        label: String,
        kind: String,
        max_percent: f64,
        next_reset_ms: Option<i64>,
        window_ms: Option<i64>,
        used: Option<f64>,
        total: Option<f64>,
        remaining: Option<f64>,
        per_key: Vec<WindowKeyPercent>,
    }

    let mut map: BTreeMap<String, Agg> = BTreeMap::new();

    for key in keys {
        let Some(snapshot) = key.quota.as_ref().filter(|q| q.ok) else {
            continue;
        };
        for limit in &snapshot.limits {
            let (wkey, label, _sort) = window_meta(&limit.kind, limit.unit);
            let percent = limit.percentage.map(normalize_percent).unwrap_or(0.0);
            let entry = map.entry(wkey).or_insert(Agg {
                label,
                kind: limit.kind.clone(),
                max_percent: 0.0,
                next_reset_ms: None,
                window_ms: window_duration_ms(&limit.kind, limit.unit),
                used: None,
                total: None,
                remaining: None,
                per_key: Vec::new(),
            });
            // 同账户多 key 共享同一额度池，取最大值代表最紧的情况。
            if percent > entry.max_percent {
                entry.max_percent = percent;
            }
            // 数字字段取最大值，与百分比口径一致
            if let Some(v) = limit.current_value.or(limit.used) {
                entry.used = Some(entry.used.map_or(v, |c| c.max(v)));
            }
            if let Some(v) = limit.usage.or(limit.limit) {
                entry.total = Some(entry.total.map_or(v, |c| c.max(v)));
            }
            if let Some(v) = limit.remaining {
                entry.remaining = Some(entry.remaining.map_or(v, |c| c.min(v)));
            }
            if let Some(reset) = limit.next_reset_time_ms {
                entry.next_reset_ms = Some(match entry.next_reset_ms {
                    Some(current) => current.min(reset),
                    None => reset,
                });
            }
            entry.per_key.push(WindowKeyPercent {
                key_id: key.id.clone(),
                key_label: key.label.clone(),
                used_percent: percent,
            });
        }
    }

    let mut windows: Vec<WindowView> = map
        .into_iter()
        .map(|(key, agg)| WindowView {
            key,
            label: agg.label,
            kind: agg.kind,
            used_percent: agg.max_percent,
            used: agg.used,
            total: agg.total,
            remaining: agg.remaining,
            next_reset_ms: agg.next_reset_ms,
            window_ms: agg.window_ms,
            key_count: agg.per_key.len(),
            per_key: agg.per_key,
        })
        .collect();
    windows.sort_by_key(|w| window_meta(&w.kind, parse_unit_from_key(w.key.as_str())).2);
    windows
}

/// 从聚合 key（`KIND:unit`）里还原 unit，用于排序。
fn parse_unit_from_key(key: &str) -> Option<u32> {
    key.rsplit_once(':')
        .and_then(|(_, unit)| unit.parse::<u32>().ok())
}

/// 在窗口列表中挑出最近的一次刷新时间：优先未来的，全过期则取最早的。
fn pick_next_reset(windows: &[WindowView], now: i64) -> Option<i64> {
    let mut future: Option<i64> = None;
    let mut any: Option<i64> = None;
    for w in windows {
        let Some(reset) = w.next_reset_ms else {
            continue;
        };
        if reset > now && future.map(|c| reset < c).unwrap_or(true) {
            future = Some(reset);
        }
        if any.map(|c| reset < c).unwrap_or(true) {
            any = Some(reset);
        }
    }
    future.or(any)
}

/// 账户级的最近刷新时间（与 `pick_next_reset` 同策略，但直接基于运行时状态）。
fn account_next_reset(guard: &Inner, account: &AccountRecord, now: i64) -> Option<i64> {
    let keys: Vec<KeyView> = guard
        .keys
        .iter()
        .filter(|k| k.account_id.as_deref() == Some(account.id.as_str()))
        .filter_map(|k| {
            let rt = guard.runtime.get(&k.id)?;
            let quota = rt.quota.as_ref().filter(|q| q.ok)?;
            Some((k, quota))
        })
        .map(|(k, quota)| KeyView {
            id: k.id.clone(),
            label: k.label.clone(),
            masked: String::new(),
            secret: String::new(),
            enabled: k.enabled,
            status: KeyStatus::Ok,
            status_label: String::new(),
            last_error: None,
            last_checked_ms: None,
            quota: Some(quota.clone()),
            account_id: account.id.clone(),
            account_pinned: k.account_pinned,
            source_group: k.group_hint.clone(),
        })
        .collect();

    let windows = aggregate_windows(&keys);
    pick_next_reset(&windows, now)
}

/// 从归组线索生成默认账户名，例如 `zcode-account:20241730945190210` → 该 ID。
fn account_name_from_hint(group: &str) -> String {
    let tail = group.rsplit(':').next().unwrap_or(group);
    if tail.is_empty() || tail == "__unknown__" {
        return "zcode 账户".to_string();
    }
    tail.to_string()
}

/// 给配额快照完全一致的账户打上同一个 twin 标记，供界面提示「疑似同一账户」。
///
/// 只有能成功拿到额度（`status == Ok`）的账户才参与比对——查不到额度的
/// 账户没有可比数据。同一身份（identity 相同）的账户本来就会归到一起，
/// 不会走到这里，所以不会误报。
fn assign_twin_groups(accounts: &mut [AccountView]) {
    let mut by_sig: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();

    for (idx, account) in accounts.iter().enumerate() {
        if account.status != KeyStatus::Ok || account.windows.is_empty() {
            continue;
        }
        let sig = quota_signature(&account.windows);
        by_sig.entry(sig).or_default().push(idx);
    }

    for (sig, idxs) in by_sig {
        if idxs.len() < 2 {
            continue;
        }
        let tag = format!("twin-{:x}", simple_hash(&sig));
        for idx in idxs {
            accounts[idx].twin_group = Some(tag.clone());
        }
    }
}

/// 用窗口的类型、用量与百分比算一个稳定签名。
///
/// **故意不纳入绝对时间戳**：不同 key 的请求时刻相差几毫秒，滚动窗口的
/// `nextResetTime` 就会不同，但那不代表它们属于不同账户。
/// 只用「同一个账户才有」的稳定特征：窗口种类、单位、额度与百分比。
fn quota_signature(windows: &[WindowView]) -> String {
    let mut parts: Vec<String> = windows
        .iter()
        .map(|w| {
            format!(
                "{}|{}|{:?}|{}|{}|{}|{}",
                w.kind,
                w.used_percent.round() as i64,
                w.window_ms,
                w.used.map(|v| v.round() as i64).unwrap_or(-1),
                w.total.map(|v| v.round() as i64).unwrap_or(-1),
                w.remaining.map(|v| v.round() as i64).unwrap_or(-1),
                // unit 从 key 里还原，作为窗口标识的一部分
                w.key.rsplit(':').next().unwrap_or("-")
            )
        })
        .collect();
    parts.sort();
    parts.join(";")
}

/// 简单的 FNV-1a 哈希，用于把签名压成短标签。
fn simple_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 账户展示名：自定义名 → 身份 → 「账户 + 首个 key 备注」。
fn account_display_name(guard: &Inner, account: &AccountRecord) -> String {
    if let Some(name) = &account.name
        && !name.trim().is_empty()
    {
        return name.clone();
    }
    let first_label = guard
        .keys
        .iter()
        .find(|k| k.account_id.as_deref() == Some(account.id.as_str()))
        .map(|k| k.label.clone())
        .unwrap_or_default();
    if !first_label.is_empty() {
        return first_label;
    }
    format!("账户 {}", &account.id[..account.id.len().min(6)])
}

/// 取一组状态里「最差」的那个作为账户整体状态。
fn worst_status(statuses: &[KeyStatus]) -> KeyStatus {
    if statuses.is_empty() {
        return KeyStatus::Idle;
    }
    if statuses.iter().all(|s| *s == KeyStatus::Disabled) {
        return KeyStatus::Disabled;
    }
    if statuses.contains(&KeyStatus::Error) {
        return KeyStatus::Error;
    }
    if statuses.contains(&KeyStatus::Cooling) {
        return KeyStatus::Cooling;
    }
    if statuses.contains(&KeyStatus::Idle) {
        return KeyStatus::Idle;
    }
    if statuses.contains(&KeyStatus::NoPlan) {
        return KeyStatus::NoPlan;
    }
    KeyStatus::Ok
}
