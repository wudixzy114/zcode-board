//! 主界面：账户卡片、配额窗口、倒计时、设置与事件日志。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::App;
use egui::{Align, Color32, CornerRadius, Frame, Layout, RichText, Stroke, Vec2};

use crate::bridge::{Bridge, Command};
use crate::icons::{self, Icon, ACCENT, BAD, BAD_FILL, BG_APP, BG_CARD, BG_CARD_ALT, BG_PANEL, BORDER, OK, TEXT_HI, TEXT_MID, WARN, WARN_FILL};
use crate::tray::{Tray, TrayCmd, TrayEvent, TrayStatus};

/// 取 egui 主窗口的 Win32 HWND。
#[cfg(windows)]
fn main_hwnd(frame: &eframe::Frame) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::HasWindowHandle;
    match frame.window_handle().ok()?.as_raw() {
        raw_window_handle::RawWindowHandle::Win32(h) => Some(h.hwnd.get() as _),
        _ => None,
    }
}

/// 隐藏主窗口（点 ✕ 收进托盘时调用）。
///
/// 不走 `ViewportCommand::Visible(false)`：实测 eframe 0.32 单视口下，
/// 主窗口隐藏后事件循环停止派发重绘，托盘后续经 egui 转发的命令
/// （恢复窗口 / 退出）永远等不到执行。Win32 直调 + 托盘线程自带
/// 消息循环，不受主窗口隐藏影响。
fn hide_main_window(frame: &mut eframe::Frame) {
    #[cfg(windows)]
    if let Some(hwnd) = main_hwnd(frame).filter(|h| !h.is_null()) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = frame;
    }
}
use zcode_model::{
    AccountView, AppState, KeyStatus, KeyView, Settings, WindowView, format_countdown,
    normalize_percent,
};

/// 用量配色的阈值。
const WARN_PERCENT: f64 = 60.0;
const BAD_PERCENT: f64 = 85.0;

pub struct BoardApp {
    bridge: Bridge,
    state: AppState,

    // 界面交互状态
    show_add: bool,
    add_text: String,
    add_label: String,
    show_settings: bool,
    show_log: bool,
    pending_delete: Option<String>,
    /// 待确认删除的账户 id（删账户会连同它下面的所有 key 一起删）。
    pending_delete_account: Option<String>,
    /// 待确认的一键清理（清理本机已不存在的同步 key）。
    pending_cleanup: bool,
    expanded: HashSet<String>,
    /// 账户改名输入框
    rename_for: Option<String>,
    rename_text: String,
    /// 本地 zcode 扫描导入
    show_import: bool,
    import_scanning: bool,
    import_result: Option<zcode_core::discover::Discovery>,
    import_selected: std::collections::HashSet<String>,
    /// 用户已忽略的「疑似同一账户」分组。
    dismissed_twins: std::collections::HashSet<String>,
    /// 系统托盘（右下角常驻）。
    tray: Tray,
    /// 上一次同步给托管的图标状态，避免重复下发。
    last_tray_status: Option<TrayStatus>,
    /// 上一次刷新托盘提示的时间（节流）。
    tip_refreshed_at: Instant,
    /// 开机启动注册表实际状态的上次探测时间（节流）。
    autostart_probe_at: Instant,
    /// 开机启动注册表的实际状态（写入失败时如实反映）。
    autostart_actual: bool,
    /// 托盘图标是否添加失败的已知会状态（ Probe 一次后就报告）。
    tray_icon_failed: bool,
    tray_icon_probed: bool,
    /// 主窗口句柄是否已绑给托盘线程。
    tray_attached: bool,
    /// 设置面板的编辑草稿：改动已下发、但后台快照尚未追上时，用它做控件基底。
    ///
    /// 没有它，每帧从快照 clone 的设置会在后台应用改动前（最长 200ms）把
    /// 文本框里正在输入的字符顶掉，checkbox 也会闪一下又弹回去。
    settings_draft: Option<Settings>,
}

impl BoardApp {
    pub fn new() -> Self {
        let bridge = Bridge::spawn(default_data_dir());
        let tray = Tray::spawn("zcode 额度看板");
        // 桌面提醒走托盘气泡（带系统提示音、归属本应用而非 PowerShell）。
        let notify_tx = tray.notify_sender();
        bridge.send(Command::SetNotifier(std::sync::Arc::new(move |title, body| {
            let _ = notify_tx.send(TrayCmd::Balloon { title, body });
        })));
        Self {
            bridge,
            state: AppState::default(),
            show_add: false,
            add_text: String::new(),
            add_label: String::new(),
            show_settings: true,
            show_log: false,
            pending_delete: None,
            pending_delete_account: None,
            pending_cleanup: false,
            expanded: HashSet::new(),
            rename_for: None,
            rename_text: String::new(),
            show_import: false,
            import_scanning: false,
            import_result: None,
            import_selected: HashSet::new(),
            dismissed_twins: HashSet::new(),
            tray,
            last_tray_status: None,
            tip_refreshed_at: Instant::now(),
            autostart_probe_at: Instant::now(),
            autostart_actual: zcode_core::autostart::is_enabled(),
            tray_icon_failed: false,
            tray_icon_probed: false,
            tray_attached: false,
            settings_draft: None,
        }
    }

    /// 定时刷新界面（倒计时需要秒级甚至更低频的重绘）。
    fn tick(&mut self, ctx: &egui::Context) {
        self.state = self.bridge.snapshot();
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn send(&self, cmd: Command) {
        self.bridge.send(cmd);
    }

    fn refresh_all(&self) {
        self.send(Command::RefreshAll);
    }

    /// 把账户状态同步到托盘与任务栏：图标颜色 + 悬停提示（2 秒节流，避免每帧都推）。
    ///
    /// 任务栏/标题栏图标与托盘图标用同一份像素、随状态一起换色——
    /// 否则托盘变黄/红时任务栏还停在启动时的绿色，两边对不上。
    fn sync_tray(&mut self, now_ms: i64) {
        // 托盘图标是否加上的结果探测一次：启动 2 秒后窗口线程早已 NIM_ADD 完成。
        if !self.tray_icon_probed && self.tip_refreshed_at.elapsed() >= Duration::from_secs(2) {
            self.tray_icon_probed = true;
            self.tray_icon_failed = !self.tray.icon_ok();
        }
        let keys: Vec<&KeyView> = self
            .state
            .accounts
            .iter()
            .flat_map(|a| a.keys.iter())
            .collect();
        let errors = keys.iter().filter(|k| k.status == KeyStatus::Error).count();
        let total = keys.len();
        let accounts = self.state.accounts.len();

        let status = if errors > 0 {
            TrayStatus::Error
        } else if total > 0 && keys.iter().all(|k| k.status == KeyStatus::Ok) {
            TrayStatus::Ok
        } else {
            TrayStatus::Idle
        };
        if self.last_tray_status != Some(status) {
            self.tray.set_status(status);
            // 任务栏/标题栏图标同步换色：egui 的 ViewportCommand::Icon 底层
            // 只设 ICON_SMALL（标题栏），任务栏（ICON_BIG）不会变；这里经
            // 托盘线程对主窗口 WM_SETICON 两个尺寸都换。
            self.tray.set_window_icon(status);
            self.last_tray_status = Some(status);
        }

        if self.tip_refreshed_at.elapsed() >= Duration::from_secs(2) {
            self.tip_refreshed_at = Instant::now();
            let tail = if errors > 0 {
                format!("· {errors} 个异常")
            } else if let Some(next) = global_next_reset(&self.state.accounts) {
                format!("· 下次刷新 {}", format_countdown(next - now_ms))
            } else {
                String::new()
            };
            self.tray.set_tip(&format!(
                "zcode 额度看板 · {accounts} 个账户 · {total} 个 key {tail}"
            ));
        }
    }

    // ─────────────────────────── 布局 ───────────────────────────

    fn top_bar(&mut self, ctx: &egui::Context, now_ms: i64) {
        egui::TopBottomPanel::top("top_bar")
            .frame(
                Frame::default()
                    .fill(BG_PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    icons::glyph(ui, Icon::Bolt, 20.0, ACCENT);
                    ui.label(
                        RichText::new("zcode 额度看板")
                            .size(17.0)
                            .color(TEXT_HI)
                            .strong(),
                    );

                    let total_keys: usize = self.state.accounts.iter().map(|a| a.keys.len()).sum();
                    ui.label(
                        RichText::new(format!(
                            "{} 个账户 · {} 个 key",
                            self.state.accounts.len(),
                            total_keys
                        ))
                        .color(TEXT_MID),
                    );

                    // right_to_left：先加的在最右。视觉顺序（左→右）为
                    // 导入、添加、分隔线、日志、设置、立即刷新。
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let refreshing = self.state.refreshing > 0;
                        let label = if refreshing {
                            "刷新中…"
                        } else {
                            "立即刷新"
                        };
                        if icons::pill_button(ui, Icon::Refresh, label, true, !refreshing, refreshing)
                            .clicked()
                        {
                            self.refresh_all();
                        }
                        if icons::icon_toggle(ui, Icon::Settings, self.show_settings, "设置")
                            .clicked()
                        {
                            self.show_settings = !self.show_settings;
                        }
                        if icons::icon_toggle(ui, Icon::Log, self.show_log, "事件日志").clicked() {
                            self.show_log = !self.show_log;
                        }
                        ui.separator();
                        if icons::pill_button(ui, Icon::Plus, "添加 key", false, true, false)
                            .clicked()
                        {
                            self.show_add = true;
                        }
                        if icons::pill_button(
                            ui,
                            Icon::Import,
                            "从本机 zcode 导入",
                            false,
                            true,
                            false,
                        )
                        .clicked()
                        {
                            self.show_import = true;
                            self.import_scanning = true;
                            self.import_result = None;
                            self.import_selected.clear();
                            self.send(Command::ScanLocalZcode);
                        }
                    });
                });

                ui.add_space(4.0);

                // 全局：下一次额度刷新 + 下一次自动刷新倒计时
                ui.horizontal(|ui| {
                    if let Some(next) = global_next_reset(&self.state.accounts) {
                        let left = format_countdown(next - now_ms);
                        let urgent = next - now_ms <= lead_ms(self);
                        let color = if urgent { BAD } else { OK };
                        label_kv(ui, "下一次额度刷新", &left, color);
                    } else {
                        label_kv(ui, "下一次额度刷新", "—", TEXT_MID);
                    }

                    if let Some(next) = self.state.next_auto_refresh_ms {
                        label_kv(
                            ui,
                            "下一次自动刷新",
                            &format_countdown(next - now_ms),
                            ACCENT,
                        );
                    } else {
                        label_kv(ui, "下一次自动刷新", "已关闭", TEXT_MID);
                    }
                });
            });
    }

    fn account_card(&mut self, ui: &mut egui::Ui, account: &AccountView, now_ms: i64) {
        let expanded = self.expanded.contains(&account.id);
        let card_color = if account.status == KeyStatus::Error {
            BAD_FILL
        } else if account.status == KeyStatus::Cooling {
            WARN_FILL
        } else {
            BG_CARD
        };

        Frame::new()
            .fill(card_color)
            .stroke(Stroke::new(1.0, BORDER))
            .corner_radius(CornerRadius::same(10))
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width() - 2.0);

                // ── 头部 ──
                ui.horizontal(|ui| {
                    let arrow = if expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    };
                    if icons::icon_toggle(ui, arrow, expanded, "展开 / 收起该账户下的 key")
                        .clicked()
                    {
                        if expanded {
                            self.expanded.remove(&account.id);
                        } else {
                            self.expanded.insert(account.id.clone());
                        }
                    }

                    let name = account.name.clone();
                    let editable = self.rename_for.as_deref() == Some(account.id.as_str());
                    if editable {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.rename_text).desired_width(220.0),
                        );
                        // 只在输入框本身聚焦时响应回车，避免别的控件按回车误提交。
                        let mut done = resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if icons::icon_button(ui, Icon::Check, "确认改名", 14.0).clicked() {
                            done = true;
                        }
                        if resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            self.rename_for = None;
                        } else if done {
                            let text = self.rename_text.clone();
                            self.send(Command::RenameAccount(account.id.clone(), text));
                            self.rename_for = None;
                        }
                    } else {
                        let name_job = RichText::new(&name).size(15.0).color(TEXT_HI).strong();
                        ui.label(name_job)
                            .on_hover_text(format!("合并了 {} 个 key", account.keys.len()));
                        if icons::icon_button(ui, Icon::Pencil, "重命名账户", 14.0).clicked() {
                            self.rename_for = Some(account.id.clone());
                            self.rename_text = account.name.clone();
                        }
                    }

                    if account.keys.len() > 1 {
                        ui.label(
                            RichText::new(format!("×{} keys", account.keys.len())).color(ACCENT),
                        );
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        icons::status_chip(ui, account.status);
                        if account.notify_muted {
                            if icons::icon_button(ui, Icon::BellOff, "已静音，点击恢复提醒", 15.0)
                                .clicked()
                            {
                                self.send(Command::SetMuted(account.id.clone(), false));
                            }
                        } else if icons::icon_button(
                            ui,
                            Icon::Bell,
                            "静音该账户的刷新提醒",
                            15.0,
                        )
                        .clicked()
                        {
                            self.send(Command::SetMuted(account.id.clone(), true));
                        }
                        if icons::icon_button(ui, Icon::Trash, "删除该账户及其所有 key", 15.0)
                            .clicked()
                        {
                            self.pending_delete_account = Some(account.id.clone());
                        }
                        if icons::icon_button(ui, Icon::Refresh, "刷新该账户", 15.0).clicked() {
                            self.send(Command::RefreshAccount(account.id.clone()));
                        }
                    });
                });

                // ── 配额窗口 ──
                if account.windows.is_empty() {
                    ui.label(RichText::new("暂无额度数据").color(TEXT_MID));
                }
                for w in &account.windows {
                    window_row(ui, w, now_ms, lead_ms(self));
                }

                // ── key 列表 ──
                if expanded {
                    ui.add_space(4.0);
                    ui.separator();
                    for key in &account.keys {
                        key_row(ui, self, key, account);
                    }
                }
            });

        ui.add_space(8.0);
    }

    fn settings_panel(&mut self, ctx: &egui::Context) {
        // 基底优先用草稿（用户刚改过、后台还没追上），否则跟随后台快照。
        let mut settings = self
            .settings_draft
            .clone()
            .unwrap_or_else(|| self.state.settings.clone());
        let mut refresh = false;

        egui::SidePanel::right("settings")
            .default_width(320.0)
            .resizable(true)
            .width_range(300.0..=640.0)
            .frame(
                Frame::default()
                    .fill(BG_PANEL)
                    .inner_margin(egui::Margin::same(12)),
            )
            .show(ctx, |ui| {
                // 内容至少撑满面板当前宽度。SidePanel 记住的宽度取自内容
                // 实际布局出来的矩形（Frame 的 outer_rect），把分组全折叠后
                // 内容变窄，面板会跟着缩水且拖不回来；撑满后面板宽度只由
                // 分隔条拖动决定，稳定可恢复。
                ui.set_min_width(ui.max_rect().width());
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        icons::glyph(ui, Icon::Settings, 17.0, TEXT_HI);
                        ui.heading("设置");
                    });
                    ui.add_space(6.0);

                    group(ui, "定时刷新", |ui| {
                        let mut on = settings.refresh_interval_secs > 0;
                        if ui.checkbox(&mut on, "启用自动刷新").changed() {
                            settings.refresh_interval_secs = if on { 60 } else { 0 };
                            refresh = true;
                        }
                        ui.add_space(2.0);
                        ui.label("刷新间隔（秒）");
                        let mut v = settings.refresh_interval_secs as f64;
                        if ui
                            .add(egui::DragValue::new(&mut v).speed(1).range(0.0..=3600.0))
                            .changed()
                        {
                            settings.refresh_interval_secs = v.max(1.0) as u64;
                            refresh = true;
                        }
                        quick_interval(ui, &mut settings, &mut refresh);
                        ui.add_space(4.0);
                        ui.label("并发探测数");
                        let mut c = settings.concurrency as f64;
                        if ui.add(egui::DragValue::new(&mut c).speed(1).range(1.0..=16.0)).changed() {
                            settings.concurrency = c as usize;
                            refresh = true;
                        }
                        ui.add_space(4.0);
                        ui.label("请求超时（秒）");
                        let mut t = settings.request_timeout_secs as f64;
                        if ui.add(egui::DragValue::new(&mut t).speed(1).range(3.0..=120.0)).changed() {
                            settings.request_timeout_secs = t as u64;
                            refresh = true;
                        }
                    });

                    group(ui, "下一次刷新提醒", |ui| {
                        if ui.checkbox(&mut settings.notify_enabled, "启用刷新提醒").changed() {
                            refresh = true;
                        }
                        ui.add_space(2.0);
                        ui.label(format!("提前 {} 分钟提醒", settings.notify_lead_minutes));
                        let mut m = settings.notify_lead_minutes as f64;
                        if ui
                            .add(egui::DragValue::new(&mut m).speed(1).range(1.0..=120.0))
                            .changed()
                        {
                            settings.notify_lead_minutes = m as u32;
                            refresh = true;
                        }
                        if ui
                            .checkbox(&mut settings.notify_desktop, "发送桌面系统通知")
                            .changed()
                        {
                            refresh = true;
                        }
                        if ui.button("发送测试通知").clicked() {
                            self.send(Command::TestNotify);
                        }
                    });

                    group(ui, "窗口与启动", |ui| {
                        if ui
                            .checkbox(&mut settings.start_with_windows, "开机自动启动")
                            .on_hover_text(
                                "写入当前用户的注册表 Run 项，Windows 登录后自动运行本工具",
                            )
                            .changed()
                        {
                            refresh = true;
                        }
                        if self.autostart_probe_at.elapsed() >= Duration::from_secs(2) {
                            self.autostart_probe_at = Instant::now();
                            self.autostart_actual = zcode_core::autostart::is_enabled();
                        }
                        ui.label(
                            RichText::new(format!(
                                "注册表状态：{}（值名 zcode-board）",
                                if self.autostart_actual {
                                    "已开启"
                                } else {
                                    "未开启"
                                }
                            ))
                            .small()
                            .color(if self.autostart_actual == settings.start_with_windows {
                                TEXT_MID
                            } else {
                                WARN
                            }),
                        );
                        ui.add_space(4.0);
                        if ui
                            .checkbox(&mut settings.minimize_to_tray, "关闭窗口时最小化到托盘")
                            .on_hover_text("点右上角 ✕ 时隐藏到任务栏托盘，程序继续在后台运行")
                            .changed()
                        {
                            refresh = true;
                        }
                        ui.label(
                            RichText::new("托盘常驻右下角：单击恢复窗口，右键可退出程序。")
                                .small()
                                .color(TEXT_MID),
                        );
                        if self.tray_icon_failed {
                            ui.label(
                                RichText::new(
                                    "⚠ 托盘图标注册失败：右下角不会出现图标，\
                                     关闭窗口将直接退出程序（可在资源管理器重启本工具）",
                                )
                                .small()
                                .color(WARN),
                            );
                        }
                    });

                    group(ui, "上游接口", |ui| {
                        ui.label("额度查询端点");
                        if ui
                            .text_edit_singleline(&mut settings.quota_endpoint)
                            .changed()
                        {
                            refresh = true;
                        }
                        ui.add_space(4.0);
                        ui.label("附加请求头（可选）");
                        let mut hdrs = settings.extra_headers.clone();
                        if ui
                            .add(
                                egui::TextEdit::multiline(&mut hdrs)
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            )
                            .changed()
                        {
                            settings.extra_headers = hdrs;
                            refresh = true;
                        }
                        ui.label(
                            RichText::new("每行一个 `Header-Name: value`，一般用不到。")
                                .small()
                                .color(TEXT_MID),
                        );
                    });

                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("数据文件：{}", self.bridge.data_path()))
                            .small()
                            .color(TEXT_MID),
                    );
                });
            });

        if refresh {
            // 记下草稿并立即下发；后台应用后快照会与草稿一致，届时回到跟随快照。
            self.settings_draft = Some(settings.clone());
            self.send(Command::UpdateSettings(settings));
        } else if self.settings_draft.is_some() && self.state.settings == settings {
            self.settings_draft = None;
        }
    }

    fn log_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(150.0)
            .frame(
                Frame::default()
                    .fill(BG_PANEL)
                    .inner_margin(egui::Margin::same(10)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    icons::glyph(ui, Icon::Log, 15.0, TEXT_HI);
                    ui.label(RichText::new("事件日志").strong().color(TEXT_HI));
                    if ui.small_button("清空").clicked() {
                        self.send(Command::ClearEvents);
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for ev in &self.state.events {
                            let color = match ev.level.as_str() {
                                "bad" => BAD,
                                "warn" => WARN,
                                _ => OK,
                            };
                            // 标题是事件的主体（如「测试通知」「探测失败：xx」），
                            // body 是补充说明；只显示 body 会丢掉关键信息。
                            let text = if ev.body.is_empty() {
                                ev.title.clone()
                            } else {
                                format!("{}：{}", ev.title, ev.body)
                            };
                            ui.label(
                                RichText::new(format!("[{}] {}", fmt_clock(ev.at_ms), text))
                                    .color(color),
                            );
                        }
                    });
            });
    }

    fn add_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_add;
        egui::Window::new("添加 key")
            .open(&mut open)
            .default_width(560.0)
            .resizable(true)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("每行一个 Z.ai API Key；也支持「备注: Key」写法");
                ui.horizontal(|ui| {
                    ui.label("备注名");
                    ui.add(egui::TextEdit::singleline(&mut self.add_label).desired_width(260.0));
                });
                ui.add(
                    egui::TextEdit::multiline(&mut self.add_text)
                        .desired_rows(7)
                        .desired_width(f32::INFINITY)
                        .hint_text("粘贴凭证…\n例如\n主号: id.secret\n备用号: id.secret"),
                );
                ui.horizontal(|ui| {
                    let can_add = !self.add_text.trim().is_empty();
                    if ui
                        .add_enabled(can_add, egui::Button::new("添加并立即探测"))
                        .clicked()
                        && can_add
                    {
                        let secrets = std::mem::take(&mut self.add_text);
                        let label = (!self.add_label.trim().is_empty())
                            .then(|| self.add_label.trim().to_string());
                        self.send(Command::AddKeys(zcode_model::AddKeyRequest {
                            secrets,
                            label,
                        }));
                        self.show_add = false;
                    }
                    if ui.button("取消").clicked() {
                        self.show_add = false;
                    }
                });
            });
        // 注意是 `&=`：窗口内容（取消 / 添加）可能已把 show_add 置 false，
        // 直接 `= open` 会把它顶回 true，表现为「按钮点了没反应」。
        self.show_add &= open;
    }

    /// 本地 zcode 扫描导入弹窗。
    fn import_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_import;
        if !open {
            return;
        }

        egui::Window::new("从本机 zcode 导入")
            .open(&mut open)
            .default_width(660.0)
            .default_height(460.0)
            .resizable(true)
            .collapsible(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                if self.import_scanning {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("正在扫描本机 zcode 的凭证文件…");
                    });
                    ui.add_space(10.0);
                    // 扫描中也要能退出：结果回来时窗口已关就直接丢弃。
                    if ui.button("取消").clicked() {
                        self.show_import = false;
                    }
                    return;
                }

                let Some(discovery) = self.import_result.clone() else {
                    ui.label("未扫描到结果。");
                    return;
                };

                if discovery.keys.is_empty() {
                    ui.label(
                        RichText::new("没有在本机找到可导入的凭证")
                            .color(TEXT_HI)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    for w in &discovery.warnings {
                        ui.label(RichText::new(format!("· {w}")).small().color(WARN));
                    }
                    ui.label(
                        RichText::new(format!("扫描位置：{}", discovery.scanned_paths.join("、")))
                            .small()
                            .color(TEXT_MID),
                    );
                    return;
                }

                ui.label(discovery.summary());

                // 与本机现有 key 比对：区分「新发现 / 已存在 / 已失效」。
                let known: HashSet<&str> = self
                    .state
                    .accounts
                    .iter()
                    .flat_map(|a| a.keys.iter())
                    .map(|k| k.secret.as_str())
                    .collect();
                let live: HashSet<&str> =
                    discovery.keys.iter().map(|k| k.secret.as_str()).collect();
                let new_count = discovery
                    .keys
                    .iter()
                    .filter(|k| !known.contains(k.secret.as_str()))
                    .count();
                let existing_count = discovery.keys.len() - new_count;
                // 失效 = 本机同步来的 key，但本次扫描里已经不在了
                // （例如 zcode 里删掉了某个 provider，或早期版本的残留）。
                let stale: Vec<&zcode_model::KeyView> = self
                    .state
                    .accounts
                    .iter()
                    .flat_map(|a| a.keys.iter())
                    .filter(|k| {
                        k.source_group
                            .as_deref()
                            .is_some_and(|g| g.starts_with("zcode-provider:"))
                            && !live.contains(k.secret.as_str())
                    })
                    .collect();

                ui.horizontal(|ui| {
                    if new_count > 0 {
                        ui.label(
                            RichText::new(format!("新发现 {new_count} 个"))
                                .color(OK)
                                .strong(),
                        );
                    }
                    if existing_count > 0 {
                        ui.label(
                            RichText::new(format!("已存在 {existing_count} 个（自动跳过）"))
                                .small()
                                .color(TEXT_MID),
                        );
                    }
                    if !stale.is_empty() {
                        ui.label(
                            RichText::new(format!("失效 {} 个", stale.len()))
                                .small()
                                .color(WARN),
                        )
                        .on_hover_text("本机 zcode 里已经不存在、但看板里还留着的同步 key");
                    }
                });
                ui.add_space(4.0);

                // 失效条目清理：一键删掉本机已不存在的同步 key。
                if !stale.is_empty() {
                    Frame::new()
                        .fill(WARN_FILL)
                        .stroke(Stroke::new(1.0, WARN))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                icons::glyph(ui, Icon::Brush, 16.0, WARN);
                                ui.label(
                                    RichText::new(format!(
                                        "有 {} 个同步 key 在本机 zcode 里已不存在",
                                        stale.len()
                                    ))
                                    .color(TEXT_HI),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.small_button("查看").clicked() {
                                        // 展开第一个失效 key 所在的账户，方便定位。
                                        if let Some(first) = stale.first() {
                                            self.expanded.insert(first.account_id.clone());
                                        }
                                    }
                                    if ui
                                        .small_button("一键清理")
                                        .on_hover_text(
                                            "删除这些 key 及其腾空的账户；手动添加的 key 不受影响",
                                        )
                                        .clicked()
                                    {
                                        self.pending_cleanup = true;
                                    }
                                });
                            });
                        });
                    ui.add_space(4.0);
                }

                ui.separator();

                egui::ScrollArea::vertical()
                    .max_height(240.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for item in &discovery.keys {
                            ui.horizontal(|ui| {
                                let imported = known.contains(item.secret.as_str());
                                let mut selected = self.import_selected.contains(&item.secret);
                                if ui
                                    .add_enabled(!imported, egui::Checkbox::new(&mut selected, ""))
                                    .changed()
                                {
                                    if selected {
                                        self.import_selected.insert(item.secret.clone());
                                    } else {
                                        self.import_selected.remove(&item.secret);
                                    }
                                }

                                ui.label(RichText::new("KEY").small().color(ACCENT));
                                ui.label(RichText::new(&item.label).strong().color(TEXT_HI));
                                if imported {
                                    ui.label(
                                        RichText::new("已导入").small().color(OK),
                                    );
                                }
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new(mask_item_secret(&item.secret))
                                            .small()
                                            .monospace()
                                            .color(TEXT_MID),
                                    );
                                });
                            });
                        }
                    });

                ui.separator();
                ui.horizontal(|ui| {
                    let count = self.import_selected.len();
                    if ui
                        .add_enabled(count > 0, egui::Button::new(format!("导入 {count} 个")))
                        .clicked()
                    {
                        let picked: Vec<_> = discovery
                            .keys
                            .iter()
                            .filter(|k| self.import_selected.contains(&k.secret))
                            .cloned()
                            .collect();
                        self.send(Command::ImportDiscovered(picked));
                        self.show_import = false;
                    }
                    if !discovery.warnings.is_empty() {
                        ui.label(
                            RichText::new(format!("{} 个警告", discovery.warnings.len()))
                                .small()
                                .color(WARN),
                        )
                        .on_hover_text(discovery.warnings.join("\n"));
                    }
                    if ui.button("取消").clicked() {
                        self.show_import = false;
                    }
                });
            });

        // 同 add_window：`&=` 避免把弹窗内已置 false 的标志顶回 true。
        self.show_import &= open;
    }

    /// 「疑似同一账户」提示横幅：多个账户的配额快照一致时，
    /// 极可能是同一账号的多个 key（例如 zcode 里配的 API 2/3/4/5）。
    fn twin_banner(&mut self, ctx: &egui::Context, _now_ms: i64) {
        let mut groups: std::collections::BTreeMap<String, Vec<AccountView>> =
            std::collections::BTreeMap::new();
        for account in &self.state.accounts {
            if let Some(tag) = &account.twin_group {
                groups.entry(tag.clone()).or_default().push(account.clone());
            }
        }
        if groups.is_empty() {
            return;
        }

        egui::TopBottomPanel::top("twin_banner")
            .frame(
                Frame::default()
                    .fill(BG_CARD)
                    .stroke(Stroke::new(1.0, ACCENT))
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                for (tag, accounts) in groups {
                    if self.dismissed_twins.contains(&tag) {
                        continue;
                    }
                    let names = accounts
                        .iter()
                        .map(|a| a.name.clone())
                        .collect::<Vec<_>>()
                        .join("、");
                    let target = accounts[0].id.clone();
                    let sources: Vec<String> = accounts[1..].iter().map(|a| a.id.clone()).collect();

                    ui.horizontal(|ui| {
                        icons::glyph(ui, Icon::Link, 16.0, ACCENT);
                        ui.label(
                            RichText::new(format!(
                                "{names} 的额度数据完全相同，疑似同一个账户"
                            ))
                            .color(TEXT_HI),
                        );
                        if ui
                            .button("合并为一个账户")
                            .on_hover_text("合并后只显示一个账户卡片，key 全部挂在它下面")
                            .clicked()
                        {
                            self.send(Command::MergeAccounts(sources.clone(), target.clone()));
                        }
                        if ui.small_button("忽略").clicked() {
                            self.dismissed_twins.insert(tag.clone());
                        }
                    });
                }
            });
    }

    fn reminder_banner(&self, ctx: &egui::Context, now_ms: i64) {
        let lead = lead_ms(self);
        let mut soon: Vec<(&AccountView, i64)> = Vec::new();
        for account in &self.state.accounts {
            if let Some(reset) = account.next_reset_ms {
                let delta = reset - now_ms;
                // 过期 60 秒后不再显示：数据已陈旧，等下一次探测拉到新窗口。
                if !account.notify_muted && delta <= lead && delta > -60_000 {
                    soon.push((account, delta));
                }
            }
        }
        if soon.is_empty() {
            return;
        }
        soon.sort_by_key(|(_, d)| *d);

        egui::TopBottomPanel::top("reminder_banner")
            .frame(
                Frame::default()
                    .fill(WARN_FILL)
                    .stroke(Stroke::new(1.0, WARN))
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    icons::glyph(ui, Icon::Clock, 16.0, WARN);
                    let names = soon
                        .iter()
                        .map(|(a, d)| format!("{}（{}）", a.name, format_countdown(*d)))
                        .collect::<Vec<_>>()
                        .join("、");
                    ui.label(
                        RichText::new(format!("即将刷新额度：{names}"))
                            .color(TEXT_HI)
                            .strong(),
                    );
                });
            });
    }
}

impl App for BoardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now_ms = chrono::Local::now().timestamp_millis();
        self.tick(ctx);

        // 启动第一帧：把主窗口句柄绑给托盘线程（托盘直接用它控窗口）。
        #[cfg(windows)]
        if !self.tray_attached
            && let Some(hwnd) = main_hwnd(_frame).filter(|h| !h.is_null())
        {
            self.tray.attach_main_wnd(hwnd);
            self.tray_attached = true;
        }

        // ── 托盘：事件 → 界面动作 ──
        // 「恢复窗口」由托盘线程直接 Win32 控窗口（事件循环停摆时也能工作），
        // 界面侧只需要处理「退出」。
        if self.tray.drain_events().contains(&TrayEvent::Exit) {
            // 事件循环可能已停摆（窗口隐藏后不再派发），Close 命令
            // 不保证送达。直接退出前手动收尾托盘（删图标）。
            // 持久化由后台线程在每次改动后即时完成，此处无需再存。
            self.tray.shutdown();
            std::process::exit(0);
        }

        // ── 关闭拦截：点 ✕ 时默认收进托盘 ──
        // 托盘图标没加上时不能隐藏：否则窗口消失且无法恢复，程序等于「丢失」。
        if ctx.input(|i| i.viewport().close_requested())
            && self.state.settings.minimize_to_tray
            && self.tray.icon_ok()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            // 隐藏走 Win32 直调：CancelClose 后若用 ViewportCommand::Visible(false)，
            // 命令会在事件循环停摆期间排队而永不执行。SW_HIDE 由系统直接完成。
            // 托盘的恢复/退出动作由托盘线程自己直调 Win32，不依赖本事件循环。
            hide_main_window(_frame);
        }

        // ── 托盘状态同步（图标 + 悬停提示，2 秒节流）──
        self.sync_tray(now_ms);

        // 轮询后台扫描结果（非阻塞）。
        if let Some(discovery) = self.bridge.try_take_scan() {
            self.import_scanning = false;
            // 扫描期间用户已关掉弹窗：结果直接丢弃，等下次再扫。
            if self.show_import {
                // 默认只勾选「还没导入过」的，已存在的自动跳过，避免重复导入的困惑。
                let known: std::collections::HashSet<&str> = self
                    .state
                    .accounts
                    .iter()
                    .flat_map(|a| a.keys.iter())
                    .map(|k| k.secret.as_str())
                    .collect();
                self.import_selected = discovery
                    .keys
                    .iter()
                    .filter(|k| !known.contains(k.secret.as_str()))
                    .map(|k| k.secret.clone())
                    .collect();
                self.import_result = Some(discovery);
            } else {
                self.import_result = None;
            }
        }

        self.reminder_banner(ctx, now_ms);
        self.twin_banner(ctx, now_ms);
        self.top_bar(ctx, now_ms);
        // 设置面板的显隐由顶栏「设置」开关控制（show_settings）。
        if self.show_settings {
            self.settings_panel(ctx);
        }

        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(BG_APP)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ctx, |ui| {
                if self.state.accounts.is_empty() {
                    empty_state(ui);
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for account in self.state.accounts.clone().iter() {
                            self.account_card(ui, account, now_ms);
                        }
                    });
            });

        if self.show_log {
            self.log_panel(ctx);
        }

        self.add_window(ctx);
        self.import_window(ctx);

        // 删除确认（内联弹层）
        if let Some(id) = self.pending_delete.clone() {
            egui::Window::new("删除确认")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("删除后该 key 的额度将不再统计，确定删除？");
                    ui.horizontal(|ui| {
                        if ui.button("确定删除").clicked() {
                            self.send(Command::RemoveKey(id));
                            self.pending_delete = None;
                        }
                        if ui.button("取消").clicked() {
                            self.pending_delete = None;
                        }
                    });
                });
        }

        // 删除账户确认（内联弹层）
        if let Some(id) = self.pending_delete_account.clone() {
            let (name, key_count) = self
                .state
                .accounts
                .iter()
                .find(|a| a.id == id)
                .map(|a| (a.name.clone(), a.keys.len()))
                .unwrap_or_else(|| (id.clone(), 0));
            egui::Window::new("删除账户确认")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "确定删除账户「{name}」及其下的 {key_count} 个 key？此操作不可撤销。"
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("确定删除").clicked() {
                            self.send(Command::RemoveAccount(id));
                            self.pending_delete_account = None;
                        }
                        if ui.button("取消").clicked() {
                            self.pending_delete_account = None;
                        }
                    });
                });
        }

        // 一键清理确认（内联弹层）
        if self.pending_cleanup {
            // 确认时以当前扫描结果为准；弹窗开着时扫描结果被清掉（理论上不会，
            // 防御性处理）就先刷新扫描而不是拿空列表去清理——空列表会把所有
            // 同步 key 当成「已失效」全部删掉。
            let secrets_valid = self
                .import_result
                .as_ref()
                .is_some_and(|d| !d.keys.is_empty());
            let secrets: Vec<String> = self
                .import_result
                .as_ref()
                .map(|d| d.keys.iter().map(|k| k.secret.clone()).collect())
                .unwrap_or_default();
            egui::Window::new("清理确认")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("将删除所有「本机 zcode 里已不存在」的同步 key 及其腾空的账户。");
                    ui.label(
                        RichText::new("手动添加的 key 不会被清理。")
                            .small()
                            .color(TEXT_MID),
                    );
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(secrets_valid, egui::Button::new("确定清理"))
                            .clicked()
                        {
                            self.send(Command::CleanupStale(secrets));
                            self.pending_cleanup = false;
                            self.show_import = false;
                        }
                        if ui.button("取消").clicked() {
                            self.pending_cleanup = false;
                        }
                    });
                });
        }
    }
}

// ─────────────────────────── 组件 ───────────────────────────

fn empty_state(ui: &mut egui::Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        icons::glyph(ui, Icon::Bolt, 44.0, TEXT_MID);
        ui.add_space(12.0);
        ui.label(RichText::new("还没有任何 key").size(20.0).color(TEXT_HI));
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "你电脑上装有 zcode 的话，点右上角「从本机 zcode 导入」即可，无需手动粘贴。",
            )
            .color(TEXT_MID),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "也可以点「添加 key」手动粘贴 Z.ai API Key。同一账户的多个 key 会自动合并。",
            )
            .color(TEXT_MID),
        );
    });
}

fn window_row(ui: &mut egui::Ui, w: &WindowView, now_ms: i64, lead_ms: i64) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(&w.label).color(TEXT_MID));

        let pct = normalize_percent(w.used_percent);
        let bar = egui::ProgressBar::new((pct / 100.0) as f32)
            .desired_width(160.0)
            .desired_height(8.0)
            .fill(usage_color(pct));
        ui.add(bar);
        ui.label(RichText::new(format!("{pct:.0}%")).color(usage_color(pct)));

        // 有具体数字时显示「已用 / 总量」，比百分比更直观
        if let (Some(used), Some(total)) = (w.used, w.total) {
            ui.label(
                RichText::new(format!("{used}/{total}"))
                    .monospace()
                    .color(TEXT_MID),
            )
            .on_hover_text(format!("已用 {} / 共 {}", used, total));
        }

        if let Some(reset) = w.next_reset_ms {
            let delta = reset - now_ms;
            let urgent = delta <= lead_ms;
            let txt = if delta <= 0 {
                "已到刷新时间".to_string()
            } else {
                format!("刷新倒计时 {}", format_countdown(delta))
            };
            ui.label(RichText::new(txt).color(if urgent { BAD } else { TEXT_MID }));

            // 时间进度：占整个窗口的比例
            if let Some(total) = w.window_ms {
                let elapsed = (total - delta.max(0).min(total)).max(0);
                let frac = (elapsed as f32 / total.max(1) as f32).clamp(0.0, 1.0);
                let bar = egui::ProgressBar::new(frac)
                    .desired_width(90.0)
                    .desired_height(6.0)
                    .fill(ACCENT);
                ui.add(bar).on_hover_text("窗口时间进度");
            }
        }
    });
}

fn key_row(ui: &mut egui::Ui, app: &mut BoardApp, key: &KeyView, account: &AccountView) {
    Frame::new()
        .fill(BG_CARD_ALT)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("KEY").small().color(ACCENT));
                if key.account_pinned {
                    icons::glyph(ui, Icon::Pin, 13.0, TEXT_MID)
                        .on_hover_text("已手动固定到该账户");
                }
                ui.label(RichText::new(&key.masked).monospace().color(TEXT_MID));

                let (badge_text, badge_color) = icons::status_color(key.status);
                ui.label(RichText::new(badge_text).small().color(badge_color));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icons::icon_button(ui, Icon::Trash, "删除该 key", 14.0).clicked() {
                        app.pending_delete = Some(key.id.clone());
                    }
                    if icons::icon_button(ui, Icon::Refresh, "刷新该 key", 14.0).clicked() {
                        app.send(Command::RefreshKey(key.id.clone()));
                    }
                    if ui
                        .small_button(if key.enabled {
                            "已启用"
                        } else {
                            "已停用"
                        })
                        .clicked()
                    {
                        app.send(Command::UpdateKey(
                            key.id.clone(),
                            zcode_model::UpdateKeyRequest {
                                label: None,
                                enabled: Some(!key.enabled),
                            },
                        ));
                    }
                    // 移动到其他账户
                    egui::ComboBox::from_id_salt(format!("mv_{}", key.id))
                        .selected_text("移到…")
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(false, "（取消固定，重新自动归组）")
                                .clicked()
                            {
                                app.send(Command::MoveKey(key.id.clone(), None));
                            }
                            for other in &app.state.accounts {
                                if other.id != account.id
                                    && ui
                                        .selectable_label(false, format!("移到 {}", other.name))
                                        .clicked()
                                {
                                    app.send(Command::MoveKey(
                                        key.id.clone(),
                                        Some(other.id.clone()),
                                    ));
                                }
                            }
                        });
                });
            });
            if let Some(err) = &key.last_error {
                // 无 plan 是中性提示，不用红色告警语气
                let color = if key.status == KeyStatus::NoPlan {
                    TEXT_MID
                } else {
                    BAD
                };
                ui.label(RichText::new(err).small().color(color));
            }
            if let Some(check) = key.last_checked_ms {
                ui.label(
                    RichText::new(format!("上次探测 {}", fmt_clock(check)))
                        .small()
                        .color(TEXT_MID),
                );
            }
        });
}

fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).color(TEXT_HI).strong())
        .default_open(true)
        .show(ui, add);
}

fn quick_interval(ui: &mut egui::Ui, settings: &mut Settings, refresh: &mut bool) {
    ui.horizontal(|ui| {
        for (label, secs) in [("30s", 30), ("1m", 60), ("5m", 300), ("15m", 900)] {
            if ui
                .selectable_label(settings.refresh_interval_secs == secs, label)
                .clicked()
            {
                settings.refresh_interval_secs = secs;
                *refresh = true;
            }
        }
    });
}

fn label_kv(ui: &mut egui::Ui, k: &str, v: &str, color: Color32) {
    ui.label(RichText::new(format!("{k} ")).color(TEXT_MID));
    ui.label(RichText::new(v).color(color).strong());
    ui.add_space(14.0);
}

fn usage_color(pct: f64) -> Color32 {
    if pct >= BAD_PERCENT {
        BAD
    } else if pct >= WARN_PERCENT {
        WARN
    } else {
        OK
    }
}

fn lead_ms(app: &BoardApp) -> i64 {
    app.state.settings.notify_lead_minutes as i64 * 60_000
}

fn global_next_reset(accounts: &[AccountView]) -> Option<i64> {
    accounts.iter().filter_map(|a| a.next_reset_ms).min()
}

/// 导入列表里的凭证脱敏。
fn mask_item_secret(secret: &str) -> String {
    zcode_model::mask_secret(secret)
}

fn fmt_clock(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "-".to_string())
}

fn default_data_dir() -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("ZCODE_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return std::path::PathBuf::from(dir);
    }
    // 数据目录跟随 exe 所在目录（而不是进程 cwd）：开机启动时 Windows 给的
    // 工作目录不可控，若按 cwd 找数据会得到一份空看板。取不到 exe 路径时才
    // 退回当前目录。
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && !dir.as_os_str().is_empty()
    {
        return dir.join("data");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("data")
}

// ─────────────────────────── 配色 ───────────────────────────
// 调色板常量统一放在 `crate::icons`（状态徽章等组件也要用），此处已 import。

/// 应用全局视觉风格（暗色、紧凑、圆角）。
pub fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(10);
    style.spacing.button_padding = Vec2::new(8.0, 4.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.window_fill = BG_PANEL;
    style.visuals.panel_fill = BG_APP;
    style.visuals.extreme_bg_color = BG_APP;
    style.visuals.faint_bg_color = BG_CARD;
    style.visuals.widgets.noninteractive.bg_fill = BG_CARD;
    style.visuals.widgets.inactive.bg_fill = BG_CARD;
    style.visuals.widgets.hovered.bg_fill = BG_CARD_ALT;
    style.visuals.widgets.active.bg_fill = BG_CARD_ALT;
    style.visuals.widgets.noninteractive.weak_bg_fill = BG_CARD;
    style.visuals.widgets.inactive.weak_bg_fill = BG_CARD;
    ctx.set_style(style);
    ctx.set_theme(egui::Theme::Dark);
}

// 让 `Widget` trait 在文件里可用（ProgressBar::add 等）
#[allow(unused_imports)]
use egui::Widget as _;
