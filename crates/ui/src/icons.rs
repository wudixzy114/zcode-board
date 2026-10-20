//! 矢量图标与界面配色。
//!
//! 图标按 Lucide/Feather 风格手工绘制：24×24 网格、2px 圆头描边，
//! 直接用 egui Painter 输出——不依赖字体或位图资源，暗色主题下
//! 每个图标线宽、风格完全一致（此前混用 emoji 在 Windows 上忽明忽暗、
//! 部分还是彩色 emoji，和整体风格割裂）。

use egui::{
    Color32, CornerRadius, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2,
};
use zcode_model::KeyStatus;

// ─────────────────────────── 配色（暗色） ───────────────────────────

pub const BG_APP: Color32 = Color32::from_rgb(17, 19, 24);
pub const BG_PANEL: Color32 = Color32::from_rgb(22, 25, 31);
pub const BG_CARD: Color32 = Color32::from_rgb(27, 30, 38);
pub const BG_CARD_ALT: Color32 = Color32::from_rgb(32, 36, 45);
pub const BAD_FILL: Color32 = Color32::from_rgb(44, 27, 30);
pub const WARN_FILL: Color32 = Color32::from_rgb(45, 39, 26);
pub const BORDER: Color32 = Color32::from_rgb(48, 53, 62);

pub const TEXT_HI: Color32 = Color32::from_rgb(232, 235, 241);
pub const TEXT_MID: Color32 = Color32::from_rgb(150, 157, 170);

pub const OK: Color32 = Color32::from_rgb(76, 209, 138);
pub const WARN: Color32 = Color32::from_rgb(235, 182, 74);
pub const BAD: Color32 = Color32::from_rgb(235, 87, 100);
pub const ACCENT: Color32 = Color32::from_rgb(96, 165, 250);
/// ACCENT 的悬停态（略亮）。
const ACCENT_HOVER: Color32 = Color32::from_rgb(120, 178, 255);
/// 主按钮（ACCENT 底色）上的深色文字。
const ON_ACCENT: Color32 = Color32::from_rgb(13, 17, 23);

// ─────────────────────────── 图标 ───────────────────────────

/// 闪电多边形（Feather zap），24×24 网格；托盘图标的软光栅化也用它。
pub const BOLT24: [(f32, f32); 6] = [
    (13.0, 2.0),
    (3.0, 14.0),
    (12.0, 14.0),
    (11.0, 22.0),
    (21.0, 10.0),
    (12.0, 10.0),
];

/// 图标目录。绘制时统一按 24×24 网格缩放到目标矩形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// 闪电（标题、品牌）。
    Bolt,
    /// 下载入托盘（从本机导入）。
    Import,
    /// 加号（添加 key）。
    Plus,
    /// 齿轮（设置）。
    Settings,
    /// 列表（事件日志）。
    Log,
    /// 循环箭头（刷新；刷新中带旋转动画）。
    Refresh,
    ChevronDown,
    ChevronRight,
    /// 铅笔（重命名）。
    Pencil,
    /// 垃圾桶（删除）。
    Trash,
    /// 铃铛（提醒开关）。
    Bell,
    /// 划线铃铛（已静音）。
    BellOff,
    /// 图钉（已固定到账户）。
    Pin,
    /// 对勾（确认）。
    Check,
    /// 两个交叠圆环（疑似同一账户）。
    Link,
    /// 时钟（刷新提醒）。
    Clock,
    /// 画笔（一键清理）。
    Brush,
    /// 脉冲折线（消耗速率）。
    Activity,
    /// 三角警告（预计用满的紧急警示）。
    Warning,
}

/// 24×24 设计网格边长。
const GRID: f32 = 24.0;

/// 在 `rect` 内绘制图标。`stroke_w` 是 24 网格下的线宽，`rotate` 为旋转弧度
/// （绕图标中心，用于「刷新中」的转动）。
pub fn draw(ui: &Ui, rect: Rect, icon: Icon, color: Color32, stroke_w: f32, rotate: f32) {
    debug_assert!(rect.width() > 0.0 && rect.height() > 0.0);
    let p = ui.painter();
    let s = Vec2::new(rect.width() / GRID, rect.height() / GRID);
    let o = rect.left_top();
    let map = |v: Vec2| -> Pos2 {
        let c = Vec2::splat(GRID / 2.0);
        let (sn, cs) = rotate.sin_cos();
        let r = v - c;
        let r = Vec2::new(r.x * cs - r.y * sn, r.x * sn + r.y * cs) + c;
        pos2(o.x + r.x * s.x, o.y + r.y * s.y)
    };
    let st = Stroke::new(stroke_w * (s.x + s.y) * 0.5, color);
    let line = |pts: &[Vec2]| {
        if pts.len() >= 2 {
            p.add(Shape::line(pts.iter().map(|v| map(*v)).collect(), st));
        }
    };
    let v = |x: f32, y: f32| Vec2::new(x, y);

    match icon {
        Icon::Bolt => {
            let mut pts: Vec<Vec2> = BOLT24.iter().map(|q| v(q.0, q.1)).collect();
            pts.push(pts[0]);
            line(&pts);
        }
        Icon::Import => {
            line(&[v(12.0, 4.0), v(12.0, 15.0)]);
            line(&[v(7.0, 10.0), v(12.0, 15.0), v(17.0, 10.0)]);
            line(&[v(4.0, 17.0), v(4.0, 20.0), v(20.0, 20.0), v(20.0, 17.0)]);
        }
        Icon::Plus => {
            line(&[v(12.0, 5.0), v(12.0, 19.0)]);
            line(&[v(5.0, 12.0), v(19.0, 12.0)]);
        }
        Icon::Settings => {
            // 齿轮：8 个梯形齿的闭折线 + 中心孔。
            let c = v(12.0, 12.0);
            let n = 8;
            let step = std::f32::consts::TAU / n as f32;
            let (r_root, r_tip) = (5.9, 8.3);
            let (a_tip, a_root) = (step * 0.155, step * 0.32);
            let mut pts = Vec::with_capacity(n * 4 + 1);
            for k in 0..n {
                let th = k as f32 * step - std::f32::consts::FRAC_PI_2;
                for (da, r) in [
                    (-a_root, r_root),
                    (-a_tip, r_tip),
                    (a_tip, r_tip),
                    (a_root, r_root),
                ] {
                    pts.push(c + v((th + da).cos(), (th + da).sin()) * r);
                }
            }
            pts.push(pts[0]);
            line(&pts);
            p.add(Shape::circle_stroke(map(c), 2.7 * s.x, st));
        }
        Icon::Log => {
            for y in [6.0, 12.0, 18.0] {
                p.add(Shape::circle_filled(map(v(5.0, y)), 0.9 * s.x, color));
                line(&[v(9.0, y), v(19.5, y)]);
            }
        }
        Icon::Refresh => {
            let c = v(12.0, 12.0);
            let r = 7.2;
            let (a0, a1) = ((-60.0f32).to_radians(), 200.0f32.to_radians());
            let steps = 22;
            let pts: Vec<Vec2> = (0..=steps)
                .map(|i| {
                    let a = a0 + (a1 - a0) * i as f32 / steps as f32;
                    c + v(a.cos(), a.sin()) * r
                })
                .collect();
            line(&pts);
            // 终端箭头（沿运动方向的切向）。
            let tip = *pts.last().unwrap();
            let t = v(-a1.sin(), a1.cos());
            let nv = v(-t.y, t.x);
            let size = 4.6;
            let w1 = tip - t * size + nv * size * 0.62;
            let w2 = tip - t * size - nv * size * 0.62;
            line(&[w1, tip]);
            line(&[w2, tip]);
        }
        Icon::ChevronDown => line(&[v(7.0, 9.5), v(12.0, 14.5), v(17.0, 9.5)]),
        Icon::ChevronRight => line(&[v(9.5, 7.0), v(14.5, 12.0), v(9.5, 17.0)]),
        Icon::Pencil => {
            // 沿 45° 轴摆放的铅笔：笔尖 + 收窄的鼻锥 + 笔身 + 橡皮分界线。
            let d = v(1.0, -1.0) / std::f32::consts::SQRT_2;
            let nv = v(1.0, 1.0) / std::f32::consts::SQRT_2;
            let tip = v(4.0, 20.0);
            let (hw, nib, len) = (1.9, 3.2, 11.0);
            let a = tip + d * nib + nv * (hw * 0.72);
            let b = tip + d * (nib + len) + nv * hw;
            let c = tip + d * (nib + len) - nv * hw;
            let e = tip + d * nib - nv * (hw * 0.72);
            line(&[tip, a, b, c, e, tip]);
            line(&[b - d * 2.6, c - d * 2.6]);
        }
        Icon::Trash => {
            line(&[v(4.0, 7.0), v(20.0, 7.0)]);
            line(&[v(9.0, 7.0), v(9.0, 4.0), v(15.0, 4.0), v(15.0, 7.0)]);
            line(&[v(6.5, 7.0), v(6.5, 20.0), v(17.5, 20.0), v(17.5, 7.0)]);
            line(&[v(10.2, 10.5), v(10.2, 16.5)]);
            line(&[v(13.8, 10.5), v(13.8, 16.5)]);
        }
        Icon::Bell | Icon::BellOff => {
            // 穹顶半圆 + 两侧外撇的裙边 + 铃锤。
            let c = v(12.0, 9.0);
            let r = 5.8;
            let steps = 14;
            let mut pts: Vec<Vec2> = (0..=steps)
                .map(|i| {
                    let a = std::f32::consts::PI + std::f32::consts::PI * i as f32 / steps as f32;
                    c + v(a.cos(), a.sin()) * r
                })
                .collect();
            pts.extend([
                v(19.3, 14.0),
                v(21.0, 17.2),
                v(3.0, 17.2),
                v(4.7, 14.0),
                pts[0],
            ]);
            line(&pts);
            let cc = v(12.0, 19.4);
            let clapper: Vec<Vec2> = [0.35f32, 0.7, 1.0, 1.4, 1.75, 2.1, 2.44, 2.79]
                .iter()
                .copied()
                .map(|a| cc + v(a.cos(), a.sin()) * 2.0)
                .collect();
            line(&clapper);
            if icon == Icon::BellOff {
                line(&[v(4.5, 4.0), v(19.5, 19.5)]);
            }
        }
        Icon::Pin => {
            line(&[
                v(9.0, 3.5),
                v(15.0, 3.5),
                v(13.0, 11.5),
                v(11.0, 11.5),
                v(9.0, 3.5),
            ]);
            line(&[v(8.6, 12.3), v(15.4, 12.3)]);
            line(&[v(12.0, 12.3), v(12.0, 21.0)]);
        }
        Icon::Check => line(&[v(5.0, 12.5), v(9.8, 17.3), v(19.0, 6.5)]),
        Icon::Link => {
            p.add(Shape::circle_stroke(map(v(9.2, 12.0)), 4.4 * s.x, st));
            p.add(Shape::circle_stroke(map(v(14.8, 12.0)), 4.4 * s.x, st));
        }
        Icon::Clock => {
            p.add(Shape::circle_stroke(map(v(12.0, 12.0)), 8.0 * s.x, st));
            line(&[v(12.0, 7.4), v(12.0, 12.0), v(15.4, 13.6)]);
        }
        Icon::Brush => {
            line(&[v(19.8, 4.2), v(11.2, 12.8)]);
            line(&[v(13.8, 15.4), v(16.4, 12.8)]);
            p.add(Shape::circle_stroke(map(v(7.6, 17.6)), 3.5 * s.x, st));
        }
        Icon::Activity => {
            // Lucide activity：心电脉冲，中间尖峰两侧缓坡。
            line(&[
                v(3.0, 12.0),
                v(7.0, 12.0),
                v(10.0, 5.0),
                v(14.0, 19.0),
                v(17.0, 12.0),
                v(21.0, 12.0),
            ]);
        }
        Icon::Warning => {
            // Lucide alert-triangle：闭合三角 + 竖线 + 圆点。
            line(&[v(12.0, 3.5), v(21.0, 19.0), v(3.0, 19.0), v(12.0, 3.5)]);
            line(&[v(12.0, 9.0), v(12.0, 14.0)]);
            p.add(Shape::circle_filled(map(v(12.0, 17.2)), 0.75 * s.x, color));
        }
    }
}

// ─────────────────────────── 控件 ───────────────────────────

/// 顶栏药丸按钮：图标 + 文字，圆角胶囊底。
///
/// `primary` 用强调色实底（当前只有「立即刷新」）；`spin` 让图标持续旋转
/// （刷新中）；`enabled = false` 时置灰且不可点。
pub fn pill_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    primary: bool,
    enabled: bool,
    spin: bool,
) -> Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let text_color = if primary && enabled {
        ON_ACCENT
    } else {
        TEXT_HI
    };
    let galley = ui
        .fonts(|f| f.layout_no_wrap(label.to_owned(), font, text_color))
        .clone();
    let h = 30.0;
    let icon_sz = 16.0;
    let pad = 12.0;
    let gap = 6.0;
    let w = pad + icon_sz + gap + galley.size().x + pad;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), sense);
    let hovered = enabled && resp.hovered();

    let (bg, border, fg) = if primary {
        if !enabled {
            (
                ACCENT.gamma_multiply(0.35),
                ACCENT.gamma_multiply(0.3),
                ON_ACCENT,
            )
        } else if hovered {
            (ACCENT_HOVER, ACCENT_HOVER, ON_ACCENT)
        } else {
            (ACCENT, ACCENT, ON_ACCENT)
        }
    } else if !enabled {
        (Color32::TRANSPARENT, BORDER, TEXT_MID)
    } else if hovered {
        (BG_CARD_ALT, BORDER, TEXT_HI)
    } else {
        (BG_CARD, BORDER, TEXT_HI)
    };

    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(9),
        bg,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );

    let angle = if spin {
        ui.ctx().request_repaint();
        (ui.input(|i| i.time) as f32 * 2.6) % std::f32::consts::TAU
    } else {
        0.0
    };
    let ir = Rect::from_min_size(
        pos2(rect.left() + pad, rect.center().y - icon_sz / 2.0),
        Vec2::splat(icon_sz),
    );
    draw(ui, ir, icon, fg, 2.0, angle);
    let tp = pos2(
        rect.left() + pad + icon_sz + gap,
        rect.center().y - galley.size().y / 2.0,
    );
    p.galley(tp, galley, fg);
    resp
}

/// 图标开关按钮（设置 / 日志 / 展开箭头）：方形，`active` 时高亮。
pub fn icon_toggle(ui: &mut Ui, icon: Icon, active: bool, tooltip: &str) -> Response {
    let size = 30.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let hovered = resp.hovered();
    let (bg, stroke, fg) = if active {
        (
            ACCENT.gamma_multiply(0.16),
            Stroke::new(1.0, ACCENT.gamma_multiply(0.45)),
            ACCENT,
        )
    } else if hovered {
        (
            Color32::from_white_alpha(10),
            Stroke::new(1.0, BORDER),
            TEXT_HI,
        )
    } else {
        (Color32::TRANSPARENT, Stroke::NONE, TEXT_MID)
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(8),
        bg,
        stroke,
        egui::StrokeKind::Middle,
    );
    draw(ui, rect.shrink(7.0), icon, fg, 2.0, 0.0);
    resp.on_hover_text(tooltip)
}

/// 小图标按钮（卡片头部 / key 行的操作）：透明底，悬停时浮现底色。
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str, icon_size: f32) -> Response {
    let btn = icon_size + 9.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(btn), Sense::click());
    let hovered = resp.hovered();
    let fg = if hovered { TEXT_HI } else { TEXT_MID };
    if hovered {
        ui.painter().rect(
            rect,
            CornerRadius::same(6),
            Color32::from_white_alpha(10),
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Middle,
        );
    }
    draw(
        ui,
        Rect::from_center_size(rect.center(), Vec2::splat(icon_size)),
        icon,
        fg,
        2.0,
        0.0,
    );
    resp.on_hover_text(tooltip)
}

/// 非交互的内联小图标（横幅、标题前的修饰），可挂 hover 提示。
pub fn glyph(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    draw(ui, rect, icon, color, 2.0, 0.0);
    resp
}

/// 状态徽章：小圆点/符号 + 文字的胶囊 chip（底色为状态色的淡透明）。
pub fn status_chip(ui: &mut Ui, status: KeyStatus) {
    let (label, color) = status_color(status);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let galley = ui
        .fonts(|f| f.layout_no_wrap(label.to_owned(), font, color))
        .clone();
    let h = 19.0;
    let dot = 10.0;
    let gap = 5.0;
    let pad = 8.0;
    let w = pad + dot + gap + galley.size().x + pad + 1.0;
    let (rect, _resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(9),
        color.gamma_multiply(0.12),
        Stroke::new(1.0, color.gamma_multiply(0.35)),
        egui::StrokeKind::Middle,
    );
    let dr = Rect::from_min_size(
        pos2(rect.left() + pad, rect.center().y - dot / 2.0),
        Vec2::splat(dot),
    );
    status_glyph(ui, dr, status, color);
    let tp = pos2(
        rect.left() + pad + dot + gap,
        rect.center().y - galley.size().y / 2.0,
    );
    p.galley(tp, galley, color);
}

/// 状态对应的文案与颜色。
pub fn status_color(status: KeyStatus) -> (&'static str, Color32) {
    match status {
        KeyStatus::Ok => ("正常", OK),
        KeyStatus::NoPlan => ("无 plan", TEXT_MID),
        KeyStatus::Idle => ("待探测", TEXT_MID),
        KeyStatus::Error => ("异常", BAD),
        KeyStatus::Cooling => ("冷却", WARN),
        KeyStatus::Disabled => ("已停用", TEXT_MID),
    }
}

/// 状态小符号：实心/空心圆、叉、旋转箭头、禁用斜杠。
pub fn status_glyph(ui: &Ui, rect: Rect, status: KeyStatus, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let r = rect.width() / 2.0;
    let st = Stroke::new(1.6, color);
    match status {
        KeyStatus::Ok => {
            p.add(Shape::circle_filled(c, r * 0.62, color));
        }
        KeyStatus::NoPlan | KeyStatus::Idle => {
            p.add(Shape::circle_stroke(c, r * 0.62, st));
        }
        KeyStatus::Error => {
            let d = r * 0.62;
            p.add(Shape::line_segment(
                [pos2(c.x - d, c.y - d), pos2(c.x + d, c.y + d)],
                st,
            ));
            p.add(Shape::line_segment(
                [pos2(c.x + d, c.y - d), pos2(c.x - d, c.y + d)],
                st,
            ));
        }
        KeyStatus::Cooling => draw(ui, rect, Icon::Refresh, color, 3.0, 0.0),
        KeyStatus::Disabled => {
            p.add(Shape::circle_stroke(c, r * 0.62, st));
            p.add(Shape::line_segment(
                [pos2(c.x - r, c.y + r), pos2(c.x + r, c.y - r)],
                st,
            ));
        }
    }
}
