//! 飞书 OpenAPI 的精简封装：只包含用到的接口。

mod bot;
mod chat;
mod client;
mod contact;
mod docs;
mod message;
mod oauth;
mod resource;

pub use bot::BotInfo;
pub use chat::Member;
pub use client::{ApiClient, ApiError, Download};
pub use docs::{BitableTable, DocMeta, GridProperties, SheetInfo, WikiNode};
pub use message::{
    Container, ItemBody, ItemMention, ItemSender, MessageItem, Page, Reply, SentMessage,
};
pub use oauth::{UserInfo, UserToken};
pub use resource::ResourceKind;
