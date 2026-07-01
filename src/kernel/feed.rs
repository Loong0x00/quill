//! 父 ↔ 子 (E′) 共享子进程的轻量二进制喂料帧 (Phase 7 T6 砖1a, ADR-0018)。
//!
//! E′ 架构 (ADR-0018):桌面 quill **父进程 own PTY + 直接渲染**;共享 = opt-in 隔离
//! **子进程** (ADR-0017 的单线程 calloop WS-kernel),字节**从父的 tee 来、不自 spawn
//! shell**;子的 WS 输入**回灌父** (父 own PTY)。父 ↔ 子不走 [`crate::kernel::proto`]
//! 的 JSON (那是控制面、跨"网络"给浏览器的),改走这里定义的**轻量二进制帧**:父→子
//! 是每-chunk 热 tee 路径,跑 `serde_json` 违背 ADR-0018 的"近免费"要求。
//!
//! ```text
//! 定长 21 字节头 + 变长 payload:
//! [kind:u8][ws_id:u64 LE][tab_id:u64 LE][len:u32 LE][payload: len 字节]
//! ```
//!
//! - **小端、手写** (不引 byteorder 等新依赖,见 ADR-0018 "零新依赖")。
//! - **(workspace, tab) 标签落二进制头** (借砖0 `StreamFocus` 概念,热路径零额外解析)。
//! - **kind**:`PtyOutput`(父→子,payload = 原始 PTY 字节)/ `Input`(子→父,payload =
//!   手机输入字节)/ `FocusChange`(父→子,payload 空,ws/tab = 新焦点)/ `Dims`(父→子,
//!   payload = `cols:u16 LE + rows:u16 LE`,桌面 PTY 当前尺寸 —— 子据此让客户端 xterm.js 按
//!   【桌面宽】渲染,忠实镜像与桌面像素对齐、宽度不匹配的折行 artifact 消失,A′ 增量1)/
//!   `WorkspaceAdd` `WorkspaceRemove`(父→子,带元数据,砖1a 仅留接口)。
//!
//! 编码走 [`encode_into`](append 进调用方 buffer,tee 热路径复用一块 buffer 零额外
//! 分配)或便利的 [`FeedFrame::encode`](自带分配)。解码走 [`FeedDecoder`] **增量**
//! 解析 —— 从 pipe 流式喂字节,处理**半包**(header / payload 没收全 → 等更多)与**粘
//! 包**(一次 `read` 含多帧 → 逐帧吐出),与 WS 帧解析同范式。

/// 帧头定长字节数:`kind(1) + ws_id(8) + tab_id(8) + len(4)`。
pub const FEED_HEADER_LEN: usize = 1 + 8 + 8 + 4;

/// 单帧 payload 上限 (防对端 bug / 流错位时读到天文数字 `len` → 巨额分配 / OOM)。
/// 16 MiB 远超正常 PTY tee chunk (KB 级) 与控制元数据,正常路径绝不触顶;触顶即视为
/// 流错位致命错 (父链可信但仍防御)。
pub const MAX_FEED_PAYLOAD: usize = 16 * 1024 * 1024;

/// 帧类型 (`kind` 字节)。判别父↔子方向 + payload 语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// 父→子:`payload` = 原始 PTY 输出字节 (父为渲染本就读进 buffer 的那批,tee 一份)。
    PtyOutput,
    /// 子→父:`payload` = 手机 (WS 客户端) 输入字节,父写进 PTY (子自己不写 PTY)。
    Input,
    /// 父→子:焦点切换。`payload` 空;`ws_id` / `tab_id` = 新焦点 (子据此更新 StreamFocus
    /// 广播给浏览器)。
    FocusChange,
    /// 父→子:桌面 PTY 当前尺寸。`payload` = `cols:u16 LE + rows:u16 LE`(见 [`encode_dims`] /
    /// [`decode_dims`]);`ws_id` / `tab_id` = 该尺寸所属焦点 (子转成 [`crate::kernel::proto::
    /// ServerMsg::Dims`] 控制帧广播,客户端据此 `term.resize` 到桌面宽,A′ 增量1)。
    Dims,
    /// 父→子:工作区新增 (带元数据,砖1a 仅留接口,控制面元数据接线在砖1b)。
    WorkspaceAdd,
    /// 父→子:工作区移除 (同 [`FrameKind::WorkspaceAdd`],砖1a 仅留接口)。
    WorkspaceRemove,
    /// 父→子 (砖2 B2):**整份 tab 列表**。`ws_id` = 工作区 id,`tab_id` = 桌面焦点 tab id
    /// (冗余便利,权威在 payload 的 active_idx);`payload` = [`encode_tab_list`] 编码的
    /// (active_idx + 每 tab 的 id/title)。任何 tab 增删 / 换序 / 焦点变时父【重发整份】(非增量)
    /// → 子据此重建 [`crate::kernel::proto::WorkspaceInfo`] 广播给手机(tab 栏)。整份重发省掉一
    /// 整类增量 desync bug(daily-drive tab 个位数,整份也就几十字节)。
    TabList,
    /// 子→父 (砖2 B4):手机发起的 tab 操作 (New / Close / Reorder) 经 back-channel 回灌父,父调既有
    /// `apply_tab_op` 执行 (E′ 里 PTY/TabList 归父)。`payload` = [`encode_tab_op`] 编码的 op;
    /// Select 是【子本地】切 viewed 不回父(见 daemon `handle_client_msg`),故不走本帧。
    TabOp,
}

impl FrameKind {
    /// 线缆字节值。显式写死 (非 `as u8`),避免日后增删 variant 时静默改变线缆编号。
    pub fn to_u8(self) -> u8 {
        match self {
            FrameKind::PtyOutput => 1,
            FrameKind::Input => 2,
            FrameKind::FocusChange => 3,
            FrameKind::WorkspaceAdd => 4,
            FrameKind::WorkspaceRemove => 5,
            // Dims=6:在 enum 里排在 WorkspaceAdd 前,但线缆字节显式延后到 6,保住既有
            // WorkspaceAdd=4 / WorkspaceRemove=5 的编号不变(线缆兼容)。
            FrameKind::Dims => 6,
            FrameKind::TabList => 7,
            FrameKind::TabOp => 8,
        }
    }

    /// 从线缆字节解析;未知值返 `None` (调用方按 [`FeedError::InvalidKind`] 处理)。
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameKind::PtyOutput),
            2 => Some(FrameKind::Input),
            3 => Some(FrameKind::FocusChange),
            4 => Some(FrameKind::WorkspaceAdd),
            5 => Some(FrameKind::WorkspaceRemove),
            6 => Some(FrameKind::Dims),
            7 => Some(FrameKind::TabList),
            8 => Some(FrameKind::TabOp),
            _ => None,
        }
    }
}

/// 一条解出的喂料帧 (owned)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedFrame {
    pub kind: FrameKind,
    pub ws_id: u64,
    pub tab_id: u64,
    pub payload: Vec<u8>,
}

impl FeedFrame {
    /// 编码成新分配的字节向量 (便利;tee 热路径请用 [`encode_into`] 复用 buffer)。
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(FEED_HEADER_LEN + self.payload.len());
        encode_into(&mut out, self.kind, self.ws_id, self.tab_id, &self.payload);
        out
    }
}

/// 把一帧 **append** 进 `buf` (头 + payload)。tee 热路径复用同一块 `buf`(`clear` 后
/// 重填)即零额外分配。`payload` 长度超 `u32` 会被 `len as u32` 截断 —— 实际 PTY chunk
/// 远低于 4 GiB,调用方 (tee 单批 KB 级) 不会触及;[`MAX_FEED_PAYLOAD`] 在解码侧兜底。
///
/// 调用方**必须**保证 `payload.len() <= MAX_FEED_PAYLOAD`(解码侧会拒超限帧;子→父大输入由
/// daemon 分帧保证)。`debug_assert` 防回归:任何把 > 16 MiB 单帧塞进来的新调用点(或忘了
/// 分帧)在 debug build 立即炸,免得 `as u32` 静默截断 → 父侧 framing 错位。
pub fn encode_into(buf: &mut Vec<u8>, kind: FrameKind, ws_id: u64, tab_id: u64, payload: &[u8]) {
    debug_assert!(
        payload.len() <= MAX_FEED_PAYLOAD,
        "feed 帧 payload {} 超上限 {MAX_FEED_PAYLOAD}(调用方须分帧;否则 as u32 截断致 framing 错位)",
        payload.len()
    );
    buf.push(kind.to_u8());
    buf.extend_from_slice(&ws_id.to_le_bytes());
    buf.extend_from_slice(&tab_id.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
}

/// [`FrameKind::Dims`] payload 字节数:`cols:u16 LE + rows:u16 LE`。
pub const DIMS_PAYLOAD_LEN: usize = 4;

/// 编码 [`FrameKind::Dims`] 的 payload(`cols:u16 LE + rows:u16 LE`)。父侧 tee 路径用,
/// 配 [`encode_into`] 拼整帧。
pub fn encode_dims(cols: u16, rows: u16) -> [u8; DIMS_PAYLOAD_LEN] {
    let mut p = [0u8; DIMS_PAYLOAD_LEN];
    p[0..2].copy_from_slice(&cols.to_le_bytes());
    p[2..4].copy_from_slice(&rows.to_le_bytes());
    p
}

/// 解码 [`FrameKind::Dims`] 的 payload → `(cols, rows)`。长度不对(流错位 / 协议不匹配)返
/// `None`,调用方忽略该帧(不致命:尺寸丢一拍下次 resize 补)。
pub fn decode_dims(payload: &[u8]) -> Option<(u16, u16)> {
    if payload.len() != DIMS_PAYLOAD_LEN {
        return None;
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Some((cols, rows))
}

/// 编码 [`FrameKind::TabList`] 的 payload(砖2 B2):`active_idx:u32 LE` + `count:u32 LE`,随后
/// `count` 个 tab 项,每项 `tab_id:u64 LE` + `title_len:u32 LE` + `title` UTF-8 字节。
///
/// 父(`window.rs`)每次 tab 增删 / 换序 / 焦点变时用它编整份列表 tee 给子;子 [`decode_tab_list`]
/// 还原后重建 [`crate::kernel::proto::WorkspaceInfo`] 广播给手机 tab 栏。`active_idx` 越界(空列表
/// 时给 0)由解码方按"无 active"处理(clamp)。
pub fn encode_tab_list(active_idx: usize, tabs: &[(u64, &str)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + tabs.len() * 16);
    out.extend_from_slice(&(active_idx as u32).to_le_bytes());
    out.extend_from_slice(&(tabs.len() as u32).to_le_bytes());
    for (id, title) in tabs {
        out.extend_from_slice(&id.to_le_bytes());
        let tb = title.as_bytes();
        out.extend_from_slice(&(tb.len() as u32).to_le_bytes());
        out.extend_from_slice(tb);
    }
    out
}

/// 解码 [`FrameKind::TabList`] 的 payload → `(active_idx, Vec<(tab_id, title)>)`。任何长度不足 /
/// title 越界(流错位 / 协议不匹配)返 `None`,调用方**忽略该帧**(非致命:元数据丢一拍下次
/// tab 变化补;不像坏 kind 那样是解码器级致命错)。非法 UTF-8 用 `from_utf8_lossy` 兜(title
/// 仅供显示,不做进一步解析)。
pub fn decode_tab_list(payload: &[u8]) -> Option<(usize, Vec<(u64, String)>)> {
    let mut p = payload;
    let active = u32_from(&mut p)? as usize;
    let count = u32_from(&mut p)? as usize;
    let mut tabs = Vec::with_capacity(count.min(1024)); // 防伪造巨 count 预分配
    for _ in 0..count {
        let id = u64_from(&mut p)?;
        let title_len = u32_from(&mut p)? as usize;
        if p.len() < title_len {
            return None;
        }
        let (title_bytes, rest) = p.split_at(title_len);
        p = rest;
        tabs.push((id, String::from_utf8_lossy(title_bytes).into_owned()));
    }
    Some((active, tabs))
}

/// 从切片头部取一个小端 `u32` 并前移游标;不足 4 字节返 `None`。
fn u32_from(p: &mut &[u8]) -> Option<u32> {
    if p.len() < 4 {
        return None;
    }
    let (head, rest) = p.split_at(4);
    *p = rest;
    Some(u32::from_le_bytes([head[0], head[1], head[2], head[3]]))
}

/// 从切片头部取一个小端 `u64` 并前移游标;不足 8 字节返 `None`。
fn u64_from(p: &mut &[u8]) -> Option<u64> {
    if p.len() < 8 {
        return None;
    }
    let (head, rest) = p.split_at(8);
    *p = rest;
    let mut a = [0u8; 8];
    a.copy_from_slice(head);
    Some(u64::from_le_bytes(a))
}

/// 解码错误。流错位 (坏 kind / 天文 len) 在可信父链下不该发生,出现即视为致命 —— 调用方
/// 应停掉子进程 (不能继续从错位的流里猜帧边界)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedError {
    /// 未知 `kind` 字节 (流错位 / 协议不匹配)。
    InvalidKind(u8),
    /// `len` 超 [`MAX_FEED_PAYLOAD`] (流错位读到天文数字,拒绝巨额分配)。
    PayloadTooLarge(usize),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeedError::InvalidKind(v) => write!(f, "未知喂料帧 kind 字节: {v}"),
            FeedError::PayloadTooLarge(n) => {
                write!(f, "喂料帧 payload 长度 {n} 超上限 {MAX_FEED_PAYLOAD}")
            }
        }
    }
}

impl std::error::Error for FeedError {}

/// 增量喂料帧解码器:从 pipe 流式喂字节,按帧边界逐帧吐出。处理半包 (等更多) + 粘包
/// (一次吐一帧)。
///
/// **内存有界**:`consumed` 游标标记已吐出的前缀,不每帧 `drain(0..)`(O(n²) 抖动);
/// 当游标追平缓冲 (全部吐完) 或前缀超阈值时才一次性压实 (见 [`FeedDecoder::push`])。
#[derive(Debug, Default)]
pub struct FeedDecoder {
    buf: Vec<u8>,
    /// 已被 [`FeedDecoder::next_frame`] 吐出的前缀字节数 (`<= buf.len()`)。
    consumed: usize,
}

impl FeedDecoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            consumed: 0,
        }
    }

    /// 喂入一批新读到的字节 (append)。喂前先压实已消费前缀,使长寿命解码器内存不随累计
    /// 吞吐线性增长 (只保留"尚未吐出"的尾部)。
    pub fn push(&mut self, bytes: &[u8]) {
        if self.consumed == self.buf.len() {
            // 已全部吐出:整块清空 (最常见,粘包/对齐时游标常追平)。
            self.buf.clear();
            self.consumed = 0;
        } else if self.consumed >= 64 * 1024 {
            // 前缀积累过多 (大量小帧未压实):一次性丢弃已消费前缀。
            self.buf.drain(..self.consumed);
            self.consumed = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// 尝试吐出下一帧。`Ok(None)` = 尚不足一整帧 (半包,等下次 `push`);`Ok(Some)` = 一帧;
    /// `Err` = 流错位 (致命,见 [`FeedError`])。粘包时连续调用直到返 `None`。
    pub fn next_frame(&mut self) -> Result<Option<FeedFrame>, FeedError> {
        let avail = &self.buf[self.consumed..];
        if avail.len() < FEED_HEADER_LEN {
            return Ok(None); // 半包:头都没收全。
        }
        // 先校验 kind:坏 kind = 流错位,不能再信 len 去找帧边界 → 立即报错 (致命)。
        let kind = match FrameKind::from_u8(avail[0]) {
            Some(k) => k,
            None => return Err(FeedError::InvalidKind(avail[0])),
        };
        let ws_id = u64_le(&avail[1..9]);
        let tab_id = u64_le(&avail[9..17]);
        let len = u32_le(&avail[17..21]) as usize;
        if len > MAX_FEED_PAYLOAD {
            return Err(FeedError::PayloadTooLarge(len));
        }
        let total = FEED_HEADER_LEN + len;
        if avail.len() < total {
            return Ok(None); // 半包:payload 没收全。
        }
        let payload = avail[FEED_HEADER_LEN..total].to_vec();
        self.consumed += total;
        Ok(Some(FeedFrame {
            kind,
            ws_id,
            tab_id,
            payload,
        }))
    }
}

/// 小端读 `u64` (调用方保证 `b.len() >= 8`:`next_frame` 已确认 header 齐)。
fn u64_le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// 小端读 `u32` (调用方保证 `b.len() >= 4`)。
fn u32_le(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_le_bytes(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: FrameKind, ws: u64, tab: u64, payload: &[u8]) -> FeedFrame {
        FeedFrame {
            kind,
            ws_id: ws,
            tab_id: tab,
            payload: payload.to_vec(),
        }
    }

    /// 头字节布局 = 小端、定长 21 字节,payload 紧随其后。
    #[test]
    fn encode_layout_is_little_endian_fixed_header() {
        let f = frame(FrameKind::PtyOutput, 0x0102_0304_0506_0708, 0x1112, b"hi");
        let bytes = f.encode();
        assert_eq!(bytes.len(), FEED_HEADER_LEN + 2);
        assert_eq!(bytes[0], 1, "kind PtyOutput=1");
        // ws_id 小端 (低字节在前)。
        assert_eq!(
            &bytes[1..9],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        // tab_id = 0x1112 小端。
        assert_eq!(&bytes[9..17], &[0x12, 0x11, 0, 0, 0, 0, 0, 0]);
        // len = 2 小端 u32。
        assert_eq!(&bytes[17..21], &[2, 0, 0, 0]);
        assert_eq!(&bytes[21..], b"hi");
    }

    /// 每个 kind 编解码往返无损 (含空 payload 的 FocusChange)。
    #[test]
    fn roundtrip_all_kinds() {
        let cases = [
            frame(FrameKind::PtyOutput, 1, 2, b"\x1b[31mred\x1b[0m"),
            frame(FrameKind::Input, 7, 9, b"RUNDONE\n"),
            frame(FrameKind::FocusChange, 42, 43, b""),
            frame(FrameKind::Dims, 1, 7, &encode_dims(212, 56)),
            frame(FrameKind::WorkspaceAdd, 5, 0, b"meta"),
            frame(FrameKind::WorkspaceRemove, 5, 0, b""),
            frame(
                FrameKind::TabList,
                1,
                7,
                &encode_tab_list(1, &[(7, "sh"), (9, "vim")]),
            ),
            frame(FrameKind::TabOp, 1, 0, b"\x01"),
        ];
        for f in cases {
            let mut dec = FeedDecoder::new();
            dec.push(&f.encode());
            let got = dec.next_frame().expect("decode ok").expect("one frame");
            assert_eq!(got, f);
            assert!(dec.next_frame().expect("decode ok").is_none(), "只一帧");
        }
    }

    /// `encode_into` 复用同一 buffer (clear 后重填),与一次性 encode 等价。
    #[test]
    fn encode_into_reuses_buffer() {
        let mut buf = Vec::new();
        encode_into(&mut buf, FrameKind::PtyOutput, 1, 1, b"aaa");
        let first = buf.clone();
        buf.clear();
        encode_into(&mut buf, FrameKind::PtyOutput, 1, 1, b"aaa");
        assert_eq!(buf, first, "复用 buffer 与新分配编码一致");
    }

    /// 半包:头没收全 → None;补齐头但 payload 没全 → None;payload 补齐 → 完整帧。
    #[test]
    fn partial_header_then_payload() {
        let f = frame(FrameKind::PtyOutput, 3, 4, b"hello world");
        let bytes = f.encode();
        let mut dec = FeedDecoder::new();

        // 只喂前 5 字节 (头不全)。
        dec.push(&bytes[..5]);
        assert!(dec.next_frame().expect("ok").is_none(), "头不全 → None");

        // 补到刚好头齐 (21 字节),payload 还没。
        dec.push(&bytes[5..FEED_HEADER_LEN]);
        assert!(
            dec.next_frame().expect("ok").is_none(),
            "头齐但 payload 没全 → None"
        );

        // 补一半 payload。
        let mid = FEED_HEADER_LEN + 3;
        dec.push(&bytes[FEED_HEADER_LEN..mid]);
        assert!(dec.next_frame().expect("ok").is_none(), "payload 半 → None");

        // 补齐剩余。
        dec.push(&bytes[mid..]);
        let got = dec.next_frame().expect("ok").expect("frame");
        assert_eq!(got, f);
    }

    /// 粘包:一次喂入两帧拼接 → 连续 next_frame 各吐一帧,第三次 None。
    #[test]
    fn coalesced_two_frames() {
        let a = frame(FrameKind::PtyOutput, 1, 1, b"AAA");
        let b = frame(FrameKind::Input, 2, 2, b"BBBB");
        let mut blob = a.encode();
        blob.extend_from_slice(&b.encode());

        let mut dec = FeedDecoder::new();
        dec.push(&blob);
        assert_eq!(dec.next_frame().expect("ok").expect("a"), a);
        assert_eq!(dec.next_frame().expect("ok").expect("b"), b);
        assert!(dec.next_frame().expect("ok").is_none());
    }

    /// 逐字节喂入 (最恶劣的半包):跨任意边界都能无丢、无错位地重建全部帧。
    #[test]
    fn byte_by_byte_feed_reconstructs_all() {
        let frames = [
            frame(FrameKind::FocusChange, 9, 10, b""),
            frame(FrameKind::PtyOutput, 9, 10, b"line one\r\nline two\r\n"),
            frame(FrameKind::Input, 9, 10, b"x"),
        ];
        let mut blob = Vec::new();
        for f in &frames {
            blob.extend_from_slice(&f.encode());
        }

        let mut dec = FeedDecoder::new();
        let mut out = Vec::new();
        for &byte in &blob {
            dec.push(&[byte]);
            while let Some(f) = dec.next_frame().expect("ok") {
                out.push(f);
            }
        }
        assert_eq!(out, frames);
    }

    /// 长寿命解码器内存有界:大量帧 push/drain 后缓冲不随累计吞吐线性涨。
    #[test]
    fn decoder_compacts_and_stays_bounded() {
        let f = frame(FrameKind::PtyOutput, 1, 1, &[b'z'; 1000]);
        let enc = f.encode();
        let mut dec = FeedDecoder::new();
        for _ in 0..1000 {
            dec.push(&enc);
            let got = dec.next_frame().expect("ok").expect("frame");
            assert_eq!(got.payload.len(), 1000);
            assert!(dec.next_frame().expect("ok").is_none());
        }
        // 全部吐完后,下次 push 会清空 → 缓冲远小于"1000 帧累计"。
        assert!(
            dec.buf.len() <= enc.len(),
            "缓冲应被压实,不随吞吐累计涨 (实际 {} 字节)",
            dec.buf.len()
        );
    }

    /// TabList payload 编解码往返(含空列表 / CJK 标题 / 越界 active),坏 payload 返 None。
    #[test]
    fn tab_list_payload_roundtrip_and_reject_bad() {
        // 正常:两个 tab,active=1。
        let tabs = [(7u64, "bash"), (9u64, "中文标题")];
        let enc = encode_tab_list(1, &tabs);
        let (active, got) = decode_tab_list(&enc).expect("decode ok");
        assert_eq!(active, 1);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (7, "bash".to_string()));
        assert_eq!(got[1], (9, "中文标题".to_string()));

        // 空列表。
        let empty = encode_tab_list(0, &[]);
        let (a, g) = decode_tab_list(&empty).expect("empty decode");
        assert_eq!(a, 0);
        assert!(g.is_empty());

        // 截断(声明 count=1 但无 tab 数据)→ None。
        let mut bad = Vec::new();
        bad.extend_from_slice(&0u32.to_le_bytes()); // active
        bad.extend_from_slice(&1u32.to_le_bytes()); // count=1
        assert_eq!(decode_tab_list(&bad), None, "count 声明与数据不符应拒绝");

        // title_len 越界 → None。
        let mut bad2 = Vec::new();
        bad2.extend_from_slice(&0u32.to_le_bytes()); // active
        bad2.extend_from_slice(&1u32.to_le_bytes()); // count=1
        bad2.extend_from_slice(&7u64.to_le_bytes()); // tab_id
        bad2.extend_from_slice(&99u32.to_le_bytes()); // title_len=99 但无字节
        assert_eq!(decode_tab_list(&bad2), None, "title_len 越界应拒绝");

        // 头都不足 → None。
        assert_eq!(decode_tab_list(&[]), None);
        assert_eq!(decode_tab_list(&[1, 2, 3]), None);
    }

    /// Dims payload 编解码往返 + 长度错拒绝。
    #[test]
    fn dims_payload_roundtrip_and_reject_bad_len() {
        for (c, r) in [(80u16, 24u16), (1, 1), (u16::MAX, u16::MAX), (212, 56)] {
            let p = encode_dims(c, r);
            assert_eq!(p.len(), DIMS_PAYLOAD_LEN);
            assert_eq!(decode_dims(&p), Some((c, r)));
        }
        assert_eq!(decode_dims(&[]), None, "空 payload 拒绝");
        assert_eq!(decode_dims(&[1, 2, 3]), None, "短 payload 拒绝");
        assert_eq!(decode_dims(&[1, 2, 3, 4, 5]), None, "长 payload 拒绝");
    }

    /// 坏 kind 字节 → InvalidKind (流错位致命错)。
    #[test]
    fn invalid_kind_errors() {
        let mut bytes = frame(FrameKind::PtyOutput, 1, 1, b"x").encode();
        bytes[0] = 0xFF; // 篡改 kind
        let mut dec = FeedDecoder::new();
        dec.push(&bytes);
        assert_eq!(dec.next_frame(), Err(FeedError::InvalidKind(0xFF)));
    }

    /// len 超上限 → PayloadTooLarge (拒绝天文分配),且在 payload 尚未到齐时即报 (不傻等)。
    #[test]
    fn oversized_len_errors_before_waiting() {
        let mut buf = Vec::new();
        buf.push(FrameKind::PtyOutput.to_u8());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&1u64.to_le_bytes());
        buf.extend_from_slice(&(MAX_FEED_PAYLOAD as u32 + 1).to_le_bytes());
        // 故意不附 payload:超大 len 应在等 payload 前就报错。
        let mut dec = FeedDecoder::new();
        dec.push(&buf);
        assert_eq!(
            dec.next_frame(),
            Err(FeedError::PayloadTooLarge(MAX_FEED_PAYLOAD + 1))
        );
    }
}
