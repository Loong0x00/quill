# ADR 0014: 补全 `--help` 探测套 bwrap 沙箱

## Status

Accepted, 2026-06-20

## Context

inline-completion (ADR 0013) 的 help-indexer 为抓补全候选, 会自动执行
`<binary> --help`:

- 启动时后台扫 **整条 `$PATH`** 的每个可执行文件 (`completion/bootstrap.rs`);
- 敲键时把当前输入的第一个 token 在 30ms 后探测一次 (`help_indexer.rs` `query_sync`)。

危险在于: 任何可执行文件只要 **不把 `--help` / 未知参数当无副作用 no-op**, quill
就会在用户**根本没敲过命令**的情况下后台静默触发它的副作用。

**真因 trigger (实锤事故)**: `gpu-psu-hold.sh --help` 把 `--help` 当成频率参数 →
`systemctl stop lactd` → 每次开终端弹 polkit "停止 lactd 需认证" 框。本机一批自写
脚本属同一危险类 (`nvidia-reset.sh` 整套 GPU 驱动卸载 + PCIe 总线复位、
`gpu_psu_test/run.sh` 锁频 + 满载循环)。原有 2s 超时 + setsid + killpg **挡不住
"瞬时" 副作用** (`systemctl stop` 远在 2s 内完成)。

逐个给脚本打参数守卫是**打地鼠**: 风险面 = quill 行为 × 全机任意可执行, 无穷且随
新脚本持续增长, 漏一个就中招。治本是让**探测本身不可能造成副作用**。

CLAUDE.md "依赖加新 crate → 必须 ADR" + 禁止清单 "数据流变跳过 ADR" 双重触发本 ADR
(bwrap 是运行时二进制依赖而非 crate, 且改了每次 `--help` 探测的 exec 数据流)。

## Decision

把 `--help` 探测的 exec 丢进 **bwrap (bubblewrap) 沙箱** 跑, 对外部 `bwrap` 二进制
建立**硬运行时依赖**。

沙箱参数 (`SANDBOX_ARGS`, `help_indexer.rs`):

```
--ro-bind / /  --dev /dev  --proc /proc  --tmpfs /run  --tmpfs /tmp
--unshare-all  --die-with-parent  --new-session  --chdir /tmp
```

效果 (本机逐条实测验证): 被探测程序即便把 `--help` 当破坏性参数, 也无法 **写宿主
文件** (整根只读 + `/tmp` `/run` 为独立 tmpfs)、**连 systemd/dbus** (socket 被 tmpfs
遮蔽)、**碰 GPU 设备** (`--dev` 极简 `/dev`, 无 `nvidia*`)、或 **杀宿主进程** (PID
namespace)。而 `--help` 的 stdout 照常流回 → **补全能力零损失**。

数据流: `run_help_command` (唯一 exec 咽喉) 改为经 `build_probe_command` 构造命令,
启动扫 + 实时探测两条入口共用 → 一处覆盖全部。开关用 `HelpIndexerConfig.sandbox`
与 `BootstrapConfig.sandbox` 两个字段, **默认 `true` (secure-by-default)**, 仅测试设
`false`。

`bwrap` **只从固定系统绝对路径** (`/usr/bin/bwrap` 等) 解析, 故意不走 `$PATH` ——
否则用户可写且排在前面的 PATH 目录 (`~/.local/bin` …) 里放个假 bwrap, 就能让探测
"自以为在沙箱里" 却裸跑, 反绕过沙箱 + 脚本拦截。可用性靠一次性功能探测
(`bwrap … -- /bin/sh -c 'exit 0'`) 判定并 `OnceLock` 缓存。

### 退化路径 (无 bwrap / 内核禁 unprivileged userns)

拒绝探测高危类: **解释型脚本** (`#!` 开头)、**组或他人可写**、**setuid/setgid**;
其余受信 ELF 仍直接探测 (绝大多数 CLI 用规范 arg parser, `--help` 安全)。
setuid/setgid 检查在沙箱判定**之前**, 两条路都拦。

## Alternatives

### Alt 1: 逐个脚本打参数守卫 (打地鼠)
风险面无穷且随新脚本增长, 漏一个就中招。作为**防御纵深**仍对已确认的 3 个脚本补了
守卫, 但不能当唯一防线。

### Alt 2: 只探测白名单 / 已知安全二进制
名单太窄则补全大面积失效, 太宽则维护成本爆炸。沙箱保留全部补全能力, 完胜。

### Alt 3: 维护 `deny_list` (代码里已有 plumbing)
黑名单形状脆 (新脚本默认危险却不在名单)。沙箱根治后无需维护; plumbing 暂留不删
(单独清理, 不混进本次 commit)。

### Alt 4: 自己用 `unshare(2)` / seccomp 手搓沙箱
等于重写 bwrap。AF_UNIX / dbus / 设备 / 挂载的正确隔离极易写错 (这本就是 bwrap 的
价值)。bwrap 在 Arch 是 flatpak 依赖基本必装; 缺失时退化路径兜底。

## Consequences

- 正面: "补全探测触发副作用" **整类** bug 被根除 (而非逐个堵), 且面向未来新脚本。
- 负面: 启动扫每个 PATH binary 多一次 bwrap fork/exec + namespace 建立 (~毫秒级,
  后台线程, 非热路径); `--help` 需联网 / dbus / 设备 / 写 `$HOME` 才能输出的少数
  工具会拿不到补全 (negative-cache 24h) —— 可接受。
- 机密性边界: 沙箱是**副作用遏制**, 非保密 (整根只读仍可读 `~/.ssh` 等, 但 net 已
  unshare 无外传通路); 暴露面与"正常运行该工具"等同, 不引入新泄露。
