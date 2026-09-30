//! 消息接口。

use serde::Deserialize;

use super::client::{ApiClient, ApiError, Call, path_segment};

/// 发送或回复成功后返回的消息。
#[derive(Debug, Clone, Deserialize)]
pub struct SentMessage {
    pub message_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
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

/// 历史消息里的一条（`GET /im/v1/messages` 与 `GET /im/v1/messages/{id}` 的元素）。
#[derive(Debug, Clone, Deserialize)]
pub struct MessageItem {
    pub message_id: String,
    #[serde(default)]
    pub root_id: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub msg_type: String,
    /// 毫秒时间戳（字符串）。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub create_time: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub deleted: bool,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub chat_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub sender: ItemSender,
    #[serde(default)]
    pub body: Option<ItemBody>,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub mentions: Vec<ItemMention>,
    /// 合并转发的子消息指向上一层。
    #[serde(default)]
    pub upper_message_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemSender {
    /// `id_type` 为 open_id 时是用户的 open_id；机器人发的消息是 app_id。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub id_type: String,
    /// `user` 或 `app`。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub sender_type: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemBody {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub content: String,
}

/// 历史消息里的 @：`id` 是字符串，而事件里是对象。
#[derive(Debug, Clone, Deserialize)]
pub struct ItemMention {
    pub key: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub name: String,
}

/// 分页结果。
#[derive(Debug, Clone, Deserialize)]
// `deserialize_with` 的字段不参与约束推导，得手写
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Page<T> {
    #[serde(default = "Vec::new", deserialize_with = "crate::nullable")]
    pub items: Vec<T>,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub has_more: bool,
    #[serde(default)]
    pub page_token: Option<String>,
}

/// 卡片消息按发送时的原始 JSON 返回，比默认的简化结构多保留文字和链接。
const RAW_CARD: &str = "user_card_content";

/// 历史消息的容器：群或话题。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container<'a> {
    Chat(&'a str),
    Thread(&'a str),
}

impl ApiClient {
    /// 单聊发给某个用户（按 open_id）。`uuid` 是幂等键。
    pub async fn send_to_user(
        &self,
        open_id: &str,
        msg_type: &str,
        content: String,
        uuid: String,
    ) -> Result<SentMessage, ApiError> {
        let body = serde_json::json!({
            "receive_id": open_id,
            "msg_type": msg_type,
            "content": content,
            "uuid": uuid,
        });
        let call = Call::post("open-apis/im/v1/messages", body).query("receive_id_type", "open_id");
        self.call(call).await
    }

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

    /// 用新的卡片 JSON 整体替换一条已发送的卡片消息。卡片的 `config` 里必须有
    /// `update_multi: true`；单条消息限 5 QPS，只能更新 14 天内的消息。
    pub async fn update_card(
        &self,
        message_id: &str,
        card: &serde_json::Value,
    ) -> Result<(), ApiError> {
        let path = format!("open-apis/im/v1/messages/{}", path_segment(message_id)?);
        let body = serde_json::json!({ "content": card.to_string() });
        let _: serde_json::Value = self.call(Call::patch(path, body)).await?;
        Ok(())
    }

    /// 上传卡片里要显示的图片，返回 `image_key`。图片不超过 10 MB。
    pub async fn upload_image(&self, image: Vec<u8>) -> Result<String, ApiError> {
        #[derive(serde::Deserialize)]
        struct Uploaded {
            image_key: String,
        }
        let uploaded: Uploaded = self
            .call(Call::upload_image("open-apis/im/v1/images", image))
            .await?;
        Ok(uploaded.image_key)
    }

    /// 按时间倒序或正序分页列出历史消息（每页最多 50 条）。话题容器不支持按
    /// 时间过滤，需要调用方自己按游标截断。
    pub async fn list_messages(
        &self,
        container: Container<'_>,
        newest_first: bool,
        page_token: Option<&str>,
    ) -> Result<Page<MessageItem>, ApiError> {
        let (kind, id) = match container {
            Container::Chat(id) => ("chat", id),
            Container::Thread(id) => ("thread", id),
        };
        let mut call = Call::get("open-apis/im/v1/messages")
            .query("container_id_type", kind)
            .query("container_id", path_segment(id)?)
            .query(
                "sort_type",
                if newest_first {
                    "ByCreateTimeDesc"
                } else {
                    "ByCreateTimeAsc"
                },
            )
            .query("page_size", "50")
            .query("card_msg_content_type", RAW_CARD);
        if let Some(token) = page_token {
            call = call.query("page_token", token);
        }
        self.call(call).await
    }

    /// 群里某个时刻（Unix 秒）之后的消息，从新到旧分页（每页最多 50 条）。
    /// 时间过滤只有群容器支持。
    pub async fn list_chat_since(
        &self,
        chat_id: &str,
        since_secs: i64,
        page_token: Option<&str>,
    ) -> Result<Page<MessageItem>, ApiError> {
        let mut call = Call::get("open-apis/im/v1/messages")
            .query("container_id_type", "chat")
            .query("container_id", path_segment(chat_id)?)
            .query("start_time", since_secs.to_string())
            .query("sort_type", "ByCreateTimeDesc")
            .query("page_size", "50")
            .query("card_msg_content_type", RAW_CARD);
        if let Some(token) = page_token {
            call = call.query("page_token", token);
        }
        self.call(call).await
    }

    /// 取单条消息。合并转发会一并返回全部子消息（平铺，用 `upper_message_id` 串起来）。
    pub async fn get_message(&self, message_id: &str) -> Result<Vec<MessageItem>, ApiError> {
        let path = format!("open-apis/im/v1/messages/{}", path_segment(message_id)?);
        let page: Page<MessageItem> = self
            .call(Call::get(path).query("card_msg_content_type", RAW_CARD))
            .await?;
        Ok(page.items)
    }
}
