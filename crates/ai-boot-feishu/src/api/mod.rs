//! 飞书 OpenAPI 的精简封装：只包含用到的接口。

mod bot;
mod client;
mod contact;
mod message;

pub use bot::BotInfo;
pub use client::{ApiClient, ApiError};
pub use message::{Reply, SentMessage};
