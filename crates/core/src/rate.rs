//! 消耗速率的纯计算：采样序列 → 速率曲线 / 当前速率。
//!
//! 速率定义为相邻两次探测之间 的用量差分。
//! 三条硬规则：
//! - **窗口重置会产生负差分**（已用量回落），那不是「负速率」，
//!   差分链在重置处断开、不跨重置计算；
//! - **时间差过小的样本对**（< 5 秒）量化噪声占主导，跳过；
//! - **时间差过大的样本对**（> 30 分钟，典型是程序关了一段又打开）
//!   算出来是「含离线空闲的平均」，不是瞬时速率，同样跳过。

use zcode_model::{RatePoint, RateSample};

/// 每个序列保留的采样上限（默认 60s 刷新 ≈ 16 小时，15m 刷新 ≈ 10 天，
/// 再久的趋势对「看当前速率变化」没有增量价值）。
pub const MAX_SAMPLES_PER_SERIES: usize = 1000;

/// 采样最长保留 7 天，超期清理。
pub const RETAIN_MS: i64 = 7 * 24 * 3_600_000;

/// 可分辨的最小采样间隔：低于它的差分按噪声丢弃。
const MIN_DT_MS: i64 = 5_000;

/// 差分可跨越的最大间隔：超过视为「离线间隙」，不是连续使用。
pub const MAX_DT_MS: i64 = 30 * 60_000;

/// 记录一次采样并顺手裁剪（超期 + 超量，从最旧的一端丢）。
pub fn record(series: &mut Vec<RateSample>, t_ms: i64, used: f64, now_ms: i64) {
    // 相同时间戳 / 时间倒退的结果（理论上不会发生）直接忽略，避免零间隔噪声。
    if series.last().is_some_and(|s| s.t_ms >= t_ms) {
        return;
    }
    series.push(RateSample { t_ms, used });
    prune(series, now_ms);
}

/// 裁剪到「7 天内 + 上限条数」。调用频率与采样频率相同，开销可忽略。
pub fn prune(series: &mut Vec<RateSample>, now_ms: i64) {
    let floor = now_ms - RETAIN_MS;
    let stale = series.iter().take_while(|s| s.t_ms < floor).count();
    if stale > 0 {
        series.drain(0..stale);
    }
    let overflow = series.len().saturating_sub(MAX_SAMPLES_PER_SERIES);
    if overflow > 0 {
        series.drain(0..overflow);
    }
}

/// 把采样序列转成速率曲线（时间升序）。
///
/// 每个点代表「截至 t_i 的一个采样区间」的速率；重置（用量回落）与
/// 离线间隙处断链跳过，间隙后的下一对采样重新起算。
pub fn compute_points(samples: &[RateSample]) -> Vec<RatePoint> {
    let mut points = Vec::with_capacity(samples.len().saturating_sub(1));
    for pair in samples.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dt = b.t_ms - a.t_ms;
        if !(MIN_DT_MS..=MAX_DT_MS).contains(&dt) || b.used < a.used {
            continue; // 噪声对 / 离线间隙 / 跨重置
        }
        let per_min = (b.used - a.used) * 60_000.0 / dt as f64;
        points.push(RatePoint {
            t_ms: b.t_ms,
            per_min,
        });
    }
    points
}

/// 当前速率：以最近一次采样为锚向前回溯，**不跨窗口重置**。
///
/// 回溯上限 `max_span_ms`（调用方按刷新间隔放大，长间隔刷新也能算出），
/// 起算下限 `min_span_ms`（跨度太短的差分只是量化台阶，宁缺毋滥）。
/// 返回 `(每分钟用量, 实际统计跨度)`。
pub fn current_rate(
    samples: &[RateSample],
    min_span_ms: i64,
    max_span_ms: i64,
) -> Option<(f64, i64)> {
    let last = *samples.last()?;
    let mut base = last;
    for s in samples.iter().rev().skip(1) {
        if last.t_ms - s.t_ms > max_span_ms || last.t_ms - s.t_ms > MAX_DT_MS {
            break; // 回溯到头 / 撞上离线间隙，用已达跨度
        }
        if s.used > base.used {
            break; // 更早的采样用量反而更大：中间发生过重置，链到此为止
        }
        base = *s;
        if last.t_ms - base.t_ms >= min_span_ms {
            break; // 跨度够了，再多回溯只会稀释「瞬时」
        }
    }
    let span = last.t_ms - base.t_ms;
    if span < min_span_ms {
        return None; // 数据还不够铺满最小统计跨度
    }
    Some(((last.used - base.used) * 60_000.0 / span as f64, span))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    fn sample(t_min: i64, used: f64) -> RateSample {
        RateSample {
            t_ms: t_min * MIN,
            used,
        }
    }

    #[test]
    fn points_basic_slope() {
        // 每 2 分钟涨 600 → 300/min 恒定。
        let s = vec![sample(0, 0.0), sample(2, 600.0), sample(4, 1200.0)];
        let pts = compute_points(&s);
        assert_eq!(pts.len(), 2);
        assert!((pts[0].per_min - 300.0).abs() < 1e-9);
        assert!((pts[1].per_min - 300.0).abs() < 1e-9);
    }

    #[test]
    fn points_flat_is_zero_rate() {
        let s = vec![sample(0, 100.0), sample(5, 100.0)];
        let pts = compute_points(&s);
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].per_min, 0.0);
    }

    #[test]
    fn points_reset_breaks_chain() {
        // 中间重置（1200 → 50）：跨重置的那一对被跳过，重置后的对正常计算。
        let s = vec![
            sample(0, 0.0),
            sample(2, 1200.0),
            sample(4, 50.0),
            sample(6, 350.0),
        ];
        let pts = compute_points(&s);
        assert_eq!(pts.len(), 2);
        assert!((pts[0].per_min - 600.0).abs() < 1e-9, "重置前 600/min");
        assert!((pts[1].per_min - 150.0).abs() < 1e-9, "重置后 150/min");
    }

    #[test]
    fn points_skip_tiny_dt() {
        // 3 秒间隔的一对：噪声，跳过。
        let s = vec![
            RateSample { t_ms: 0, used: 0.0 },
            RateSample {
                t_ms: 3_000,
                used: 1e9,
            },
            RateSample {
                t_ms: 63_000,
                used: 2e9,
            },
        ];
        let pts = compute_points(&s);
        assert_eq!(pts.len(), 1); // 只剩 0→63s 的一对
        assert!(pts[0].per_min > 0.0);
    }

    #[test]
    fn points_skip_offline_gap() {
        // 相邻采样间隔 45 分钟（> 30 分钟上限）：视为离线间隙，不产速率点；
        // 间隙两侧各自与更近的邻居成对。
        let s = vec![
            sample(0, 0.0),
            sample(1, 100.0),
            sample(46, 600.0),
            sample(47, 900.0),
        ];
        let pts = compute_points(&s);
        assert_eq!(pts.len(), 2, "跨间隙的 (1,46) 一对应被跳过");
        assert!((pts[0].per_min - 100.0).abs() < 1e-9);
        assert!((pts[1].per_min - 300.0).abs() < 1e-9);
    }

    #[test]
    fn current_uses_trailing_window() {
        let s = vec![
            sample(0, 0.0),
            sample(2, 600.0),
            sample(4, 1200.0),
            sample(6, 1500.0),
        ];
        // 跨度足够时只回溯到 min_span（1 分钟），速率贴近最近区间：
        // 最近 2 分钟涨了 300 → 平均 150/min。
        let (rate, span) = current_rate(&s, MIN, 30 * MIN).unwrap();
        assert!((rate - 150.0).abs() < 1e-9);
        assert_eq!(span, 2 * MIN);
    }

    #[test]
    fn current_widens_when_data_sparse() {
        // 采样间隔 10 分钟 > min_span：回溯到上一条，用完整区间算平均。
        let s = vec![sample(0, 0.0), sample(10, 3000.0)];
        let (rate, span) = current_rate(&s, MIN, 30 * MIN).unwrap();
        assert!((rate - 300.0).abs() < 1e-9);
        assert_eq!(span, 10 * MIN);
    }

    #[test]
    fn current_stops_at_reset() {
        // 60 分钟前用量更高（中途重置过）：链在重置处断，
        // 只用重置之后的 2 个样本。
        let s = vec![sample(0, 9000.0), sample(30, 100.0), sample(32, 700.0)];
        let (rate, span) = current_rate(&s, MIN, 60 * MIN).unwrap();
        assert!((rate - 300.0).abs() < 1e-9);
        assert_eq!(span, 2 * MIN);
    }

    #[test]
    fn current_respects_max_span() {
        // 间隔 20 分钟、回溯上限 15 分钟：上一条超出上限，跨度不足 → None。
        let s = vec![sample(0, 0.0), sample(20, 2000.0)];
        assert!(current_rate(&s, MIN, 15 * MIN).is_none());
        // 放宽上限后即可计算（20 分钟仍低于离线间隙阈值 30 分钟）。
        let (rate, _) = current_rate(&s, MIN, 30 * MIN).unwrap();
        assert!((rate - 100.0).abs() < 1e-9);
    }

    #[test]
    fn current_skips_offline_gap() {
        // 上一条采样在 50 分钟前（程序关过一段）：不是连续使用，算不出瞬时速率。
        let s = vec![sample(0, 0.0), sample(50, 5000.0)];
        assert!(current_rate(&s, MIN, 60 * MIN).is_none());
    }

    #[test]
    fn current_needs_min_span() {
        // 只有 30 秒跨度：不足以铺满 1 分钟的最小统计跨度。
        let s = vec![
            RateSample { t_ms: 0, used: 0.0 },
            RateSample {
                t_ms: 30_000,
                used: 100.0,
            },
        ];
        assert!(current_rate(&s, MIN, 30 * MIN).is_none());
        assert!(current_rate(&[], MIN, 30 * MIN).is_none());
    }

    #[test]
    fn record_prunes_age_and_count() {
        let now = 100 * 24 * 3_600_000;
        let mut series = Vec::new();
        // 8 天前的一条：超期，记录新样本时被清掉。
        series.push(RateSample {
            t_ms: now - 8 * 24 * 3_600_000,
            used: 1.0,
        });
        record(&mut series, now, 2.0, now);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].t_ms, now);

        // 撑到上限：最旧的被挤掉（时间轴取真实量级，避免超龄裁剪误伤）。
        let mut full: Vec<RateSample> = (0..=MAX_SAMPLES_PER_SERIES)
            .map(|i| RateSample {
                t_ms: i as i64 * MIN,
                used: i as f64,
            })
            .collect();
        let last_t = (MAX_SAMPLES_PER_SERIES + 1) as i64 * MIN;
        record(&mut full, last_t, 0.0, last_t);
        assert_eq!(full.len(), MAX_SAMPLES_PER_SERIES);
        assert_eq!(full[0].t_ms, 2 * MIN, "最旧的两条被挤掉");
    }

    #[test]
    fn record_rejects_out_of_order() {
        let mut series = Vec::new();
        record(&mut series, 10_000, 1.0, 10_000);
        record(&mut series, 9_000, 2.0, 10_000); // 时间倒退：忽略
        record(&mut series, 10_000, 3.0, 10_000); // 时间相同：忽略
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].used, 1.0);
    }
}
