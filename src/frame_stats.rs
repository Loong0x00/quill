//! 帧率统计采集点。
//!
//! Phase 6 的 soak test 需要一个长期稳定的信号来观察帧卡顿与 RSS 漂移 —— 这里
//! 只埋采集点:每满一个 [`FRAME_WINDOW`] 就通过 `tracing::info!`(target =
//! `quill::frame`)打一行结构化日志,不聚合、不导出、不报警。
//!
//! 时间源用 [`std::time::Instant`],Linux 下走 `CLOCK_MONOTONIC`,
//! 不受系统时间跳变影响。

use std::time::{Duration, Instant};

use tracing::info;

/// 单次聚合窗口的帧数。Phase 1 硬编码 60,改动须过 ADR。
pub const FRAME_WINDOW: usize = 60;

/// 一个窗口填满后的聚合结果。
///
/// 业务模块可以直接拿去做断言或手动格式化,结构上不绑死 `tracing` 输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    /// 本窗口累计的帧间隔样本数。恒等于 [`FRAME_WINDOW`]。
    pub frames: usize,
    /// 窗口首尾之间的总耗时,也是所有样本之和。
    pub elapsed: Duration,
    pub avg: Duration,
    pub min: Duration,
    pub max: Duration,
}

/// 帧率采集器。非线程安全,由渲染循环单线程持有。
#[derive(Debug)]
pub struct FrameStats {
    last_present: Option<Instant>,
    intervals: Vec<Duration>,
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameStats {
    pub fn new() -> Self {
        Self {
            last_present: None,
            intervals: Vec::with_capacity(FRAME_WINDOW),
        }
    }

    /// 记录一次 frame present,返回窗口填满时的 [`Snapshot`]。
    ///
    /// 首次调用只建立基线时间点,不产生间隔样本,故返回 `None`。之后每次
    /// 调用产生一个 `now - last` 间隔;累计到 [`FRAME_WINDOW`] 时聚合、
    /// 清零 interval buffer、把 `last_present` 留作下一窗口的基线,
    /// 返回 `Some(snapshot)`。
    ///
    /// 窗口不满时调用方**不应该**主动 flush(ticket T-0106 明确留白),
    /// 这样能保证 soak 日志每行 `frames` 字段恒为 [`FRAME_WINDOW`]。
    pub fn record_present(&mut self, now: Instant) -> Option<Snapshot> {
        let prev = match self.last_present {
            Some(prev) => prev,
            None => {
                self.last_present = Some(now);
                return None;
            }
        };
        self.last_present = Some(now);
        let dt = now.saturating_duration_since(prev);
        self.intervals.push(dt);
        if self.intervals.len() >= FRAME_WINDOW {
            let snap = aggregate(&self.intervals);
            self.intervals.clear();
            Some(snap)
        } else {
            None
        }
    }

    /// 渲染循环的便捷入口:记录帧 + 窗口满时自动走 `tracing::info!`。
    ///
    /// target 固定为 `quill::frame`,Phase 6 soak 可按 target 过滤。
    /// 字段均为 f64 毫秒,便于外部 `awk` / `jq` 解析;窗口未满时什么都不发。
    pub fn record_and_log(&mut self, now: Instant) {
        if let Some(snap) = self.record_present(now) {
            info!(
                target: "quill::frame",
                frames = snap.frames,
                elapsed_ms = duration_ms(snap.elapsed),
                avg_ms = duration_ms(snap.avg),
                min_ms = duration_ms(snap.min),
                max_ms = duration_ms(snap.max),
                "frame stats"
            );
        }
    }
}

fn aggregate(intervals: &[Duration]) -> Snapshot {
    // 先累加再除以 N,避免"逐次加平均"的精度塌陷。
    let sum: Duration = intervals.iter().copied().sum();
    let min = intervals.iter().copied().min().unwrap_or_default();
    let max = intervals.iter().copied().max().unwrap_or_default();
    let len = intervals.len().max(1) as u32;
    let avg = sum / len;
    Snapshot {
        frames: intervals.len(),
        elapsed: sum,
        avg,
        min,
        max,
    }
}

fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats_from_deltas(deltas: &[Duration]) -> Option<Snapshot> {
        let mut stats = FrameStats::new();
        let mut t = Instant::now();
        assert!(stats.record_present(t).is_none(), "首次调用只建基线");
        let mut out = None;
        for dt in deltas {
            t += *dt;
            out = stats.record_present(t);
        }
        out
    }

    #[test]
    fn baseline_then_uniform_window_fills_once() {
        // 60 个等长间隔 -> frames=60,min/avg/max 全等于单步长,elapsed = 60*step
        let step = Duration::from_micros(16_667); // ~16.667ms,模拟 60Hz
        let deltas = vec![step; FRAME_WINDOW];

        let snap = make_stats_from_deltas(&deltas).expect("满窗口应产出 snapshot");
        assert_eq!(snap.frames, FRAME_WINDOW);
        assert_eq!(snap.elapsed, step * FRAME_WINDOW as u32);
        assert_eq!(snap.min, step);
        assert_eq!(snap.max, step);
        assert_eq!(snap.avg, step);
    }

    #[test]
    fn varied_intervals_preserve_min_avg_max() {
        // 刻意混入最小值和最大值,检验聚合顺序无关。
        let mut deltas = vec![Duration::from_millis(16); FRAME_WINDOW];
        deltas[10] = Duration::from_micros(14_900); // min: 14.9ms
        deltas[40] = Duration::from_micros(22_100); // max: 22.1ms

        let snap = make_stats_from_deltas(&deltas).expect("满窗口应产出 snapshot");
        assert_eq!(snap.frames, FRAME_WINDOW);
        assert_eq!(snap.min, Duration::from_micros(14_900));
        assert_eq!(snap.max, Duration::from_micros(22_100));
        // elapsed 直接拿所有间隔累加验证(避免 avg 浮点比较)。
        let expected_sum: Duration = deltas.iter().copied().sum();
        assert_eq!(snap.elapsed, expected_sum);
        assert_eq!(snap.avg, expected_sum / FRAME_WINDOW as u32);
    }

    #[test]
    fn partial_window_returns_none_and_does_not_flush() {
        let mut stats = FrameStats::new();
        let mut t = Instant::now();
        stats.record_present(t);
        // 窗口只填到 FRAME_WINDOW - 1,不应 flush。
        for _ in 0..(FRAME_WINDOW - 1) {
            t += Duration::from_millis(16);
            assert!(stats.record_present(t).is_none(), "未满不应产出 snapshot",);
        }
    }

    #[test]
    fn second_window_starts_fresh_after_snapshot() {
        let step1 = Duration::from_millis(16);
        let step2 = Duration::from_millis(20);

        let mut stats = FrameStats::new();
        let mut t = Instant::now();
        stats.record_present(t);

        let mut first = None;
        for _ in 0..FRAME_WINDOW {
            t += step1;
            first = stats.record_present(t);
        }
        let first = first.expect("第一个窗口应产出 snapshot");
        assert_eq!(first.avg, step1);

        // 第二个窗口应只由 step2 间隔组成,不受第一个窗口的 step1 污染。
        let mut second = None;
        for _ in 0..FRAME_WINDOW {
            t += step2;
            second = stats.record_present(t);
        }
        let second = second.expect("第二个窗口应产出 snapshot");
        assert_eq!(second.frames, FRAME_WINDOW);
        assert_eq!(second.min, step2);
        assert_eq!(second.max, step2);
        assert_eq!(second.avg, step2);
    }

    #[test]
    fn duration_ms_is_fractional_milliseconds() {
        assert!((duration_ms(Duration::from_micros(16_667)) - 16.667).abs() < 1e-6);
        assert_eq!(duration_ms(Duration::from_millis(1)), 1.0);
    }
}
