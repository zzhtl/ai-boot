//! 飞书开放平台协议层：长连接收事件与卡片回调，以及少量 OpenAPI。
//!
//! 这里不做任何业务判断——白名单、会话、卡片内容都在上层。

pub mod api;
pub mod event;
pub mod ws;
