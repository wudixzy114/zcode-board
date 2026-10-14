//! egui / eframe 入口。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bridge;
mod icons;
mod tray;

use eframe::NativeOptions;
use egui::{IconData, ViewportBuilder, vec2};
use std::sync::Arc;

/// 命令行参数：`--scan` 只扫描本机 zcode 凭证并打印，不启动界面。
/// 便于排查「为什么没扫到 key」。
fn main() -> eframe::Result<()> {
    // debug 构建带控制台，日志直接输出，方便排查托盘 / 后端问题；
    // release 是 GUI 子系统（无控制台），日志静默。
    #[cfg(debug_assertions)]
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::args().any(|a| a == "--scan" || a == "--scan-debug") {
        let debug = std::env::args().any(|a| a == "--scan-debug");
        let discovery = zcode_core::discover::scan();
        if debug {
            println!("扫描位置：{}", discovery.scanned_paths.join("、"));
            for w in &discovery.warnings {
                println!("警告：{w}");
            }
        }
        for key in &discovery.keys {
            println!(
                "{} | 归组 {} | {}…",
                key.label,
                key.group,
                &key.secret[..key.secret.len().min(6)]
            );
        }
        println!("共 {} 条", discovery.keys.len());
        return Ok(());
    }

    // 通知自检：只起托盘并发一条气泡，不启动界面。
    // 用来验证通知的声音与归属（应显示为 zcode 额度看板，而非 PowerShell）。
    if std::env::args().any(|a| a == "--test-notify") {
        let mut tray = tray::Tray::spawn("zcode 额度看板");
        std::thread::sleep(std::time::Duration::from_millis(800));
        tray.notify(
            "zcode 额度看板",
            "测试通知：额度刷新提醒将这样送达（应有系统提示音）。",
        );
        std::thread::sleep(std::time::Duration::from_secs(10));
        tray.shutdown();
        return Ok(());
    }

    let options = NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title("zcode 额度看板")
            .with_inner_size(vec2(1150.0, 780.0))
            .with_min_inner_size(vec2(880.0, 560.0))
        .with_icon(Arc::new(IconData {
            // 启动先给绿色：首帧 sync_tray 会按实际状态换色
            // （任务栏/标题栏与托盘图标保持一致）。
            rgba: tray::icon_rgba(tray::TrayStatus::Ok),
            width: tray::ICON_SIZE,
            height: tray::ICON_SIZE,
        })),
        ..Default::default()
    };

    eframe::run_native(
        "zcode 额度看板",
        options,
        Box::new(|cc| {
            bridge::setup_fonts(&cc.egui_ctx);
            app::configure_style(&cc.egui_ctx);
            Ok(Box::new(app::BoardApp::new()))
        }),
    )
}
