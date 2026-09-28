//! 长连接收到的事件负载（schema 2.0）。
//!
//! 只声明用到的字段，其余忽略：飞书会往事件里加字段，解析不能因此失败。

use serde::Deserialize;

pub const MESSAGE_RECEIVE: &str = "im.message.receive_v1";

#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub schema: String,
    pub header: EventHeader,
    #[serde(default)]
    pub event: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventHeader {
    #[serde(default)]
    pub event_id: String,
    pub event_type: String,
    #[serde(default)]
    pub create_time: String,
    #[serde(default)]
    pub app_id: String,
}

/// `im.message.receive_v1`。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageReceived {
    pub sender: Sender,
    pub message: Message,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Sender {
    #[serde(default)]
    pub sender_id: UserId,
    /// 用户消息为 `user`。
    #[serde(default)]
    pub sender_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserId {
    #[serde(default)]
    pub open_id: String,
    #[serde(default)]
    pub union_id: String,
    #[serde(default)]
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub message_id: String,
    #[serde(default)]
    pub root_id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    /// 只有话题内的消息才有。
    #[serde(default)]
    pub thread_id: Option<String>,
    pub chat_id: String,
    /// `p2p` 或 `group`。
    pub chat_type: String,
    pub message_type: String,
    /// 对应 `message_type` 的 JSON 字符串。
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<Mention>,
    /// 毫秒时间戳（字符串）。
    #[serde(default)]
    pub create_time: String,
}

/// 文本里的 `@_user_N` 占位符与被 @ 对象的对应关系。
#[derive(Debug, Clone, Deserialize)]
pub struct Mention {
    pub key: String,
    #[serde(default)]
    pub id: UserId,
    #[serde(default)]
    pub name: String,
}

impl Message {
    pub fn is_p2p(&self) -> bool {
        self.chat_type == "p2p"
    }

    pub fn mentions_open_id(&self, open_id: &str) -> bool {
        !open_id.is_empty() && self.mentions.iter().any(|m| m.id.open_id == open_id)
    }

    /// 话题 ID；空串按没有处理。
    pub fn thread(&self) -> Option<&str> {
        self.thread_id.as_deref().filter(|t| !t.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档里的样例（字段有删减）。
    const SAMPLE: &str = r#"{
      "schema": "2.0",
      "header": {"event_id": "5e3702a84e847582be8db7fb73283c02", "event_type": "im.message.receive_v1",
                 "create_time": "1608725989000", "token": "t", "app_id": "cli_x", "tenant_key": "k"},
      "event": {
        "sender": {"sender_id": {"union_id": "on_1", "user_id": "u1", "open_id": "ou_sender"},
                   "sender_type": "user", "tenant_key": "k"},
        "message": {"message_id": "om_1", "root_id": "om_0", "parent_id": "om_0",
                    "create_time": "1609073151345", "update_time": "1609073151345",
                    "chat_id": "oc_1", "thread_id": "omt_1", "chat_type": "group", "message_type": "text",
                    "content": "{\"text\":\"@_user_1 hello\"}",
                    "mentions": [{"key": "@_user_1", "id": {"union_id": "on_b", "user_id": "", "open_id": "ou_bot"},
                                  "name": "Bot", "tenant_key": "k"}],
                    "user_agent": "ua"}
      }
    }"#;

    #[test]
    fn parses_the_documented_sample() {
        let envelope: Envelope = serde_json::from_str(SAMPLE).expect("信封");
        assert_eq!(envelope.header.event_type, MESSAGE_RECEIVE);
        let received: MessageReceived = serde_json::from_value(envelope.event).expect("消息事件");
        assert_eq!(received.sender.sender_id.open_id, "ou_sender");
        assert_eq!(received.message.thread(), Some("omt_1"));
        assert!(!received.message.is_p2p());
        assert!(received.message.mentions_open_id("ou_bot"));
        assert!(!received.message.mentions_open_id(""));
    }

    #[test]
    fn optional_fields_may_be_missing() {
        let raw = r#"{"sender":{"sender_type":"user"},
                      "message":{"message_id":"om_1","chat_id":"oc_1","chat_type":"p2p","message_type":"text"}}"#;
        let received: MessageReceived = serde_json::from_str(raw).expect("最小负载");
        assert!(received.message.is_p2p());
        assert_eq!(received.message.thread(), None);
        assert!(received.sender.sender_id.open_id.is_empty());
    }
}
