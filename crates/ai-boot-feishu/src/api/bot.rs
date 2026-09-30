//! 机器人自身信息。

use serde::Deserialize;

use super::client::{ApiClient, ApiError, Call};

#[derive(Debug, Clone, Deserialize)]
pub struct BotInfo {
    pub open_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub app_name: String,
}

impl ApiClient {
    /// 机器人自己的 open_id，用来判断群消息是否 @ 了它。
    pub async fn bot_info(&self) -> Result<BotInfo, ApiError> {
        let raw = self.call_raw(Call::get("open-apis/bot/v3/info")).await?;
        // 这个接口的数据放在顶层 `bot` 字段而不是 `data` 下；两种都接受，
        // 以 M0 实测为准
        let bot = raw
            .get("bot")
            .or_else(|| raw.get("data").and_then(|d| d.get("bot")))
            .cloned()
            .ok_or_else(|| ApiError::Decode("响应里没有 bot 字段".into()))?;
        serde_json::from_value(bot).map_err(|err| ApiError::Decode(err.to_string()))
    }
}
