# CODE FIX REPORT

整体总结：已修复高严重安全项、滚轮 3 行默认值，并让 clippy 与全量测试通过。

## 修复明细

1. `src/wl/keyboard.rs:702` / `src/wl/keyboard.rs:707`
   - 修了啥：移除 Wayland keymap unsafe `mmap` 路径，改为 clone fd 后按 `size` 有界读取；增加 1MiB 上限、短读校验和 UTF-8 校验，避免超大读取和 mmap SIGBUS。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

2. `src/wl/window.rs:383` / `src/wl/window.rs:388` / `src/wl/window.rs:1815` / `src/wl/window.rs:2173`
   - 修了啥：clipboard paste 和 DnD pipe 累积读取增加 16MiB 上限，超过上限时关闭对应 fd、清理 DnD/paste 状态，避免恶意 selection source 造成 OOM。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

3. `src/wl/selection.rs:591` / `src/wl/selection.rs:595` / `src/wl/selection.rs:925`
   - 修了啥：bracketed paste 启用时过滤内嵌 `ESC[200~` / `ESC[201~` 标记，避免粘贴内容提前结束后注入真实输入；新增嵌套分隔符单测。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

4. `src/wl/window.rs:384` / `src/wl/window.rs:748` / `src/wl/window.rs:899`
   - 修了啥：Wayland configure 推导出的 grid 尺寸 clamp 到 `1000 x 1000`，PTY winsize 改为 `u16::try_from` 后传入，避免异常 resize 触发超大分配或静默截断。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

5. `src/wl/pointer.rs:493` / `src/wl/pointer.rs:1092` / `src/wl/pointer.rs:1735`
   - 修了啥：滚轮默认保持每 notch 3 行；smooth Axis 每个累积步也乘以 3，`pending_scroll_lines` 同源，所以普通滚屏和 alternate screen 转 cursor key 都适用。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

6. `src/term/mod.rs:1709` / `src/wl/render.rs:5766` / `src/wl/render.rs:6382`
   - 修了啥：修正 review 里列出的 5 个旧测试漂移点：ANSI 色断言改到 Tango/alacritty 默认；显式背景空格应绘制；顶点字节数断言改用 `VERTEX_BYTES`；smooth Axis 断言随滚轮 3 行更新。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

7. `src/wl/render.rs:3937`
   - 修了啥：`render_headless` 入口加进程内互斥锁，避免同一测试进程并发初始化 headless wgpu 时触发驱动层 SIGSEGV。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

8. `src/wl/render.rs:1675` / `src/wl/render.rs:2507` / `src/wl/render.rs:4131`
   - 修了啥：背景 fill quad 改为接收调用方传入的 alpha；live 路径继续用窗口 alpha，headless 路径用 1.0，恢复 PNG 中心区域不透明契约。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

9. `tests/ime_preedit_render.rs:121` / `tests/single_tab_no_bar_e2e.rs:25`
   - 修了啥：更新漂移的 PNG 测试坐标和测试输入：preedit 扫描区域加入 titlebar 偏移并按 CJK 4 cell 宽度检查；单 tab 测试空白 cell 改用默认 bg，让 terminal clear 色透出。
   - 验证：`cargo clippy --all-targets --all-features -- -D warnings` 通过；`cargo test --all-targets` 通过。

## 验证结果

- `cargo clippy --all-targets --all-features -- -D warnings`：通过。
- `cargo test --all-targets`：通过。
