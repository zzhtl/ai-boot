//! 长连接数据帧的入口：解析 → 判断是否与机器人有关 → 白名单 → 落库 → 交给后续处理。
//!
//! 这段逻辑跑在 ACK 的预算内（见 `WsConfig::handler_budget`），只做本地判断和
//! 一次 SQLite 写入，不调用任何外部接口。落库失败会返回错误，ACK 带 500，
//! 由飞书重推；落库成功后即便进程崩溃，重启时也能从收件箱恢复。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use ai_boot_feishu::event::{Envelope, MESSAGE_RECEIVE, MessageReceived};
use ai_boot_feishu::ws::{DataKind, FrameHandler};
use tokio::sync::mpsc;

use crate::echo::Job;
use crate::store::{NewInput, Store, now_ms};
use crate::whitelist::Whitelist;

pub struct Ingest {
    store: Store,
    whitelist: Arc<Whitelist>,
    /// 机器人自己的 open_id，启动后异步取到。取到之前，群消息一律按没 @ 它处理。
    bot_open_id: Arc<OnceLock<String>>,
    jobs: mpsc::Sender<Job>,
    /// 回过「无权限」的人。每个进程周期只回一次，避免被人刷屏。
    denied: Mutex<HashSet<String>>,
}

impl Ingest {
    pub fn new(
        store: Store,
        whitelist: Arc<Whitelist>,
        bot_open_id: Arc<OnceLock<String>>,
        jobs: mpsc::Sender<Job>,
    ) -> Self {
        Self {
            store,
            whitelist,
            bot_open_id,
            jobs,
            denied: Mutex::new(HashSet::new()),
        }
    }

    async fn on_message(&self, envelope: Envelope, raw: &str) -> Result<Option<Vec<u8>>, String> {
        let received: MessageReceived = match serde_json::from_value(envelope.event) {
            Ok(received) => received,
            Err(err) => {
                tracing::warn!(%err, "无法解析的消息事件，已丢弃");
                return Ok(None);
            }
        };
        if received.sender.sender_type != "user" {
            return Ok(None);
        }
        let message = &received.message;
        let sender = received.sender.sender_id.open_id.as_str();
        let addressed = message.is_p2p()
            || self
                .bot_open_id
                .get()
                .is_some_and(|bot| message.mentions_open_id(bot));
        if !addressed {
            return Ok(None);
        }

        if !self.whitelist.allows(sender) {
            tracing::info!(open_id = sender, chat_type = %message.chat_type, "非白名单用户，已忽略");
            if message.is_p2p() && self.first_denial(sender) {
                let _ = self.jobs.try_send(Job::Deny {
                    message_id: message.message_id.clone(),
                });
            }
            return Ok(None);
        }

        let inserted = self
            .store
            .insert_input(&NewInput {
                message_id: &message.message_id,
                chat_id: &message.chat_id,
                chat_type: &message.chat_type,
                sender_open_id: sender,
                payload: raw,
                received_at_ms: now_ms(),
            })
            .await
            .map_err(|err| format!("{err:#}"))?;
        if !inserted {
            tracing::debug!(message_id = %message.message_id, "重复推送，已忽略");
            return Ok(None);
        }
        tracing::info!(message_id = %message.message_id, chat_type = %message.chat_type, "收到消息");
        let job = Job::Echo {
            message_id: message.message_id.clone(),
        };
        if self.jobs.try_send(job).is_err() {
            tracing::warn!(message_id = %message.message_id, "处理队列已满，消息留在收件箱，重启后恢复");
        }
        Ok(None)
    }

    fn first_denial(&self, open_id: &str) -> bool {
        self.denied
            .lock()
            .map(|mut set| set.insert(open_id.to_owned()))
            .unwrap_or(false)
    }
}

#[async_trait::async_trait]
impl FrameHandler for Ingest {
    async fn handle(&self, kind: DataKind, payload: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        let Ok(raw) = String::from_utf8(payload) else {
            // 重推也修不好，回 200 丢掉，免得飞书反复重试
            tracing::warn!(?kind, "事件负载不是 UTF-8，已丢弃");
            return Ok(None);
        };
        let envelope: Envelope = match serde_json::from_str(&raw) {
            Ok(envelope) => envelope,
            Err(err) => {
                tracing::warn!(%err, ?kind, "无法解析的事件负载，已丢弃");
                return Ok(None);
            }
        };
        match envelope.header.event_type.as_str() {
            MESSAGE_RECEIVE => self.on_message(envelope, &raw).await,
            other => {
                tracing::debug!(event_type = other, "未处理的事件类型");
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "ou_bot";
    const BOSS: &str = "ou_boss";

    struct Fixture {
        ingest: Ingest,
        jobs: mpsc::Receiver<Job>,
        store: Store,
        _dir: tempfile::TempDir,
    }

    async fn fixture(bot_known: bool) -> Fixture {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("数据库");
        let bot = Arc::new(OnceLock::new());
        if bot_known {
            bot.set(BOT.to_owned()).expect("首次设置");
        }
        let (tx, jobs) = mpsc::channel(8);
        let ingest = Ingest::new(
            store.clone(),
            Arc::new(Whitelist::new([BOSS.to_owned()])),
            bot,
            tx,
        );
        Fixture {
            ingest,
            jobs,
            store,
            _dir: dir,
        }
    }

    fn event(
        message_id: &str,
        sender: &str,
        sender_type: &str,
        chat_type: &str,
        mention_bot: bool,
    ) -> Vec<u8> {
        let mentions = if mention_bot {
            serde_json::json!([{"key": "@_user_1", "id": {"open_id": BOT}, "name": "bot"}])
        } else {
            serde_json::json!([])
        };
        serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "e", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": sender}, "sender_type": sender_type},
                "message": {"message_id": message_id, "chat_id": "oc_1", "chat_type": chat_type,
                            "message_type": "text", "content": "{\"text\":\"hi\"}", "mentions": mentions}
            }
        })
        .to_string()
        .into_bytes()
    }

    async fn handle(f: &Fixture, payload: Vec<u8>) {
        let reply = f
            .ingest
            .handle(DataKind::Event, payload)
            .await
            .expect("处理成功");
        assert!(reply.is_none());
    }

    fn next_job(f: &mut Fixture) -> Option<String> {
        match f.jobs.try_recv().ok()? {
            Job::Echo { message_id } => Some(format!("echo:{message_id}")),
            Job::Deny { message_id } => Some(format!("deny:{message_id}")),
        }
    }

    #[tokio::test]
    async fn a_whitelisted_mention_is_stored_once_and_queued() {
        let mut f = fixture(true).await;
        handle(&f, event("om_1", BOSS, "user", "group", true)).await;
        handle(&f, event("om_1", BOSS, "user", "group", true)).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("echo:om_1"));
        assert_eq!(next_job(&mut f), None, "重复推送不应再排队");
        assert_eq!(
            f.store.pending_inputs().await.expect("查询"),
            vec!["om_1".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_private_message_needs_no_mention() {
        let mut f = fixture(true).await;
        handle(&f, event("om_2", BOSS, "user", "p2p", false)).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("echo:om_2"));
    }

    #[tokio::test]
    async fn group_messages_that_do_not_mention_the_bot_are_ignored() {
        let mut f = fixture(true).await;
        handle(&f, event("om_3", BOSS, "user", "group", false)).await;
        assert_eq!(next_job(&mut f), None);
        assert!(f.store.pending_inputs().await.expect("查询").is_empty());
    }

    #[tokio::test]
    async fn until_the_bot_id_is_known_group_mentions_are_not_trusted() {
        let mut f = fixture(false).await;
        handle(&f, event("om_4", BOSS, "user", "group", true)).await;
        assert_eq!(next_job(&mut f), None);
    }

    #[tokio::test]
    async fn outsiders_are_ignored_in_groups_and_told_once_in_private() {
        let mut f = fixture(true).await;
        handle(&f, event("om_5", "ou_other", "user", "group", true)).await;
        assert_eq!(next_job(&mut f), None);
        handle(&f, event("om_6", "ou_other", "user", "p2p", false)).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("deny:om_6"));
        handle(&f, event("om_7", "ou_other", "user", "p2p", false)).await;
        assert_eq!(next_job(&mut f), None, "同一个人只提示一次");
        assert!(
            f.store.pending_inputs().await.expect("查询").is_empty(),
            "外人的消息不落库"
        );
    }

    #[tokio::test]
    async fn messages_from_other_bots_are_ignored() {
        let mut f = fixture(true).await;
        handle(&f, event("om_8", BOSS, "app", "p2p", false)).await;
        assert_eq!(next_job(&mut f), None);
    }

    #[tokio::test]
    async fn garbage_payloads_are_dropped_without_asking_for_redelivery() {
        let f = fixture(true).await;
        handle(&f, b"not json".to_vec()).await;
        handle(&f, vec![0xff, 0xfe]).await;
    }
}
