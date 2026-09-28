//! M1 的回声处理：验证「收到消息 → 落库 → 在话题里回复」这条链路是通的。
//! M2 起由正式的分析流程取代。
//!
//! 回复内容是固定文本，不回显用户输入：用户输入里的 `<at>` 之类标签一旦被
//! 原样发出去，就可能变成 @所有人。

use std::sync::Arc;

use ai_boot_feishu::api::{ApiClient, Reply};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::store::{Store, now_ms};

pub enum Job {
    Echo { message_id: String },
    Deny { message_id: String },
}

const ECHO_CARD: &str = r#"{"schema":"2.0","config":{"update_multi":true},"body":{"elements":[{"tag":"markdown","content":"✅ 已收到。排查流程还在接入中，这是链路自检的回声。"}]}}"#;
const DENY_TEXT: &str = "你没有使用这个机器人的权限。";

pub async fn run(api: Arc<ApiClient>, store: Store, mut jobs: mpsc::Receiver<Job>) {
    while let Some(job) = jobs.recv().await {
        match job {
            Job::Echo { message_id } => echo(&api, &store, &message_id).await,
            Job::Deny { message_id } => deny(&api, &message_id).await,
        }
    }
}

async fn echo(api: &ApiClient, store: &Store, message_id: &str) {
    let reply = Reply {
        message_id,
        msg_type: "interactive",
        content: ECHO_CARD.to_owned(),
        reply_in_thread: true,
        uuid: idempotency_key("echo", message_id),
    };
    match api.reply(reply).await {
        Ok(sent) => {
            tracing::info!(
                message_id,
                thread_id = sent.thread_id.as_deref().unwrap_or_default(),
                "已在话题里回复"
            );
            if let Err(err) = store.mark_done(message_id, now_ms()).await {
                tracing::error!("{err:#}");
            }
        }
        Err(err) => tracing::warn!(%err, message_id, "回复失败，消息留在收件箱，重启后重试"),
    }
}

async fn deny(api: &ApiClient, message_id: &str) {
    let reply = Reply {
        message_id,
        msg_type: "text",
        content: serde_json::json!({ "text": DENY_TEXT }).to_string(),
        reply_in_thread: false,
        uuid: idempotency_key("deny", message_id),
    };
    if let Err(err) = api.reply(reply).await {
        tracing::warn!(%err, message_id, "回复「无权限」失败");
    }
}

/// 同一条消息的同一种回复，重试时用同一个 uuid：飞书保证 1 小时内只成功一次。
fn idempotency_key(kind: &str, message_id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("ai-boot/{kind}/{message_id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_echo_card_is_a_json_2_card() {
        let card: serde_json::Value = serde_json::from_str(ECHO_CARD).expect("卡片 JSON");
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["update_multi"], true);
    }

    #[test]
    fn idempotency_keys_are_stable_and_distinct_per_kind() {
        let a = idempotency_key("echo", "om_1");
        assert_eq!(a, idempotency_key("echo", "om_1"));
        assert_ne!(a, idempotency_key("deny", "om_1"));
        assert!(a.len() <= 50, "飞书的 uuid 参数最长 50 个字符");
    }
}
