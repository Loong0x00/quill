//! `FrameStats` 集成烟雾测试。
//!
//! 模拟 tick:用合成的 [`Instant`] 按固定步长前进,不依赖 wgpu 渲染。
//! 验证计数、聚合、跨窗口重置三条不变式 —— 这是 ticket T-0106 的核心验收点。

use std::time::{Duration, Instant};

use quill::frame_stats::{FrameStats, Snapshot, FRAME_WINDOW};

/// 把 `deltas` 喂给一个新建的 `FrameStats`(含首次基线),返回最后一次
/// `record_present` 的结果。测试里的标准夹具。
fn drive(deltas: &[Duration]) -> (FrameStats, Option<Snapshot>) {
    let mut stats = FrameStats::new();
    let mut t = Instant::now();
    stats.record_present(t);
    let mut last = None;
    for dt in deltas {
        t += *dt;
        last = stats.record_present(t);
    }
    (stats, last)
}

#[test]
fn fills_exactly_at_frame_window_boundary() {
    let step = Duration::from_millis(16);
    let spike = Duration::from_secs(10);

    // 前 FRAME_WINDOW - 1 个间隔不应触发 snapshot。
    let mut stats = FrameStats::new();
    let mut t = Instant::now();
    stats.record_present(t);
    for _ in 0..(FRAME_WINDOW - 1) {
        t += step;
        assert!(stats.record_present(t).is_none(), "差一帧不应 flush");
    }

    // 第 FRAME_WINDOW 个间隔刻意用大 spike,确保聚合结果包含它(最终帧
    // 必须进窗口,否则 max/elapsed 会漏掉)。
    t += spike;
    let snap = stats
        .record_present(t)
        .expect("第 FRAME_WINDOW 帧应触发 snapshot");
    assert_eq!(snap.frames, FRAME_WINDOW);
    assert_eq!(snap.min, step);
    assert_eq!(snap.max, spike);
    assert_eq!(snap.elapsed, step * (FRAME_WINDOW as u32 - 1) + spike);
}

#[test]
fn uniform_60hz_emits_snapshot_with_frames_60() {
    // 60Hz 标定间隔 ~16.667ms,喂完刚好一个窗口。
    let step = Duration::from_micros(16_667);
    let deltas = vec![step; FRAME_WINDOW];
    let (_stats, snap) = drive(&deltas);
    let snap = snap.expect("满窗口应产出 snapshot");
    assert_eq!(snap.frames, FRAME_WINDOW);
    assert_eq!(snap.min, step);
    assert_eq!(snap.max, step);
    assert_eq!(snap.avg, step);
    assert_eq!(snap.elapsed, step * FRAME_WINDOW as u32);
}

#[test]
fn cross_window_counts_stay_locked_at_frame_window() {
    // 连续驱动三个窗口,每个窗口的 frames 字段都必须恒为 FRAME_WINDOW。
    let step = Duration::from_millis(16);
    let mut stats = FrameStats::new();
    let mut t = Instant::now();
    stats.record_present(t);

    for window_idx in 0..3 {
        let mut snap = None;
        for _ in 0..FRAME_WINDOW {
            t += step;
            snap = stats.record_present(t);
        }
        let snap = snap.unwrap_or_else(|| panic!("第 {window_idx} 个窗口应 flush"));
        assert_eq!(
            snap.frames, FRAME_WINDOW,
            "frames 字段必须恒为 {FRAME_WINDOW}",
        );
    }
}
