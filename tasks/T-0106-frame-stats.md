# T-0106 帧率日志

**Phase**: 1
**Assigned**: impl-t0106
**Status**: claimed
**Budget**: tokenBudget=100k, walltime=3600s, cost=$5
**Dependencies**: T-0105

## Goal

在终端里跑 `RUST_LOG=info cargo run` 之后,每渲染 60 帧,日志就多一行,内容类似 `frame stats: frames=60 elapsed_ms=1005 avg_ms=16.75 min_ms=14.9 max_ms=22.1`。数值可以和上面略有差异,关键是"每 60 帧一行、内容包含 frame 数 / 经过毫秒数 / 平均每帧毫秒数 / 最小最大"。日志走 `tracing`,不是 `println!`。

这行日志的用途是 Phase 6 soak test 拿来看有没有帧突然卡住、RSS 稳定性、等等。Phase 1 先把采集点埋好。

## Scope

- In:
  - 修改渲染模块:每次 frame 被 present 后记录一次当前时间(`std::time::Instant::now()`)
  - 新增(或放在同一模块下)一个 `FrameStats` 结构:滚动窗口记录最近 60 帧耗时,满了就 `tracing::info!` 一次再清零
  - 日志字段用 `tracing` 的结构化字段(`%frames`、`?elapsed_ms`),不是 `format!`
- Out:
  - 任何聚合到文件 / Prometheus 的导出
  - 命令行参数调整窗口长度(先硬编码 60)
  - 帧耗时异常报警(Phase 6 soak 再加)
  - CPU 占用 / RSS 采样

## Acceptance

- [ ] `cargo build` 通过
- [ ] `cargo test` 通过
- [ ] `cargo clippy -- -D warnings` 通过
- [ ] `cargo fmt --check` 通过
- [ ] 审码 放行
- [ ] 手测:`RUST_LOG=info cargo run` 启动后每 1 秒左右(60 帧 @ 60Hz)出现一行 frame stats
- [ ] frame stats 行中 `frames` 字段恒为 60,`min_ms / avg_ms / max_ms` 字段为合理正数
- [ ] 关窗退出前若累计帧不足 60,不强制 flush(留白也行)
- [ ] 单元测试:`FrameStats` 纯逻辑有单测 —— 喂 60 个假时间点,验证聚合结果正确(只测结构体行为,不测 tracing 输出)

## Context

- `CLAUDE.md` — "禁止清单":不用 `println!`
- `CLAUDE.md` — "技术栈":日志锁 `tracing`
- `ROADMAP.md` Phase 1 `T-0106`,Phase 6 `T-0601` soak test 会复用这个点
- `tracing` docs:https://docs.rs/tracing
- `tracing-subscriber` docs:https://docs.rs/tracing-subscriber

## Implementation notes

- `Instant::now()` 在 Linux 走 `CLOCK_MONOTONIC`,不受系统时间调整影响,直接用就好
- 60 这个数字做成 `const FRAME_WINDOW: usize = 60;`,后续要调动只改常量
- 结构体里存 `Vec<Duration>` 或更轻的 `[Duration; 60]` 环形数组都行,60 帧而已,不用过度优化
- 聚合时先算 `sum` 再除,别先算平均再累加,保留精度
- `tracing::info!` 的 target 用 `"quill::frame"` 这类,便于 Phase 6 按 target 过滤
