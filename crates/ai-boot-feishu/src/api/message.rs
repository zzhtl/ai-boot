//! 消息接口。

use serde::Deserialize;

use super::client::{ApiClient, ApiError, Call, path_segment};

/// 发送或回复成功后返回的消息。
#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: String,
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub root_id: Option<String>,
    /// 以话题形式回复时才有。
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// 回复一条消息。
pub struct Reply<'a> {
    /// 被回复的消息。
    pub message_id: &'a str,
    /// `text` / `post` / `interactive` 等。
    pub msg_type: &'a str,
    /// 对应 `msg_type` 的 JSON 字符串。
    pub content: String,
    /// 以话题形式回复；被回复的消息已在话题里时，飞书默认就回在话题里。
    pub reply_in_thread: bool,
    /// 幂等键：同一 uuid 在 1 小时内至多成功回复一次。重试依赖它不重复发。
    pub uuid: String,
}

impl ApiClient {
    pub async fn reply(&self, reply: Reply<'_>) -> Result<SentMessage, ApiError> {
        let path = format!(
            "open-apis/im/v1/messages/{}/reply",
            path_segment(reply.message_id)?
        );
        let body = serde_json::json!({
            "msg_type": reply.msg_type,
            "content": reply.content,
            "reply_in_thread": reply.reply_in_thread,
            "uuid": reply.uuid,
        });
        self.call(Call::post(path, body)).await
    }
}
