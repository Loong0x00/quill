//! `quill-dump` —— 最小诊断客户端 (Phase 7 T2, ADR-0015 Phase 1 §8)。
//!
//! 连 `quill-kernel` 的 unix socket,读一条快照(`Snapshot`)JSON 行,反序列化后
//! 把 dims / cursor / 每行 `row_texts` 打到 stdout。验"被驱动的 tab → Snapshot →
//! unix socket → 客户端"这条 spine 通了。
//!
//! 用法:`quill-dump [--socket=<path>]`(默认与 daemon 同路径)。

// ADR 0001:crate 根 deny,与 lib/main/quill-kernel 一致(本 bin 无 unsafe)。
#![deny(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::{anyhow, Context, Result};

use quill::kernel::daemon;
use quill::kernel::proto::Snapshot;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let socket_path = match daemon::parse_socket_arg(&args) {
        Some(p) => p,
        None => daemon::default_socket_path()?,
    };

    let stream = UnixStream::connect(&socket_path).with_context(|| {
        format!(
            "连接 socket {} 失败 (quill-kernel daemon 在跑吗?)",
            socket_path.display()
        )
    })?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .context("读 Snapshot JSON 行失败")?;
    if n == 0 {
        return Err(anyhow!("连接已关闭,未收到任何 Snapshot"));
    }

    let snap: Snapshot = serde_json::from_str(line.trim_end()).context("反序列化 Snapshot 失败")?;

    // why writeln! 而非 println!:dump 工具的产物本就是 stdout 文本(非调试日志),
    // 用显式 Writer + `?` 传播 broken-pipe,符合 CLAUDE.md「println! 仅指调试」精神。
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "tab_id={} dims={}x{} cursor=(col={},line={}) visible={} shape={:?} title={:?}",
        snap.tab_id,
        snap.cols,
        snap.rows,
        snap.cursor.col,
        snap.cursor.line,
        snap.cursor.visible,
        snap.cursor.shape,
        snap.title,
    )?;
    writeln!(out, "--- row_texts ({} rows) ---", snap.row_texts.len())?;
    for (i, row) in snap.row_texts.iter().enumerate() {
        writeln!(out, "{i:>3} |{row}")?;
    }

    Ok(())
}
