//! 界面进程与 tokio 后台任务之间的桥接。
//!
//! egui 跑在主线程；所有网络请求、持久化、调度都在独立线程的 tokio 运行时里完成，
//! 通过标准库 mpsc 下发命令，通过 `Arc<Mutex<AppState>>` 回传状态快照。

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zcode_core::app::Manager;
use zcode_model::{AddKeyRequest, AppState, MoveKeyRequest, Settings, UpdateKeyRequest};

/// 界面发给后台的命令。
pub enum Command {
    RefreshAll,
    RefreshKey(String),
    RefreshAccount(String),
    AddKeys(AddKeyRequest),
    RemoveKey(String),
    /// 删除整个账户（含其下所有 key）。
    RemoveAccount(String),
    /// 一键清理失效的本机同步 key，参数是本机最新扫描出的 secret 列表。
    CleanupStale(Vec<String>),
    UpdateKey(String, UpdateKeyRequest),
    MoveKey(String, Option<String>),
    RenameAccount(String, String),
    SetMuted(String, bool),
    UpdateSettings(Settings),
    TestNotify,
    /// 清空事件日志。
    ClearEvents,
    /// 扫描本机 zcode 并导入（对话框用）。
    ScanLocalZcode,
    /// 确认导入扫描结果。
    ImportDiscovered(Vec<zcode_core::discover::DiscoveredKey>),
    /// 合并疑似同一账户的多个账户。
    MergeAccounts(Vec<String>, String),
    /// 注册桌面通知出口（启动时界面发一次，指向托盘气泡）。
    SetNotifier(zcode_core::app::DesktopNotifier),
}

/// 后台任务句柄。
pub struct Bridge {
    state: Arc<Mutex<AppState>>,
    tx: std::sync::mpsc::Sender<Command>,
    data_path: String,
    /// 后台线程把扫描结果通过这条通道送回界面。
    scan_results: std::sync::mpsc::Receiver<zcode_core::discover::Discovery>,
}

/// 后台线程启动时回传的信息。
struct Backend {}

impl Bridge {
    /// 启动 tokio 后台线程并返回桥接句柄。
    pub fn spawn(data_dir: PathBuf) -> Self {
        let state = Arc::new(Mutex::new(AppState::default()));
        let (tx, rx) = channel::<Command>();
        let (ready_tx, ready_rx) = channel::<Backend>();
        let (scan_result_tx, scan_results) = channel::<zcode_core::discover::Discovery>();
        let display_path = data_dir.join("state.json").display().to_string();

        let thread_state = state.clone();
        let thread_dir = data_dir.clone();
        let thread_scan_tx = scan_result_tx.clone();
        // 界面侧只保留接收端：扫描由后台发起，结果经独立通道回传。
        drop(scan_result_tx);
        std::thread::Builder::new()
            .name("zcode-backend".into())
            .spawn(move || backend_thread(thread_dir, thread_state, rx, ready_tx, thread_scan_tx))
            .expect("启动后台线程失败");

        // 等后台初始化完成，界面首次渲染即可拿到数据。
        let _ = ready_rx.recv_timeout(Duration::from_secs(20));

        Self {
            state,
            tx,
            data_path: display_path,
            scan_results,
        }
    }

    /// 非阻塞地尝试取一次扫描结果。
    pub fn try_take_scan(&self) -> Option<zcode_core::discover::Discovery> {
        self.scan_results.try_recv().ok()
    }

    /// 读取最新的状态快照。
    pub fn snapshot(&self) -> AppState {
        match self.state.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn data_path(&self) -> String {
        self.data_path.clone()
    }

    /// 下发一条命令（非阻塞，队列满时丢弃）。
    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }
}

/// 后台线程主体：tokio 运行时 + 命令循环 + 状态快照推送。
fn backend_thread(
    data_dir: PathBuf,
    state: Arc<Mutex<AppState>>,
    rx: Receiver<Command>,
    ready: std::sync::mpsc::Sender<Backend>,
    scan_result_tx: std::sync::mpsc::Sender<zcode_core::discover::Discovery>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            tracing::error!(error = %err, "创建 tokio 运行时失败，后台任务不可用");
            let _ = ready.send(Backend {});
            return;
        }
    };

    runtime.block_on(async move {
        let manager = Manager::new(&data_dir).await;
        zcode_core::scheduler::spawn_background(manager.clone());
        let _ = ready.send(Backend {});

        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let mut got = false;
            loop {
                match rx.try_recv() {
                    Ok(cmd) => {
                        handle_command(&manager, cmd, &scan_result_tx).await;
                        got = true;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            let _ = got;

            let snapshot = manager.view().await;
            if let Ok(mut guard) = state.lock() {
                *guard = snapshot;
            }

            tick.tick().await;
        }
    });
}

async fn handle_command(
    manager: &Manager,
    cmd: Command,
    scan_result_tx: &std::sync::mpsc::Sender<zcode_core::discover::Discovery>,
) {
    match cmd {
        Command::RefreshAll => {
            let _ = manager.refresh_keys(None).await;
            manager.nudge_auto_refresh_now().await;
        }
        Command::RefreshKey(id) => {
            let _ = manager.refresh_keys(Some(vec![id.clone()])).await;
            manager.nudge_auto_refresh_now().await;
        }
        Command::RefreshAccount(id) => {
            let ids = manager.key_ids_for_account(&id).await;
            let _ = manager.refresh_keys(Some(ids)).await;
            manager.nudge_auto_refresh_now().await;
        }
        Command::AddKeys(req) => {
            let _ = manager.add_keys(&req).await;
        }
        Command::RemoveKey(id) => {
            manager.remove_key(&id).await;
        }
        Command::RemoveAccount(id) => {
            let removed = manager.remove_account(&id).await;
            tracing::info!(removed, account = %id, "删除账户");
        }
        Command::CleanupStale(secrets) => {
            let live: std::collections::HashSet<String> = secrets.into_iter().collect();
            let removed = manager.cleanup_stale(&live).await;
            tracing::info!(removed, "清理失效的本机同步 key");
        }
        Command::UpdateKey(id, req) => {
            manager.update_key(&id, &req).await;
        }
        Command::MoveKey(id, account_id) => {
            manager.move_key(&id, &MoveKeyRequest { account_id }).await;
        }
        Command::RenameAccount(id, name) => {
            manager.rename_account(&id, &name).await;
        }
        Command::SetMuted(id, muted) => {
            manager.set_account_muted(&id, muted).await;
        }
        Command::UpdateSettings(settings) => {
            manager.update_settings(settings).await;
        }
        Command::TestNotify => {
            manager.test_notify().await;
        }
        Command::ClearEvents => {
            manager.clear_events().await;
        }
        Command::ScanLocalZcode => {
            // 磁盘读写放到阻塞线程池，避免卡住运行时。
            let discovery = tokio::task::spawn_blocking(zcode_core::discover::scan)
                .await
                .unwrap_or_default();
            let _ = scan_result_tx.send(discovery);
        }

        Command::ImportDiscovered(keys) => {
            let (imported, total) = manager.import_discovered(&keys).await;
            manager.nudge_auto_refresh_now().await;
            tracing::info!(imported, total, "从本机 zcode 导入完成");
        }
        Command::MergeAccounts(sources, target) => {
            let ok = manager.merge_accounts(&sources, &target).await;
            tracing::info!(ok, "合并疑似同一账户");
        }
        Command::SetNotifier(f) => {
            manager.set_desktop_notifier(f);
        }
    }
}

/// 加载系统中文字体，避免 egui 内置字体缺 CJK 字形导致方块字。
pub fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    const CANDIDATES: &[(&str, &str)] = &[
        // Windows：SimHei / DengXian 是干净的 TTF，ttf_parser 支持度最好
        ("cjk", r"C:\Windows\Fonts\simhei.ttf"),
        ("cjk_alt", r"C:\Windows\Fonts\Deng.ttf"),
        // Linux
        (
            "cjk_alt",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ),
        ("cjk_alt", "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc"),
        // macOS
        ("cjk_alt", "/System/Library/Fonts/PingFang.ttc"),
        // Windows 雅黑放最后：.ttc 集合格式兼容性差一些
        ("cjk_ttc", r"C:\Windows\Fonts\msyh.ttc"),
    ];

    for (name, path) in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let data = egui::FontData::from_owned(bytes);
        fonts.font_data.insert((*name).to_owned(), Arc::new(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.entry(family).or_default();
            if !list.contains(&(*name).to_owned()) {
                list.push((*name).to_owned());
            }
        }
    }

    ctx.set_fonts(fonts);
}
