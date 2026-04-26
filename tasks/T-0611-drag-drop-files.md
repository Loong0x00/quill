# T-0611 拖拽文件 → 文件路径插入 (Wayland DnD wl_data_device drop)

**Phase**: 7+ (daily-drive feel, Claude Code workflow 必需)
**Assigned**: writer-T0611
**Status**: in-review
**Budget**: tokenBudget=200k (跨 wl_data_device DnD enter/motion/drop/leave + text/uri-list mime + URI 解析 + 多文件路径拼接 + bracketed paste)
**Dependencies**: T-0607 (wl_data_device 已 bind, paste pipe 路径已通)
**Priority**: P0 (user 实测 Claude Code daily drive 必需 — 拖文件给 prompt 引用上下文)

## Goal

支持从 Nautilus / Files / 任意 Wayland file manager 拖拽文件 / 文件夹到 quill
窗口, 自动转换 file:// URI 为本地路径并插入 PTY (跟 paste 同套路 bracketed
wrap). 多文件用空格分隔. user 在 Claude Code 里拖文件给 prompt 引用 (跟
ChatGPT / Claude.ai 网页版同款体验).

## Bug / Pain

User 实测在 Claude Code 里要给 prompt 引用文件路径必须手敲完整路径
(`/home/user/quill/src/wl/window.rs`), 长路径 + 多文件复制粘贴效率低. 主流
GUI 终端 (ghostty / Warp / iTerm2 / GNOME Terminal) 都支持拖拽文件转路径,
quill 缺这功能影响 daily drive。

## Scope

### In

#### A. wl_data_device DnD event handler 扩展
- 当前 T-0607 wl_data_device 只处理 selection event (CLIPBOARD), 加 DnD 路径:
  - `Enter(serial, surface, x, y, offer)`: 拖入 surface, 记 offer + 接受 mime
  - `Motion(time, x, y)`: 拖动期间, 不消费 (可能用于 hover 高亮但 P3)
  - `Drop`: 释放, trigger paste 同款 pipe read 路径
  - `Leave`: 拖出 surface, 清 offer
- 新 PointerAction / 内部 op enum: `DropFile(Vec<PathBuf>)`

#### B. text/uri-list mime 优先 + 多 mime fallback
- DnD source offer mime 列表 (Nautilus 主流给):
  - `text/uri-list` ← 优先 (RFC 2483 标准 file:// URI per line)
  - `application/x-kde-cutselection` (KDE 文件管理)
  - `text/plain;charset=utf-8` ← fallback (拖文本 / 不拖文件路径)
  - `text/plain`
- accept_mime 选第一个匹配 (text/uri-list 优先), Drop 后 receive 该 mime
- 走 T-0607 paste pipe 路径 (offer.receive(mime, write_fd) → 异步 read_fd)

#### C. URI 解析 → 本地路径
- text/uri-list 格式: 每行一个 URI, 以 `\r\n` 或 `\n` 分隔, `#` 开头是注释
- 仅接受 `file://` scheme (https / ftp 等其它 scheme tracing warn 不消费)
- URL decode: `file:///home/user/My%20Doc.txt` → `/home/user/My Doc.txt`
- 拒绝 host (`file://localhost/...` 或 `file://hostname/...`) — 仅本地接受 (RFC 8089 4.2)
- 多 URI 拼接: 路径间空格分隔, **shell-escape 防特殊字符** (e.g. 空格 → `\ ` 或单引号包裹)

#### D. shell-escape 路径
- 路径含空格 / `'"$\` 等 → 单引号包裹 + 内部 `'` 转 `'\''` (POSIX 标准)
- 纯 ASCII + 安全字符 → 原文不包
- 多文件: `'path/with space.txt' /simple/path.rs '/another/space.md'`
- 跟 zsh / bash 拖入 file 的标准行为对齐 (drag-and-drop 整理后路径直接可粘贴 cmdline)

#### E. Bracketed paste 包装 (跟 T-0607 同)
- 启用 bracketed paste 时 (term.is_bracketed_paste()) 包 `\x1b[200~ ... \x1b[201~`
- 多文件路径拼接后整段 wrap, 不是每条 URI 各 wrap
- shell readline / Claude Code 接到知道是粘贴不是真键入

#### F. 测试
- src/wl/dnd.rs (新模块) 或 src/wl/selection.rs 复用:
  - parse_uri_list: 单 URI / 多 URI / file:// scheme / 非 file scheme reject / URL decode / 注释跳过
  - shell_escape: 空格 / 单引号 / 美元符 / 反斜杠 / 中文 / 表情符号
  - DropFile op build (paths → cmdline string)
- 集成测试不易 (Wayland DnD 协议要 mock compositor), 走纯 fn 单测 + 手测
- 手测 deliverable: cargo run --release, Nautilus 拖文件入 quill → cmdline 出现 shell-escaped 路径

### Out

- **不做**: 拖图片自动 OCR / 拖图片插入 base64 (Claude Code 多模态 phase 8+)
- **不做**: 拖网页 URL 自动转 wget / curl 命令 (P3 智能化)
- **不做**: 拖文件夹自动展开内容 (只插路径)
- **不做**: 拖出 (drag from quill to 别处) — 是 source side, P3
- **不做**: hover 高亮拖入区 (drop zone 视觉反馈, P2)
- **不动**: src/text / src/pty / src/ime / src/wl/keyboard / docs/invariants.md / docs/adr / Cargo.toml

## Acceptance

- [ ] 4 门 release 全绿
- [ ] Nautilus / Files 拖文件入 quill 窗口 → cmdline 出现 shell-escaped 路径
- [ ] 多文件拖入: 路径空格分隔
- [ ] 路径含空格 / 单引号: 单引号包裹 + 转义
- [ ] 中文路径: URL decode + UTF-8 输出 (T-0801 hotfix display_text 同源数据)
- [ ] 非 file:// scheme (https / ftp 等): tracing warn 不消费
- [ ] bracketed paste 跟随 term mode
- [ ] 总测试 379 + ≥10 ≈ 389+ pass
- [ ] **手测**: Nautilus 拖单文件 / 多文件 / 含空格文件 / 含中文文件 / 拖文本块 (text/plain 不是 URI) 全顺
- [ ] 审码放行

## 必读

1. /home/user/quill-impl-drag-drop/CLAUDE.md
2. /home/user/quill-impl-drag-drop/docs/conventions.md
3. /home/user/quill-impl-drag-drop/docs/invariants.md
4. /home/user/quill-impl-drag-drop/tasks/T-0611-drag-drop-files.md (派单)
5. /home/user/quill-impl-drag-drop/src/wl/window.rs (T-0607 wl_data_device + paste pipe 现状, 你扩 DnD event)
6. /home/user/quill-impl-drag-drop/src/wl/selection.rs (T-0607 SelectionState + bracketed_paste_wrap, 你复用 wrap)
7. WebFetch https://wayland.app/protocols/wayland (wl_data_device 协议 events)
8. RFC 2483 (text/uri-list mime spec)
9. RFC 8089 (file URI scheme)

## 已知陷阱

- **Wayland DnD 协议异步**: Drop 后 compositor 通过 receive 给 fd, quill 走异步
  read 同 T-0607 paste 路径. 不要 block 主循环
- **drop_performed event vs 协议 v3 vs v4**: wl_data_device v3+ 有 drop_performed
  确认, sctk 0.19.2 已封装. 用 sctk 的 device handle 不直接 raw event
- **多 mime accept**: source offer 多 mime 时 Enter event 给所有 mime, accept_mime
  选 text/uri-list 优先. 选错了 (e.g. 选 text/plain) 拿到的可能是 file 名而不是
  路径
- **URL decode 边缘**: `%2F` (slash) / `%20` (space) / `%E4%B8%AD` (UTF-8 字节) 全
  要 decode. percent-encoding crate 是标准 (但派单不引新 crate, 用 std 自实)
- **shell escape 跟 bracketed paste 联动**: bracketed paste 启用时, paste 内容是
  literal 不被 shell 解释, 但 Claude Code TUI 不一定走 shell — Claude Code 自己
  parse, 可能仍当 shell 命令处理. 走 POSIX 单引号 escape 最稳
- **拖图片 / 二进制**: source offer 可能给 image/png 等 mime. 当前 Out 段拒绝 (只
  接 file URI), tracing warn 不崩
- **path 不存在**: 拖入的路径可能 source 端有效但 quill 端不存在 (跨用户 / 跨 mount).
  不验存在性, 直接插入 cmdline, 让 user / shell / Claude Code 自己处理 errno
- **INV-010**: wl_data_offer / wl_data_device 类型仍仅 src/wl/window.rs 模块私有,
  PathBuf 是 std 类型 OK
- 不要 git add -A
- HEREDOC commit
- ASCII name writer-T0611

## 路由

writer name = "writer-T0611"。

## 预算

token=200k, wallclock=3-4h.
