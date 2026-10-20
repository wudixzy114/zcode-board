//! Windows 系统托盘：常驻右下角、单击恢复窗口、右键菜单（显示主界面 / 退出）。
//!
//! 实现思路：起一个后台线程，创建一个**创建后从不显示的隐藏顶层窗口**，
//! 自己跑消息循环，托盘图标的回调消息直接发给这个窗口——不需要给 egui 的窗口
//! 做子类化，也不会打扰主界面的事件循环。
//!
//! 不用 message-only 窗口（`HWND_MESSAGE` 父窗口）的原因：它无法成为前台窗口，
//! `SetForegroundWindow` 会失败，右键菜单弹出后点击别处关不掉（KB135788 的经典坑）。
//! 隐藏顶层窗口两样都行，代价只是任务管理器里多一个不可见窗口。

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

// ──────────────────────── 对外类型 ────────────────────────

/// 托盘 -> 界面 的事件。
///
/// 「恢复窗口」不是事件：托盘线程直接用 Win32 控制主窗口
/// （窗口隐藏后 egui 事件循环停摆，经它转发不可靠），见 `AttachMainWnd`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// 托盘菜单选择「退出」（发出前托盘已把主窗口唤醒）。
    Exit,
}

/// 界面 -> 托盘 的命令。
pub enum TrayCmd {
    /// 更新悬停提示。
    SetTip(String),
    /// 切换图标状态（正常 / 中性 / 异常）。
    SetStatus(TrayStatus),
    /// 同步把主窗口（任务栏 / 标题栏）图标换成该状态色。
    ///
    /// egui 的 `ViewportCommand::Icon` 底层是 winit 的 `set_window_icon`，
    /// 它只设置 ICON_SMALL（标题栏），任务栏用的 ICON_BIG 永远不会更新；
    /// 这里直接对主窗口 `WM_SETICON` 两个尺寸都换。
    SetWindowIcon(TrayStatus),
    /// 弹一条气泡通知（Win10+ 由系统渲染成 Toast 横幅，带默认提示音，
    /// 归属显示为本应用而不是 PowerShell）。
    Balloon { title: String, body: String },
    /// 绑定托盘要控制的主窗口（egui 窗口的 Win32 HWND，数值形式以便跨线程）。
    #[cfg(windows)]
    AttachMainWnd(isize),
    /// 结束托盘线程并删除图标。
    Shutdown,
}

/// 托盘图标档位（决定闪电颜色），由启用通知的 plan 用量聚合而来，
/// 见 `app.rs` 的 `aggregate_status`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    /// 绿色闪电：启用通知的 plan 里至少一个用量低于告警线。
    Green,
    /// 黄色闪电：没有绿档，但至少一个落在 60%–85% 告警区间。
    Yellow,
    /// 红色闪电：没有绿/黄档——全红、无数据，或没有启用通知的 plan。
    Red,
}

/// 托盘句柄。界面线程持有，用来下发命令与接收事件。
pub struct Tray {
    cmds: Sender<TrayCmd>,
    events: Receiver<TrayEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
    last_tip: std::sync::Mutex<String>,
    /// Shell_NotifyIcon 是否成功（图标是否真的加上了）。测试与排查用。
    icon_ok: Arc<AtomicBool>,
}

impl Tray {
    /// 启动托盘。失败时退化为无效句柄，所有方法变为空操作。
    pub fn spawn(initial_tip: &str) -> Self {
        let (cmd_tx, cmd_rx) = channel::<TrayCmd>();
        let (ev_tx, ev_rx) = channel::<TrayEvent>();
        let tip0 = initial_tip.to_string();
        let icon_ok = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new()
            .name("zcode-tray".into())
            .spawn({
                let icon_ok = icon_ok.clone();
                move || tray_thread(tip0, cmd_rx, ev_tx, icon_ok)
            })
            .ok();

        Self {
            cmds: cmd_tx,
            events: ev_rx,
            thread,
            last_tip: std::sync::Mutex::new(initial_tip.to_string()),
            icon_ok,
        }
    }

    /// 命令通道的克隆句柄：给桌面通知出口用（闭包要 'static，不能借用 Tray）。
    pub fn notify_sender(&self) -> std::sync::mpsc::Sender<TrayCmd> {
        self.cmds.clone()
    }

    /// 托盘图标是否成功加到了任务栏（Shell_NotifyIcon NIM_ADD 的返回）。
    pub fn icon_ok(&self) -> bool {
        self.icon_ok.load(Ordering::Relaxed)
    }

    /// 非阻塞地取尽托盘事件。
    pub fn drain_events(&self) -> Vec<TrayEvent> {
        self.events.try_iter().collect()
    }

    /// 更新悬停提示（内容没变时不重复下发）。
    pub fn set_tip(&self, tip: &str) {
        let mut last = self.last_tip.lock().unwrap_or_else(|e| e.into_inner());
        if *last != tip {
            *last = tip.to_string();
            let _ = self.cmds.send(TrayCmd::SetTip(tip.to_string()));
        }
    }

    /// 更新图标状态。
    pub fn set_status(&self, status: TrayStatus) {
        let _ = self.cmds.send(TrayCmd::SetStatus(status));
    }

    /// 同步更新主窗口（任务栏 / 标题栏）图标为该状态色。
    /// 主窗口须已通过 [`Self::attach_main_wnd`] 绑定。
    pub fn set_window_icon(&self, status: TrayStatus) {
        let _ = self.cmds.send(TrayCmd::SetWindowIcon(status));
    }

    /// 弹一条气泡通知（标题 + 正文），走系统通知中心。
    pub fn notify(&self, title: &str, body: &str) {
        let _ = self.cmds.send(TrayCmd::Balloon {
            title: title.to_string(),
            body: body.to_string(),
        });
    }

    /// 绑定主窗口句柄：托盘的「显示/退出」动作直接用 Win32 控制这个窗口，
    /// 不再依赖 egui 事件循环转发（窗口隐藏后 egui 事件循环会停摆，
    /// 经它转发的命令可能永远不被处理）。
    #[cfg(windows)]
    pub fn attach_main_wnd(&self, hwnd: windows_sys::Win32::Foundation::HWND) {
        let _ = self
            .cmds
            .send(TrayCmd::AttachMainWnd(hwnd as *mut _ as isize));
    }

    /// 关闭托盘（删图标、停线程）。重复调用安全。
    pub fn shutdown(&mut self) {
        let _ = self.cmds.send(TrayCmd::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ──────────────────────── 图标生成 ────────────────────────

/// 状态配色，顺序与 `status_index` 对应。
const STATUS_COLORS: [(TrayStatus, [u8; 3]); 3] = [
    (TrayStatus::Green, [76, 209, 138]),
    (TrayStatus::Yellow, [235, 182, 74]),
    (TrayStatus::Red, [235, 87, 100]),
];

/// 图标边长（32，和下面的放大倍数对应）。
pub const ICON_SIZE: u32 = 32;

#[cfg_attr(not(windows), allow(dead_code))]
fn status_index(status: TrayStatus) -> usize {
    match status {
        TrayStatus::Green => 0,
        TrayStatus::Yellow => 1,
        TrayStatus::Red => 2,
    }
}

/// 偶奇规则判断点是否在多边形内（射线法）。
fn point_in_poly(x: f32, y: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// 生成 32×32 的 RGBA 图标像素（egui 的 `IconData` 也用它）。
///
/// 闪电形状复用界面图标库的 24×24 多边形，做 4× 超采样光栅化：
/// 每个像素用 16 个采样点的覆盖比例作 alpha，边缘平滑无锯齿
/// （旧版是 16×16 点阵硬放大，边缘全是台阶）。
#[cfg_attr(not(windows), allow(dead_code))]
pub fn icon_rgba(status: TrayStatus) -> Vec<u8> {
    let color = STATUS_COLORS[status_index(status)].1;
    let size = ICON_SIZE;
    let ss = 4u32; // 每个方向的超采样数
    let margin = 1.0f32; // 四周留 1px 透明边，避免贴边削平
    let scale = (size as f32 - margin * 2.0) / 24.0;

    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            for sy in 0..ss {
                for sx in 0..ss {
                    let dx = px as f32 + (sx as f32 + 0.5) / ss as f32;
                    let dy = py as f32 + (sy as f32 + 0.5) / ss as f32;
                    let gx = (dx - margin) / scale;
                    let gy = (dy - margin) / scale;
                    if point_in_poly(gx, gy, &crate::icons::BOLT24) {
                        hits += 1;
                    }
                }
            }
            let alpha = (hits * 255 / (ss * ss)) as u8;
            if alpha == 0 {
                continue;
            }
            let i = ((py * size + px) * 4) as usize;
            rgba[i] = color[0];
            rgba[i + 1] = color[1];
            rgba[i + 2] = color[2];
            rgba[i + 3] = alpha;
        }
    }
    rgba
}

// ──────────────────────── Windows 实现 ────────────────────────

#[cfg(windows)]
mod win {
    use super::{ICON_SIZE, TrayCmd, TrayEvent, TrayStatus, icon_rgba, status_index};
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Receiver;
    use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_RESPECT_QUIET_TIME, NIIF_USER, NIM_ADD,
        NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
        DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, HICON, ICON_BIG,
        ICON_SMALL, ICONINFO, IDC_ARROW, IsIconic, LoadCursorW, MF_STRING, MSG,
        MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage,
        QS_ALLINPUT, RegisterClassExW, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
        TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_COMMAND,
        WM_CONTEXTMENU, WM_CREATE, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_QUIT,
        WM_RBUTTONUP, WM_SETICON, WNDCLASSEXW, WS_OVERLAPPED,
    };

    /// 线程内共享的托盘状态（窗口过程通过 TLS 访问）。
    pub(super) struct TrayState {
        pub events: std::sync::mpsc::Sender<TrayEvent>,
        pub icons: [HICON; 3],
        pub status: TrayStatus,
        pub tip: String,
        pub added: bool,
        /// Shell_NotifyIcon 最近一次是否成功。
        pub icon_ok: Arc<AtomicBool>,
        /// 主窗口（egui 窗口）句柄：托盘动作直接控制它。
        pub main_wnd: Cell<HWND>,
    }

    thread_local! {
        static STATE: Cell<*mut TrayState> = const { Cell::new(std::ptr::null_mut()) };
    }

    fn set_state(ptr: *mut TrayState) {
        STATE.with(|c| c.set(ptr));
    }

    /// 取当前托盘状态（仅托盘线程调用）。
    fn with_state<R>(f: impl FnOnce(&mut TrayState) -> R) -> Option<R> {
        let raw = STATE.with(|c| c.get());
        if raw.is_null() {
            return None;
        }
        // SAFETY: 指针由本线程在消息循环前放入，且只在本线程使用。
        Some(unsafe { f(&mut *raw) })
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 托盘「显示主窗口」：直接 Win32 恢复主窗口并抢前台焦点。
    ///
    /// 不经过 egui 的 `ViewportCommand`：实测 eframe 0.32 单视口下，
    /// 主窗口被 `SW_HIDE` 隐藏后事件循环停止派发重绘，排队中的 viewport
    /// 命令永远等不到执行窗口。Win32 直调不依赖那条路径。
    unsafe fn show_main_window(st: &TrayState) {
        unsafe {
            let hwnd = st.main_wnd.get();
            if hwnd.is_null() {
                return;
            }
            let cmd = if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            ShowWindow(hwnd, cmd);
            SetForegroundWindow(hwnd);
        }
    }

    /// 用 RGBA 像素创建带透明通道的 HICON。
    unsafe fn make_icon(rgba: &[u8], size: u32) -> HICON {
        unsafe {
            // 32bpp 自顶向下的 DIB，像素格式 BGRA。
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: size as i32,
                    biHeight: -(size as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [Default::default(); 1],
            };
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let color = CreateDIBSection(
                std::ptr::null_mut(),
                &bmi,
                DIB_RGB_COLORS,
                &mut bits,
                std::ptr::null_mut(),
                0,
            );
            if color.is_null() || bits.is_null() {
                return std::ptr::null_mut();
            }
            // RGBA -> BGRA 写入 DIB 像素。
            let dst = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
            for (src, dst) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = src[3];
            }
            // AND 掩码全 0 = 不透明，透明完全由颜色位图的 alpha 决定。
            let mask = CreateBitmap(size as i32, size as i32, 1, 1, std::ptr::null());
            let info = ICONINFO {
                fIcon: 1,
                xHotspot: 0,
                yHotspot: 0,
                hbmMask: mask,
                hbmColor: color,
            };
            let icon = CreateIconIndirect(&info);
            DeleteObject(color);
            DeleteObject(mask);
            icon
        }
    }

    /// 把托盘图标改动推给 shell。`icon`/`tip` 决定这次更新哪些字段。
    unsafe fn notify(hwnd: HWND, st: &mut TrayState, icon: bool, tip: bool) {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        // 回调消息必须注册：缺了 NIF_MESSAGE，shell 不会把托盘上的鼠标事件
        // 发回窗口，单击恢复 / 右键菜单就全都不会触发。
        nid.uCallbackMessage = TRAY_CALLBACK;
        let mut flags = NIF_MESSAGE;
        if icon {
            flags |= NIF_ICON;
            nid.hIcon = st.icons[status_index(st.status)];
        }
        if tip {
            flags |= NIF_TIP;
            let tip = wide(&st.tip);
            let n = tip.len().min(nid.szTip.len());
            nid.szTip[..n].copy_from_slice(&tip[..n]);
        }
        nid.uFlags = flags;
        let msg = if st.added { NIM_MODIFY } else { NIM_ADD };
        // SAFETY: nid 字段均已按 uFlags 初始化。
        let ok = unsafe { Shell_NotifyIconW(msg, &nid) };
        st.icon_ok.store(ok != 0, Ordering::Relaxed);
        st.added = true;
    }

    unsafe fn delete_icon(hwnd: HWND, st: &mut TrayState) {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_UID;
        // SAFETY: nid 最小字段集已初始化。
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
        st.added = false;
    }

    /// 把主窗口的任务栏（ICON_BIG）与标题栏（ICON_SMALL）图标换成状态图标。
    ///
    /// HICON 由托盘状态持有、线程存活期间一直有效，异步 Post 即可。
    /// egui 的 `ViewportCommand::Icon` 底层只设置 ICON_SMALL，任务栏
    /// （用 ICON_BIG）不会跟着变，所以这里两个尺寸都换。
    unsafe fn set_main_window_icon(st: &TrayState, status: TrayStatus) {
        unsafe {
            let hwnd = st.main_wnd.get();
            if hwnd.is_null() {
                return;
            }
            let icon = st.icons[status_index(status)] as LPARAM;
            // SAFETY: hwnd 已绑定；icon 归托盘线程所有且随线程存活。
            PostMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, icon);
            PostMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, icon);
        }
    }

    /// 经托盘图标弹气泡通知：Win10+ 由系统渲染成 Toast 横幅。
    ///
    /// 用 NIIF_USER 挂自己的状态图标；通知归属显示为本应用（而不是
    /// notify-rust 默认借用的 PowerShell AUMID），且默认带系统提示音。
    unsafe fn show_balloon(hwnd: HWND, st: &TrayState, title: &str, body: &str) {
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = TRAY_UID;
            nid.uFlags = NIF_INFO;
            nid.hIcon = st.icons[status_index(st.status)];
            nid.dwInfoFlags = NIIF_USER | NIIF_RESPECT_QUIET_TIME;
            // szInfoTitle 64 / szInfo 256（含 NUL），超长截断防御。
            let t: Vec<u16> = title.encode_utf16().take(63).collect();
            let b: Vec<u16> = body.encode_utf16().take(255).collect();
            nid.szInfoTitle[..t.len()].copy_from_slice(&t);
            nid.szInfo[..b.len()].copy_from_slice(&b);
            // SAFETY: 按 uFlags=NIF_INFO 初始化完毕。
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// 窗口过程：只处理托盘回调和窗口销毁。
    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            match msg {
                WM_CREATE => {
                    with_state(|st| notify(hwnd, st, true, true));
                    0
                }
                TRAY_CALLBACK => {
                    match lparam as u32 {
                        // 左键单击 / 双击：恢复窗口。
                        WM_LBUTTONDBLCLK | WM_LBUTTONUP => {
                            with_state(|st| show_main_window(st));
                            0
                        }
                        // 右键：弹出菜单。
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            show_menu(hwnd);
                            0
                        }
                        _ => 0,
                    }
                }
                WM_COMMAND => {
                    with_state(|st| match wparam {
                        MENU_SHOW => {
                            show_main_window(st);
                        }
                        MENU_EXIT => {
                            // 同 show_menu：先唤醒窗口再发退出事件。
                            show_main_window(st);
                            let _ = st.events.send(TrayEvent::Exit);
                        }
                        _ => {}
                    });
                    0
                }
                WM_DESTROY => {
                    with_state(|st| delete_icon(hwnd, st));
                    PostQuitMessage(0);
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    /// 在光标处弹出托盘菜单（TPM_RETURNCMD，同步返回命令 id）。
    unsafe fn show_menu(hwnd: HWND) {
        unsafe {
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // 弹菜单前必须把前台窗口切到自己的窗口，否则菜单关不掉。
            SetForegroundWindow(hwnd);

            let menu = CreatePopupMenu();
            if !menu.is_null() {
                let show = wide("显示主界面");
                let exit = wide("退出");
                AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr());
                AppendMenuW(menu, MF_STRING, MENU_EXIT, exit.as_ptr());
                let cmd = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    pt.x,
                    pt.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                );
                DestroyMenu(menu);
                with_state(|st| match cmd as usize {
                    MENU_SHOW => {
                        show_main_window(st);
                    }
                    MENU_EXIT => {
                        // 先把主窗口拉起来：若窗口正隐藏着，egui 事件循环处于
                        // 停摆状态，不发这一下界面侧永远收不到 Exit 事件。
                        show_main_window(st);
                        let _ = st.events.send(TrayEvent::Exit);
                    }
                    _ => {}
                });
                // TrackPopupMenu 的已知坑：补一条 WM_NULL 让菜单把焦点还回来。
                PostMessageW(hwnd, WM_NULL, 0, 0);
            }
        }
    }

    /// 托盘线程主体：自建 message-only 窗口 + 消息循环。
    pub(super) fn tray_thread(
        initial_tip: String,
        cmds: Receiver<TrayCmd>,
        events: std::sync::mpsc::Sender<TrayEvent>,
        icon_ok: Arc<AtomicBool>,
    ) {
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let class = wide(CLASS_NAME);

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
                hIconSm: std::ptr::null_mut(),
            };
            let atom = RegisterClassExW(&wc);
            if atom == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                // 注册失败且不是「类已存在」：放弃托盘，不影响主程序。
                tracing::warn!(err = GetLastError(), "注册托盘窗口类失败");
                return;
            }

            let state = Box::into_raw(Box::new(TrayState {
                events,
                icons: [
                    make_icon(&icon_rgba(TrayStatus::Green), ICON_SIZE),
                    make_icon(&icon_rgba(TrayStatus::Yellow), ICON_SIZE),
                    make_icon(&icon_rgba(TrayStatus::Red), ICON_SIZE),
                ],
                status: TrayStatus::Yellow,
                tip: initial_tip,
                added: false,
                icon_ok,
                main_wnd: Cell::new(std::ptr::null_mut()),
            }));
            set_state(state);

            let title = wide("zcode 额度看板");
            // 隐藏顶层窗口：无父窗口、从不 ShowWindow，视觉上与 message-only 一样
            // 不可见，但可以被 SetForegroundWindow（右键菜单才能在点击别处时收起）。
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                tracing::warn!("创建托盘消息窗口失败，托盘不可用");
                if let Some(raw) = take_state() {
                    let st = &mut *raw;
                    for icon in st.icons {
                        if !icon.is_null() {
                            DestroyIcon(icon);
                        }
                    }
                    drop(Box::from_raw(raw));
                }
                return;
            }

            // 主循环：既处理窗口消息，也处理界面下发的命令。
            let mut quitting = false;
            let mut destroyed = false;
            let mut quit_deadline: Option<std::time::Instant> = None;
            loop {
                if !quitting {
                    while let Ok(cmd) = cmds.try_recv() {
                        match cmd {
                            TrayCmd::SetTip(tip) => {
                                with_state(|st| {
                                    st.tip = tip;
                                    notify(hwnd, st, false, true);
                                });
                            }
                            TrayCmd::SetStatus(status) => {
                                with_state(|st| {
                                    st.status = status;
                                    notify(hwnd, st, true, false);
                                });
                            }
                            TrayCmd::SetWindowIcon(status) => {
                                with_state(|st| set_main_window_icon(st, status));
                            }
                            TrayCmd::Balloon { title, body } => {
                                with_state(|st| show_balloon(hwnd, st, &title, &body));
                            }
                            TrayCmd::AttachMainWnd(main) => {
                                with_state(|st| {
                                    st.main_wnd.set(main as *mut _);
                                    // 绑定即同步一次任务栏图标，不等下次状态变化。
                                    set_main_window_icon(st, st.status);
                                });
                            }
                            TrayCmd::Shutdown => {
                                quitting = true;
                                break;
                            }
                        }
                    }
                }
                if quitting && !destroyed {
                    // 销毁窗口：WM_DESTROY 会删图标并发 PostQuitMessage，
                    // 随后在消息泵里收到 WM_QUIT，统一收尾。
                    DestroyWindow(hwnd);
                    destroyed = true;
                    quit_deadline =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(2));
                }

                // 等消息或超时（超时用于重新检查命令队列）。
                MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 200, QS_ALLINPUT, 0);

                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_QUIT {
                        // 收尾：删图标、销毁资源、退出线程。
                        if let Some(raw) = take_state() {
                            let st = &mut *raw;
                            for icon in st.icons {
                                if !icon.is_null() {
                                    DestroyIcon(icon);
                                }
                            }
                            drop(Box::from_raw(raw));
                        }
                        return;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                // WM_QUIT 理应紧随其后；2 秒还没到就强制收尾，避免挂死。
                if quit_deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break;
                }
            }

            // 异常兜底：确保资源一定释放。
            if let Some(raw) = take_state() {
                let st = &mut *raw;
                delete_icon(hwnd, st);
                for icon in st.icons {
                    if !icon.is_null() {
                        DestroyIcon(icon);
                    }
                }
                drop(Box::from_raw(raw));
            }
        }
    }

    /// 取出并清空 TLS 里的状态指针（仅托盘线程调用）。
    fn take_state() -> Option<*mut TrayState> {
        let raw = STATE.with(|c| c.replace(std::ptr::null_mut()));
        if raw.is_null() { None } else { Some(raw) }
    }

    /// 命令队列常量（与父模块共用，避免魔法数散落）。
    const MENU_SHOW: usize = 1001;
    const MENU_EXIT: usize = 1002;
    const TRAY_UID: u32 = 1;
    /// 托盘图标回调消息 id。
    const TRAY_CALLBACK: u32 = WM_APP + 1;
    const CLASS_NAME: &str = "zcode_board_tray_window";
    /// 注册类已存在的错误码。
    const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
}

#[cfg(windows)]
use win::tray_thread;

#[cfg(not(windows))]
fn tray_thread(
    _tip: String,
    cmds: Receiver<TrayCmd>,
    _events: Sender<TrayEvent>,
    _icon_ok: Arc<AtomicBool>,
) {
    // 非 Windows：没有托盘概念，阻塞等待命令通道关闭（Tray drop 时触发）。
    while cmds.recv().is_ok() {}
}

// ──────────────────────── 测试 ────────────────────────

#[cfg(all(windows, test))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId};

    /// 按类名找**本进程**的托盘隐藏顶层窗口。
    ///
    /// 不能只按类名 FindWindow：应用本体可能正在同一会话里运行，
    /// 它的托盘窗口类名相同，会误匹配到别人的窗口（表现为
    /// 「shutdown 后窗口应销毁」莫名失败）。
    fn find_tray_window() -> HWND {
        let class: Vec<u16> = "zcode_board_tray_window"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let me = std::process::id();
        let mut found: HWND = std::ptr::null_mut();
        // SAFETY: 类名以 NUL 结尾；逐个枚举顶层窗口并按 PID 过滤。
        unsafe {
            let mut hwnd = FindWindowExW(
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                class.as_ptr(),
                std::ptr::null(),
            );
            while !hwnd.is_null() {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid == me {
                    found = hwnd;
                    break;
                }
                hwnd = FindWindowExW(std::ptr::null_mut(), hwnd, class.as_ptr(), std::ptr::null());
            }
        }
        found
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        cond()
    }

    #[test]
    fn tray_window_lifecycle() {
        let mut tray = Tray::spawn("zcode 额度看板");

        // 消息窗口应被创建（最多等 2 秒）。
        assert!(
            wait_until(|| !find_tray_window().is_null(), Duration::from_secs(2)),
            "托盘消息窗口应被创建"
        );

        // 图标应真的加到任务栏（Shell_NotifyIcon NIM_ADD 返回成功）。
        assert!(
            wait_until(|| tray.icon_ok(), Duration::from_secs(2)),
            "Shell_NotifyIcon NIM_ADD 应成功"
        );

        // 命令通道：更新提示与状态不应 panic。
        tray.set_tip("zcode 额度看板 · 5 个 key");
        tray.set_status(TrayStatus::Red);
        tray.set_status(TrayStatus::Green);
        assert_eq!(tray.drain_events().len(), 0, "无用户操作时不应有事件");

        // 关闭后窗口应销毁。
        tray.shutdown();
        assert!(
            wait_until(|| find_tray_window().is_null(), Duration::from_secs(2)),
            "shutdown 后托盘窗口应销毁"
        );
    }

    #[test]
    fn icon_pixels_bolt() {
        let rgba = icon_rgba(TrayStatus::Green);
        assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        // 中心点（闪电躯干内部）应点亮为绿色，角落应为透明；
        // 内部像素覆盖率为 100%，alpha 应是实打实的 255。
        let center = ((16 * ICON_SIZE + 16) * 4) as usize;
        assert_eq!(rgba[center + 3], 255, "图标中心应不透明");
        assert_eq!(
            [rgba[center], rgba[center + 1], rgba[center + 2]],
            [76, 209, 138],
            "中心应为 Green 档的绿色"
        );
        assert_eq!(rgba[3], 0, "角落应透明");
        assert_eq!(icon_rgba(TrayStatus::Red)[center + 1], 87, "Red 图标为红色");
        // 抗锯齿：边缘上应存在 0 < alpha < 255 的过渡像素。
        let has_soft_edge = rgba.chunks_exact(4).any(|px| px[3] > 0 && px[3] < 255);
        assert!(has_soft_edge, "超采样光栅化应产生平滑的边缘过渡");
    }
}
