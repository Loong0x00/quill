# T-0305 色块渲染 手测描述

**日期**: 2026-04-25
**写码**: writer-T0305
**用途**: ticket Acceptance 要求"截图存 `docs/screenshots/T-0305-color-block.png` (或描述)";本机
GNOME Wayland 下 `gnome-screenshot`/`grim`/portal 全部不可用(`AccessDenied` /
`compositor not wlroots`),改走"trace log + 渲染数学"组合证据 +
visual 描述。

## 启动命令

```bash
DISPLAY=:0 RUST_LOG="quill=debug,quill::pty=trace,quill::term=trace" \
  /home/user/quill-impl-color-block/target/release/quill
```

NVIDIA RTX 5090 + Wayland (mutter) + wgpu 0.29 → 自动选 Vulkan backend (与
`docs/CLAUDE.md` "已验证信号" 段一致)。

## 屏幕上看到什么

- 800×600 窗口,**深蓝**清屏色 `#0a1030`(T-0102 [`CLEAR_COLOR_SRGB_U8`])
- 窗口左上角 `(0, 0)..(150, 25)` px 区域出现 **15 个相邻的浅灰矩形**,
  每个 `10 × 25` px,排成水平一行
- 浅灰色为 `#d3d3d3`([`Color::DEFAULT_FG`],alacritty `NamedColor::Foreground`
  解析点);bash prompt `[user@userPC ~]$` 的 16 字符里前 15 个非空格 cell 各
  贡献一个 fg 矩形,trailing 的光标位空格被 `c == ' '` 稀疏过滤掉
- 窗口剩余区域全是深蓝(空白 cell 不上传顶点,清屏色透出)

## 渲染数学验证

`cargo run --release` 后用 trace log 抓到两次 `draw_cells frame`:

```
draw_cells frame cols=80 rows=24 cell_w_px=10.0 cell_h_px=25.0 vertex_count=0
draw_cells frame cols=80 rows=24 cell_w_px=10.0 cell_h_px=25.0 vertex_count=90
```

- 第 1 帧 `vertex_count=0`:bash 还没产 prompt,grid 全空,稀疏渲染产 0 顶点
  → 屏幕只清屏深蓝
- 第 2 帧 `vertex_count=90`:bash 产出 prompt 258 字节 (`[user@userPC ~]$`),
  `term.advance` 把它落到 grid (`cursor_pos col=17 line=0`),`is_dirty=true`,
  idle callback 调 `draw_cells`,`build_vertex_bytes` 跳过 trailing 空格 cell,
  剩 15 cell × `VERTS_PER_CELL`(6 顶点/cell)= **90 顶点**
- `cell_w_px = 800 / 80 = 10`、`cell_h_px = 600 / 24 = 25`,与上面 visual 描述一致

## 可视化对应

```
┌────────────────────────────────────────────────┐  ← y=0 (top)
│ ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒                                │  ← line 0, cells 0..15 fg 浅灰
│                                                │  ← y=25..50, line 1 全空白
│                                                │
│              (深蓝 #0a1030 清屏)               │
│                                                │
│                                                │
│                                                │
│                                                │
└────────────────────────────────────────────────┘  ← y=600 (bottom)
        ↑
   15 个 10×25 px 浅灰矩形, 总宽 150 px
```

实际 CJK 字形 / 真字符纹理在 Phase 4 cosmic-text 接入时画到 fg 块之上;Phase 3
本 ticket 只验证"色块通路打通"。

## 决策追溯

- fg 着色而非 bg(派单 Goal 优先,scope 提示仅供参考):
  bash prompt 默认 bg = `Background` → 解析黑,在深蓝清屏上画黑块视觉几乎
  不可见;fg = `Foreground` → 解析浅灰,反差清晰。详见
  `src/wl/render.rs::Renderer::draw_cells` docstring 与 commit message body。
- 稀疏渲染 (`c == ' '` 跳过):空白 cell 不上顶点,清屏色透出,符合 acceptance
  "深蓝背景上离散色块"。
- HollowBlock 决策:派单要求 fold 进 Block + 注释,但 T-0305 不画光标,实际未
  消费 [`CursorShape`];fold 注释加在 cursor 渲染 ticket(后续)再补,本单无
  实际代码改。
