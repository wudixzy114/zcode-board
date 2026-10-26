//! 主界面：账户卡片、配额窗口、倒计时、设置与事件日志。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::App;
use egui::{
    Align, Color32, CornerRadius, Frame, Layout, Pos2, RichText, Sense, Shape, Stroke, Vec2, pos2,
    vec2,
};

use crate::bridge::{Bridge, Command};
use crate::icons::{
    self, ACCENT, BAD, BAD_FILL, BG_APP, BG_CARD, BG_CARD_ALT, BG_PANEL, BORDER, Icon, OK, TEXT_HI,
    TEXT_MID, WARN, WARN_FILL,
};
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
    AccountView, AppState, KeyStatus, KeyView, RatePoint, Settings, WindowView, format_countdown,
    format_rate,
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
        bridge.send(Command::SetNotifier(std::sync::Arc::new(
            move |title, body| {
                let _ = notify_tx.send(TrayCmd::Balloon { title, body });
            },
        )));
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

    /// 把用量档位同步到托盘与任务栏：图标颜色 + 悬停提示（2 秒节流，避免每帧都推）。
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

        // 图标档位跟随「启用通知的 plan」的用量而非探测成败：
        // 任意绿→绿，否则任意黄→黄，否则红（含全局关通知、全静音、无数据）。
        let status = tray_status_from(&self.state);
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
                        if icons::pill_button(
                            ui,
                            Icon::Refresh,
                            label,
                            true,
                            !refreshing,
                            refreshing,
                        )
                        .clicked()
                        {
                            self.refresh_all();
                        }
                        if icons::icon_toggle(ui, Icon::Settings, self.show_settings, "设置")
                            .clicked()
                        {
                            self.show_settings = !self.show_settings;
                        }
                        if icons::icon_toggle(ui, Icon::Log, self.show_log, "事件日志").clicked()
                        {
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

                // 全局：下一次额度刷新 + 下一次自动刷新倒计时 + 速率区间选择
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

                    // 速率图表的回看区间（持久化在设置里，重启后保持）。
                    ui.separator();
                    icons::glyph(ui, Icon::Activity, 13.0, TEXT_MID)
                        .on_hover_text("各窗口卡片显示「区间内速率变化」，在这里选回看区间");
                    ui.label(RichText::new("速率区间").color(TEXT_MID));
                    let current = self.state.settings.rate_span_secs;
                    for (label, secs) in [
                        ("30m", 1_800),
                        ("1h", 3_600),
                        ("6h", 21_600),
                        ("24h", 86_400),
                        ("全部", 0),
                    ] {
                        if ui
                            .selectable_label(
                                current == secs,
                                RichText::new(label).small().color(if current == secs {
                                    ACCENT
                                } else {
                                    TEXT_MID
                                }),
                            )
                            .clicked()
                        {
                            let mut next = self.state.settings.clone();
                            next.rate_span_secs = secs;
                            // 与设置面板同一套草稿机制：面板开着也立刻看到一致状态。
                            self.settings_draft = Some(next.clone());
                            self.send(Command::UpdateSettings(next));
                        }
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
                        let mut done =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
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
                        } else if icons::icon_button(ui, Icon::Bell, "静音该账户的刷新提醒", 15.0)
                            .clicked()
                        {
                            self.send(Command::SetMuted(account.id.clone(), true));
                        }
                        if icons::icon_button(ui, Icon::Trash, "删除该账户及其所有 key", 15.0)
                            .clicked()
                        {
                            self.pending_delete_account = Some(account.id.clone());
                        }
                        if icons::icon_button(ui, Icon::Refresh, "刷新该账户", 15.0).clicked()
                        {
                            self.send(Command::RefreshAccount(account.id.clone()));
                        }
                    });
                });

                // ── 配额窗口：短窗口（5 小时档）主展示，周 / 月降级为紧凑摘要行 ──
                if account.windows.is_empty() {
                    ui.label(RichText::new("暂无额度数据").color(TEXT_MID));
                } else {
                    let (primary, secondary): (Vec<&WindowView>, Vec<&WindowView>) = account
                        .windows
                        .iter()
                        .partition(|w| is_primary_window(w));
                    for w in &primary {
                        primary_window(ui, w, now_ms, lead_ms(self), self.state.settings.rate_span_secs);
                    }
                    if !secondary.is_empty() {
                        ui.add_space(4.0);
                        secondary_window_rows(
                            ui,
                            &secondary,
                            now_ms,
                            &account.id,
                            lead_ms(self),
                        );
                    }
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
                        if ui
                            .add(egui::DragValue::new(&mut c).speed(1).range(1.0..=16.0))
                            .changed()
                        {
                            settings.concurrency = c as usize;
                            refresh = true;
                        }
                        ui.add_space(4.0);
                        ui.label("请求超时（秒）");
                        let mut t = settings.request_timeout_secs as f64;
                        if ui
                            .add(egui::DragValue::new(&mut t).speed(1).range(3.0..=120.0))
                            .changed()
                        {
                            settings.request_timeout_secs = t as u64;
                            refresh = true;
                        }
                    });

                    group(ui, "下一次刷新提醒", |ui| {
                        if ui
                            .checkbox(&mut settings.notify_enabled, "启用刷新提醒")
                            .changed()
                        {
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
                            .color(
                                if self.autostart_actual == settings.start_with_windows {
                                    TEXT_MID
                                } else {
                                    WARN
                                },
                            ),
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
                                    ui.label(RichText::new("已导入").small().color(OK));
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
                            RichText::new(format!("{names} 的额度数据完全相同，疑似同一个账户"))
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

// ─────────────────────────── 配额窗口（分层展示） ───────────────────────────

/// 短窗口时长上限：≤6 小时的窗口（即 5 小时档）视为主要展示对象。
const PRIMARY_WINDOW_MAX_MS: i64 = 6 * 3_600_000;

/// 主窗口判定：5 小时档短窗口是日常最紧、最常看的额度约束，
/// 周 / 月等长周期窗口（以及无法推断时长的未知窗口）降级为摘要行。
fn is_primary_window(w: &WindowView) -> bool {
    w.window_ms.is_some_and(|ms| ms <= PRIMARY_WINDOW_MAX_MS)
        || w
            .key
            .rsplit_once(':')
            .and_then(|(_, unit)| unit.parse::<u32>().ok())
            == Some(3)
}

/// 主窗口大展示：亮底容器 + 大号百分比 + 全宽粗进度条 +
/// 大字「X 后重置 / 预计 X 后用满」+ 速率曲线。
///
/// 这两个倒计时回答的是「我现在还敢不敢放开用」，是整个看板的核心信息，
/// 因此用主字号；周 / 月窗口见 [`secondary_window_rows`]。
fn primary_window(
    ui: &mut egui::Ui,
    w: &WindowView,
    now_ms: i64,
    lead_ms: i64,
    rate_span_secs: u64,
) {
    Frame::new()
        .fill(BG_CARD_ALT)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width() - 2.0);
            let pct = w.used_percent;
            let color = usage_color(pct);

            // 行 1：窗口名（左） + 已用/总量 与 大号百分比（右）
            ui.horizontal(|ui| {
                ui.label(RichText::new(&w.label).size(12.5).color(TEXT_MID));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{pct:.0}%"))
                            .size(24.0)
                            .strong()
                            .color(color),
                    )
                    .on_hover_text(per_key_tooltip(w));
                    if let (Some(used), Some(total)) = (w.used, w.total) {
                        ui.label(
                            RichText::new(format!("{used}/{total}"))
                                .monospace()
                                .size(13.0)
                                .color(TEXT_MID),
                        )
                        .on_hover_text(format!("已用 {} / 共 {}", used, total));
                    }
                });
            });

            // 行 2：全宽粗进度条（时间进度移入悬停提示，不再单独占一条小条）
            full_width_bar(ui, (pct / 100.0) as f32, 12.0, color)
                .on_hover_text(if let (Some(total_ms), Some(reset)) = (w.window_ms, w.next_reset_ms)
                {
                    let delta = (reset - now_ms).clamp(0, total_ms);
                    let frac = 1.0 - delta as f32 / total_ms.max(1) as f32;
                    format!(
                        "窗口时间进度 {:.0}%（剩 {}）",
                        frac * 100.0,
                        fmt_countdown_compact(reset - now_ms)
                    )
                } else {
                    "窗口时间进度".to_string()
                });

            // 行 3：核心倒计时（大字）——左「X 后重置」，右「预计 X 后用满」
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(reset) = w.next_reset_ms {
                    let delta = reset - now_ms;
                    let urgent = delta <= lead_ms;
                    if delta > 0 {
                        icons::glyph(ui, Icon::Clock, 15.0, if urgent { BAD } else { OK });
                        ui.label(
                            RichText::new(format!("{} 后重置", fmt_countdown_compact(delta)))
                                .size(16.0)
                                .strong()
                                .color(if urgent { BAD } else { TEXT_HI }),
                        )
                        .on_hover_text(format!(
                            "精确 {}，时刻 {}",
                            format_countdown(delta),
                            fmt_hm(reset)
                        ));
                        ui.label(
                            RichText::new(fmt_hm(reset))
                                .small()
                                .color(TEXT_MID),
                        );
                    } else {
                        ui.label(
                            RichText::new("已到刷新时间")
                                .size(16.0)
                                .strong()
                                .color(BAD),
                        );
                    }
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // right_to_left 中先加的在最右：文字最右、图标在其左侧。
                    match eta_state(w, now_ms) {
                        Some(EtaState::Exhaust { eta_ms, reset_first }) => {
                            // 重置先到 = 本窗口实际不会用满，弱色即可；
                            // 否则按紧急程度红 / 黄警示。
                            let urgent = !reset_first && eta_ms <= 6 * 3_600_000;
                            let color = if !reset_first && eta_ms <= lead_ms {
                                BAD
                            } else if urgent {
                                WARN
                            } else {
                                TEXT_MID
                            };
                            let hover = if reset_first {
                                format!(
                                    "按当前速率 {} 后才会用完，而重置（{} 后）先到，届时额度会刷新",
                                    fmt_duration_rough(eta_ms),
                                    fmt_countdown_compact(
                                        w.next_reset_ms.unwrap_or(now_ms) - now_ms
                                    )
                                )
                            } else {
                                "剩余量 ÷ 当前速率的线性外推，仅供参考".to_string()
                            };
                            ui.label(
                                RichText::new(format!("预计 {} 后用满", fmt_duration_rough(eta_ms)))
                                    .size(16.0)
                                    .strong()
                                    .color(color),
                            )
                            .on_hover_text(hover);
                            if urgent {
                                icons::glyph(ui, Icon::Warning, 15.0, color);
                            }
                        }
                        Some(EtaState::Idle) => {
                            ui.label(RichText::new("当前无消耗").size(16.0).color(TEXT_MID));
                        }
                        None => {}
                    }
                });
            });

            rate_section(ui, w, rate_span_secs);
        });
    ui.add_space(4.0);
}

/// 次要窗口（周 / 月等长周期）：单行紧凑摘要，Grid 对齐，
/// 刻意小字弱化——它们只在本周 / 本月预算吃紧时才值得细看。
///
/// 「预计 X 后用满」在数据满足时照常显示：月度预算的耗尽时间
/// （如「预计 3.0 天后用满」）恰恰是这类窗口最值得看的信息。
fn secondary_window_rows(
    ui: &mut egui::Ui,
    windows: &[&WindowView],
    now_ms: i64,
    account_id: &str,
    lead_ms: i64,
) {
    egui::Grid::new(format!("sec_windows_{account_id}"))
        .num_columns(5)
        .spacing([12.0, 5.0])
        .show(ui, |ui| {
            for w in windows {
                let pct = w.used_percent;
                ui.label(RichText::new(&w.label).small().color(TEXT_MID));
                ui.add(
                    egui::ProgressBar::new((pct / 100.0) as f32)
                        .desired_width(76.0)
                        .desired_height(5.0)
                        .fill(usage_color(pct)),
                );
                let mut txt = format!("{pct:.0}%");
                if let (Some(used), Some(total)) = (w.used, w.total) {
                    txt = format!("{pct:.0}% · {used}/{total}");
                }
                ui.label(
                    RichText::new(txt)
                        .small()
                        .monospace()
                        .color(usage_color(pct)),
                )
                .on_hover_text(per_key_tooltip(w));
                // 预计用满：月度预算 5 小时内耗尽算相当紧急，沿用与主窗口
                // 相同的紧急阈值（提醒提前量 → 红，6 小时 → 黄）。
                match eta_state(w, now_ms) {
                    Some(EtaState::Exhaust { eta_ms, reset_first }) => {
                        let color = if reset_first {
                            TEXT_MID
                        } else if eta_ms <= lead_ms {
                            BAD
                        } else if eta_ms <= 6 * 3_600_000 {
                            WARN
                        } else {
                            TEXT_MID
                        };
                        ui.label(
                            RichText::new(format!("预计 {} 后用满", fmt_duration_rough(eta_ms)))
                                .small()
                                .color(color),
                        )
                        .on_hover_text(if reset_first {
                            "按当前速率要到这之后才会用完，而重置会先到，届时额度刷新"
                        } else {
                            "剩余量 ÷ 当前速率的线性外推，仅供参考"
                        });
                    }
                    Some(EtaState::Idle) => {
                        ui.label(RichText::new("无消耗").small().color(TEXT_MID));
                    }
                    None => {
                        ui.label(RichText::new("").small());
                    }
                }
                if let Some(reset) = w.next_reset_ms {
                    let delta = (reset - now_ms).max(0);
                    ui.label(
                        RichText::new(format!("{} 后重置", fmt_duration_rough(delta)))
                            .small()
                            .color(TEXT_MID),
                    );
                } else {
                    ui.label(RichText::new("—").small().color(TEXT_MID));
                }
                ui.end_row();
            }
        });
}

/// 「预计用满」的展示三态。
#[derive(Debug)]
enum EtaState {
    /// 按当前速率 X 后用满。reset_first 表示重置会先到（届时额度刷新、
    /// 实际不会用满）——数字照样给出，只是弱色 + 悬停说明。
    Exhaust { eta_ms: i64, reset_first: bool },
    /// 有速率统计但当前消耗为 0（如月度窗口长时间没有调用）。
    Idle,
}

/// 按当前速率线性外推「多久后用满」。
///
/// 重置先到时不再返回 None 把整个数字藏起来——用户明确要看这个数，
/// 藏起来只会让人以为坏了；改为返回 reset_first 标记交展示层弱化。
fn eta_state(w: &WindowView, now_ms: i64) -> Option<EtaState> {
    let rate = w.rate.as_ref()?;
    // basis = 0 表示连「当前速率」都没统计出来（跨度不足 / 离线间隙）。
    if rate.basis_ms <= 0 {
        return None;
    }
    if rate.current_per_min <= 0.0 {
        return Some(EtaState::Idle);
    }
    // 百分比口径的速率（上游只给百分比的窗口，以及数值换算成百分比
    // 展示的窗口）统一用 100% − 已用百分比 作剩余量——数值剩余量与
    // 百分比速率混用会算出错误的外推时间。
    let remaining = if rate.percent_based {
        (100.0 - w.used_percent).max(0.0)
    } else {
        match w.remaining {
            Some(r) => r,
            None => return None,
        }
    };
    if remaining <= 0.0 {
        return None;
    }
    let eta_ms = (remaining / rate.current_per_min * 60_000.0) as i64;
    let reset_first = w
        .next_reset_ms
        .is_some_and(|reset| eta_ms >= reset - now_ms);
    Some(EtaState::Exhaust { eta_ms, reset_first })
}

/// 各 key 用量百分比的悬停明细（聚合值代表最紧的一个 key）。
fn per_key_tooltip(w: &WindowView) -> String {
    if w.per_key.is_empty() {
        return format!("{} 个 key 聚合，取最紧值", w.key_count);
    }
    let lines: Vec<String> = w
        .per_key
        .iter()
        .map(|p| format!("{}：{:.0}%", p.key_label, p.used_percent))
        .collect();
    lines.join("\n")
}

/// 大字倒计时的人话格式：<1 分钟「42秒」；<1 小时「37分钟」；
/// <1 天「2小时13分」；≥1 天「3.5天」。
fn fmt_countdown_compact(delta_ms: i64) -> String {
    let s = (delta_ms / 1000).max(0);
    if s < 60 {
        format!("{s}秒")
    } else if s < 3_600 {
        format!("{}分钟", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3_600, (s % 3_600) / 60);
        if m > 0 {
            format!("{h}小时{m}分")
        } else {
            format!("{h}小时")
        }
    } else {
        format!("{:.1}天", s as f64 / 86_400.0)
    }
}

// ─────────────────────────── 消耗速率 ───────────────────────────

/// 主窗口下方的速率区：当前速率 / 区间峰均 + 速率曲线。
/// 「预计 X 后用满」已上移到主窗口倒计时行，这里不再重复。
///
/// 数据来源是后台每次刷新积累的用量采样（见 core::rate）：
/// - 当前速率 = 最近一段时间的平均差分（不跨窗口重置、不跨离线间隙）；
/// - 曲线 = 所选区间内的逐区间瞬时速率，随每次刷新向右生长。
fn rate_section(ui: &mut egui::Ui, w: &WindowView, span_secs: u64) {
    let Some(rate) = &w.rate else {
        // 有信号（数值或百分比）的窗口都在采样：刚导入时提示积累中。
        if w.used.is_some() || w.used_percent > 0.0 {
            ui.label(
                RichText::new("速率：等待更多采样点（每次刷新积累一个，两个起绘曲线）")
                    .small()
                    .color(TEXT_MID),
            );
        }
        return;
    };

    // 内部速率统一是「每分钟」；百分比序列粗量化，展示放大到 %/h 更可读。
    let display_scale = if rate.percent_based { 60.0 } else { 1.0 };
    let scoped = filter_rate_points(&rate.points, span_secs);

    ui.horizontal(|ui| {
        icons::glyph(ui, Icon::Activity, 13.0, ACCENT);
        if rate.basis_ms > 0 {
            ui.label(
                RichText::new(format!(
                    "当前 {} {}",
                    format_rate(rate.current_per_min * display_scale),
                    rate.unit
                ))
                .strong()
                .color(TEXT_HI),
            );
            ui.label(
                RichText::new(format!("（近 {}）", fmt_span_rough(rate.basis_ms)))
                    .small()
                    .color(TEXT_MID),
            );
        } else {
            ui.label(
                RichText::new("当前 —（采样跨度不足或离线间隙）")
                    .small()
                    .color(TEXT_MID),
            );
        }

        if let Some((peak, avg)) = rate_stats(scoped) {
            ui.label(
                RichText::new(format!(
                    "峰 {} · 均 {}",
                    format_rate(peak * display_scale),
                    format_rate(avg * display_scale)
                ))
                .small()
                .color(TEXT_MID),
            )
            .on_hover_text(format!(
                "所选区间内逐次刷新的瞬时速率：最高 {} {}，平均 {} {}",
                format_rate(peak * display_scale),
                rate.unit,
                format_rate(avg * display_scale),
                rate.unit
            ));
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let n = scoped.len();
            ui.label(
                RichText::new(format!("{n} 个采样点 · 区间 {}", span_label(span_secs)))
                    .small()
                    .color(TEXT_MID),
            );
        });
    });

    if scoped.len() >= 2 {
        rate_chart(ui, scoped, &rate.unit, display_scale);
    } else if !rate.points.is_empty() {
        ui.label(
            RichText::new(format!(
                "区间 {} 内只有 {}/2 个采样点，曲线等待积累",
                span_label(span_secs),
                scoped.len()
            ))
            .small()
            .color(TEXT_MID),
        );
    }
}

/// 速率迷你曲线：面积折线 + 峰值参考线 + 时间轴标签 + 悬停读数。
/// `display_scale` 把内部的「每分钟」换算到展示单位（百分比序列 ×60 → %/h）。
fn rate_chart(ui: &mut egui::Ui, points: &[RatePoint], unit: &str, display_scale: f64) {
    let height = 46.0;
    // 宽度钳到有限值：上游布局一旦给出无限宽（见 full_width_bar 的注释），
    // 虚线循环会吃满内存挂死界面——宁可画窄，不能挂死。
    let width = ui.available_width().min(4096.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let p = ui.painter();

    let t0 = points[0].t_ms;
    let t1 = points[points.len() - 1].t_ms;
    let span_px = (t1 - t0).max(1) as f32;
    let peak = points
        .iter()
        .map(|s| s.per_min)
        .fold(f64::MIN, f64::max)
        .max(1e-9);
    let v_top = peak * 1.18; // 峰值线上方留出标签空间

    let inner = rect.shrink(2.0);
    let axis_y = inner.bottom() - 11.0; // 底部留时间标签的高度
    let plot_h = (axis_y - inner.top() - 10.0).max(8.0); // 顶部留峰值标签
    let x_of = |t: i64| inner.left() + (t - t0) as f32 / span_px * inner.width();
    let y_of = |v: f64| axis_y - (v / v_top) as f32 * plot_h;

    // 基线与面积折线
    p.line_segment(
        [pos2(inner.left(), axis_y), pos2(inner.right(), axis_y)],
        Stroke::new(1.0, BORDER),
    );
    let pts: Vec<Pos2> = points
        .iter()
        .map(|s| pos2(x_of(s.t_ms), y_of(s.per_min)))
        .collect();
    let mut area = pts.clone();
    area.push(pos2(pts[pts.len() - 1].x, axis_y));
    area.push(pos2(pts[0].x, axis_y));
    p.add(Shape::convex_polygon(
        area,
        ACCENT.gamma_multiply(0.13),
        Stroke::NONE,
    ));
    p.add(Shape::line(pts.clone(), Stroke::new(1.5, ACCENT)));

    // 峰值虚线 + 末端点
    let peak_y = y_of(peak);
    let dash = 4.0;
    let mut x = inner.left();
    while x < inner.right() {
        let end = (x + dash).min(inner.right());
        p.line_segment(
            [pos2(x, peak_y), pos2(end, peak_y)],
            Stroke::new(1.0, WARN.gamma_multiply(0.55)),
        );
        x += dash * 2.0;
    }
    p.add(Shape::circle_filled(pts[pts.len() - 1], 2.4, ACCENT));

    // 标签：左/右端时间、峰值刻度
    let small = egui::TextStyle::Small.resolve(ui.style()).size * 0.86;
    p.text(
        pos2(inner.left(), inner.top()),
        egui::Align2::LEFT_TOP,
        format!("峰 {} {}", format_rate(peak * display_scale), unit),
        egui::FontId::proportional(small),
        WARN.gamma_multiply(0.9),
    );
    p.text(
        pos2(inner.left(), axis_y + 2.0),
        egui::Align2::LEFT_TOP,
        fmt_hm(t0),
        egui::FontId::proportional(small),
        TEXT_MID,
    );
    p.text(
        pos2(inner.right(), axis_y + 2.0),
        egui::Align2::RIGHT_TOP,
        fmt_hm(t1),
        egui::FontId::proportional(small),
        TEXT_MID,
    );

    // 悬停：最近点十字线 + 读数（点在图上跟随，文字走系统 tooltip）
    if let Some(hover) = resp.hover_pos()
        && let Some(s) = points.iter().min_by(|a, b| {
            (x_of(a.t_ms) - hover.x)
                .abs()
                .total_cmp(&(x_of(b.t_ms) - hover.x).abs())
        })
    {
        let (cx, cy) = (x_of(s.t_ms), y_of(s.per_min));
        p.line_segment(
            [pos2(cx, inner.top()), pos2(cx, axis_y)],
            Stroke::new(1.0, TEXT_MID.gamma_multiply(0.6)),
        );
        p.add(Shape::circle_filled(pos2(cx, cy), 3.0, TEXT_HI));
        resp.on_hover_text(format!(
            "{} · {} {}",
            fmt_hm(s.t_ms),
            format_rate(s.per_min * display_scale),
            unit
        ));
    }
}

/// 区间过滤：以最后一个采样点为锚，保留锚点前 `span_secs` 秒内的点；0 = 全部。
fn filter_rate_points(points: &[RatePoint], span_secs: u64) -> &[RatePoint] {
    if span_secs == 0 || points.len() < 2 {
        return points;
    }
    let start = points[points.len() - 1].t_ms - span_secs as i64 * 1000;
    let idx = points.partition_point(|p| p.t_ms < start);
    &points[idx..]
}

/// 区间内逐点统计（峰 / 简单平均）。
fn rate_stats(points: &[RatePoint]) -> Option<(f64, f64)> {
    if points.is_empty() {
        return None;
    }
    let peak = points.iter().map(|p| p.per_min).fold(f64::MIN, f64::max);
    let avg = points.iter().map(|p| p.per_min).sum::<f64>() / points.len() as f64;
    Some((peak.max(0.0), avg.max(0.0)))
}

/// 速率区间的显示名。
fn span_label(secs: u64) -> &'static str {
    match secs {
        0 => "全部",
        1_800 => "30 分钟",
        3_600 => "1 小时",
        21_600 => "6 小时",
        86_400 => "24 小时",
        _ => "自定义",
    }
}

/// 统计跨度的粗略人话（「近 X」用）。
fn fmt_span_rough(ms: i64) -> String {
    let mins = (ms / 60_000).max(1);
    if mins < 60 {
        format!("{mins} 分钟")
    } else {
        format!("{:.0} 小时", mins as f64 / 60.0)
    }
}

/// 预计用满的时长人话（分钟 / 小时 / 天）。
fn fmt_duration_rough(ms: i64) -> String {
    let mins = (ms / 60_000).max(0);
    if mins < 60 {
        format!("{mins} 分钟")
    } else if mins < 48 * 60 {
        format!("{:.1} 小时", mins as f64 / 60.0)
    } else {
        format!("{:.1} 天", mins as f64 / 1440.0)
    }
}

/// `HH:MM`（图表时间轴标签）。
fn fmt_hm(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "-".to_string())
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
                    icons::glyph(ui, Icon::Pin, 13.0, TEXT_MID).on_hover_text("已手动固定到该账户");
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

/// 全宽进度条。`desired_width` 必须是有限值：
/// egui 0.32 对 `INFINITY` 的 allocate 会产出无限宽矩形并把它并进布局游标，
/// 毒化后续 `available_width()`（也变 INFINITY）——速率图的虚线循环
/// `while x < inner.right()` 随之永不终止，界面挂起、内存暴涨。
fn full_width_bar(ui: &mut egui::Ui, fraction: f32, height: f32, color: Color32) -> egui::Response {
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_width(ui.available_width())
            .desired_height(height)
            .fill(color),
    )
}

fn usage_color(pct: f64) -> Color32 {
    match usage_level(pct) {
        TrayStatus::Green => OK,
        TrayStatus::Yellow => WARN,
        TrayStatus::Red => BAD,
    }
}

/// 用量分档：与窗口进度条同一套阈值（60% / 85%）。
fn usage_level(pct: f64) -> TrayStatus {
    if pct >= BAD_PERCENT {
        TrayStatus::Red
    } else if pct >= WARN_PERCENT {
        TrayStatus::Yellow
    } else {
        TrayStatus::Green
    }
}

/// 单个 plan（账户）的档位：取其全部配额窗口的最高用量分档——最紧的窗口说了算。
/// 没有任何窗口数据时返回 `None`，不参与绿/黄判定，只落红兜底。
fn account_level(a: &AccountView) -> Option<TrayStatus> {
    a.windows
        .iter()
        .map(|w| w.used_percent)
        .max_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
        .map(usage_level)
}

/// 图标档位聚合：启用通知的 plan（全局开关开 + 未静音）里
/// 任意绿→绿，否则任意黄→黄，否则红。
///
/// 刷新失败时窗口数据保留上次值，图标不会因网络抖动闪变。
fn tray_status_from(state: &AppState) -> TrayStatus {
    if !state.settings.notify_enabled {
        return TrayStatus::Red;
    }
    aggregate_status(
        state
            .accounts
            .iter()
            .filter(|a| !a.notify_muted)
            .map(account_level),
    )
}

fn aggregate_status(levels: impl Iterator<Item = Option<TrayStatus>>) -> TrayStatus {
    let mut any_yellow = false;
    for level in levels {
        match level {
            Some(TrayStatus::Green) => return TrayStatus::Green,
            Some(TrayStatus::Yellow) => any_yellow = true,
            _ => {}
        }
    }
    if any_yellow {
        TrayStatus::Yellow
    } else {
        TrayStatus::Red
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_level_thresholds() {
        assert_eq!(usage_level(0.0), TrayStatus::Green);
        assert_eq!(usage_level(59.9), TrayStatus::Green);
        assert_eq!(usage_level(60.0), TrayStatus::Yellow);
        assert_eq!(usage_level(84.9), TrayStatus::Yellow);
        assert_eq!(usage_level(85.0), TrayStatus::Red);
        assert_eq!(usage_level(120.0), TrayStatus::Red);
    }

    #[test]
    fn aggregate_best_wins() {
        use TrayStatus::{Green, Red, Yellow};
        let lvl = |pct: f64| Some(usage_level(pct));

        // 任意绿 → 绿（红黄都被绿掩盖，图标回答的是「还有没有可放心用的 plan」）。
        assert_eq!(
            aggregate_status([lvl(30.0), lvl(70.0), lvl(90.0)].into_iter()),
            Green
        );
        // 无绿有黄 → 黄。
        assert_eq!(aggregate_status([lvl(70.0), lvl(90.0)].into_iter()), Yellow);
        // 仅红 → 红。
        assert_eq!(aggregate_status([lvl(90.0)].into_iter()), Red);
        // 无数据的 plan 只落红兜底，不会挡住黄档。
        assert_eq!(aggregate_status([None, lvl(65.0)].into_iter()), Yellow);
        assert_eq!(aggregate_status([None, lvl(90.0)].into_iter()), Red);
        // 空集合（全局关通知 / 全静音时走不到这里，但聚合本身兜底）→ 红。
        assert_eq!(aggregate_status([None].into_iter()), Red);
        assert_eq!(aggregate_status(std::iter::empty()), Red);
    }

    /// 造一条速率序列：从 `t0_min` 开始每分钟一个点，速率按 `rates`。
    fn pts(t0_min: i64, rates: &[f64]) -> Vec<RatePoint> {
        rates
            .iter()
            .enumerate()
            .map(|(i, r)| RatePoint {
                t_ms: (t0_min + i as i64) * 60_000,
                per_min: *r,
            })
            .collect()
    }

    #[test]
    fn rate_span_filter_anchors_at_latest_point() {
        let points = pts(0, &[10.0; 120]); // 2 小时
        // 回看 30 分钟：只留最后 31 个点（闭区间含左边界锚点），且始终包含
        // 最后一个点作为右端锚。
        let scoped = filter_rate_points(&points, 1_800);
        assert_eq!(scoped.len(), 31);
        assert_eq!(scoped[scoped.len() - 1].t_ms, points[points.len() - 1].t_ms);
        // 0 = 全部。
        assert_eq!(filter_rate_points(&points, 0).len(), 120);
        // 单点 / 空序列安全返回。
        assert_eq!(filter_rate_points(&points[..1], 1_800).len(), 1);
        assert!(filter_rate_points(&[], 1_800).is_empty());
    }

    #[test]
    fn rate_stats_peak_and_avg() {
        let points = pts(0, &[100.0, 300.0, 200.0]);
        let (peak, avg) = rate_stats(&points).unwrap();
        assert_eq!(peak, 300.0);
        assert!((avg - 200.0).abs() < 1e-9);
        assert!(rate_stats(&[]).is_none());
    }

    #[test]
    fn duration_and_span_formatting() {
        assert_eq!(fmt_duration_rough(5 * 60_000), "5 分钟");
        assert_eq!(fmt_duration_rough(90 * 60_000), "1.5 小时");
        assert_eq!(fmt_duration_rough(3 * 24 * 3_600_000), "3.0 天");
        assert_eq!(fmt_span_rough(45 * 60_000), "45 分钟");
        assert_eq!(fmt_span_rough(5 * 60_000), "5 分钟");
        assert_eq!(fmt_span_rough(300 * 60_000), "5 小时");
    }

    /// 用有限屏幕跑一帧真实 egui 布局（回归测试用）。
    /// `ctx.run` 会多 pass 调用，闭包须为 `Fn`。
    fn run_in_test_ui(f: impl Fn(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 300.0))),
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui));
        });
    }

    /// 回归：全宽进度条不能把布局游标毒化成无限宽。
    ///
    /// 历史事故：直接 `desired_width(f32::INFINITY)` 会让 egui 产出无限宽
    /// 矩形并并进游标，后续 `available_width()` 变 INFINITY，速率图的虚线
    /// 循环永不终止——界面挂起（Windows 事件 1002）+ 内存暴涨到 2GB。
    #[test]
    fn full_width_bar_does_not_poison_layout() {
        // 先证明毒源确实存在于 egui 0.32（若未来升级 egui 修复了该行为，
        // 此断言失败是提醒我们重新评估防御措施，属预期）。
        run_in_test_ui(|ui| {
            ui.add(egui::ProgressBar::new(0.5).desired_width(f32::INFINITY));
            assert!(
                !ui.available_width().is_finite(),
                "egui 0.32 的 INFINITY 进度条未毒化游标？防御前提已变，请复查"
            );
        });

        // 我们的 helper 必须保持有限宽，且真的占满可用宽度。
        run_in_test_ui(|ui| {
            let before = ui.available_width();
            let resp = full_width_bar(ui, 0.5, 12.0, OK);
            assert!(ui.available_width().is_finite());
            assert_eq!(resp.rect.width(), before);
            // 下一行再取可用宽度依然正常（游标未被毒化）。
            let after = ui.available_width();
            assert!(after.is_finite() && after > 0.0);
        });
    }

    #[test]
    fn countdown_compact_formatting() {
        assert_eq!(fmt_countdown_compact(42_000), "42秒");
        assert_eq!(fmt_countdown_compact(37 * 60_000), "37分钟");
        assert_eq!(
            fmt_countdown_compact((2 * 3_600 + 13 * 60) * 1000),
            "2小时13分"
        );
        assert_eq!(fmt_countdown_compact(5 * 3_600_000), "5小时");
        assert_eq!(fmt_countdown_compact(3 * 86_400_000), "3.0天");
        assert_eq!(fmt_countdown_compact(-5_000), "0秒");
    }

    /// 造一个只填判定 / ETA 相关字段的窗口视图。
    fn test_window(key: &str, window_ms: Option<i64>) -> WindowView {
        WindowView {
            key: key.to_string(),
            label: String::new(),
            kind: "TOKENS_LIMIT".to_string(),
            used_percent: 30.0,
            used: None,
            total: None,
            remaining: None,
            next_reset_ms: None,
            window_ms,
            key_count: 1,
            per_key: Vec::new(),
            rate: None,
        }
    }

    #[test]
    fn primary_window_classification() {
        // 5 小时档（unit 3 / 时长 ≤6h）是主窗口。
        let mut w = test_window("TOKENS_LIMIT:3", Some(5 * 3_600_000));
        assert!(is_primary_window(&w));
        // 时长缺失但 unit 是 3，同样按主窗口处理。
        w.window_ms = None;
        assert!(is_primary_window(&w));
        // 周 / 月等长周期与未知窗口降级为摘要行。
        assert!(!is_primary_window(&test_window("TOKENS_LIMIT:6", Some(7 * 86_400_000))));
        assert!(!is_primary_window(&test_window("TIME_LIMIT:5", None)));
    }

    #[test]
    fn eta_three_states() {
        let now = 1_000_000_000_i64;
        let mut w = test_window("TOKENS_LIMIT:3", Some(5 * 3_600_000));
        let rate = |per_min: f64, basis: i64, pct: bool| {
            Some(zcode_model::RateView {
                unit: "tok/min".to_string(),
                percent_based: pct,
                points: Vec::new(),
                current_per_min: per_min,
                basis_ms: basis,
            })
        };
        // 没有速率数据 → None。
        assert!(eta_state(&w, now).is_none());
        // 速率存在但 basis = 0（统计不出「当前速率」）→ None。
        w.rate = rate(100.0, 0, false);
        assert!(eta_state(&w, now).is_none());
        // 有速率但消耗为 0 → Idle（显示「当前无消耗」而不是消失）。
        w.rate = rate(0.0, 60_000, false);
        assert!(matches!(eta_state(&w, now), Some(EtaState::Idle)));

        // 剩余 10000，速率 100/min → 100 分钟后用满；重置在 2 小时后 → 正常展示。
        w.rate = rate(100.0, 60_000, false);
        w.remaining = Some(10_000.0);
        w.next_reset_ms = Some(now + 2 * 3_600_000);
        match eta_state(&w, now) {
            Some(EtaState::Exhaust { eta_ms, reset_first }) => {
                assert_eq!(eta_ms, 100 * 60_000);
                assert!(!reset_first);
            }
            other => panic!("expected Exhaust, got {other:?}"),
        }
        // 重置 50 分钟后先到 → 数字保留，reset_first 标记弱化展示。
        w.next_reset_ms = Some(now + 50 * 60_000);
        match eta_state(&w, now) {
            Some(EtaState::Exhaust { reset_first, .. }) => assert!(reset_first),
            other => panic!("expected Exhaust, got {other:?}"),
        }
        // 百分比序列没有剩余量数字：用 100% − 已用百分比兜底。
        w.rate = rate(1.0, 60_000, true); // %/min
        w.remaining = None;
        w.used_percent = 70.0; // 剩 30% → 30 分钟
        w.next_reset_ms = Some(now + 2 * 3_600_000);
        match eta_state(&w, now) {
            Some(EtaState::Exhaust { eta_ms, reset_first }) => {
                assert_eq!(eta_ms, 30 * 60_000);
                assert!(!reset_first);
            }
            other => panic!("expected Exhaust, got {other:?}"),
        }
    }

}
