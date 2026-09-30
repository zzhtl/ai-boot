//! 告警：需要人去服务器上处理的故障（Agent 登录失效、运行环境自检不通过），
//! 私聊白名单里的人。同一类每小时最多一次，免得刷屏。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ai_boot_feishu::api::ApiClient;

use crate::whitelist::Whitelist;

const EVERY: Duration = Duration::from_secs(3600);

pub struct Alerts {
    api: Arc<ApiClient>,
    whitelist: Arc<Whitelist>,
    last: Mutex<HashMap<String, Instant>>,
}

impl Alerts {
    pub fn new(api: Arc<ApiClient>, whitelist: Arc<Whitelist>) -> Self {
        Self {
            api,
            whitelist,
            last: Mutex::new(HashMap::new()),
        }
    }

    /// 发一条告警。`kind` 相同的告警一小时内只发一次。
    pub async fn notify(&self, kind: &str, text: &str) {
        if !self.due(kind) {
            return;
        }
        tracing::warn!(kind, "告警：{text}");
        let content = serde_json::json!({ "text": format!("【ai-boot 告警】{text}") }).to_string();
        for open_id in self.whitelist.open_ids() {
            if let Err(err) = self
                .api
                .send_to_user(
                    &open_id,
                    "text",
                    content.clone(),
                    uuid::Uuid::now_v7().to_string(),
                )
                .await
            {
                tracing::warn!(%err, "发送告警失败");
            }
        }
    }

    fn due(&self, kind: &str) -> bool {
        let Ok(mut last) = self.last.lock() else {
            return false;
        };
        let now = Instant::now();
        match last.get(kind) {
            Some(at) if now.duration_since(*at) < EVERY => false,
            _ => {
                last.insert(kind.to_owned(), now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn each_kind_is_sent_at_most_once_an_hour_to_every_whitelisted_person() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "tenant_access_token": "t", "expire": 7200})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/open-apis/im/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"code": 0, "msg": "ok", "data": {"message_id": "om_a"}})),
            )
            // 两类告警各发给两个人；同一类的第二次被限流
            .expect(4)
            .mount(&server)
            .await;
        let base = url::Url::parse(&format!("{}/", server.uri())).expect("地址");
        let api = Arc::new(ApiClient::new(base, "cli_x", SecretString::from("s")).expect("客户端"));
        let alerts = Alerts::new(
            api,
            Arc::new(Whitelist::new(["ou_a".to_owned(), "ou_b".to_owned()])),
        );
        alerts.notify("auth", "Agent 登录失效").await;
        alerts.notify("auth", "Agent 登录失效").await;
        alerts.notify("self_check", "出现了 Bash").await;
    }
}
