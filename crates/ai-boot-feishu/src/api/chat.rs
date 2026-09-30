//! 群相关接口。

use serde::Deserialize;

use super::client::{ApiClient, ApiError, Call, path_segment};
use super::message::Page;

/// 群成员（不含机器人）。
#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    pub member_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub name: String,
}

/// 翻页上限：100 人一页，20 页足够覆盖绝大多数群，也防止异常时无限翻。
const MAX_PAGES: usize = 20;

impl ApiClient {
    /// 群成员的 open_id 与姓名。
    pub async fn chat_members(&self, chat_id: &str) -> Result<Vec<Member>, ApiError> {
        let path = format!("open-apis/im/v1/chats/{}/members", path_segment(chat_id)?);
        let mut members = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut call = Call::get(path.clone())
                .query("member_id_type", "open_id")
                .query("page_size", "100");
            if let Some(token) = &token {
                call = call.query("page_token", token.clone());
            }
            let page: Page<Member> = self.call(call).await?;
            members.extend(page.items);
            match page.page_token.filter(|t| page.has_more && !t.is_empty()) {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        Ok(members)
    }
}
