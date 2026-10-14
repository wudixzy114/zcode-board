// 用真实本机凭证做一次探测（仅在显式开启时运行，避免常规测试联网）。
// 运行：cargo test -p zcode-core --test real_probe -- --ignored --nocapture
#[tokio::test]
#[ignore = "需要真实网络与凭证"]
async fn probe_local_keys() {
    use zcode_core::app::Manager;
    let dir = std::env::temp_dir().join("zcode-realprobe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let manager = Manager::new(&dir).await;

    let discovery = zcode_core::discover::scan();
    println!("\n扫描到 {} 条凭证", discovery.keys.len());
    for w in &discovery.warnings {
        println!("  警告: {w}");
    }
    for k in &discovery.keys {
        println!(
            "  - {} | {} | {}…",
            k.label,
            k.group,
            &k.secret[..6.min(k.secret.len())]
        );
    }

    let (imported, _total) = manager.import_discovered(&discovery.keys).await;
    println!("\n导入 {imported} 条");

    // 等探测结束
    for _ in 0..80 {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let v = manager.view().await;
        let done = v.refreshing == 0
            && v.accounts.iter().all(|a| {
                a.keys
                    .iter()
                    .all(|k| k.status != zcode_model::KeyStatus::Idle)
            });
        if done {
            break;
        }
    }

    let v = manager.view().await;
    println!("\n=== 探测结果 ===");
    for a in &v.accounts {
        println!("账户 {} ({} keys, {:?})", a.name, a.keys.len(), a.status);
        for k in &a.keys {
            println!(
                "   {} → {:?} {}",
                k.label,
                k.status,
                k.last_error.clone().unwrap_or_default()
            );
            if let Some(q) = &k.quota
                && q.ok
            {
                for l in &q.limits {
                    println!(
                        "      {} unit={:?} number={:?} {}% used={:?}/{:?} remaining={:?} reset={:?}",
                        l.kind,
                        l.unit,
                        l.number,
                        l.percentage.unwrap_or(0.0),
                        l.current_value,
                        l.usage,
                        l.remaining,
                        l.next_reset_time_ms
                    );
                    for d in &l.usage_details {
                        println!("         明细 {} = {}", d.model_code, d.usage);
                    }
                }
            }
        }
        for w in &a.windows {
            println!(
                "   [窗口] {} {}% used={:?} total={:?} next_reset={:?}",
                w.label, w.used_percent, w.used, w.total, w.next_reset_ms
            );
        }
    }
}
