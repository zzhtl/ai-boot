//! 通讯录接口。

use serde::Deserialize;

use super::client::{ApiClient, ApiError, Call};

/// 单次请求最多 50 个邮箱。
const BATCH: usize = 50;

/// 文档里有两种响应：旧的 `user_list`（`user_id_type=open_id` 时 open_id 放在
/// `user_id` 里），新的 `items`（有单独的 `open_id`）。两种都认。
#[derive(Deserialize)]
struct BatchGetIdData {
    #[serde(default)]
    user_list: Vec<UserIdEntry>,
    #[serde(default)]
    items: Vec<UserIdEntry>,
}

#[derive(Deserialize)]
struct UserIdEntry {
    #[serde(default)]
    open_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

impl ApiClient {
    /// 按邮箱查 open_id，返回 `(邮箱, open_id)`。查不到的邮箱不在结果里——
    /// 原因可能是不在应用的通讯录数据范围内、已离职，或者给的是企业邮箱
    /// 而不是用户资料里的邮箱。
    pub async fn open_ids_by_email(
        &self,
        emails: &[String],
    ) -> Result<Vec<(String, String)>, ApiError> {
        let mut found = Vec::new();
        for chunk in emails.chunks(BATCH) {
            let call = Call::post(
                "open-apis/contact/v3/users/batch_get_id",
                serde_json::json!({ "emails": chunk, "include_resigned": false }),
            )
            .query("user_id_type", "open_id");
            let data: BatchGetIdData = self.call(call).await?;
            found.extend(
                data.user_list
                    .into_iter()
                    .chain(data.items)
                    .filter_map(|entry| {
                        let email = entry.email.filter(|e| !e.is_empty())?;
                        let open_id = entry
                            .open_id
                            .filter(|id| !id.is_empty())
                            .or(entry.user_id)
                            .filter(|id| !id.is_empty())?;
                        Some((email, open_id))
                    }),
            );
        }
        Ok(found)
    }
}
