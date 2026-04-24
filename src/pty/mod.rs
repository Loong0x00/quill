//! PTY 子进程 + master fd 封装(Phase 2)。
//!
//! 外部入口只有 [`PtyHandle`] + 五个方法,语义上对应 "shell 子进程 + 非阻塞
//! master fd" 抽象。封装策略:
//! - 整个 crate 里 **只有本模块** 能持有 PTY 相关 fd;其它模块通过
//!   [`PtyHandle::raw_fd`] 拿 `RawFd` 注册 calloop,但 **不负责 close**
//!   (INV-005 配套)。
//! - 字段声明顺序保证 drop 时 "reader → master → child" —— reader 先 drop
//!   释放 `try_clone_reader` 的 dup fd;master drop 关闭主 fd,slave 端
//!   读到 EOF / 写出 SIGHUP;child 最后 drop(**不阻塞 `wait`**,未 reap 的
//!   子进程由进程退出 / T-0205 的 `try_wait` 兜底)。见 `docs/invariants.md`
//!   INV-008。
//! - master fd 在 `spawn_program` 返回前置 `O_NONBLOCK`,防止 T-0203 读字节时
//!   把整个事件循环阻塞住(INV-009)。
//! - `spawn_shell` 与 `spawn_program` 共享 openpty + slave drop 逻辑;前者
//!   内部调后者。分两个出口是为了让 T-0206 的集成测试能直接 spawn `echo`
//!   而不需要经过 shell。
//!
//! 填实进度(Phase 2 结):
//! - T-0201:`spawn_shell` / `spawn_program`
//! - T-0202:`raw_fd`(返回缓存的 master fd)
//! - T-0203:`read`(包装 `try_clone_reader()` 的 reader)
//! - T-0204:`resize`(包装 `master.resize(PtySize)`,接口预留给 Phase 3 T-0306)
//! - T-0205:`try_wait`(包装 `child.try_wait()`,非阻塞收尸)
//!
//! `PtyHandle` 的五个 pub 方法全部填实。

use std::io::{self, Read};
use std::os::fd::RawFd;

use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

/// 子进程 + master 端封装。
///
/// 字段声明顺序即 Rust 正向 drop 顺序(见模块 doc + INV-008):
/// 1. `reader` —— 释放 `try_clone_reader` 的 dup fd
/// 2. `master` —— 关闭主 fd,slave 端读到 EOF 并/或收到 SIGHUP
/// 3. `child`  —— 持有子进程句柄,Drop 不阻塞 wait;未 reap 由进程退出 / T-0205 兜底
///
/// 三者组合在一起保证:外界只能通过显式方法观察子进程状态(`try_wait` in T-0205);
/// 丢失 handle 不会 leak fd 或 zombie(至少不会 leak **我们** 的引用)。
// T-0205 起所有字段都被 public 方法直接/间接消费,不再需要 `#[allow(dead_code)]`。
pub struct PtyHandle {
    reader: Box<dyn Read + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Master 端裸 fd,`spawn_program` 返回前由 `master.as_raw_fd()` 捕获并
    /// 校验为 `Some`;之后 [`raw_fd`] 直接返回本字段,**不再**调用
    /// `MasterPty::as_raw_fd()`(其签名 `Option<RawFd>` 迫使 unwrap,违反
    /// CLAUDE.md 的无 `unwrap` / `expect` 准则)。
    ///
    /// `RawFd` 无 `Drop`,放在 `child` 之后(最末)不影响 INV-008 的资源释放序。
    master_fd: RawFd,
}

impl PtyHandle {
    /// 起一个登录 shell(当前写死 `bash -l`),返回持有 master 端的句柄。
    ///
    /// why:Phase 2 的产出是"窗口打开后 spawn shell";这是唯一被 `main.rs` 调用
    /// 的构造入口。`cols`/`rows` 在 Phase 2 由 T-0202 硬编码 80x24,Phase 3 T-0306
    /// 接窗口 resize 后才会真动态传入。
    pub fn spawn_shell(cols: u16, rows: u16) -> Result<Self> {
        Self::spawn_program("bash", &["-l"], cols, rows)
    }

    /// 起任意程序(给 T-0206 集成测试用,将来也可能服务 Phase 6 的 shell 配置)。
    ///
    /// why:bash 作为登录 shell 会进入交互态,不便于集成测试断言具体 stdout。
    /// 暴露通用 spawn 让测试能直接跑 `echo hello` 这类一次性命令。`spawn_shell`
    /// 内部就是调本函数。
    pub fn spawn_program(program: &str, args: &[&str], cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("portable_pty openpty 失败")?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        // `CommandBuilder::new` 默认带一套 base_env(PATH / USER 等),`TERM` 不在
        // 其中。若父进程 `TERM` 已设,继承之;否则用 `xterm-256color` —— 几乎所有
        // 现代 ncurses / readline 程序都认这个值,比空字符串安全得多。
        let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
        cmd.env("TERM", term);

        // 顺序敏感:`slave.spawn_command(cmd)` **必须** 在 `drop(pair.slave)` 之前。
        // 调换顺序会导致 spawn 时 slave 已无效 / master 永远读不到 EOF(子进程
        // 退出后也没人通知)。这是 portable-pty 文档里明写过的坑,T-0201 ticket
        // Implementation notes 也复述了一遍。
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn_command({program:?}) 失败"))?;
        drop(pair.slave);

        // INV-009:master fd 必须 O_NONBLOCK。必须在返回 PtyHandle 之前设好,
        // T-0203 的 `read` 拿到的是同一 OFD(或 dup 过来的,OFD 级 flags 随 dup
        // 继承),直接读不会阻塞整个事件循环。
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("MasterPty::as_raw_fd() 返回 None —— 非 unix 后端?"))?;
        set_nonblocking(master_fd).context("master fd 设 O_NONBLOCK 失败")?;

        // reader 从 master dup 一份专读句柄。必须在 `pair.master` move 进 struct
        // 之前拿:`try_clone_reader(&self)` 借用不可变 master,move 之后借用冲突。
        let reader = pair
            .master
            .try_clone_reader()
            .context("portable_pty try_clone_reader 失败")?;

        // 字段顺序 = drop 序(INV-008):reader → master → child → master_fd(i32,无 Drop)。
        Ok(Self {
            reader,
            master: pair.master,
            child,
            master_fd,
        })
    }

    /// 返回 master fd,供 calloop `Generic` source 注册。
    ///
    /// why:INV-005 要求所有 IO fd 进同一 `calloop::EventLoop`。本方法是 PTY 这
    /// 条 fd 接入事件循环的唯一入口。**所有权仍在 `PtyHandle`**,调用方不得 close
    /// 这个 fd,drop 由 `PtyHandle::drop` 负责。
    ///
    /// 语义担保:返回的 fd **已经** 置了 `O_NONBLOCK`(由 [`spawn_program`] 在
    /// 构造时 `fcntl` 一次,见 INV-009)。[`spawn_program`] 会在 fd 为 `None`
    /// 时提前返错,这里不需要处理 `Option`,不引入 `unwrap` / `expect`。
    pub fn raw_fd(&self) -> RawFd {
        self.master_fd
    }

    /// 从 master 端非阻塞读字节。返回读到的字节数;0 表示 EOF(通常意味着子进程退出)。
    ///
    /// why:Phase 2 只把字节 `tracing::trace!` 出来(T-0203 本 ticket);Phase 3
    /// T-0302 才把字节喂 `alacritty_terminal::Term`。保持读字节与后续消费解耦,
    /// 是本方法的职责分界。master fd 必须已置 `O_NONBLOCK`(INV-009,由
    /// `spawn_program` 在构造时 `fcntl` 一次,参 `set_nonblocking`),
    /// `WouldBlock` 由调用方自行忽略(通常是"没更多数据,等下一个 readable 事件")。
    ///
    /// reader 是 `try_clone_reader()` dup 出来的独立 fd,但 OFD 级 flags 随 dup 继承,
    /// 所以 master 上设的 `O_NONBLOCK` 在 reader 上也生效。
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }

    /// 把新尺寸推给 master,底层 `ioctl(TIOCSWINSZ)` + 给前台进程组发 `SIGWINCH`。
    ///
    /// why:**Phase 2 阶段本 crate 内部无调用方**(spawn 时 cols/rows 已在
    /// `spawn_program` 写入 `PtySize`,本 Phase 不接窗口 resize)—— 接口预留给
    /// **Phase 3 T-0306** 的 Wayland `configure` → 像素尺寸 → cell 尺寸 → 本方法
    /// 那条通路。现在就开出来是为了让 `PtyHandle` 的公共 API 在 Phase 2 一次定
    /// 稳;将来 Phase 3 直接调,不用改模块内部。
    ///
    /// `&self` 而非 `&mut self` 是因为 `portable-pty::MasterPty::resize` 签名就
    /// `&self`(底层 ioctl 不改 Rust 侧状态,只改 kernel 的 winsize 元数据),
    /// 照抄减少无谓变异。
    ///
    /// `pixel_width` / `pixel_height` 写死 0 —— 语义 "client 不关心 / 未知";
    /// Phase 4 接 HiDPI 时可能要填 cell 像素尺寸给 terminal-side size-report
    /// 用,到时再扩。
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("PTY resize ioctl 失败")
    }

    /// 非阻塞查子进程退出状态。`Ok(None)` = 仍在跑,`Ok(Some(code))` = 已退出。
    ///
    /// why:T-0205 在 PTY EOF / EIO 时把子进程收尸并往主循环 `should_exit` 置位。
    /// **禁止阻塞 `wait()`**,否则整个事件循环卡死(INV-005)。
    ///
    /// 返回 code 的语义:取 `ExitStatus::exit_code() as i32`。若子进程被 signal
    /// 杀死,`portable-pty` 的 `ExitStatus::exit_code()` 通常返 0,真正 signal
    /// 信息在 `ExitStatus::signal() -> Option<&str>` 里 —— 本 API 故意**不**暴露
    /// signal 细节(T-0205 scope 只要"是否退出 + 数字 code",消费方当前只 tracing
    /// 一下,不做 shell-style `128+signum` 编码;Phase 6 加 config 时再扩)。
    pub fn try_wait(&mut self) -> Result<Option<i32>> {
        let status = self
            .child
            .try_wait()
            .context("portable-pty Child::try_wait 失败")?;
        Ok(status.map(|s| s.exit_code() as i32))
    }
}

/// 对任意 `RawFd` 开 `O_NONBLOCK`。保留成自由函数 + 明确 `SAFETY:` 注释块,
/// 与 `src/wl/render.rs` 里 wgpu 的裸指针 unsafe 风格对齐。
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY:
    // - `fd` 来自 `portable_pty::MasterPty::as_raw_fd()`,由 `openpty` 刚创建的
    //   master 端,**进程内活跃**、`PtyHandle` 尚未组装成功前不存在任何别的
    //   clone,单线程构造。
    // - `libc::fcntl` 对已关闭的 fd 返回 `EBADF` 而非 UB —— 即便调用期间某条
    //   未知路径提前关了 fd,最坏结果是本函数返回 io 错误,没有内存安全问题。
    // - `F_GETFL` / `F_SETFL` 只读写 open-file-description 级别的 status flags
    //   (O_NONBLOCK / O_APPEND 等),**不释放资源、不改 fd 所有权**,因此不会
    //   与 struct 字段的 drop 顺序(INV-008)相互作用。
    // - 返回值检查完整:两次 fcntl 都按 `< 0` 判错并转成 `io::Error::last_os_error`,
    //   没有静默吞错。
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

// ---------- test-only 帮助 ----------
//
// 本 ticket 不对外开 `raw_fd()` / `try_wait()`(见 scope Out),但单测需要验证
// O_NONBLOCK 置位 + 回收子进程。这两个帮助仅在 `#[cfg(test)]` 下编译,
// 生产代码绝对拿不到。T-0202 / T-0205 写码把公开版本填实后,这里可以移除。

#[cfg(test)]
impl PtyHandle {
    /// 测试专用:阻塞 wait 子进程退出,防止单测跑完后残留僵尸。
    ///
    /// T-0205 会把正式的 `try_wait()` 填实(非阻塞,返回 `Option<i32>`);
    /// 但 T-0205 之前,单测要在测试末尾收僵尸,只能用这个阻塞版本。
    pub(crate) fn wait_child_for_test(&mut self) -> io::Result<u32> {
        let status = self.child.wait()?;
        Ok(status.exit_code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-0201 acceptance:`spawn_program("true", &[], 80, 24)` 成功返回;
    /// 显式 wait 回收后 drop 不 panic / 不 leak zombie(测试内就能验)。
    #[test]
    fn spawn_program_true_succeeds_and_exits_cleanly() {
        let mut handle =
            PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn `true` 应成功");
        let code = handle.wait_child_for_test().expect("wait true 应成功");
        assert_eq!(code, 0, "/usr/bin/true 退出码应为 0");
        drop(handle);
    }

    /// T-0201 acceptance:`spawn_shell` 能真的拉起 `bash -l`。不做交互,不读
    /// 字节(那是 T-0203);只验证返回 Ok + drop 路径不 panic。
    #[test]
    fn spawn_shell_returns_ok_and_drops_cleanly() {
        let handle = PtyHandle::spawn_shell(80, 24).expect("spawn `bash -l` 应成功");
        // 不 wait:bash 作为交互式登录 shell 在收到 SIGHUP 前可能不退出,wait 会卡。
        // Drop 时 master 关闭 → slave EOF + SIGHUP → bash 很快退出,僵尸由 OS init
        // 在本测试进程退出后收养回收。单测 scope 只验"构造 + Drop 不 panic"。
        drop(handle);
    }

    /// T-0201 acceptance:"单测验证 O_NONBLOCK:spawn 后 fcntl(F_GETFL) 返回值与
    /// O_NONBLOCK 按位与非零"。对应 INV-009。T-0202 起 fd 通过 `raw_fd()` 获取。
    #[test]
    fn master_fd_is_nonblocking_after_spawn() {
        let mut handle = PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn");
        let fd = handle.raw_fd();

        // SAFETY: fd 仍由 handle 持有,未被 close;本次 fcntl 只读 flag,不影响 drop 序。
        #[allow(unsafe_code)]
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(
            flags >= 0,
            "fcntl F_GETFL 应成功 (errno: {})",
            io::Error::last_os_error()
        );
        assert!(
            flags & libc::O_NONBLOCK != 0,
            "INV-009: master fd 必须 O_NONBLOCK, 实际 flags = {flags:#x}"
        );

        let _ = handle.wait_child_for_test();
    }

    /// T-0202 acceptance:`raw_fd()` 返回的 fd 与 `MasterPty::as_raw_fd()` 实时
    /// 查询一致 —— 保证 `spawn_program` 缓存的 fd 没偏离真实 master fd。这一致性
    /// 被 calloop Generic source 的注册隐式依赖,断错会发生向错误 fd 注册的灾难。
    #[test]
    fn raw_fd_matches_master_live_fd() {
        let mut handle = PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn");
        let cached = handle.raw_fd();
        let live = handle
            .master
            .as_raw_fd()
            .expect("unix backend 应返回 Some(fd)");
        assert_eq!(cached, live, "raw_fd() 缓存应与 master.as_raw_fd() 一致");
        let _ = handle.wait_child_for_test();
    }

    /// 防御性:多次 spawn(串行)都能独立成功、独立 wait 回收。主要防 "一次 spawn
    /// 成功就把全局状态污染掉" 这类隐式假设。
    #[test]
    fn spawn_program_is_reusable_serially() {
        for _ in 0..3 {
            let mut h = PtyHandle::spawn_program("true", &[], 80, 24).expect("spawn");
            let code = h.wait_child_for_test().expect("wait");
            assert_eq!(code, 0);
        }
    }

    /// T-0203 acceptance:`spawn "echo hi"` 后循环非阻塞 read,聚合缓冲应以
    /// `"hi\r\n"`(PTY 默认把 `\n` 转 CRLF)或 `"hi\n"`(若 `onlcr` 被关)开头。
    /// 证明 `PtyHandle::read` 真从子进程 stdout 拿到字节 —— 这是 Phase 2 主要产出。
    #[test]
    fn read_captures_echo_hi_output() {
        use std::time::{Duration, Instant};

        let mut handle = PtyHandle::spawn_program("echo", &["hi"], 80, 24).expect("spawn echo hi");
        let mut out: Vec<u8> = Vec::new();
        let mut buf = [0u8; 256];
        let start = Instant::now();
        let deadline = Duration::from_secs(2);

        loop {
            if start.elapsed() > deadline {
                panic!("2 秒内没能读到 echo 输出,实际积累: {out:?}");
            }
            match handle.read(&mut buf) {
                Ok(0) => break, // EOF(少见;Linux PTY 常给 EIO)
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    // 读到 "hi\r\n" 或 "hi\n" 就够了,不用非得等 EOF/EIO
                    if out.starts_with(b"hi\r\n") || out.starts_with(b"hi\n") {
                        break;
                    }
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // 子进程还没写字节到 master;短 sleep 后重试。
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => panic!("非预期 read 错误: {e} (kind={:?})", e.kind()),
            }
        }

        assert!(
            out.starts_with(b"hi\r\n") || out.starts_with(b"hi\n"),
            "echo 输出应以 'hi\\r\\n' 或 'hi\\n' 开头 (PTY 默认 onlcr);实际: {out:?}"
        );

        let _ = handle.wait_child_for_test();
    }

    /// 回归守门:直接不等字节,`read` 非阻塞应立即返回 `WouldBlock`,不卡整个
    /// 事件循环(CLAUDE.md 架构不变式 3、INV-009 保障)。
    #[test]
    fn read_returns_wouldblock_when_no_data_yet() {
        // 起 /bin/sleep 让 master 100ms 内不会收到任何字节。
        let mut handle = PtyHandle::spawn_program("sleep", &["0.1"], 80, 24).expect("spawn sleep");
        let mut buf = [0u8; 32];
        // spawn sleep 0.1 不写 stdout,理论上第一次 read 立即返 WouldBlock;
        // 10 次重试是防某些 PTY 初始化瞬间字节(terminfo / termios 交涉的边界),
        // 只要能见到过一次 WouldBlock 就合格(证明非阻塞路径可达)。
        let mut saw_wouldblock = false;
        for _ in 0..10 {
            match handle.read(&mut buf) {
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    saw_wouldblock = true;
                    break;
                }
                Ok(_) => {} // 有的话继续试
                Err(ref e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => panic!("意外错误 {e:?}"),
            }
        }
        assert!(
            saw_wouldblock,
            "非阻塞 read 应至少观察到一次 WouldBlock(INV-009 未生效?)"
        );
        let _ = handle.wait_child_for_test();
    }

    /// 尺寸参数原样落到 `PtySize` —— 非 0 值不能被 silently 吞成默认值。留给 T-0204
    /// resize 之前,至少保证构造阶段传进去的尺寸不被改写。通过 `master.get_size()`
    /// 间接观察。
    #[test]
    fn master_get_size_reflects_spawn_dimensions() {
        let mut handle = PtyHandle::spawn_program("true", &[], 120, 40).expect("spawn");
        let size = handle.master.get_size().expect("get_size 应成功");
        assert_eq!(size.cols, 120, "cols 不应被改写");
        assert_eq!(size.rows, 40, "rows 不应被改写");
        let _ = handle.wait_child_for_test();
    }
}
