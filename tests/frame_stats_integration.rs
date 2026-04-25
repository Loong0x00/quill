//! T-0399 P1-1: `FrameStats::record_and_log` 真接 tracing 子系统的集成测试。
//!
//! 验证 mainline-audit P1-1 修复闭环 —— `record_and_log` 在窗口填满时通过
//! `tracing::info!(target = "quill::frame", ...)` 真打一行结构化日志, Phase 6
//! soak 才能按 target=`quill::frame` 过滤聚合。
//!
//! 不真启 wayland / wgpu —— 直接喂合成 `Instant` 走 [`FrameStats::record_and_log`],
//! 用 `tracing_subscriber::fmt` + 自定义 `MakeWriter` 把所有 trace 重定向到内存
//! buffer, 跑完后字符串搜 `quill::frame` 验真打了。
//!
//! 配套 `tests/frame_stats_smoke.rs` (T-0106 验 record_present 内部逻辑) —— 本
//! 测试验"内部 → tracing 字符串"那截桥。

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quill::frame_stats::{FrameStats, FRAME_WINDOW};
use tracing::subscriber::with_default;
use tracing_subscriber::fmt::MakeWriter;

/// 共享 bytes buffer + Make/Writer 实现, 让 fmt subscriber 把所有 trace 写到这里。
#[derive(Clone, Default)]
struct CapturedWriter {
    inner: Arc<Mutex<Vec<u8>>>,
}

impl CapturedWriter {
    fn new() -> Self {
        Self::default()
    }

    fn dump(&self) -> String {
        let bytes = self.inner.lock().expect("CapturedWriter mutex").clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl io::Write for CapturedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner
            .lock()
            .expect("CapturedWriter mutex")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturedWriter {
    type Writer = CapturedWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 满 `FRAME_WINDOW` 帧 → tracing 子系统应能在 buffer 里看到一行带
/// `quill::frame` target 的 info 行。
#[test]
fn record_and_log_emits_quill_frame_target_after_full_window() {
    let writer = CapturedWriter::new();
    let writer_for_subscriber = writer.clone();

    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer_for_subscriber)
        .with_target(true)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();

    with_default(subscriber, || {
        let mut stats = FrameStats::new();
        let mut t = Instant::now();
        let step = Duration::from_micros(16_667); // ~60Hz

        // 首次调用建立基线, 不计入 interval。
        stats.record_and_log(t);

        // 喂 FRAME_WINDOW 个 interval 让窗口填满 — 第 FRAME_WINDOW 次会
        // 触发 record_present 内部 aggregate + tracing::info!。
        for _ in 0..FRAME_WINDOW {
            t += step;
            stats.record_and_log(t);
        }
    });

    let captured = writer.dump();
    assert!(
        captured.contains("quill::frame"),
        "FrameStats::record_and_log 应在窗口满时打 target=quill::frame, 实际 captured:\n{captured}"
    );
    assert!(
        captured.contains("frame stats"),
        "tracing event message 应是 'frame stats', captured:\n{captured}"
    );
    assert!(
        captured.contains(&format!("frames={}", FRAME_WINDOW)),
        "tracing 字段应含 frames=60, captured:\n{captured}"
    );
}

/// 反向验: 窗口未满 (只喂 FRAME_WINDOW - 1 个 interval) 不应打 target=quill::frame。
/// 锁住 `record_present` 在 partial window 返 None → record_and_log 不调
/// `tracing::info!` 这条行为 (与 src/frame_stats.rs::tests::partial_window_returns_none
/// 是同一不变式的 tracing 桥层验证)。
#[test]
fn record_and_log_silent_on_partial_window() {
    let writer = CapturedWriter::new();
    let writer_for_subscriber = writer.clone();

    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer_for_subscriber)
        .with_target(true)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();

    with_default(subscriber, || {
        let mut stats = FrameStats::new();
        let mut t = Instant::now();
        let step = Duration::from_micros(16_667);

        stats.record_and_log(t); // 基线
        for _ in 0..(FRAME_WINDOW - 1) {
            t += step;
            stats.record_and_log(t);
        }
    });

    let captured = writer.dump();
    assert!(
        !captured.contains("quill::frame"),
        "窗口未满不应打 target=quill::frame, 实际 captured:\n{captured}"
    );
}
