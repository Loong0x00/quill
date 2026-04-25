// 与 main.rs 同策略:deny + 局部 allow,让 ADR 0001 的"显式豁免"机制可用。
#![deny(unsafe_code)]

pub mod frame_stats;
pub mod ime;
pub mod pty;
pub mod term;
pub mod text;
pub mod wl;
