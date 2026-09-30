//! 长连接收到的事件负载（schema 2.0）。
//!
//! 只声明用到的字段，其余忽略：飞书会往事件里加字段，解析不能因此失败。

use serde::Deserialize;

pub const MESSAGE_RECEIVE: &str = "im.message.receive_v1";
/// 新版卡片回传交互（点按钮等）。长连接模式下，响应随 ACK 一起回给飞书。
pub const CARD_ACTION: &str = "card.action.trigger";

#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub schema: String,
    pub header: EventHeader,
    #[serde(default)]
    pub event: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EventHeader {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub event_id: String,
    pub event_type: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub create_time: String,
    #[serde(default, deserialize_with = "crate::nullable")]
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
    #[serde(default, deserialize_with = "crate::nullable")]
    pub sender_id: UserId,
    /// 用户消息为 `user`。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub sender_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UserId {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub open_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub union_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
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
    #[serde(default, deserialize_with = "crate::nullable")]
    pub content: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub mentions: Vec<Mention>,
    /// 毫秒时间戳（字符串）。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub create_time: String,
}

/// 文本里的 `@_user_N` 占位符与被 @ 对象的对应关系。
#[derive(Debug, Clone, Deserialize)]
pub struct Mention {
    pub key: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub id: UserId,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub name: String,
}

/// `card.action.trigger`。
#[derive(Debug, Clone, Deserialize)]
pub struct CardAction {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub operator: Operator,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub action: ActionBody,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub context: ActionContext,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Operator {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub open_id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionBody {
    /// 触发交互的组件，按钮是 `button`。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub tag: String,
    /// 组件 `behaviors` 里 `callback` 的 `value`，原样带回。
    #[serde(default)]
    pub value: serde_json::Value,
    /// 输入框提交的内容（`tag` 是 `input` 时）。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub input_value: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ActionContext {
    /// 被点击的卡片消息。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub open_message_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub open_chat_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// 卡片回调的响应：只弹一条 toast，不改卡片。飞书要求 3 秒内返回。
pub fn toast_response(kind: ToastKind, content: &str) -> Vec<u8> {
    serde_json::json!({ "toast": { "type": kind.as_str(), "content": content } })
        .to_string()
        .into_bytes()
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

    /// 线上实测：应用没开员工 ID 权限时 `user_id` 是显式的 `null`，私聊没有 @ 时
    /// `mentions` 也可能是 `null`。只认 `#[serde(default)]` 的话整条消息会被丢掉。
    #[test]
    fn explicit_nulls_fall_back_to_defaults() {
        let event = serde_json::json!({
            "sender": {"sender_id": {"union_id": "on_1", "user_id": null, "open_id": "ou_sender"},
                       "sender_type": "user", "tenant_key": null},
            "message": {"message_id": "om_1", "root_id": null, "parent_id": null, "thread_id": null,
                        "create_time": "1609073151345", "chat_id": "oc_1", "chat_type": "p2p",
                        "message_type": "text", "content": "{\"text\":\"hi\"}", "mentions": null,
                        "user_agent": null}
        });
        let received: MessageReceived = serde_json::from_value(event).expect("消息事件");
        assert_eq!(received.sender.sender_id.open_id, "ou_sender");
        assert_eq!(received.sender.sender_id.user_id, "");
        assert!(received.message.is_p2p());
        assert!(received.message.mentions.is_empty());
        assert_eq!(received.message.thread(), None);
    }

    /// 官方文档「卡片回传交互」里的样例（字段有删减）。
    const CARD_ACTION_SAMPLE: &str = r#"{
      "schema": "2.0",
      "header": {"event_id": "f7984f25108f8137722bb63cee927e66", "token": "t",
                 "create_time": "1603977298000000", "event_type": "card.action.trigger",
                 "tenant_key": "k", "app_id": "cli_x"},
      "event": {
        "operator": {"tenant_key": "k", "user_id": "u1", "open_id": "ou_xxx"},
        "token": "c-xxxx",
        "action": {"value": {"action": "stop", "turn": "t1"}, "tag": "button"},
        "host": "im_message",
        "context": {"open_message_id": "om_xxx", "open_chat_id": "oc_xxx"}
      }
    }"#;

    #[test]
    fn parses_a_card_action() {
        let envelope: Envelope = serde_json::from_str(CARD_ACTION_SAMPLE).expect("信封");
        assert_eq!(envelope.header.event_type, CARD_ACTION);
        let action: CardAction = serde_json::from_value(envelope.event).expect("回调");
        assert_eq!(action.operator.open_id, "ou_xxx");
        assert_eq!(action.action.tag, "button");
        assert_eq!(action.action.value["turn"], "t1");
        assert_eq!(action.context.open_message_id, "om_xxx");
    }

    #[test]
    fn a_toast_response_has_the_documented_shape() {
        let body: serde_json::Value =
            serde_json::from_slice(&toast_response(ToastKind::Success, "已停止")).expect("JSON");
        assert_eq!(
            body,
            serde_json::json!({"toast": {"type": "success", "content": "已停止"}})
        );
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
