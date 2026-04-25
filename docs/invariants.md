# quill 不变式登记册 (invariants)

> **本文件目的**:凡是"违反则 UB / 数据损坏 / 死锁 / 资源泄露"的硬约束,必须在此处登记。
>
> 不依赖 git 历史、不依赖 commit message、不依赖任何人"记得"。
> 新 agent 进项目必读。每次 merge 若涉及 unsafe / drop 顺序 / 并发 / 协议 FFI,都要往此处追加。
>
> 每条形式:**标题 / 代码位置 / 约束 / 违反后果 / 验证方式**

---

## INV-001: `State` 字段声明顺序决定 wl 指针生命周期

**位置**:`src/wl/window.rs` 的 `struct State { .. }`

**约束**:字段声明顺序必须是
```
renderer  ←  先 drop
window
conn      ←  最后 drop(在此范围内)
```

renderer 可以在 registry_state / output_state 之后(它们不持 wl 指针)。renderer 必须在 window **之前**,window 必须在 conn **之前**。

**为啥**:`renderer` 持有 `wgpu::Surface`,`wgpu::Surface` 内部保留 `wl_display` 和 `wl_surface` 的裸指针。Rust 按**字段声明顺序正向 drop**(第一个声明的先 drop)。若 window/conn 在 renderer 之前 drop,裸指针成悬指,renderer drop 时访问 → UB。

**违反后果**:Use-after-free,可能表现为段错误、静默数据损坏、Vulkan validation layer 报错,或"看起来工作但偶尔崩"。

**验证**:
- Code review 时必须检查字段顺序
- `cargo run` + `valgrind --tool=memcheck` 能捕获(访问已释放内存)
- 手工测试:关窗口然后再开,看有没有崩

---

## INV-002: `Renderer` 字段声明顺序决定 wgpu 内部生命周期

**位置**:`src/wl/render.rs` 的 `struct Renderer { .. }`

**约束**:字段声明顺序必须是 (10 字段, T-0306 复核确认)
```
surface              ←  先 drop (第 1, 持 wgpu::Instance 引用)
cell_pipeline        (Option<wgpu::RenderPipeline>, T-0305+, 持 device 引用)
cell_vertex_buffer   (Option<wgpu::Buffer>, T-0305+, 持 device 引用)
cell_buffer_capacity (usize, T-0305+, 仅元数据无 GPU 引用, 顺序无关但放此处保 visual locality)
device               (持 GPU context, 必须在 surface + pipeline + buffer 后 drop)
queue                (依赖 device, 但 wgpu 内部引用计数允许任意顺序)
config               (SurfaceConfiguration POD, 顺序无关)
clear                (wgpu::Color POD, 顺序无关)
surface_is_srgb      (bool POD, T-0305+, 顺序无关)
instance             ←  最后 drop (Vulkan/GL 底层实例)
```

**为啥**:`wgpu::Surface` 依赖 `wgpu::Instance` 保持 Vulkan/GL 底层实例存活。surface 必须在 instance 之前 drop。`RenderPipeline` / `Buffer` (T-0305 cell 渲染加) 持 device 内部引用, 必须在 device drop 之前 drop。device / queue 依赖 adapter (已构造后立即 drop, 但 device 内部引用保持 GPU context 存活), 所以 device 在 queue 前或后都行, 但必须在 surface + cell_pipeline + cell_vertex_buffer 之后、instance 之前。POD 字段 (cell_buffer_capacity / config / clear / surface_is_srgb) drop 顺序无关, 但保持声明顺序 visual locality 让 review 时易查。

**违反后果**:wgpu 内部访问已释放 Vulkan/GL 实例 → UB。通常是 Vulkan validation 抓到,但 release build 无 validation 时可能静默崩溃。

**验证**:字段顺序 lint(手工 review)+ 长跑测试。

---

## INV-003: `unsafe fn Renderer::new` 的调用方合同

**位置**:`src/wl/render.rs::Renderer::new`

**约束**:调用方(目前只有 `State::init_renderer_and_draw`)必须保证:
1. `display_ptr` 是当前进程活跃 `wayland_client::Connection` 的 `wl_display` 裸指针,非 null
2. `surface_ptr` 是当前进程活跃 `smithay_client_toolkit::shell::xdg::window::Window` 的 `wl_surface` 裸指针,非 null
3. **两枚指针在返回的 `Renderer` 整个生命周期内保持有效**
   —— 这条通过 INV-001 的字段顺序保证

**违反后果**:wgpu create_surface 内部使用悬指针 → UB。

**验证**:调用处有 null 检查(`NonNull::new()`),生命周期靠 INV-001。

---

## INV-004: `forbid(unsafe_code)` 降级为 `deny`

**位置**:`src/lib.rs` + `src/main.rs` 顶端

**约束**:crate 根用 `#![deny(unsafe_code)]` 而非 `#![forbid(unsafe_code)]`。每一处 `unsafe` 块必须:
- 紧邻上一行 `#[allow(unsafe_code)]` 放行
- 带 `// SAFETY: <不变式>` 注释说明前置条件

**为啥 deny 而非 forbid**:`forbid` 在 crate 根无法被 inner `#[allow]` 降级,ADR 0001 承诺的"局部豁免"机制不可用。

**违反后果**:若改回 `forbid`,wgpu 集成(需要 `create_surface_unsafe`)就编不过。

**验证**:`rg '^#!\[forbid\(unsafe_code\)\]' src/` 应无结果(除非未来模块内部用)。ADR 0001 + 本条共同约束。

---

## INV-005: `calloop::EventLoop` 是唯一 IO 调度器

**位置**:项目全局(CLAUDE.md 也有此条,这里登记为不变式)

**约束**:所有 IO fd(wayland / pty / timerfd / xkb / d-bus / signal)必须注册到同一个 `calloop::EventLoop`。**绝不起 thread pool 做 IO**。

**违反后果**:thread 之间互相 starve,终端变卡或 hang —— 正是 Ghostty 的 GTK4 event starvation bug。

**验证**:架构层面不变式,code review 和 PR 标题中明确。

---

## INV-006: `WindowCore::resize_dirty` 的消费合同

**位置**:`src/wl/window.rs::WindowCore::resize_dirty`

**约束**:
- **置位**:由 `handle_event(Configure {...})` 在尺寸变化时置 `true`
- **清零**:上层(T-0103 wgpu swapchain 重建者)**必须**在每次 resize 处理完 **显式** `core.resize_dirty = false`
- **语义**:布尔脏标记,**不是**队列。连续多次 resize 合并到单次脏标记。

**违反后果**:
- 若忘了清零:每帧都当成"有 resize",导致重复重建 swapchain,性能砸穿
- 若不检查就重建:即使尺寸没变也重建,浪费 GPU 资源

**验证**:`tests/state_machine.rs` 里有 `idempotent_same_size_configure_does_not_re_dirty` + `consecutive_resizes_merge_to_single_dirty`。

---

## INV-007: `WindowCore` state machine 是纯逻辑,无副作用

**位置**:`src/wl/window.rs::handle_event`

**约束**:`handle_event(&mut WindowCore, WindowEvent) -> WindowAction` 只改 `WindowCore` 字段,**不**做:
- I/O(网络、文件)
- Wayland request(不调 `window.commit()` 等)
- GPU 调用
- 日志(`tracing` 允许?**暂时允许**,因为它是 out-of-band)

所有副作用通过返回的 `WindowAction` 传给调用方。

**为啥**:保持 headless 可测试性。`tests/state_machine.rs` 的 9 个测试全依赖这个。

**违反后果**:测试被迫 mock I/O,chain 越拉越长,最终"测试自己"失效。

---

## INV-008: `PtyHandle` 字段声明顺序决定 PTY / 子进程 drop 语义

**位置**:`src/pty/mod.rs::PtyHandle`

**约束**:字段声明顺序必须是
```
reader   ← 先 drop (第 1)
master
child    ← 最后 drop (第 3)
```

**为啥**:
- `reader` 是 `try_clone_reader()` 拿的 dup fd,依赖 master fd 的 OFD 存活。
  让它先 drop,把自己那份 dup fd 先归还内核。
- `master` 拥有主 fd,drop 时关闭 master 端 → kernel 给 slave 端 EOF,
  前台进程组收到 `SIGHUP`。必须在 `child` 之前发 SIGHUP,否则 child 还活着
  却拿不到任何通知,形同 leak。
- `child` 最后 drop。Drop 本身 **不阻塞 `wait()`**(单线程事件循环禁止
  任意阻塞,见 INV-005);未 reap 的子进程靠本进程退出 / T-0205 的
  `try_wait` 兜底。

**违反后果**:
- 若 `child` 在 `master` 前 drop,`portable_pty::Child::Drop` 只 drop 句柄、不
  送 signal,子进程 detach 成 orphan,后续才被 SIGHUP,没有时间窗保证。
- 若 `reader` 在 `master` 后 drop,reader 持有的 dup fd 指向一个已被关闭的
  OFD,后续对 reader 的任意 `read` 会得到 EBADF,但这一问题通常只在
  未来 T-0203 引入非 fd 级 close 顺序假设时才显现 —— 提前固化避免踩坑。

**验证**:
- Code review 时必须检查 `PtyHandle` 字段顺序
- `src/pty/mod.rs::tests` 有 `spawn_program_true_succeeds_and_exits_cleanly`
  等测试走完整 Drop 路径

---

## INV-009: PTY master fd 必须 `O_NONBLOCK`

**位置**:`src/pty/mod.rs::PtyHandle::spawn_program`

**约束**:`spawn_program` / `spawn_shell` 返回之前,**必须** 对 master fd 调
`fcntl(F_SETFL, flags | O_NONBLOCK)`。未来 T-0202 把该 fd 接进 calloop、
T-0203 调 `reader.read` 时,**默认假设** fd 是非阻塞的。

**为啥**:calloop 单线程事件循环(INV-005)里任何阻塞 `read` 都会卡住
全部 IO(Wayland / 渲染 / signal),正是 Ghostty / GTK4 踩过的 "event
starvation" 坑的反面。非阻塞 read 得到 `WouldBlock` 时直接返回给 calloop
等下一次 readiness。

**违反后果**:终端会间歇性 freeze —— 子进程写得慢时整个 UI 不响应。最坏情况:
子进程挂起但未退出,quill 拿不到输出也吃不到键盘 → 用户只能 SIGKILL。

**验证**:
- `src/pty/mod.rs::tests::master_fd_is_nonblocking_after_spawn` 用
  `fcntl(F_GETFL)` 读 flags,按位断言 `O_NONBLOCK` 位为 1
- Code review 时确认 `spawn_program` 在返回 `PtyHandle` 之前已调
  `set_nonblocking`;若路径里有 `?` 提前返回,需在该分支也保证 fd 被关闭
  (当前实现:`set_nonblocking` 失败时 `pair.master` 还在栈上,提前返回时
  自动 drop 关闭,无 leak)

---

## 条目编号规则

- 顺序编号 `INV-001` `INV-002` ...
- 删除某条时**不**回收编号,留 tombstone "INV-XXX: (已作废 <日期> 原因 ...)"
- 每条尽量短,细节放对应代码注释 + 链接回 `docs/invariants.md`
- 每次 audit 发现新约束 → 追加
