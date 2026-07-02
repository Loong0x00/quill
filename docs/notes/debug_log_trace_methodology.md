<!-- migrated from claude memory: debug_log_trace_over_inference_2026-04-26.md on 2026-05-11 -->
---
name: Debug log trace 比 Lead 推理快 10x (Wayland 视觉 bug)
description: Wayland / GPU / 协议层 bug 优先 RUST_LOG=debug + grep 时序分析定位真因, 比静态读代码推理快 10 倍. T-0611 拖文件 v1+v2 真因 (set_actions 漏 + Drop+Leave race) 都是 log 直接给的, 静态推理猜不到
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**经验**: Wayland / GPU / 异步协议层 bug 反复出现"看代码 OK 但实际不
work", 不要靠 Lead 静态推理读代码 — 直接让用户跑 RUST_LOG=debug 给 log,
grep 时序分析 5 分钟定位真因.

**实证 (quill T-0611 拖文件, 2026-04-26)**:

T-0611 静态读代码 4 门绿 + 31 单测 + reviewer PASS. user 实测拖文件不 work.

**v1 真因 (log 直接给)**:
```
DnD enter accepted=Some("text/uri-list")
DnD leave  ← 应该是 Drop, 实际只 Leave
```
log 时序立刻看出: enter + accept 后**没 Drop event**, 协议层有问题.
Lead 知道 wayland-client 协议 → 推到 set_actions 漏调.

**v2 真因 (log 直接给)**:
```
07:34:40.003249  DnD drop event (pending_drop set)
07:34:40.003261  DnD leave              ← 12 微秒后 Leave
07:34:40.003273  DnD drop 但未 accept 任何 mime, 跳过
```
12 微秒级时序差是 Drop → Leave race, Leave handler 清状态太早.
**这种微秒级时序问题静态读代码绝对推不出, log 直接 trivial**.

**Why log 比推理快**:
1. 协议异步事件**真实时序** vs 推理"应该是这个序" 经常差几 micros
2. 多源事件交织 (mutter + sctk + calloop + wayland-client) 静态读代码没
   办法"在脑子里跑事件循环"
3. 用户实操真实 modifier / 鼠标位置 / 时序 vs Lead 假设的"理想 case",
   差距大
4. log 自带可重现 — 同样问题再 grep 一次马上知道是不是同因 vs 推理"上次
   是 X 这次估计也是 X" 概率假设

**SOP**:
1. 用户报"功能不 work" 但代码看着 OK → 不要先猜
2. 立刻让用户跑 `RUST_LOG=quill=debug cargo run --release 2>&1 | tee /tmp/debug.log`
3. 用户实操触发 bug (拖文件 / 按键 / 点击)
4. user 把 log 关键段贴回 (用 `grep -iE "<keyword>"` 过滤)
5. Lead 时序分析 → 定位真因 → 5 分钟修

**反例**: 没 log 之前我猜 T-0611 v1 真因可能是:
- mime 不匹配 (错的)
- Nautilus 没 offer text/uri-list (错的)
- pipe2 失败 (错的)
- bracketed paste wrap 问题 (错的)

直到看 log "DnD enter ... DnD leave 中间无 drop" 立刻确认协议层缺 set_actions.

**怎么记忆**: 任何 quill / Wayland / GPU / 协议层 bug, **let user run with
debug log first, don't think about root cause until log arrives**. Lead 静
态推理是次选, log 时序分析是首选.

跨项目复用: 任何异步事件驱动系统 (network protocol / GPU pipeline /
async runtime / actor model) 调试都适用. log + 时序 > 推理 + 假设。
