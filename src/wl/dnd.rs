//! T-0611 DnD (drag-and-drop) 文件 → 路径插入的纯逻辑层.
//!
//! ## 模块边界 (INV-010 类型隔离)
//!
//! 本模块**完全不引** wayland-client / wayland-protocols / sctk 类型. 输入是
//! `&str` (text/uri-list raw bytes 解码后) + 输出是 `Vec<PathBuf>` / `String`.
//! 真协议路径 (`wl_data_device::Event::{Enter, Motion, Drop, Leave}`) 在
//! `wl/window.rs` 持协议 handle, 通过本模块的纯 fn 把 URI bytes → cmdline 字符串.
//!
//! ## 公开 API
//!
//! - [`parse_uri_list`]: text/uri-list (RFC 2483) → `Vec<PathBuf>`. 跳过 `#`
//!   注释行, 仅接 `file://` scheme + 拒绝带 host 的 file URI (RFC 8089 §4.2),
//!   percent-decode (`%XX` → bytes → UTF-8 string).
//! - [`shell_escape_path`]: POSIX shell 单引号包裹 + 内部 `'` → `'\''`. 与 zsh /
//!   bash drag-and-drop 标准行为一致, Claude Code TUI / shell readline 接到都能
//!   作单一 token 解析.
//! - [`build_drop_command`]: `Vec<PathBuf>` → 空格分隔的 shell-escaped 命令行.
//!
//! ## why 自实 URL decode 而非 percent-encoding crate
//!
//! 派单硬约束"不引新 crate". std 已含 `u8::from_str_radix` + UTF-8 验证, 自实
//! ~30 行覆盖 RFC 3986 §2.1 + RFC 8089 file URI 已足. percent-encoding crate
//! 仅做同样事 + 加一层 trait 抽象, 收益不抵新 dep 成本.
//!
//! ## why 不验存在性
//!
//! 派单已知陷阱: 拖入路径可能 source 端有效但 quill 端不存在 (跨用户 / 跨 mount).
//! 不在本层验 — 让 user / shell / Claude Code 自己处理 errno (与 zsh 拖文件
//! 同行为, 拖什么插什么).

use std::path::PathBuf;

/// 解析 text/uri-list (RFC 2483) 字节, 返 `file://` scheme 的本地路径列表.
///
/// **格式** (RFC 2483):
/// - 每行一个 URI, `\r\n` 或 `\n` 分隔
/// - `#` 开头行是注释, 跳过
/// - 空行跳过
///
/// **过滤**:
/// - 仅 `file://` scheme; `https://` / `ftp://` 等其它 scheme 静默丢弃 (调用方
///   自行 tracing::warn). 派单 In #C "非 file:// scheme: tracing warn 不消费".
/// - `file://hostname/...` 拒绝 (RFC 8089 §4.2 — host 不空时该 URI 指远程主机
///   的文件, quill 非网络透明终端). 仅接 `file:///path` (空 host) 或老式
///   `file:/path` (无 host 部分).
/// - URL decode 失败 (无效 `%XX` 或 decode 后非 UTF-8) → 跳过该 URI.
///
/// **why `Vec<PathBuf>` 而非 `Iterator`**: 拖入文件数典型 < 100, 分配开销 <<
/// shell-escape + bracketed wrap + pty.write 总成本. KISS (与 selection.rs
/// `selected_cells_*` 同决策).
pub fn parse_uri_list(input: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for raw_line in input.split(['\n', '\r']) {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(path) = parse_file_uri(line) {
            out.push(path);
        }
    }
    out
}

/// 单条 URI → `Option<PathBuf>` (仅 file:// scheme + 空 host + 合法 percent
/// decode 时 `Some`).
///
/// 拆出独立 fn 让单测易覆盖各 reject 路径.
fn parse_file_uri(uri: &str) -> Option<PathBuf> {
    // RFC 8089: file URI 形式 "file:" hier-part. 接受 "file://path" (空 host)
    // 与 "file:/path" (无 authority). "file://host/path" host 非空时拒绝.
    let after_scheme = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("file:"))?;

    // 若原 URI 是 "file://...", after_scheme 当前可能是 "/path" (空 host) 或
    // "host/path". 区分: 若开头是 '/', host 空, after_scheme 直接是路径; 否则
    // 找下一个 '/' 把 host 截掉, host 非空则拒绝.
    let path_part = if uri.starts_with("file://") {
        if let Some(rest) = after_scheme.strip_prefix('/') {
            // 空 host + 绝对路径. rest 是 "path" (不带前导 /), 后面补上.
            // 但 "//path" 也合法 (host=空, path=/path), 走 strip 一次.
            // 实际: "file:///home/user/x" → after_scheme="/home/user/x" →
            // strip_prefix('/') 后 rest="home/user/x", 我们要拼回 "/home/user/x".
            format!("/{}", rest)
        } else {
            // after_scheme 不以 '/' 开头 = "host/path" 形式, host 非空 → 拒绝.
            // RFC 8089 §4.2 "file URI with non-empty host part on non-local
            // resources is not supported".
            return None;
        }
    } else {
        // "file:/path" 形式 (无 // authority). after_scheme = "/path" 直接用.
        after_scheme.to_string()
    };

    let decoded = url_decode(&path_part)?;
    Some(PathBuf::from(decoded))
}

/// RFC 3986 §2.1 percent-decode. `%XX` → byte; 其它 char 原样. decode 后字节
/// 必须是合法 UTF-8 (filesystem 路径 99.99% UTF-8, 极端 invalid byte 直接拒绝
/// — 与 OsString::from_vec 路径分裂, 这里走 KISS).
///
/// 失败返 None: 无效 `%X<EOF>` / `%XY` (X/Y 非 hex) / decode 后非 UTF-8.
fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            // 需要后两 char 是 hex.
            if i + 2 >= bytes.len() {
                return None;
            }
            let h1 = hex_digit_value(bytes[i + 1])?;
            let h2 = hex_digit_value(bytes[i + 2])?;
            out.push((h1 << 4) | h2);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `b'0'..=b'9' / b'A'..=b'F' / b'a'..=b'f'` → 0..=15. 其它 None.
fn hex_digit_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// 路径 → POSIX 单引号 shell-escape. 含特殊字符 → 单引号包裹 + 内部 `'` →
/// `'\''` (POSIX 标准: 单引号内任何字符不解析, 真 `'` 走"先关单引 + 转义反斜
/// 杠+单引 + 再开单引"组合).
///
/// **safe-char 定义** (与 bash / zsh `printf '%q'` 对齐): `[A-Za-z0-9_./-]` 不
/// 需 escape. 中文 / emoji / 空格 / `'"$\` 等都走单引号包裹. 派单 In #D "纯
/// ASCII + 安全字符 → 原文不包".
///
/// **why 不走双引号**: 双引号内 `$` `` ` `` `\` `"` 仍被 shell 解析 — 拖入
/// `$HOME/x` 文件名会被展开成 `/home/user/x` 真因. 单引号最简单最安全 (alacritty
/// / kitty 同决策).
///
/// **why path → String**: PathBuf::to_string_lossy() — 路径含 invalid UTF-8 时
/// 用 U+FFFD 替换. quill 走纯 UTF-8 (parse_uri_list 已 utf8 验证), 不会触发
/// lossy 路径; 防御性 fallback 用 lossy 而非 panic.
pub fn shell_escape_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(is_shell_safe_char) {
        return s.into_owned();
    }
    // 单引号包裹: 'x' → 'x', 'a'b' → 'a'\''b'.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// shell 安全字符: ASCII 字母 / 数字 / `_` / `.` / `/` / `-`. 其它 (含中文 / 空
/// 格 / `'"$` 等) 都需要 escape.
///
/// **why `-` 也安全**: 路径如 `/usr/bin/foo-bar` 不需要 escape; 不跟参数 `-x`
/// 混淆是因为 cmdline 拼接时已由空格分隔, shell 按位置参数解析.
fn is_shell_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-')
}

/// `Vec<PathBuf>` → 空格分隔的 shell-escaped 命令行字符串.
///
/// 多文件: `'path with space.txt' /simple/path.rs '/another/space.md'`.
/// 单文件: `/simple/path.rs` 或 `'path with space.txt'`.
/// 空: `""` (调用方 should skip pty.write).
///
/// **不在末尾加换行 / 空格**: 让调用方决定是否包 bracketed paste + pty 输入是
/// 否需要末尾 trailing space. 与 selection.rs `extract_selection_text` 同 KISS
/// 决策.
pub fn build_drop_command(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| shell_escape_path(p))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_uri_list ----

    #[test]
    fn parse_single_file_uri() {
        let input = "file:///home/user/x.txt\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/home/user/x.txt")]);
    }

    #[test]
    fn parse_multiple_uris_lf_only() {
        let input = "file:///a\nfile:///b\nfile:///c\n";
        let paths = parse_uri_list(input);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ]
        );
    }

    #[test]
    fn parse_skips_comment_lines() {
        let input = "# this is a comment\r\nfile:///real/path\r\n# trailing comment\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/real/path")]);
    }

    #[test]
    fn parse_skips_empty_lines() {
        let input = "\r\nfile:///x\r\n\r\nfile:///y\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn parse_rejects_https_scheme() {
        let input = "https://example.com/x\r\n";
        let paths = parse_uri_list(input);
        assert!(paths.is_empty(), "https:// should be rejected");
    }

    #[test]
    fn parse_rejects_ftp_scheme() {
        let input = "ftp://server/file.txt\r\n";
        assert!(parse_uri_list(input).is_empty());
    }

    #[test]
    fn parse_rejects_file_with_nonempty_host() {
        // RFC 8089 §4.2: file://hostname/path 拒绝 (远程, 非本机).
        let input = "file://otherhost/path\r\n";
        assert!(parse_uri_list(input).is_empty());
    }

    #[test]
    fn parse_decodes_percent_space() {
        let input = "file:///home/user/My%20Doc.txt\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/home/user/My Doc.txt")]);
    }

    #[test]
    fn parse_decodes_utf8_chinese() {
        // "中" = U+4E2D = UTF-8 bytes E4 B8 AD
        let input = "file:///home/user/%E4%B8%AD%E6%96%87.txt\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/home/user/中文.txt")]);
    }

    #[test]
    fn parse_rejects_invalid_percent_escape() {
        // "%2" 不完整 — decode 失败, URI 跳过.
        let input = "file:///bad%2\r\nfile:///good\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/good")]);
    }

    #[test]
    fn parse_rejects_non_hex_percent() {
        // "%ZZ" 非 hex.
        let input = "file:///bad%ZZ\r\nfile:///good\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/good")]);
    }

    #[test]
    fn parse_handles_mixed_scheme_filtered() {
        let input = "file:///a\r\nhttps://example.com/b\r\nfile:///c\r\nftp://x/y\r\nfile:///d\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/c"),
                PathBuf::from("/d"),
            ]
        );
    }

    #[test]
    fn parse_handles_old_style_no_authority() {
        // "file:/path" 形式 (无 // authority) — 老式但合法.
        let input = "file:/etc/hosts\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(paths, vec![PathBuf::from("/etc/hosts")]);
    }

    #[test]
    fn parse_empty_input_returns_empty() {
        assert!(parse_uri_list("").is_empty());
        assert!(parse_uri_list("\r\n\r\n").is_empty());
    }

    // ---- shell_escape_path ----

    #[test]
    fn escape_safe_path_unwrapped() {
        let p = PathBuf::from("/usr/bin/foo");
        assert_eq!(shell_escape_path(&p), "/usr/bin/foo");
    }

    #[test]
    fn escape_path_with_dash_dot_underscore_safe() {
        let p = PathBuf::from("/path/to/my-file_v1.2.txt");
        assert_eq!(shell_escape_path(&p), "/path/to/my-file_v1.2.txt");
    }

    #[test]
    fn escape_space_wrapped_in_single_quotes() {
        let p = PathBuf::from("/home/user/My Doc.txt");
        assert_eq!(shell_escape_path(&p), "'/home/user/My Doc.txt'");
    }

    #[test]
    fn escape_single_quote_in_path() {
        // POSIX: 'a'b' → 'a'\''b'
        let p = PathBuf::from("/path/it's.txt");
        assert_eq!(shell_escape_path(&p), "'/path/it'\\''s.txt'");
    }

    #[test]
    fn escape_dollar_sign() {
        let p = PathBuf::from("/path/$HOME.txt");
        assert_eq!(shell_escape_path(&p), "'/path/$HOME.txt'");
    }

    #[test]
    fn escape_backslash() {
        let p = PathBuf::from("/path/back\\slash.txt");
        assert_eq!(shell_escape_path(&p), "'/path/back\\slash.txt'");
    }

    #[test]
    fn escape_double_quote() {
        let p = PathBuf::from("/path/quoted\".txt");
        assert_eq!(shell_escape_path(&p), "'/path/quoted\".txt'");
    }

    #[test]
    fn escape_chinese_path_wrapped() {
        // 中文是非 ASCII safe-char, 走单引号包裹保 cmdline 干净.
        let p = PathBuf::from("/home/user/中文.txt");
        assert_eq!(shell_escape_path(&p), "'/home/user/中文.txt'");
    }

    #[test]
    fn escape_emoji_path_wrapped() {
        let p = PathBuf::from("/home/user/🦀.rs");
        assert_eq!(shell_escape_path(&p), "'/home/user/🦀.rs'");
    }

    #[test]
    fn escape_empty_path() {
        let p = PathBuf::from("");
        assert_eq!(shell_escape_path(&p), "''");
    }

    #[test]
    fn escape_multiple_single_quotes() {
        let p = PathBuf::from("a'b'c");
        // 'a' + '\'' + 'b' + '\'' + 'c' = 'a'\''b'\''c'
        assert_eq!(shell_escape_path(&p), "'a'\\''b'\\''c'");
    }

    // ---- build_drop_command ----

    #[test]
    fn build_single_safe_path() {
        let paths = vec![PathBuf::from("/usr/bin/foo")];
        assert_eq!(build_drop_command(&paths), "/usr/bin/foo");
    }

    #[test]
    fn build_multi_safe_paths() {
        let paths = vec![
            PathBuf::from("/a/b"),
            PathBuf::from("/c/d"),
            PathBuf::from("/e/f"),
        ];
        assert_eq!(build_drop_command(&paths), "/a/b /c/d /e/f");
    }

    #[test]
    fn build_mixed_safe_and_escaped() {
        let paths = vec![
            PathBuf::from("/path/with space.txt"),
            PathBuf::from("/simple/path.rs"),
            PathBuf::from("/another/with space.md"),
        ];
        assert_eq!(
            build_drop_command(&paths),
            "'/path/with space.txt' /simple/path.rs '/another/with space.md'"
        );
    }

    #[test]
    fn build_empty_paths() {
        let paths: Vec<PathBuf> = Vec::new();
        assert_eq!(build_drop_command(&paths), "");
    }

    // ---- end-to-end uri_list → cmdline ----

    #[test]
    fn end_to_end_nautilus_drop() {
        // 模拟 Nautilus 拖 3 个文件 (空格 / 中文 / 普通) 的真 wire format.
        let input = "file:///home/user/My%20Doc.txt\r\n\
                     file:///home/user/%E4%B8%AD%E6%96%87.rs\r\n\
                     file:///etc/hosts\r\n";
        let paths = parse_uri_list(input);
        let cmd = build_drop_command(&paths);
        assert_eq!(
            cmd,
            "'/home/user/My Doc.txt' '/home/user/中文.rs' /etc/hosts"
        );
    }

    #[test]
    fn end_to_end_with_comment_and_empty_lines() {
        let input = "# uri-list version 2 marker\r\n\
                     \r\n\
                     file:///home/user/x.txt\r\n";
        let paths = parse_uri_list(input);
        assert_eq!(build_drop_command(&paths), "/home/user/x.txt");
    }
}
