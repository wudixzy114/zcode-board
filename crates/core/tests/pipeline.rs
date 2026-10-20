//! 端到端集成测试：用本地假 HTTP 服务器跑通
//! 「拉取额度 → 解析 → 多 key 自动归组 → 窗口聚合 → 提前提醒」整条链路。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zcode_core::app::Manager;
use zcode_model::{AddKeyRequest, AppState, KeyStatus, MoveKeyRequest, Settings};

/// 假上游：按请求路径分别返回额度与身份数据。
async fn spawn_fake_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();

                // 请求行形如 `GET /path HTTP/1.1`
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                let body = if path.contains("noplan") {
                    // 真实 API 在账号没有 coding plan 时的响应
                    r#"{"code":500,"msg":"当前用户不存在coding plan","success":false}"#.to_string()
                } else if path.contains("quota/limit") {
                    // 动态生成时间：5 小时窗口 30 分钟后重置，周窗口 7 天后重置。
                    let now = zcode_core::util::now_ms();
                    let five_hour_reset = now + 30 * 60_000;
                    let week_reset = now + 7 * 24 * 3_600_000;
                    format!(
                        r#"{{"success":true,"msg":"","data":{{"limits":[
                            {{"type":"TOKENS_LIMIT","percentage":42.5,"unit":3,"nextResetTime":{five_hour_reset}}},
                            {{"type":"TOKENS_LIMIT","percentage":12.0,"unit":6,"nextResetTime":{week_reset}}},
                            {{"type":"TIME_LIMIT","unit":5,"number":1,"usage":4000,"currentValue":184,"remaining":3816,"percentage":4,"nextResetTime":{monthly_reset},"usageDetails":[{{"modelCode":"search-prime","usage":141}},{{"modelCode":"web-reader","usage":43}}]}}
                        ]}}}}"#,
                        monthly_reset = now + 29 * 24 * 3_600_000
                    )
                } else if path.contains("getCustomerInfo") {
                    r#"{"code":0,"msg":"ok","data":{"email":"me@example.com",
                        "organizations":[{"organizationId":4242,"organizationName":"默认机构"}]}}"#
                        .to_string()
                } else {
                    r#"{"success":false,"msg":"unknown path"}"#.to_string()
                };

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://127.0.0.1:{}", addr.port())
}

/// 轮询等待条件成立，最多等 10 秒。
async fn wait_until<F>(manager: &Manager, mut predicate: F)
where
    F: FnMut(&AppState) -> bool,
{
    for _ in 0..100 {
        let view = manager.view().await;
        if predicate(&view) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等待条件超时：视图状态未达预期");
}

fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zcode-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create dir");
    dir
}

fn settings_for(base: &str) -> Settings {
    Settings {
        refresh_interval_secs: 60,
        concurrency: 4,
        request_timeout_secs: 5,
        cooling_secs: 60,
        notify_enabled: false,
        notify_lead_minutes: 10,
        notify_desktop: false,
        quota_endpoint: format!("{base}/api/monitor/usage/quota/limit"),
        extra_headers: String::new(),
        start_with_windows: false,
        minimize_to_tray: false,
        rate_span_secs: 3600,
    }
}

#[tokio::test]
async fn keys_with_same_group_hint_share_account_and_aggregate_windows() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("grouping");
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;

    // 同一个 provider 下的两个 key → 归到同一账户。
    let discovered = vec![
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 2".into(),
            secret: "id1.secret1".into(),
            group: "zcode-provider:bigmodel-standard-api-2".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 3".into(),
            secret: "id2.secret2".into(),
            group: "zcode-provider:bigmodel-standard-api-2".into(),
        },
    ];
    let (imported, _) = manager.import_discovered(&discovered).await;
    assert_eq!(imported, 2);

    wait_until(&manager, |view| {
        view.accounts.len() == 1 && view.accounts[0].keys.len() == 2
    })
    .await;

    let view = manager.view().await;
    let account = &view.accounts[0];

    assert_eq!(account.name, "bigmodel-standard-api-2");
    assert_eq!(account.keys.len(), 2);
    assert!(
        account.keys.iter().all(|k| k.status == KeyStatus::Ok),
        "全部 key 应探测成功，实际: {:?}",
        account.keys.iter().map(|k| k.status).collect::<Vec<_>>()
    );

    // 窗口聚合：三个窗口（5小时 Token / 周 Token / 月度调用）
    let five_hour = account
        .windows
        .iter()
        .find(|w| w.kind == "TOKENS_LIMIT" && w.label.contains("5 小时"));
    let week = account
        .windows
        .iter()
        .find(|w| w.kind == "TOKENS_LIMIT" && w.label.contains("周"));
    let monthly = account.windows.iter().find(|w| w.kind == "TIME_LIMIT");
    assert!(
        five_hour.is_some(),
        "应出现 5 小时 Token 窗口: {:?}",
        account.windows
    );
    assert!(week.is_some(), "应出现周窗口: {:?}", account.windows);
    assert!(
        monthly.is_some(),
        "应出现月度调用窗口: {:?}",
        account.windows
    );
    assert!((five_hour.unwrap().used_percent - 42.5).abs() < 1e-6);
    // 月度窗口应有完整数字
    let monthly = monthly.unwrap();
    assert_eq!(monthly.used, Some(184.0));
    assert_eq!(monthly.total, Some(4000.0));
    assert_eq!(monthly.remaining, Some(3816.0));

    // 账户级下一次刷新时间应存在
    assert!(account.next_reset_ms.is_some());
}

#[tokio::test]
async fn unknown_identity_stays_separate_then_manual_merge_works() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("merge");
    let manager = Manager::new(&dir).await;

    manager.update_settings(settings_for(&base)).await;

    manager
        .add_keys(&AddKeyRequest {
            secrets: "id1.secret1\nid2.secret2\n".to_string(),
            label: None,
        })
        .await;

    wait_until(&manager, |view| {
        view.accounts.len() == 2 && view.accounts.iter().all(|a| a.keys.len() == 1)
    })
    .await;

    // 手动把第二个 key 并入第一个账户。
    let view = manager.view().await;
    let target = view.accounts[0].id.clone();
    let moving = view.accounts[1].keys[0].id.clone();
    manager
        .move_key(
            &moving,
            &MoveKeyRequest {
                account_id: Some(target.clone()),
            },
        )
        .await;

    let view = manager.view().await;
    assert_eq!(view.accounts.len(), 1, "手动合并后应只剩一个账户");
    assert_eq!(view.accounts[0].keys.len(), 2);
    assert!(view.accounts[0].keys.iter().any(|k| k.account_pinned));
}

#[tokio::test]
async fn import_discovered_groups_by_zcode_account_id() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("import");
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;

    // 模拟从本机 zcode 扫到的结果：账户 A 有 2 个 key，账户 B 有 1 个。
    let discovered = vec![
        zcode_core::discover::DiscoveredKey {
            label: "智谱 coding-plan · 20241730945190210".into(),
            secret: "id1.secret1".into(),
            group: "zcode-account:20241730945190210".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "智谱 coding-plan · 20241730945190210".into(),
            secret: "id2.secret2".into(),
            group: "zcode-account:20241730945190210".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "智谱 coding-plan · 999888777".into(),
            secret: "id3.secret3".into(),
            group: "zcode-account:999888777".into(),
        },
    ];

    let (imported, total) = manager.import_discovered(&discovered).await;
    assert_eq!(imported, 3);
    assert_eq!(total, 3);

    // 等待后台探测完成。
    wait_until(&manager, |view| {
        view.accounts.len() == 2
            && view.accounts.iter().all(|a| !a.keys.is_empty())
            && view
                .accounts
                .iter()
                .all(|a| a.keys.iter().all(|k| k.status == KeyStatus::Ok))
    })
    .await;

    let view = manager.view().await;
    let account_a = view
        .accounts
        .iter()
        .find(|a| a.name == "20241730945190210")
        .expect("账户 A 应按 zcode 账户 ID 归组");
    assert_eq!(account_a.keys.len(), 2, "同一账户 ID 的两个 key 应合并");
    assert_eq!(account_a.name, "20241730945190210");

    let account_b = view
        .accounts
        .iter()
        .find(|a| a.name == "999888777")
        .expect("账户 B 应独立存在");
    assert_eq!(account_b.keys.len(), 1);

    // 重复导入同一批应全部被跳过。
    let (imported_again, _) = manager.import_discovered(&discovered).await;
    assert_eq!(imported_again, 0, "重复凭证应被跳过");
}

#[tokio::test]
async fn reminder_fires_once_per_reset_window() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("reminder");
    let manager = Manager::new(&dir).await;

    let mut settings = settings_for(&base);
    settings.notify_enabled = true;
    settings.notify_lead_minutes = 100_000; // 提前量极大，必然命中
    manager.update_settings(settings).await;

    manager
        .add_keys(&AddKeyRequest {
            secrets: "id1.secret1\n".to_string(),
            label: None,
        })
        .await;

    wait_until(&manager, |view| {
        view.accounts.len() == 1 && view.accounts[0].next_reset_ms.is_some()
    })
    .await;

    // 第一次检查应触发提醒，第二次不应重复。
    manager.check_reminders().await;
    let events_after_first = manager.view().await.events.len();
    manager.check_reminders().await;
    let events_after_second = manager.view().await.events.len();

    assert!(events_after_first > 0, "应产生提醒事件");
    assert_eq!(
        events_after_first, events_after_second,
        "同一个刷新周期内不应重复提醒"
    );
}

#[tokio::test]
async fn no_plan_is_neutral_state_not_error() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("noplan");
    let manager = Manager::new(&dir).await;

    // 把额度端点指向 noplan 分支，模拟「账号没有 coding plan」
    let mut settings = settings_for(&base);
    settings.quota_endpoint = format!("{base}/api/monitor/usage/quota/limit?noplan=1");
    manager.update_settings(settings).await;

    manager
        .add_keys(&AddKeyRequest {
            secrets: "id1.secret1\n".to_string(),
            label: None,
        })
        .await;

    wait_until(&manager, |view| {
        view.accounts.len() == 1 && view.accounts[0].keys.len() == 1
    })
    .await;

    let view = manager.view().await;
    let key = &view.accounts[0].keys[0];
    assert_eq!(
        key.status,
        KeyStatus::NoPlan,
        "无 plan 应是中性状态而非异常"
    );
    assert!(
        key.last_error
            .as_deref()
            .unwrap_or("")
            .contains("coding plan"),
        "错误信息应说明原因: {:?}",
        key.last_error
    );
    // 账户整体状态也应反映为 NoPlan
    assert_eq!(view.accounts[0].status, KeyStatus::NoPlan);
}

#[tokio::test]
async fn identical_quota_gets_twin_group_and_can_merge() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("twin");
    let manager = Manager::new(&dir).await;

    // 身份端点失效 → 各自独立成户；额度数据一致 → 应被识别为疑似同一账户
    let settings = settings_for(&base);
    manager.update_settings(settings).await;

    manager
        .add_keys(&AddKeyRequest {
            secrets: "id1.secret1\nid2.secret2\nid3.secret3\n".to_string(),
            label: None,
        })
        .await;

    wait_until(&manager, |view| {
        view.accounts.len() == 3
            && view
                .accounts
                .iter()
                .all(|a| a.keys.iter().all(|k| k.status == KeyStatus::Ok))
    })
    .await;

    // 三个账户额度完全相同 → 应被打上同一个 twin 标记
    let view = manager.view().await;
    let tags: std::collections::HashSet<Option<String>> =
        view.accounts.iter().map(|a| a.twin_group.clone()).collect();
    assert_eq!(tags.len(), 1, "三个账户应共享同一个 twin 标记: {tags:?}");
    assert!(
        tags.into_iter().next().flatten().is_some(),
        "twin 标记应为 Some"
    );

    // 合并到第一个账户
    let target = view.accounts[0].id.clone();
    let sources: Vec<String> = view.accounts[1..].iter().map(|a| a.id.clone()).collect();
    assert!(manager.merge_accounts(&sources, &target).await);

    let view = manager.view().await;
    assert_eq!(view.accounts.len(), 1, "合并后应只剩一个账户");
    assert_eq!(
        view.accounts[0].keys.len(),
        3,
        "三个 key 应全部挂到目标账户"
    );
    assert!(
        view.accounts[0].keys.iter().all(|k| k.account_pinned),
        "合并后的 key 应固定归属，避免被重新归组打散"
    );
}

#[tokio::test]
async fn quota_details_are_parsed() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("details");
    let manager = Manager::new(&dir).await;

    let settings = settings_for(&base);
    manager.update_settings(settings).await;

    manager
        .add_keys(&AddKeyRequest {
            secrets: "id1.secret1\n".to_string(),
            label: None,
        })
        .await;

    wait_until(&manager, |view| {
        view.accounts.len() == 1 && view.accounts[0].keys.len() == 1
    })
    .await;

    let view = manager.view().await;
    let quota = view.accounts[0].keys[0]
        .quota
        .as_ref()
        .expect("应有额度快照");
    assert!(quota.ok);

    // TIME_LIMIT unit 5 应有完整的数字字段
    let monthly = quota
        .limits
        .iter()
        .find(|l| l.kind == "TIME_LIMIT" && l.unit == Some(5))
        .expect("应有 TIME_LIMIT unit=5 窗口");
    assert_eq!(monthly.usage, Some(4000.0));
    assert_eq!(monthly.current_value, Some(184.0));
    assert_eq!(monthly.remaining, Some(3816.0));
    assert_eq!(monthly.number, Some(1));
    assert_eq!(monthly.usage_details.len(), 2, "应解析出 2 条模型明细");
    assert_eq!(monthly.usage_details[0].model_code, "search-prime");
    assert_eq!(monthly.usage_details[0].usage, 141.0);

    // 聚合后的窗口应带「已用/总量」
    let monthly_window = view.accounts[0]
        .windows
        .iter()
        .find(|w| w.kind == "TIME_LIMIT")
        .expect("应有 TIME_LIMIT 聚合窗口");
    assert_eq!(monthly_window.used, Some(184.0));
    assert_eq!(monthly_window.total, Some(4000.0));
    assert_eq!(monthly_window.remaining, Some(3816.0));
}

// ──────────────────────── 重复导入 / 删除 / 清理 ────────────────────────

#[tokio::test]
async fn importing_twice_does_not_duplicate_entries() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("double-import");
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;

    let discovered = vec![
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 2".into(),
            secret: "id1.secret1".into(),
            group: "zcode-provider:bigmodel-standard-api-2".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 3".into(),
            secret: "id2.secret2".into(),
            group: "zcode-provider:bigmodel-standard-api-3".into(),
        },
    ];

    let (first, _) = manager.import_discovered(&discovered).await;
    assert_eq!(first, 2);
    // 再导一次：secret 已存在，应全部跳过。
    let (second, total) = manager.import_discovered(&discovered).await;
    assert_eq!(second, 0);
    assert_eq!(total, 2);

    wait_until(&manager, |view| {
        view.accounts.len() == 2 && view.accounts.iter().all(|a| a.keys.len() == 1)
    })
    .await;

    // 合并成同一账户后再导一次，也不应产生重复卡片。
    let view = manager.view().await;
    let ids: Vec<String> = view.accounts.iter().map(|a| a.id.clone()).collect();
    assert!(manager.merge_accounts(&ids[1..], &ids[0]).await);
    let (third, _) = manager.import_discovered(&discovered).await;
    assert_eq!(third, 0);

    wait_until(&manager, |view| {
        view.accounts.len() == 1 && view.accounts[0].keys.len() == 2
    })
    .await;
}

#[tokio::test]
async fn remove_account_deletes_its_keys_and_card() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("remove-account");
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;

    let discovered = vec![
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API".into(),
            secret: "id1.secret1".into(),
            group: "zcode-provider:bigmodel-standard-api".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 2".into(),
            secret: "id2.secret2".into(),
            group: "zcode-provider:bigmodel-standard-api-2".into(),
        },
    ];
    manager.import_discovered(&discovered).await;
    wait_until(&manager, |view| view.accounts.len() == 2).await;

    let view = manager.view().await;
    let target = view.accounts[0].id.clone();
    let key_count = view.accounts[0].keys.len();
    assert!(key_count >= 1);

    let removed = manager.remove_account(&target).await;
    assert_eq!(removed, key_count);

    let view = manager.view().await;
    assert_eq!(view.accounts.len(), 1, "删除后应只剩另一个账户");
    let total_keys: usize = view.accounts.iter().map(|a| a.keys.len()).sum();
    assert_eq!(total_keys, 1, "被删账户的 key 应一并消失");

    // 重启后也不应复活（持久化生效）。
    let manager2 = Manager::new(&dir).await;
    let view = manager2.view().await;
    assert_eq!(view.accounts.len(), 1);
}

#[tokio::test]
async fn cleanup_stale_only_removes_vanished_scan_keys() {
    let base = spawn_fake_upstream().await;
    let dir = test_dir("cleanup-stale");
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;

    // 手动粘贴的 key（group_hint = None）+ 两个本机同步 key。
    manager
        .add_keys(&AddKeyRequest {
            secrets: "manual.secret".to_string(),
            label: Some("手动key".into()),
        })
        .await;
    let discovered = vec![
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 2".into(),
            secret: "id1.secret1".into(),
            group: "zcode-provider:bigmodel-standard-api-2".into(),
        },
        zcode_core::discover::DiscoveredKey {
            label: "BigModel API 3".into(),
            secret: "id2.secret2".into(),
            group: "zcode-provider:bigmodel-standard-api-3".into(),
        },
    ];
    manager.import_discovered(&discovered).await;
    wait_until(&manager, |view| {
        view.accounts.iter().map(|a| a.keys.len()).sum::<usize>() == 3
    })
    .await;

    // 本机现在只剩 id1.secret1：id2.secret2 属于「已失效」。
    let live: std::collections::HashSet<String> = ["id1.secret1".to_string()].into_iter().collect();
    let removed = manager.cleanup_stale(&live).await;
    assert_eq!(removed, 1, "只应清掉本机已不存在的那个同步 key");

    let view = manager.view().await;
    let secrets: Vec<&str> = view
        .accounts
        .iter()
        .flat_map(|a| a.keys.iter())
        .map(|k| k.secret.as_str())
        .collect();
    assert!(secrets.contains(&"manual.secret"), "手动 key 不应被清理");
    assert!(
        secrets.contains(&"id1.secret1"),
        "仍在用的同步 key 不应被清理"
    );
    assert!(!secrets.contains(&"id2.secret2"), "失效的同步 key 应被清掉");
    // 失效 key 所在的账户应被回收，不留下空卡片。
    assert!(
        view.accounts.iter().all(|a| !a.keys.is_empty()),
        "不应有幽灵账户卡片"
    );
}

#[tokio::test]
async fn stale_state_file_is_migrated_on_load() {
    let dir = test_dir("migrate");
    // 模拟历史版本写下的脏数据：废弃来源的 key、重复 secret、空账户。
    let legacy = serde_json::json!({
        "keys": [
            {"id": "k1", "label": "ZCode 业务 JWT", "secret": "jwt-a", "enabled": true,
             "account_id": "a-jwt", "group_hint": "zcode-jwt", "account_pinned": false, "created_at_ms": 1},
            {"id": "k2", "label": "BigModel OAuth token", "secret": "oauth-a", "enabled": true,
             "account_id": "a-oauth", "group_hint": "zcode-oauth:bigmodel", "account_pinned": false, "created_at_ms": 2},
            {"id": "k3", "label": "老配置 key", "secret": "cfg-a", "enabled": true,
             "account_id": "a-cfg", "group_hint": "zcode-config:builtin:x", "account_pinned": false, "created_at_ms": 3},
            {"id": "k4", "label": "BigModel API 2", "secret": "id1.secret1", "enabled": true,
             "account_id": null, "group_hint": "zcode-provider:p2", "account_pinned": false, "created_at_ms": 4},
            {"id": "k5", "label": "BigModel API 2 重复", "secret": "id1.secret1", "enabled": true,
             "account_id": null, "group_hint": "zcode-provider:p2", "account_pinned": false, "created_at_ms": 5},
            {"id": "k6", "label": "手动 key", "secret": "manual-a", "enabled": true,
             "account_id": null, "group_hint": null, "account_pinned": false, "created_at_ms": 6}
        ],
        "accounts": [
            {"id": "a-jwt", "name": "zcode-jwt", "group_tag": "zcode:zcode-jwt", "last_notified_reset_ms": null, "notify_muted": false, "created_at_ms": 1},
            {"id": "a-ghost", "name": "bigmodel", "group_tag": null, "last_notified_reset_ms": null, "notify_muted": false, "created_at_ms": 2},
            {"id": "a-jwt-dup", "name": "zcode-jwt", "group_tag": "zcode:zcode-jwt", "last_notified_reset_ms": null, "notify_muted": false, "created_at_ms": 3}
        ],
        "settings": Settings::default()
    });
    std::fs::write(
        dir.join("state.json"),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let _manager = Manager::new(&dir).await;
    // 迁移发生在落库前：直接读回文件断言（视图要等刷新归组后才显示 key）。
    let text = std::fs::read_to_string(dir.join("state.json")).unwrap();
    let persisted: serde_json::Value = serde_json::from_str(&text).unwrap();
    let keys = persisted["keys"].as_array().unwrap();
    let secrets: Vec<&str> = keys.iter().map(|k| k["secret"].as_str().unwrap()).collect();

    assert!(
        !secrets
            .iter()
            .any(|s| s.starts_with("jwt-") || s.starts_with("oauth-") || s.starts_with("cfg-")),
        "废弃来源的残留 key 应被迁移清掉，实际: {secrets:?}"
    );
    assert_eq!(
        secrets.iter().filter(|s| **s == "id1.secret1").count(),
        1,
        "重复 secret 应去重，实际: {secrets:?}"
    );
    assert!(secrets.contains(&"manual-a"), "手动添加的 key 必须保留");

    let accounts = persisted["accounts"].as_array().unwrap();
    let names: Vec<&str> = accounts
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert!(
        accounts
            .iter()
            .all(|a| !a["id"].as_str().unwrap().is_empty()),
        "空账户（幽灵卡片）应被清掉，实际: {names:?}"
    );
    assert!(
        !names.contains(&"zcode-jwt"),
        "废弃来源的账户应被清掉，实际: {names:?}"
    );
    assert!(
        !names.contains(&"bigmodel"),
        "空账户应被清掉，实际: {names:?}"
    );
}

// ─────────────────────── 速率：采样 → 曲线 → 持久化 ───────────────────────

/// 每次请求用量都递增的假上游：模拟真实消耗，让速率差分有非零信号。
async fn spawn_growing_upstream() -> String {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let counter = Arc::new(AtomicI64::new(0));

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            let n = counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let Ok(read) = socket.read(&mut buf).await else {
                    return;
                };
                let _ = String::from_utf8_lossy(&buf[..read]);

                // 第 n 次请求：月度调用窗口已用 n*300（数值 + 百分比都有）；
                // 5 小时 Token 窗口复刻真实上游——只有百分比、没有数值用量。
                let used = n * 300;
                let now = zcode_core::util::now_ms();
                let body = format!(
                    r#"{{"success":true,"msg":"","data":{{"limits":[
                        {{"type":"TIME_LIMIT","unit":5,"number":1,"usage":100000,"currentValue":{used},"remaining":{},"percentage":{},"nextResetTime":{}}},
                        {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":{},"nextResetTime":{}}}
                    ]}}}}"#,
                    100_000 - used,
                    used * 100 / 100_000,
                    now + 29 * 24 * 3_600_000,
                    40 + n,
                    now + 5 * 3_600_000
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://127.0.0.1:{}", addr.port())
}

#[tokio::test]
async fn rate_accumlates_across_refreshes_and_survives_restart() {
    let base = spawn_growing_upstream().await;
    let dir = test_dir("rate");

    {
        let manager = Manager::new(&dir).await;
        manager.update_settings(settings_for(&base)).await;
        manager
            .add_keys(&AddKeyRequest {
                secrets: "id1.secret1".to_string(),
                label: None,
            })
            .await;

        // 第一次刷新：有窗口但还没有速率（只有 1 个采样点）。
        wait_until(&manager, |view| {
            view.accounts
                .first()
                .is_some_and(|a| a.windows.iter().any(|w| w.used.is_some()))
        })
        .await;
        let view = manager.view().await;
        assert!(
            view.accounts[0].windows.iter().all(|w| w.rate.is_none()),
            "单个采样点不应有速率"
        );

        // 间隔超过最小可分辨间隔（5 秒）再刷一次：差分成立，速率出现。
        tokio::time::sleep(Duration::from_millis(5_500)).await;
        let ids = manager.all_key_ids().await;
        manager.run_refresh(ids, 2).await;

        wait_until(&manager, |view| {
            view.accounts
                .first()
                .is_some_and(|a| a.windows.iter().any(|w| w.rate.is_some()))
        })
        .await;

        let view = manager.view().await;
        let windows = &view.accounts[0].windows;

        // 月度调用窗口：数值序列 → 次/min，量级 ~3000/min。
        let monthly = windows
            .iter()
            .find(|w| w.kind == "TIME_LIMIT")
            .expect("应有月度调用窗口");
        let rate = monthly.rate.as_ref().expect("月度窗口应有速率");
        assert_eq!(rate.unit, "次/min");
        assert!(!rate.percent_based);
        assert!(!rate.points.is_empty(), "两次采样至少产出一个速率点");
        // 两次请求相隔 ~5.5s、用量差 300 → 速率约 3000/min，放宽到 > 1000 防计时抖动。
        assert!(
            rate.points.iter().all(|p| p.per_min > 1000.0),
            "速率点应为正且量级正确: {:?}",
            rate.points
        );

        // 5 小时 Token 窗口：上游只给百分比 → 百分比差分序列，单位 %/h。
        let five_hour = windows
            .iter()
            .find(|w| w.kind == "TOKENS_LIMIT")
            .expect("应有 5 小时 Token 窗口");
        let rate = five_hour.rate.as_ref().expect("5 小时窗口也应有速率");
        assert_eq!(rate.unit, "%/h");
        assert!(rate.percent_based, "无数值用量的窗口应走百分比序列");
        assert!(
            !rate.points.is_empty(),
            "百分比序列两次采样也应产出速率点: {:?}",
            rate.points
        );

        // 强制落盘（正常路径有 60 秒节流），给重启续存断言用。
        manager.persist_history_now().await;
    }

    // 重启：历史从 history.json 读回。窗口视图要等第一次刷新重建
    // （窗口来自内存 runtime），刷一次后速率曲线应带着旧点回来。
    let manager = Manager::new(&dir).await;
    manager.update_settings(settings_for(&base)).await;
    let ids = manager.all_key_ids().await;
    manager.run_refresh(ids, 2).await;
    wait_until(&manager, |view| {
        view.accounts
            .first()
            .is_some_and(|a| a.windows.iter().any(|w| w.rate.is_some()))
    })
    .await;
    let view = manager.view().await;
    let rate = view.accounts[0]
        .windows
        .iter()
        .find(|w| w.rate.is_some())
        .and_then(|w| w.rate.as_ref())
        .expect("重启刷新后速率应恢复");
    assert!(
        !rate.points.is_empty(),
        "重启前的采样点应从 history.json 恢复"
    );
}
