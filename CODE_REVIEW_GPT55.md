# Rust 终端模拟器代码审查

审查维度：安全漏洞、代码风格、可维护性。审查对象为 `src/`、`tests/`、`Cargo.toml`、`Cargo.lock`。  
验证命令：

- `cargo clippy --all-targets --all-features -- -D warnings`：通过，无 warning。
- `cargo test --all-targets`：失败，lib 单测阶段 `388 passed; 5 failed; 5 ignored`。
- `cargo tree -d`：存在重复依赖版本，最直接可收敛的是 `unicode-width 0.1/0.2`。

## 高

### 1. `src/wl/keyboard.rs:707`：Wayland keymap `mmap` 信任 compositor 传入的 `size`，可被异常 fd/size 触发 SIGBUS 或大内存分配

问题描述：`load_keymap_from_fd` 直接用协议传入的 `size` 做 `mmap`，然后 `slice.to_vec()`。注释里已经承认“compositor 撒谎超实际 fd 大小会 SIGBUS”。这不是 Rust panic，而是进程级同步信号崩溃；同时未限制 `size`，恶意/异常 compositor 可要求映射并复制超大 keymap。这个 unsafe 块不必要，且安全性依赖外部守约。

具体 patch：

```diff
diff --git a/src/wl/keyboard.rs b/src/wl/keyboard.rs
@@
-use std::os::fd::{AsRawFd, OwnedFd};
+use std::io::{Read as _, Seek as _};
+use std::os::fd::{AsFd, OwnedFd};
@@
-/// mmap fd 的 size 字节 → UTF-8 String。失败任意一步返 anyhow Error, 调用方
+/// 读取 keymap fd 的 size 字节 → UTF-8 String。失败任意一步返 anyhow Error, 调用方
 /// (`handle_keymap_event`) 仅 warn, 不 panic。
-///
-/// **why mmap 而非 read**: wl_keyboard 协议保证 fd 内容是 size 字节的 keymap
-/// 文本; 但有些 compositor (mutter / weston) 用 `MAP_PRIVATE` shm fd, read
-/// 不一定合法 (可能 ENOSYS 或返 0)。mmap 是 wayland 客户端事实上的协议
-/// 处理方式 (libwayland-client / SCTK / wlroots 客户端示例都 mmap)。
 fn load_keymap_from_fd(fd: &OwnedFd, size: usize) -> Result<String> {
+    const MAX_KEYMAP_BYTES: usize = 1024 * 1024;
     if size == 0 {
         return Err(anyhow!("keymap size = 0"));
     }
-    // SAFETY:
-    // - `fd` 是 wl_keyboard 协议保证活的 OwnedFd, 我们仅借用 raw fd 不夺所有权
-    // - mmap PROT_READ + MAP_PRIVATE: 进程私有只读映射, 改不破坏其他进程
-    // - size 直接来自 wl_keyboard 协议 (compositor 推); 若 compositor 撒谎超
-    //   实际 fd 大小, mmap 在 read 越界时给 SIGBUS, 但该协议层假设 compositor
-    //   守约 (与 SCTK / wlroots / cosmic-comp 假设一致)
-    // - munmap 在 mmap 成功后必跑 (defer 通过 scope 末尾显式 libc::munmap),
-    //   `?` 早返路径不触达 mmap 后, 无 leak
-    // - 本块**不**长持映射: 立即拷贝 size 字节到 Vec, mmap 区域随后 munmap 释放
-    #[allow(unsafe_code)]
-    let bytes = unsafe {
-        let raw_fd = fd.as_raw_fd();
-        let ptr = libc::mmap(
-            std::ptr::null_mut(),
-            size,
-            libc::PROT_READ,
-            libc::MAP_PRIVATE,
-            raw_fd,
-            0,
-        );
-        if ptr == libc::MAP_FAILED {
-            return Err(anyhow!(
-                "mmap keymap fd 失败: {}",
-                std::io::Error::last_os_error()
-            ));
-        }
-        // SAFETY: ptr 是有效 PROT_READ 映射区, 长度 size; from_raw_parts 只读
-        // 一次后立即拷贝到 Vec, 之后 munmap, 借用不外泄
-        let slice = std::slice::from_raw_parts(ptr as *const u8, size);
-        let bytes = slice.to_vec();
-        if libc::munmap(ptr, size) != 0 {
-            // munmap 失败极罕见, 仅 warn 不返错 — bytes 已拿到, 调用方可用
-            tracing::warn!(target: "quill::keyboard", "munmap keymap 区失败: {}", std::io::Error::last_os_error());
-        }
-        bytes
-    };
+    if size > MAX_KEYMAP_BYTES {
+        return Err(anyhow!("keymap size {size} exceeds {MAX_KEYMAP_BYTES} byte limit"));
+    }
+    let dup = fd
+        .as_fd()
+        .try_clone_to_owned()
+        .context("clone keymap fd 失败")?;
+    let mut file = std::fs::File::from(dup);
+    file.rewind().context("rewind keymap fd 失败")?;
+    let mut bytes = Vec::with_capacity(size);
+    file.by_ref()
+        .take(size as u64)
+        .read_to_end(&mut bytes)
+        .context("read keymap fd 失败")?;
+    if bytes.len() != size {
+        return Err(anyhow!("keymap fd short read: expected {size}, got {}", bytes.len()));
+    }
```

### 2. `src/wl/window.rs:2137` / `src/wl/window.rs:1788`：clipboard paste / DnD pipe 读取无上限，恶意 selection source 可造成 OOM

问题描述：`paste_read_tick` 和 `drop_read_tick` 在每次 `read` 后直接 `Vec::extend_from_slice`，直到 EOF 才处理。Wayland clipboard/DnD 数据来自其他客户端，经 compositor 转发；恶意 source 可以持续输出或输出超大内容，导致终端进程内存增长到 OOM。

具体 patch：

```diff
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
 const PTY_READ_BUF: usize = 4096;
+const MAX_PASTE_PIPE_BYTES: usize = 16 * 1024 * 1024;
+
+fn extend_pipe_buf_bounded(buf: &mut Vec<u8>, chunk: &[u8], purpose: &'static str) -> bool {
+    if buf.len().saturating_add(chunk.len()) > MAX_PASTE_PIPE_BYTES {
+        tracing::warn!(
+            target: "quill::pointer",
+            purpose,
+            limit = MAX_PASTE_PIPE_BYTES,
+            current = buf.len(),
+            incoming = chunk.len(),
+            "paste/DnD pipe payload exceeds limit; aborting read"
+        );
+        return false;
+    }
+    buf.extend_from_slice(chunk);
+    true
+}
@@
-        if n > 0 {
-            drop_state
-                .borrow_mut()
-                .buf
-                .extend_from_slice(&chunk[..n as usize]);
+        if n > 0 {
+            let ok = {
+                let mut s = drop_state.borrow_mut();
+                extend_pipe_buf_bounded(&mut s.buf, &chunk[..n as usize], "dnd")
+            };
+            if !ok {
+                drop_state.borrow_mut().fd = None;
+                if let Some(offer) = data.state.dnd_current_offer.take() {
+                    offer.destroy();
+                }
+                data.state.dnd_current_offer_mimes.clear();
+                data.state.dnd_accepted_mime = None;
+                return Ok(PostAction::Remove);
+            }
             continue;
         }
@@
-        if n > 0 {
-            pasta_state
-                .borrow_mut()
-                .buf
-                .extend_from_slice(&chunk[..n as usize]);
+        if n > 0 {
+            let ok = {
+                let mut s = pasta_state.borrow_mut();
+                extend_pipe_buf_bounded(&mut s.buf, &chunk[..n as usize], "paste")
+            };
+            if !ok {
+                pasta_state.borrow_mut().fd = None;
+                data.state.paste_in_progress = false;
+                return Ok(PostAction::Remove);
+            }
             continue;
         }
```

### 3. `src/wl/selection.rs:594`：bracketed paste 未过滤内嵌结束标记，恶意剪贴板可提前结束 paste 并把后续字节当真实键入

问题描述：当前注释明确写“派单接受此 risk”。如果 pasted text 中含 `ESC [ 201 ~`，shell/readline 会认为 bracketed paste 已结束，后续换行和命令字节可能作为真实输入处理。这属于用户输入 sanitize 缺口，影响 clipboard paste 和 DnD text fallback。

具体 patch：

```diff
diff --git a/src/wl/selection.rs b/src/wl/selection.rs
@@
-/// **dirty pty 字节裸保留**: 粘贴文本若已含 `\x1b[201~` (恶意 / 凑巧) 会过早
-/// 终止 paste, shell 把后续部分当真键入. 派单接受此 risk (alacritty / foot
-/// 同等不过滤; 真防御走 shell `bind 'set enable-bracketed-paste off'` 关).
 pub fn bracketed_paste_wrap(text: &str, enabled: bool) -> Vec<u8> {
     if !enabled {
         return text.as_bytes().to_vec();
     }
+    let sanitized = text
+        .replace("\x1b[200~", "")
+        .replace("\x1b[201~", "");
     let mut out = Vec::with_capacity(text.len() + 12);
     out.extend_from_slice(b"\x1b[200~");
-    out.extend_from_slice(text.as_bytes());
+    out.extend_from_slice(sanitized.as_bytes());
     out.extend_from_slice(b"\x1b[201~");
     out
 }
@@
+    #[test]
+    fn bracketed_paste_strips_nested_delimiters() {
+        let out = bracketed_paste_wrap("a\x1b[201~\nrm -rf /", true);
+        assert_eq!(out, b"\x1b[200~a\nrm -rf /\x1b[201~");
+    }
```

### 4. `src/wl/window.rs:722`：Wayland configure 尺寸未设上限，可让 `Term::resize` 分配超大 grid，且 `pty.resize(cols as u16)` 后续会截断

问题描述：`cells_from_surface_px` 只做下限 `max(1)`，不做上限。异常 compositor 或协议 bug 传入极大 surface size 时，会把 `cols/rows` 传给 alacritty grid，造成内存/CPU DoS；之后 `propagate_resize_if_dirty` 里 `cols as u16` / `rows as u16` 还会静默截断，终端状态和 PTY winsize 不一致。

具体 patch：

```diff
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
 const RESIZE_THROTTLE_MS: u64 = 60;
+const MAX_GRID_COLS: usize = 1000;
+const MAX_GRID_ROWS: usize = 1000;
@@
 pub(crate) fn cells_from_surface_px(width: u32, height: u32, tab_count: usize) -> (usize, usize) {
@@
-    (cols.max(1), rows.max(1))
+    (
+        cols.clamp(1, MAX_GRID_COLS),
+        rows.clamp(1, MAX_GRID_ROWS),
+    )
 }
@@
-        if let Err(err) = tab.pty().resize(cols as u16, rows as u16) {
+        let cols_u16 = u16::try_from(cols).unwrap_or(u16::MAX);
+        let rows_u16 = u16::try_from(rows).unwrap_or(u16::MAX);
+        if let Err(err) = tab.pty().resize(cols_u16, rows_u16) {
```

## 中

### 1. `src/wl/window.rs:1428`：`pending_pty_writes` 无上限，PTY 背压时反复 paste 可继续堆内存

问题描述：`queue_or_write_pty` 在已有队列时直接 append，`WouldBlock` 和 partial write 也无限 append。即使第 2 条给 pipe 读设置了上限，用户或恶意 clipboard owner 多次触发 paste 仍可在 PTY 不排水时让队列无限增长。

具体 patch：

```diff
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
 const MAX_PASTE_PIPE_BYTES: usize = 16 * 1024 * 1024;
+const MAX_PENDING_PTY_BYTES: usize = 4 * 1024 * 1024;
+
+fn queue_pending_pty_bytes(state: &mut State, bytes: &[u8], purpose: &'static str) {
+    let available = MAX_PENDING_PTY_BYTES.saturating_sub(state.pending_pty_writes.len());
+    if bytes.len() > available {
+        tracing::warn!(
+            target: "quill::pty",
+            purpose,
+            queued = state.pending_pty_writes.len(),
+            incoming = bytes.len(),
+            limit = MAX_PENDING_PTY_BYTES,
+            "pending PTY write queue full; dropping incoming tail"
+        );
+        state.pending_pty_writes.extend_from_slice(&bytes[..available]);
+        return;
+    }
+    state.pending_pty_writes.extend_from_slice(bytes);
+}
@@
     if !state.pending_pty_writes.is_empty() {
-        state.pending_pty_writes.extend_from_slice(bytes);
+        queue_pending_pty_bytes(state, bytes, purpose);
         return;
     }
@@
-            state.pending_pty_writes.extend_from_slice(&bytes[n..]);
+            queue_pending_pty_bytes(state, &bytes[n..], purpose);
@@
-            state.pending_pty_writes.extend_from_slice(bytes);
+            queue_pending_pty_bytes(state, bytes, purpose);
```

### 2. `src/wl/window.rs:1828`：DnD 写 PTY 与 paste 背压策略不一致，partial write 直接丢尾部

问题描述：paste EOF 路径已经用 `queue_or_write_pty` 保证 bracketed paste 尾标记最终写入；DnD EOF 路径却直接 `pty.write(&wrapped)`，partial 或 WouldBlock 时丢弃剩余字节。拖入长 URI list 或慢 PTY 消费方时，shell 会收到截断命令。

具体 patch：

```diff
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
-                let pty = data.state.tabs_unchecked().active().pty();
-                match pty.write(&wrapped) {
-                    Ok(n) if n == wrapped.len() => {
-                        tracing::debug!(
-                            target: "quill::pointer",
-                            n,
-                            bracketed,
-                            mime = %mime,
-                            "DnD: wrote bytes to pty"
-                        );
-                    }
-                    Ok(n) => {
-                        tracing::warn!(
-                            target: "quill::pointer",
-                            wrote = n,
-                            total = wrapped.len(),
-                            "DnD: pty.write 部分写入, 剩余字节丢弃 (背压)"
-                        );
-                    }
-                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
-                        tracing::warn!(
-                            target: "quill::pointer",
-                            n = wrapped.len(),
-                            "DnD: pty.write WouldBlock, 字节丢弃 (背压, INV-005 不重试)"
-                        );
-                    }
-                    Err(e) => {
-                        tracing::warn!(
-                            target: "quill::pointer",
-                            error = %e,
-                            "DnD: pty.write 失败"
-                        );
-                    }
-                }
+                queue_or_write_pty(&mut data.state, &wrapped, "dnd");
```

### 3. `src/wl/window.rs:537`：单次 PTY readable callback 无预算，上游持续输出时可长期占用事件循环

问题描述：`pty_read_tick_inner` 在一个 callback 里循环 `read` 到 `WouldBlock`。如果子进程持续快速输出，callback 可能连续处理大量数据，拖延 Wayland dispatch、渲染、输入和 pending paste drain。这是拒绝服务/交互饥饿风险，不是内存安全问题。

具体 patch：

```diff
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
 const PTY_READ_BUF: usize = 4096;
+const MAX_PTY_BYTES_PER_TICK: usize = 1024 * 1024;
@@
     let mut buf = [0u8; PTY_READ_BUF];
+    let mut bytes_this_tick = 0usize;
     loop {
@@
                     if let Some(t) = term.as_mut() {
@@
                     }
+                    bytes_this_tick += n;
+                    if bytes_this_tick >= MAX_PTY_BYTES_PER_TICK {
+                        tracing::debug!(
+                            target: "quill::pty",
+                            bytes_this_tick,
+                            "PTY read budget exhausted; yielding to event loop"
+                        );
+                        return Ok(PostAction::Continue);
+                    }
                 }
                 continue;
```

### 4. `src/pty/mod.rs:39` / `src/wl/window.rs:2056`：关闭 tab 时 drop child handle 但不保留 wait 句柄，quill 继续运行期间可能留下 zombie

问题描述：注释写 “Drop 不阻塞 wait”，`close_tab_idx` 先 `tab_list.remove(idx)`，`TabInstance` 被 drop 后 `PtyHandle.child` 也被 drop。master 关闭会让 shell 收到 HUP，但父进程还活着且已丢失 child handle，子进程退出后无法 `wait`，会留下 zombie 直到 quill 退出。

具体 patch：

```diff
diff --git a/src/pty/mod.rs b/src/pty/mod.rs
@@
+pub struct ReapChild {
+    child: Box<dyn Child + Send + Sync>,
+}
+
+impl ReapChild {
+    pub fn try_wait(&mut self) -> Result<Option<i32>> {
+        let status = self.child.try_wait().context("portable-pty Child::try_wait 失败")?;
+        Ok(status.map(|s| s.exit_code() as i32))
+    }
+}
+
 impl PtyHandle {
+    pub fn into_reap_child(self) -> ReapChild {
+        let PtyHandle {
+            reader,
+            master,
+            child,
+            master_fd: _,
+        } = self;
+        drop(reader);
+        drop(master);
+        ReapChild { child }
+    }
 }
diff --git a/src/tab/mod.rs b/src/tab/mod.rs
@@
+    pub fn into_reap_child(self) -> crate::pty::ReapChild {
+        self.pty.into_reap_child()
+    }
diff --git a/src/wl/window.rs b/src/wl/window.rs
@@
     pending_pty_writes: Vec::new(),
+    closing_children: Vec<crate::pty::ReapChild>,
@@
-    let removed_tab_id = {
+    let removed = {
         let tab_list = match data.state.tabs.as_mut() {
             Some(t) => t,
             None => return,
         };
         if idx >= tab_list.len() {
             return;
         }
         let removed = tab_list.remove(idx);
-        removed.map(|t| t.id())
+        removed.map(|t| (t.id(), t.into_reap_child()))
     };
-    let Some(removed_id) = removed_tab_id else {
+    let Some((removed_id, child)) = removed else {
         return;
     };
+    data.state.closing_children.push(child);
@@
+fn reap_closing_children(state: &mut State) {
+    state.closing_children.retain_mut(|child| match child.try_wait() {
+        Ok(Some(code)) => {
+            tracing::debug!(target: "quill::pty", exit_code = code, "closed tab child reaped");
+            false
+        }
+        Ok(None) => true,
+        Err(err) => {
+            tracing::warn!(target: "quill::pty", ?err, "closed tab child reap failed; dropping handle");
+            false
+        }
+    });
+}
@@
             drain_pending_pty_writes(&mut data.state);
+            reap_closing_children(&mut data.state);
```

### 5. 多处测试断言与当前实现漂移，`cargo test --all-targets` 红灯

文件 + 行号：`src/term/mod.rs:1714`、`src/term/mod.rs:1748`、`src/wl/pointer.rs:1764`、`src/wl/render.rs:5761`、`src/wl/render.rs:6358`。  
问题描述：clippy 通过但测试不通过，维护性上比 warning 更严重。失败原因是测试期望旧契约：调色板期望 xterm classic，但实现和注释部分已改成 Tango；smooth axis 测试期望沉默，但 dispatch 已重新消费 smooth axis；render 测试期望旧顶点大小/空格 bg skip，但实现已改为显式背景空格可绘制。

具体 patch：

```diff
diff --git a/src/term/mod.rs b/src/term/mod.rs
@@
-        assert_eq!(
-            Color::from_alacritty(AlacColor::Named(NamedColor::Red)),
-            Color { r: 170, g: 0, b: 0 },
-            "ANSI Named::Red → xterm-classic (170, 0, 0)"
-        );
+        assert_eq!(
+            Color::from_alacritty(AlacColor::Named(NamedColor::Red)),
+            Color { r: 204, g: 0, b: 0 },
+            "ANSI Named::Red → Tango/alacritty default"
+        );
@@
-            Color { r: 170, g: 0, b: 0 },
+            Color { r: 204, g: 0, b: 0 },
diff --git a/src/wl/pointer.rs b/src/wl/pointer.rs
@@
-        assert_eq!(
-            handle_pointer_event(event_smooth, &mut state, false, 0),
-            PointerAction::Nothing,
-            "T-0618: smooth Axis 不再消费"
-        );
+        assert_eq!(
+            handle_pointer_event(event_smooth, &mut state, false, 0),
+            PointerAction::Scroll(-10),
+            "mutter smooth Axis value=100.0 应按 10px/line 转成 Scroll(-10)"
+        );
diff --git a/src/wl/render.rs b/src/wl/render.rs
@@
-    fn build_vertex_bytes_still_skips_space_cell() {
+    fn build_vertex_bytes_keeps_explicit_bg_space_cell() {
@@
-        assert!(bytes.is_empty(), "空格 cell 仍优先跳过");
+        assert_eq!(bytes.len(), 6 * VERTEX_BYTES, "Bg pass 必须绘制显式背景空格");
@@
-        assert_eq!(cell.len(), 6 * 20);
+        assert_eq!(cell.len(), 6 * VERTEX_BYTES);
```

## 低

### 1. `Cargo.toml:29`：直接依赖 `unicode-width = "0.1"`，但依赖树同时已有 `unicode-width 0.2`

问题描述：`cargo tree -d` 显示 `unicode-width v0.1.14` 只由本 crate 直接引入，而 `alacritty_terminal`/`naga` 已使用 `unicode-width v0.2.2`。这是不必要的重复依赖版本，维护时容易出现宽度规则不一致。

具体 patch：

```diff
diff --git a/Cargo.toml b/Cargo.toml
@@
-unicode-width = "0.1"
+unicode-width = "0.2"
```

### 2. `src/wl/pointer.rs:1040`：`apply_axis_vertical` 已标 deprecated 但仍保留并返回旧语义，和 dispatch 实际 smooth axis 行为相互矛盾

问题描述：`handle_pointer_event` 现在调用 `apply_axis_smooth`，但 `apply_axis_vertical` 仍存在并返回 `Nothing`，测试也曾依赖它。这个死代码会误导后续维护者，以为 smooth axis 不消费。

具体 patch：

```diff
diff --git a/src/wl/pointer.rs b/src/wl/pointer.rs
@@
-#[allow(dead_code)] // T-0618: deprecated, 保留签名防老测试 / 触摸板复活时直接补
-pub(crate) fn apply_axis_vertical(state: &mut PointerState, value: f64) -> PointerAction {
-    // T-0618: deprecated. Axis smooth path 在 dispatch 已 short-circuit 不再调用.
-    // 保留 fn 签名仅给老测试 + 万一有 caller 漏改. 触摸板支持时复活.
-    let _ = (state, value);
-    PointerAction::Nothing
-}
-
 /// T-0618: 纵向 AxisValue120 (Wayland 1.21+ 离散滚轮 notch) → Scroll(±N) 决策.
```

### 3. `src/main.rs:160`：`--headless-screenshot` 使用 `File::create`，会静默截断已有文件并跟随 symlink

问题描述：这是 CLI 显式路径，不是提权漏洞；但对安全默认值来说，截图模式不应默认覆盖任意已有路径。尤其在脚本或 root 环境下运行时，误传路径会直接截断。

具体 patch：

```diff
diff --git a/src/main.rs b/src/main.rs
@@
-use std::fs::File;
+use std::fs::OpenOptions;
@@
-    let mut file =
-        File::create(path).with_context(|| format!("创建 PNG 文件 {} 失败", path.display()))?;
+    let mut file = OpenOptions::new()
+        .write(true)
+        .create_new(true)
+        .open(path)
+        .with_context(|| format!("创建 PNG 文件 {} 失败 (文件已存在时不会覆盖)", path.display()))?;
```

### 4. `src/pty/mod.rs:82`：`spawn_program` 接受 `cols=0` 或 `rows=0`，公共 API 可构造非法/退化 PTY 尺寸

问题描述：窗口路径对 grid 下限做了 clamp，但 `PtyHandle::spawn_program` 是 public，测试/未来调用方可以传 0。底层 PTY 对 0 winsize 的行为依平台不同，错误会延后到 shell/TUI 查询窗口尺寸时暴露。

具体 patch：

```diff
diff --git a/src/pty/mod.rs b/src/pty/mod.rs
@@
     pub fn spawn_program(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self> {
+        if cols == 0 || rows == 0 {
+            return Err(anyhow!("PTY size must be non-zero, got cols={cols}, rows={rows}"));
+        }
         let pty_system = native_pty_system();
```

### 5. `src/term/mod.rs:1103` / `src/term/mod.rs:1174`：公开读行 API 对越界 line 会 panic

问题描述：`line_text`、`display_text`、`display_text_with_spacers` 都直接索引 alacritty grid。当前内部调用先走 dimensions，所以主路径不炸；但这些是 public API，越界参数来自测试或未来功能时会直接 panic，不符合其余模块偏 `Result`/`Option` 的错误处理风格。

具体 patch：

```diff
diff --git a/src/term/mod.rs b/src/term/mod.rs
@@
     pub fn line_text(&self, line: usize) -> String {
@@
         let grid = self.term.grid();
+        if line >= grid.screen_lines() {
+            return String::new();
+        }
         let row = &grid[Line(line as i32)];
@@
     pub fn display_text(&self, line: usize) -> String {
@@
         let grid = self.term.grid();
+        if line >= grid.screen_lines() {
+            return String::new();
+        }
@@
     pub fn display_text_with_spacers(&self, line: usize) -> String {
@@
         let grid = self.term.grid();
+        if line >= grid.screen_lines() {
+            return String::new();
+        }
```

## 整体结论

当前代码把 PTY、渲染、输入、终端状态机分层得比较清楚，`unsafe` 也集中在少数边界；但安全边界里仍有几处必须先修：keymap fd 的 unsafe `mmap`、clipboard/DnD 无上限读、bracketed paste 内嵌终止标记、resize 尺寸无上限。这些都是具体可触发的 DoS 或输入注入风险。代码风格上 clippy 已干净，但测试套件当前红灯，说明实现契约和测试/注释发生漂移；先让测试回绿，再处理 zombie reaping、背压上限和重复依赖，维护成本会明显下降。
