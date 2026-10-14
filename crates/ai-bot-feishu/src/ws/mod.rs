//! 长连接：事件订阅与卡片回调的接收通道。

mod ack;
mod assembler;
mod client;
pub mod frame;
mod go_sdk;

pub use client::{DataKind, FrameHandler, WsClient, WsConfig};
pub use go_sdk::run_go_sdk;
