//! 长连接数据帧的入口。
//!
//! - 消息：解析 → 判断是否与机器人有关 → 白名单 → 落库 → 交给会话调度。
//! - 卡片按钮：白名单 → 交给会话调度 → 把它的应答作为 toast 随 ACK 返回。
//!
//! 这段逻辑跑在 ACK 的预算内（见 `WsConfig::handler_budget`），只做本地判断和
//! SQLite 读写，不调用任何外部接口。落库失败会返回错误，ACK 带 500，由飞书
//! 重推；落库成功后即便进程崩溃，重启时也能从收件箱恢复。

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ai_boot_feishu::event::{
    CARD_ACTION, CardAction, Envelope, MESSAGE_RECEIVE, Message, MessageReceived,
};
use ai_boot_feishu::ws::{DataKind, FrameHandler};
use serde::Deserialize as _;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::callback::{Action, Toast};
use crate::conversation::Job;
use crate::store::{NewInput, Origin, Store, now_ms};
use crate::whitelist::Whitelist;

/// 等会话调度应答按钮的时间。飞书要求 3 秒内响应，ACK 本身的预算是 2 秒。
const ACTION_REPLY_WAIT: Duration = Duration::from_millis(1500);

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
        let received = match MessageReceived::deserialize(&envelope.event) {
            Ok(received) => received,
            Err(err) => {
                tracing::warn!(%err, shape = %shape(&envelope.event), "无法解析的消息事件，已丢弃");
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
                .is_some_and(|bot| message.mentions_open_id(bot))
            || self
                .is_follow_up(message)
                .await
                .map_err(|err| format!("{err:#}"))?;
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
        let job = Job::Input {
            message_id: message.message_id.clone(),
        };
        if self.jobs.try_send(job).is_err() {
            tracing::warn!(message_id = %message.message_id, "处理队列已满，消息留在收件箱，重启后恢复");
        }
        Ok(None)
    }

    /// 群里没 @ 机器人的消息，也可能是追问：在机器人开的话题里说的话，或者
    /// 引用了机器人的卡片。别人开的话题里，只有 @ 或引用卡片才算。
    async fn is_follow_up(&self, message: &Message) -> anyhow::Result<bool> {
        if let Some(thread) = message.thread() {
            let root = message.root_id.as_deref().filter(|r| !r.is_empty());
            if let Some(conversation) = self.store.find_conversation(Some(thread), root).await?
                && conversation.origin == Origin::NewThread
            {
                return Ok(true);
            }
        }
        if let Some(parent) = message.parent_id.as_deref().filter(|p| !p.is_empty()) {
            return Ok(self.store.conversation_by_card(parent).await?.is_some());
        }
        Ok(false)
    }

    async fn on_card_action(&self, envelope: Envelope) -> Result<Option<Vec<u8>>, String> {
        let action = match CardAction::deserialize(&envelope.event) {
            Ok(action) => action,
            Err(err) => {
                tracing::warn!(%err, shape = %shape(&envelope.event), "无法解析的卡片回调");
                return Ok(Some(Toast::warning("无法识别的操作").to_bytes()));
            }
        };
        let operator = action.operator.open_id.as_str();
        if !self.whitelist.allows(operator) {
            tracing::info!(open_id = operator, "非白名单用户点了卡片按钮，已拒绝");
            return Ok(Some(Toast::error("你没有操作这个机器人的权限").to_bytes()));
        }
        let Some((kind, turn_id)) = Action::parse(&action.action.value) else {
            return Ok(Some(Toast::warning("无法识别的操作").to_bytes()));
        };
        tracing::info!(action = ?kind, turn = %turn_id, "卡片按钮");
        let (reply, answer) = oneshot::channel();
        let job = match kind {
            Action::FollowUp => Job::FollowUp {
                turn_id,
                card_message_id: action.context.open_message_id.clone(),
                chat_id: action.context.open_chat_id.clone(),
                text: action.action.input_value.clone(),
                operator: operator.to_owned(),
                reply,
            },
            kind => Job::Action {
                action: kind,
                turn_id,
                operator: operator.to_owned(),
                reply,
            },
        };
        if self.jobs.try_send(job).is_err() {
            return Ok(Some(Toast::warning("系统繁忙，稍后再试").to_bytes()));
        }
        let toast = match tokio::time::timeout(ACTION_REPLY_WAIT, answer).await {
            Ok(Ok(toast)) => toast,
            // 应答慢了不代表没执行：操作已经排上，只是来不及告诉用户结果
            _ => Toast::info("已收到，正在处理"),
        };
        Ok(Some(toast.to_bytes()))
    }

    fn first_denial(&self, open_id: &str) -> bool {
        self.denied
            .lock()
            .map(|mut set| set.insert(open_id.to_owned()))
            .unwrap_or(false)
    }
}

/// 事件的结构骨架：保留字段名、null 和布尔，字符串与数字只留类型。解析失败时打进日志，
/// 一眼看出是哪个字段和预期不符，又不把聊天内容写进日志。
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::String(_) => Value::String("<str>".to_owned()),
        Value::Number(_) => Value::String("<num>".to_owned()),
        other => other.clone(),
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
            CARD_ACTION => self.on_card_action(envelope).await,
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
        event_with(
            message_id,
            sender,
            sender_type,
            chat_type,
            mention_bot,
            serde_json::json!({}),
        )
    }

    /// `extra` 并进 message（话题 ID、引用的消息等）。
    fn event_with(
        message_id: &str,
        sender: &str,
        sender_type: &str,
        chat_type: &str,
        mention_bot: bool,
        extra: serde_json::Value,
    ) -> Vec<u8> {
        let mentions = if mention_bot {
            serde_json::json!([{"key": "@_user_1", "id": {"open_id": BOT}, "name": "bot"}])
        } else {
            serde_json::json!([])
        };
        let mut message = serde_json::json!({
            "message_id": message_id, "chat_id": "oc_1", "chat_type": chat_type,
            "message_type": "text", "content": "{\"text\":\"hi\"}", "mentions": mentions
        });
        if let (Some(message), Some(extra)) = (message.as_object_mut(), extra.as_object()) {
            message.extend(extra.clone());
        }
        serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "e", "event_type": "im.message.receive_v1"},
            "event": {
                "sender": {"sender_id": {"open_id": sender}, "sender_type": sender_type},
                "message": message
            }
        })
        .to_string()
        .into_bytes()
    }

    fn card_action(operator: &str, value: serde_json::Value) -> Vec<u8> {
        serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "e", "event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": operator},
                "action": {"tag": "button", "value": value},
                "context": {"open_message_id": "om_card", "open_chat_id": "oc_1"}
            }
        })
        .to_string()
        .into_bytes()
    }

    async fn conversation(store: &Store, id: &str, origin: Origin, thread: &str, root: &str) {
        store
            .create_conversation(&crate::store::NewConversation {
                id,
                chat_id: "oc_1",
                chat_type: "group",
                origin,
                thread_id: Some(thread),
                root_message_id: root,
                owner_open_id: BOSS,
                backend: "claude",
                now_ms: 1,
            })
            .await
            .expect("建会话");
    }

    fn toast_text(reply: Option<Vec<u8>>) -> String {
        let body: serde_json::Value =
            serde_json::from_slice(&reply.expect("要回 toast")).expect("JSON");
        body["toast"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
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
            Job::Input { message_id } => Some(format!("ask:{message_id}")),
            Job::Deny { message_id } => Some(format!("deny:{message_id}")),
            Job::Action {
                action, turn_id, ..
            } => Some(format!("{action:?}:{turn_id}")),
            Job::FollowUp { turn_id, text, .. } => Some(format!("follow_up:{turn_id}:{text}")),
        }
    }

    #[tokio::test]
    async fn a_whitelisted_mention_is_stored_once_and_queued() {
        let mut f = fixture(true).await;
        handle(&f, event("om_1", BOSS, "user", "group", true)).await;
        handle(&f, event("om_1", BOSS, "user", "group", true)).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_1"));
        assert_eq!(next_job(&mut f), None, "重复推送不应再排队");
        assert_eq!(
            f.store.unassigned_inputs().await.expect("查询"),
            vec!["om_1".to_owned()]
        );
    }

    #[tokio::test]
    async fn a_private_message_needs_no_mention() {
        let mut f = fixture(true).await;
        handle(&f, event("om_2", BOSS, "user", "p2p", false)).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_2"));
    }

    /// 线上第一条私聊就是这样被丢掉的：`user_id`、`mentions` 这些用不上的字段是显式 null。
    #[tokio::test]
    async fn fields_that_come_as_null_do_not_drop_the_message() {
        let mut f = fixture(true).await;
        let payload = serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "e", "event_type": "im.message.receive_v1", "app_id": null},
            "event": {
                "sender": {"sender_id": {"open_id": BOSS, "user_id": null, "union_id": null},
                           "sender_type": "user", "tenant_key": null},
                "message": {"message_id": "om_null", "root_id": null, "parent_id": null,
                            "chat_id": "oc_1", "chat_type": "p2p", "message_type": "text",
                            "content": "{\"text\":\"hi\"}", "mentions": null, "user_agent": null}
            }
        });
        handle(&f, payload.to_string().into_bytes()).await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_null"));
    }

    #[test]
    fn the_logged_shape_keeps_structure_but_not_content() {
        let event = serde_json::json!({"message": {"content": "机密", "mentions": null,
                                                   "count": 3, "flag": true, "ids": ["a"]}});
        assert_eq!(
            shape(&event),
            serde_json::json!({"message": {"content": "<str>", "mentions": null,
                                           "count": "<num>", "flag": true, "ids": ["<str>"]}})
        );
    }

    #[tokio::test]
    async fn group_messages_that_do_not_mention_the_bot_are_ignored() {
        let mut f = fixture(true).await;
        handle(&f, event("om_3", BOSS, "user", "group", false)).await;
        assert_eq!(next_job(&mut f), None);
        assert!(f.store.unassigned_inputs().await.expect("查询").is_empty());
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
            f.store.unassigned_inputs().await.expect("查询").is_empty(),
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
    async fn in_a_thread_the_bot_opened_no_mention_is_needed() {
        let mut f = fixture(true).await;
        conversation(&f.store, "c1", Origin::NewThread, "omt_1", "om_root").await;
        let follow_up = serde_json::json!({"thread_id": "omt_1", "root_id": "om_root"});
        handle(
            &f,
            event_with("om_10", BOSS, "user", "group", false, follow_up.clone()),
        )
        .await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_10"));
        // 外人在话题里说话照样不理
        handle(
            &f,
            event_with("om_11", "ou_other", "user", "group", false, follow_up),
        )
        .await;
        assert_eq!(next_job(&mut f), None);
    }

    #[tokio::test]
    async fn before_the_thread_id_is_recorded_the_root_identifies_the_conversation() {
        let mut f = fixture(true).await;
        f.store
            .create_conversation(&crate::store::NewConversation {
                id: "c1",
                chat_id: "oc_1",
                chat_type: "group",
                origin: Origin::NewThread,
                thread_id: None,
                root_message_id: "om_root",
                owner_open_id: BOSS,
                backend: "claude",
                now_ms: 1,
            })
            .await
            .expect("建会话");
        let follow_up = serde_json::json!({"thread_id": "omt_new", "root_id": "om_root"});
        handle(
            &f,
            event_with("om_12", BOSS, "user", "group", false, follow_up),
        )
        .await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_12"));
    }

    #[tokio::test]
    async fn in_someone_elses_thread_only_mentions_and_card_quotes_count() {
        let mut f = fixture(true).await;
        conversation(&f.store, "c2", Origin::ExistingThread, "omt_2", "om_r2").await;
        let in_thread = serde_json::json!({"thread_id": "omt_2", "root_id": "om_r2"});
        handle(
            &f,
            event_with("om_20", BOSS, "user", "group", false, in_thread.clone()),
        )
        .await;
        assert_eq!(next_job(&mut f), None, "别人的话题里闲聊不算追问");

        f.store
            .insert_input(&NewInput {
                message_id: "om_q",
                chat_id: "oc_1",
                chat_type: "group",
                sender_open_id: BOSS,
                payload: "{}",
                received_at_ms: 1,
            })
            .await
            .expect("写入");
        f.store
            .create_turn("c2", "t1", "om_q", 1)
            .await
            .expect("建轮次");
        f.store
            .set_turn_card("t1", "om_card")
            .await
            .expect("记卡片");
        let quoting =
            serde_json::json!({"thread_id": "omt_2", "root_id": "om_r2", "parent_id": "om_card"});
        handle(
            &f,
            event_with("om_21", BOSS, "user", "group", false, quoting),
        )
        .await;
        assert_eq!(next_job(&mut f).as_deref(), Some("ask:om_21"));
    }

    #[tokio::test]
    async fn a_card_button_is_forwarded_and_its_answer_becomes_the_toast() {
        let mut f = fixture(true).await;
        let value = crate::callback::Action::Stop.value("t-9");
        let ingest = &f.ingest;
        let (reply, ()) = tokio::join!(
            ingest.handle(DataKind::Event, card_action(BOSS, value)),
            async {
                match f.jobs.recv().await {
                    Some(Job::Action {
                        action,
                        turn_id,
                        operator,
                        reply,
                    }) => {
                        assert_eq!(operator, BOSS);
                        assert_eq!(action, Action::Stop);
                        assert_eq!(turn_id, "t-9");
                        let _ = reply.send(Toast::success("正在停止"));
                    }
                    _ => panic!("应当转给会话调度"),
                }
            }
        );
        assert_eq!(toast_text(reply.expect("处理成功")), "正在停止");
    }

    /// 答案卡输入框里回车：内容在 `input_value`，连同卡片和群一起转给会话调度。
    #[tokio::test]
    async fn text_typed_on_the_card_becomes_a_follow_up() {
        let mut f = fixture(true).await;
        let payload = serde_json::json!({
            "schema": "2.0",
            "header": {"event_id": "e", "event_type": "card.action.trigger"},
            "event": {
                "operator": {"open_id": BOSS},
                "action": {
                    "tag": "input",
                    "input_value": "连接池监控是正常的",
                    "value": crate::callback::Action::FollowUp.value("t-9"),
                },
                "context": {"open_message_id": "om_card", "open_chat_id": "oc_1"}
            }
        })
        .to_string()
        .into_bytes();
        let ingest = &f.ingest;
        let (reply, ()) = tokio::join!(ingest.handle(DataKind::Event, payload), async {
            match f.jobs.recv().await {
                Some(Job::FollowUp {
                    turn_id,
                    card_message_id,
                    chat_id,
                    text,
                    operator,
                    reply,
                }) => {
                    assert_eq!(
                        (turn_id.as_str(), card_message_id.as_str(), chat_id.as_str()),
                        ("t-9", "om_card", "oc_1")
                    );
                    assert_eq!(text, "连接池监控是正常的");
                    assert_eq!(operator, BOSS);
                    let _ = reply.send(Toast::success("收到，正在分析"));
                }
                _ => panic!("应当转成追问"),
            }
        });
        assert_eq!(toast_text(reply.expect("处理成功")), "收到，正在分析");
    }

    #[tokio::test]
    async fn outsiders_and_unknown_buttons_only_get_a_toast() {
        let mut f = fixture(true).await;
        let value = crate::callback::Action::Retry.value("t-9");
        let reply = f
            .ingest
            .handle(DataKind::Event, card_action("ou_other", value))
            .await
            .expect("处理成功");
        assert!(toast_text(reply).contains("没有操作"));
        let reply = f
            .ingest
            .handle(
                DataKind::Card,
                card_action(BOSS, serde_json::json!({"action": "rm", "turn": "t"})),
            )
            .await
            .expect("处理成功");
        assert!(toast_text(reply).contains("无法识别"));
        assert_eq!(next_job(&mut f), None);
    }

    #[tokio::test]
    async fn a_slow_answer_still_gets_a_toast_in_time() {
        let f = fixture(true).await;
        let value = crate::callback::Action::Stop.value("t-9");
        // 没人应答：到点给一个「正在处理」，不能让 ACK 超时
        let started = std::time::Instant::now();
        let reply = f
            .ingest
            .handle(DataKind::Event, card_action(BOSS, value))
            .await
            .expect("处理成功");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(toast_text(reply).contains("正在处理"));
    }

    #[tokio::test]
    async fn garbage_payloads_are_dropped_without_asking_for_redelivery() {
        let f = fixture(true).await;
        handle(&f, b"not json".to_vec()).await;
        handle(&f, vec![0xff, 0xfe]).await;
    }
}
