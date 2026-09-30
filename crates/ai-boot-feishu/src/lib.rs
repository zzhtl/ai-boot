//! 飞书开放平台协议层：长连接收事件与卡片回调，以及少量 OpenAPI。
//!
//! 这里不做任何业务判断——白名单、会话、卡片内容都在上层。

pub mod api;
pub mod event;
pub mod ws;

/// `#[serde(default)]` 只管字段缺失；飞书对没权限或不适用的字段会显式给 `null`
/// （实测：应用没开员工 ID 权限时，事件里 `user_id` 就是 `null`），也按缺省值处理，
/// 不能因为一个用不上的字段让整条事件或响应解析失败。
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(<Option<T> as serde::Deserialize>::deserialize(deserializer)?.unwrap_or_default())
}
