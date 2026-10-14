//! 后台调度：定时刷新 + 下一次刷新时间提醒。

use std::time::Duration;

use tokio::time::sleep;
use zcode_model::Settings;

use crate::app::Manager;
use crate::util::now_ms;

/// 提醒检查的粒度。倒计时只需要秒级精度，3 秒足够且不吃 CPU。
const REMINDER_TICK: Duration = Duration::from_secs(3);

/// 启动所有后台任务。调用方需已在 tokio 运行时上下文中。
pub fn spawn_background(manager: Manager) {
    tokio::spawn(refresh_loop(manager.clone()));
    tokio::spawn(reminder_loop(manager));
}

/// 定时刷新循环。刷新间隔可在运行时调整，改完下一个周期即生效。
async fn refresh_loop(manager: Manager) {
    // 启动后立刻拉一次，避免刚打开界面时空白。
    initial_refresh(&manager).await;

    loop {
        let settings = manager.settings().await;
        if settings.refresh_interval_secs == 0 {
            // 关闭自动刷新：仍周期性回看设置，方便随时重新启用。
            manager.set_next_auto_refresh(None).await;
            wait_or_wake(&manager, Duration::from_secs(5)).await;
            continue;
        }

        let next = now_ms() + (settings.refresh_interval_secs as i64 * 1000);
        manager.set_next_auto_refresh(Some(next)).await;
        wait_until(&manager, next).await;

        let ids = manager.all_key_ids().await;
        if !ids.is_empty() {
            let concurrency = manager.settings().await.concurrency.max(1);
            manager.run_refresh(ids, concurrency).await;
        }
    }
}

async fn initial_refresh(manager: &Manager) {
    let ids = manager.all_key_ids().await;
    if ids.is_empty() {
        return;
    }
    let settings: Settings = manager.settings().await;
    manager.run_refresh(ids, settings.concurrency.max(1)).await;
}

/// 提醒循环：按固定粒度检查是否进入「下一次刷新」的提前提醒窗口。
async fn reminder_loop(manager: Manager) {
    loop {
        manager.check_reminders().await;
        sleep(REMINDER_TICK).await;
    }
}

/// 等待到指定时间戳，期间设置变更或手动刷新会提前唤醒。
async fn wait_until(manager: &Manager, target_ms: i64) {
    let remaining = target_ms - now_ms();
    if remaining <= 0 {
        return;
    }
    let remaining = Duration::from_millis(remaining as u64);
    tokio::select! {
        _ = sleep(remaining) => {}
        _ = manager.settings_changed.notified() => {}
        _ = manager.refresh_requested.notified() => {}
    }
}

/// 等待固定时长，设置变更或手动刷新会提前唤醒。
async fn wait_or_wake(manager: &Manager, duration: Duration) {
    tokio::select! {
        _ = sleep(duration) => {}
        _ = manager.settings_changed.notified() => {}
        _ = manager.refresh_requested.notified() => {}
    }
}
