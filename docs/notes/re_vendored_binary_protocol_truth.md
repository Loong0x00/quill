<!-- migrated from claude memory: reverse_engineer_vendored_binary_for_protocol_truth_2026-04-26.md on 2026-05-11 -->
---
name: 反编译 vendored binary 找协议真值 (比 RFC/猜测/实验快 10x)
description: 集成第三方应用 (Claude Code / VS Code / Slack) 时, 不要靠协议 RFC 推 / 不要猜 / 不要实验 trial-error. 直接 strings + grep 反编译它的 binary 找 keybindings.json / 配置 fragment, 拿到字面真值. T-0612 Shift+Enter 找到 \x1b\r 用了 5 分钟
type: feedback
originSessionId: 8250e730-19a5-479b-86d8-29f26fec010b
---
**经验**: 当 quill 这种 terminal / IDE adjacent project 需要发出某 escape
sequence 让特定应用 (Claude Code TUI / VS Code 扩展 / Slack) 接受, 不要:
- ❌ 推 xterm/kitty/CSI-u/modifyOtherKeys 协议 (太多种, 应用支持哪个不
  确定)
- ❌ 猜 \n vs \r vs \x1b\r vs CSI-u (4 种全试一遍要 30 分钟)
- ❌ 调试 / 实验 (Claude Code TUI 没 verbose log)

**直接反编译目标应用 binary**:
```bash
# Claude Code 装在 ~/.local/share/claude/versions/X.Y.Z (228MB ELF binary)
strings /home/user/.local/share/claude/versions/2.1.119 \
  | grep -aiE "shift.?enter|shift.?return|shiftEnter" \
  | head -20

# 找到 VS Code keybindings.json 字面写法:
# {key:"shift+enter", command:"workbench.action.terminal.sendSequence",
#  args:{text:"\x1B\r"}, when:"terminalFocus"}
```

5 分钟拿到字面真值 \x1b\r (Esc + CR = Alt-Enter / Esc-Cr alacritty 配置同款).
直接 hardcode 在 quill keyboard.rs Return + shift_active 路径.

**Why 比其它方法快**:
- 应用 binary 自己**就是协议规范**, 反编译就是直接读权威 source
- bun-compiled / electron / single binary app 大部分 strings 可读 (JS bundle
  没混淆, Rust binary 也不 strip 字符串)
- 比"读应用文档"快 (大部分应用不公开内部 keybinding 配置)
- 比"问 maintainer"快 (Mitchell Hashimoto 不会回邮件)
- 比"trial-error"快 (无 verbose log 就只能猜)

**适用场景**:
- terminal 模拟器 → IDE / TUI app 兼容性 (Shift+Enter / 修饰键 / Tab
  completion / 颜色协议)
- 浏览器扩展 / 自动化 → 网页 app (找内部 API endpoint 字符串)
- 调试 closed-source CLI tool 行为 (gh / docker / kubectl 这种)

**实证**:
- T-0612 Shift+Enter (2026-04-26): 5 分钟反编译 → \x1b\r → Lead 直修 5 行
  hotfix → user 实测 work
- 之前我猜的"应该是 C (CSI-u 协议)" 是错的, 反编译验证才知是 B
  (\x1b\r alt-enter)

**反编译命令模板**:
```bash
# bun / node single binary
strings BINARY | grep -aiE "KEY_PATTERN" | head -20

# Electron app (asar archive)
npx asar extract path/to/app.asar /tmp/app && rg "KEY_PATTERN" /tmp/app

# Rust binary (release strip 后字符串少, debug build 够多)
strings BINARY | grep -aiE "KEY_PATTERN"
```

**怎么记忆**: 集成第三方应用前**先 strings 找它内部对协议的真实用法**,
比 RFC + 实验都快. 应用自己写的 keymap config 就是它对协议的承诺。

跨项目复用: 任何"我要让 X 应用接受我发的字节"问题, 反编译 X 比读 X
文档 / 问 X maintainer / 实验都快.
