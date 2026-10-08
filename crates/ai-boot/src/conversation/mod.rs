//! 会话调度：把收件箱里的消息分到会话，同一会话里的轮次串行执行。
//!
//! 一个群一个会话：同一个群里的提问都续接同一个 Agent 会话，聊天记录和附件只发
//! 新增的部分，不重复发。私聊上一轮结束两小时内的新消息续接原来的会话，隔久了新开；
//! 引用卡片或在卡片上补充，不论隔多久都接着那张卡片的会话。
//!
//! - registry（单个任务）：给消息找会话或建会话、把按钮回调转给对应会话、
//!   回收空闲的 actor。它是唯一建会话的地方，所以不会为同一个话题建出两个会话。
//! - actor（每个会话一个，只在有事可做时存在）：连发合并、排队、停止、重试。
//!   它不调飞书接口，按钮回调等它的应答不会被网络拖住。
//! - 每一轮在自己的任务里执行（`Runner::run_turn`），是这一轮卡片的唯一写入方。

mod actor;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ai_boot_feishu::event::MessageReceived;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use uuid::Uuid;

use crate::callback::{Action, Toast};
use crate::context;
use crate::maintenance::Maintenance;
use crate::runner::{Runner, parse_event};
use crate::store::{Conversation, NewConversation, Origin, Store, now_ms};
use actor::{Actor, Idle, Msg};

pub enum Job {
    /// 收件箱里新到的一条消息。
    Input { message_id: String },
    /// 在答案卡的输入框里补充、纠正：接着那张卡片所在的会话问。
    FollowUp {
        turn_id: String,
        /// 输入框所在的卡片消息和群。
        card_message_id: String,
        chat_id: String,
        text: String,
        /// 输入的人（已过白名单）。
        operator: String,
        reply: oneshot::Sender<Toast>,
    },
    /// 非白名单用户私聊机器人：回一次「无权限」。
    Deny { message_id: String },
    /// 卡片按钮。
    Action {
        action: Action,
        turn_id: String,
        /// 点按钮的人（已过白名单）。
        operator: String,
        reply: oneshot::Sender<Toast>,
    },
}

/// 只收到附件时，连发合并的窗口放宽到几倍。
const ATTACHMENT_ONLY_FACTOR: u32 = 4;
/// 私聊的新消息，上一轮结束这么久以内的接着原来的会话问。
const P2P_CONTINUE: Duration = Duration::from_secs(2 * 60 * 60);
/// 清空上下文时，最多等这么久让这个聊天里在跑的轮次停下来、写完收尾。CLI 收到
/// SIGINT 后有 10 秒宽限，这里留足余量。
const CLEAR_WAIT: Duration = Duration::from_secs(60);

/// 一个活着的 actor。`sent` 是发给它的消息数，回收时拿来确认没有在途消息。
struct Slot {
    tx: mpsc::UnboundedSender<Msg>,
    sent: u64,
}

/// 某人在某个聊天里刚开的会话。新消息本该开新会话，但「先发截图、再打字提问」
/// 是一件事：窗口内的连发并进这个会话，提问那一轮才看得到截图（私聊没有群聊记录可补）。
struct Recent {
    conversation_id: String,
    since: Instant,
    attachment_only: bool,
}

pub struct Registry {
    store: Store,
    runner: Arc<Runner>,
    bot_open_id: Arc<OnceLock<String>>,
    default_backend: String,
    /// 连发合并的窗口：一轮的第一条消息之后这么久内的消息并进同一轮。
    debounce: Duration,
    actors: HashMap<String, Slot>,
    /// （聊天，发送人）→ 刚开的会话。
    recent: HashMap<(String, String), Recent>,
    idle_tx: mpsc::UnboundedSender<Idle>,
    idle_rx: mpsc::UnboundedReceiver<Idle>,
    /// 清空上下文时删工作目录、Agent 会话记录，整理库和备份。没有就只删库里的行。
    eraser: Option<Arc<Maintenance>>,
}

impl Registry {
    pub fn new(
        store: Store,
        runner: Arc<Runner>,
        bot_open_id: Arc<OnceLock<String>>,
        default_backend: String,
        debounce: Duration,
    ) -> Self {
        let (idle_tx, idle_rx) = mpsc::unbounded_channel();
        Self {
            store,
            runner,
            bot_open_id,
            default_backend,
            debounce,
            actors: HashMap::new(),
            recent: HashMap::new(),
            idle_tx,
            idle_rx,
            eraser: None,
        }
    }

    pub fn with_eraser(mut self, eraser: Arc<Maintenance>) -> Self {
        self.eraser = Some(eraser);
        self
    }

    pub async fn run(mut self, mut jobs: mpsc::Receiver<Job>) {
        loop {
            tokio::select! {
                job = jobs.recv() => match job {
                    Some(job) => self.handle(job).await,
                    None => break,
                },
                Some(idle) = self.idle_rx.recv() => self.retire(&idle),
            }
        }
    }

    async fn handle(&mut self, job: Job) {
        match job {
            Job::Input { message_id } => self.input(&message_id).await,
            Job::FollowUp {
                turn_id,
                card_message_id,
                chat_id,
                text,
                operator,
                reply,
            } => {
                // 在卡片输入框里说「清空上下文」也要真的清空，不能当成一句追问交给模型
                if context::is_clear_command(&text) {
                    let _ = reply.send(Toast::success("正在清空上下文"));
                    let now = now_ms();
                    self.clear(&chat_id, now, now, &card_message_id).await;
                    return;
                }
                let toast = self
                    .follow_up(&turn_id, &card_message_id, &chat_id, &text, &operator)
                    .await;
                let _ = reply.send(toast);
            }
            Job::Deny { message_id } => {
                let runner = Arc::clone(&self.runner);
                tokio::spawn(async move { runner.deny(&message_id).await });
            }
            Job::Action {
                action,
                turn_id,
                operator,
                reply,
            } => match self.store.turn(&turn_id).await {
                Ok(Some(turn)) => self.send(
                    turn.conversation_id,
                    Msg::Action {
                        action,
                        turn_id,
                        operator,
                        reply,
                    },
                ),
                Ok(None) => {
                    let _ = reply.send(Toast::warning("找不到这一轮，可能已被清理"));
                }
                Err(err) => {
                    tracing::error!("{err:#}");
                    let _ = reply.send(Toast::error("操作失败，稍后再试"));
                }
            },
        }
    }

    async fn input(&mut self, message_id: &str) {
        let input = match self.store.input(message_id).await {
            Ok(Some(input)) => input,
            Ok(None) => {
                tracing::error!(message_id, "收件箱里没有这条消息");
                return;
            }
            Err(err) => {
                tracing::error!(message_id, "{err:#}");
                return;
            }
        };
        let received = match parse_event(&input.payload) {
            Ok(received) => received,
            Err(err) => {
                tracing::warn!(message_id, "{err}，已丢弃");
                if let Err(err) = self.store.mark_done(message_id, now_ms()).await {
                    tracing::error!("{err:#}");
                }
                return;
            }
        };
        let bot = self.bot_open_id.get().map(String::as_str);
        if context::is_clear_command(&context::event_text(&received, bot)) {
            let reset_at = received
                .message
                .create_time
                .parse::<i64>()
                .unwrap_or_else(|_| now_ms());
            let chat_id = received.message.chat_id.clone();
            self.clear(&chat_id, input.received_at_ms, reset_at, message_id)
                .await;
            return;
        }
        let conversation_id = match input.conversation_id {
            Some(id) => id,
            None => match self.resolve(message_id, &received).await {
                Ok(id) => id,
                Err(err) => {
                    // 消息留在收件箱里，重启时重新分派
                    tracing::error!(message_id, "分派消息失败：{err:#}");
                    return;
                }
            },
        };
        self.send(
            conversation_id,
            Msg::Input {
                message_id: message_id.to_owned(),
                attachment_only: attachment_only(&received),
                sender: received.sender.sender_id.open_id.clone(),
            },
        );
    }

    /// 清空上下文：停掉这个聊天里在跑和排队的轮次，删掉它的全部会话数据——库里的行、
    /// 工作目录（聊天记录、附件、clone 的仓库）、Agent 的会话记录——再整理库、换新备份，
    /// 记下清空的时刻（`reset_at_ms`），之后的提问不再带上这之前的群聊。`received_at_ms`
    /// 及之前收到的消息一起删，之后才到的照常处理；确认回复在 `reply_to` 下面。失败时
    /// 「清空」那条消息留在收件箱，重启后再做一遍（清空本身可以重复做）。
    async fn clear(
        &mut self,
        chat_id: &str,
        received_at_ms: i64,
        reset_at_ms: i64,
        reply_to: &str,
    ) {
        let chat_id = chat_id.to_owned();
        let ids = match self.store.chat_conversation_ids(&chat_id).await {
            Ok(ids) => ids,
            Err(err) => {
                tracing::error!(chat = %chat_id, "清空上下文失败：{err:#}");
                return;
            }
        };
        let mut closing = Vec::new();
        for id in &ids {
            if let Some(slot) = self.actors.remove(id) {
                let (done, closed) = oneshot::channel();
                if slot.tx.send(actor::Msg::Close { done }).is_ok() {
                    closing.push(closed);
                }
            }
        }
        if tokio::time::timeout(CLEAR_WAIT, futures_util::future::join_all(closing))
            .await
            .is_err()
        {
            tracing::warn!(chat = %chat_id, "等在跑的轮次停下来超时，照样清空");
        }
        self.recent.retain(|(chat, _), _| *chat != chat_id);
        let cleared = match self
            .store
            .clear_chat(&chat_id, received_at_ms, reset_at_ms)
            .await
        {
            Ok(cleared) => cleared,
            Err(err) => {
                tracing::error!(chat = %chat_id, "清空上下文失败：{err:#}");
                return;
            }
        };
        if let Some(eraser) = &self.eraser {
            eraser.erase(&cleared.conversations, now_ms()).await;
        }
        tracing::info!(
            conversations = cleared.conversations.len(),
            turns = cleared.turns,
            "已清空上下文"
        );
        let runner = Arc::clone(&self.runner);
        let reply_to = reply_to.to_owned();
        tokio::spawn(async move { runner.confirm_cleared(&reply_to, cleared.turns).await });
    }

    /// 卡片输入框里的补充：没有对应的聊天消息，按一条文字消息落进收件箱（ID 以
    /// `context::CARD_INPUT_PREFIX` 开头，引用的是那张卡片），之后和普通消息走同一条路。
    async fn follow_up(
        &mut self,
        turn_id: &str,
        card_message_id: &str,
        chat_id: &str,
        text: &str,
        operator: &str,
    ) -> Toast {
        let text = text.trim();
        if text.is_empty() {
            return Toast::info("内容是空的");
        }
        let conversation = match self.store.turn(turn_id).await {
            Ok(Some(turn)) => match self.store.conversation(&turn.conversation_id).await {
                Ok(Some(conversation)) => conversation,
                Ok(None) => {
                    return Toast::warning(
                        "这个会话已经清理（过了保留期或清空了上下文），直接 @ 我提问即可",
                    );
                }
                Err(err) => {
                    tracing::error!("{err:#}");
                    return Toast::error("操作失败，稍后再试");
                }
            },
            Ok(None) => {
                return Toast::warning(
                    "这个会话已经清理（过了保留期或清空了上下文），直接 @ 我提问即可",
                );
            }
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        };
        let chat_type = if conversation.origin == Origin::P2p {
            "p2p"
        } else {
            "group"
        };
        let message_id = format!("{}{}", context::CARD_INPUT_PREFIX, Uuid::now_v7().simple());
        let now = now_ms();
        let text: String = text.chars().take(context::CARD_INPUT_CHARS).collect();
        let payload = serde_json::json!({
            "schema": "2.0",
            "header": { "event_id": &message_id, "event_type": "im.message.receive_v1" },
            "event": {
                "sender": { "sender_id": { "open_id": operator }, "sender_type": "user" },
                "message": {
                    "message_id": &message_id,
                    "chat_id": chat_id,
                    "chat_type": chat_type,
                    "message_type": "text",
                    "content": serde_json::json!({ "text": text }).to_string(),
                    "create_time": now.to_string(),
                    "parent_id": card_message_id,
                    "root_id": card_message_id,
                    "mentions": [],
                },
            },
        })
        .to_string();
        let stored = self
            .store
            .insert_input(&crate::store::NewInput {
                message_id: &message_id,
                chat_id,
                chat_type,
                sender_open_id: operator,
                payload: &payload,
                received_at_ms: now,
            })
            .await;
        if let Err(err) = stored {
            tracing::error!("{err:#}");
            return Toast::error("操作失败，稍后再试");
        }
        if let Err(err) = self
            .store
            .assign_conversation(&message_id, &conversation.id)
            .await
        {
            tracing::error!("{err:#}");
            return Toast::error("操作失败，稍后再试");
        }
        self.send(
            conversation.id,
            Msg::Input {
                message_id,
                attachment_only: false,
                sender: operator.to_owned(),
            },
        );
        Toast::success("收到，正在分析")
    }

    /// 给消息找会话，找不到就建一个。
    async fn resolve(
        &mut self,
        message_id: &str,
        received: &MessageReceived,
    ) -> anyhow::Result<String> {
        let message = &received.message;
        let thread = message.thread();
        let root = message.root_id.as_deref().filter(|r| !r.is_empty());
        let mut found = if message.is_p2p() {
            self.store.find_conversation(thread, root).await?
        } else {
            // 一个群一个会话（话题里的也算）
            self.store.chat_conversation(&message.chat_id).await?
        };
        if found.is_none()
            && let Some(parent) = message.parent_id.as_deref().filter(|p| !p.is_empty())
        {
            found = self.store.conversation_by_card(parent).await?;
        }
        let sender = received.sender.sender_id.open_id.as_str();
        let key = (message.chat_id.clone(), sender.to_owned());
        if found.is_none() && thread.is_none() {
            found = self
                .recent_conversation(&key, attachment_only(received))
                .await?;
        }
        // 私聊隔一阵再问多半还是前面那件事（同一个故障一问再问）：两小时内续接，不用每条
        // 都从零查起；隔久了再新开，免得背着不相关的上下文、每一步都变慢
        if found.is_none() && thread.is_none() && message.is_p2p() {
            let since = now_ms() - i64::try_from(P2P_CONTINUE.as_millis()).unwrap_or(i64::MAX);
            found = self
                .store
                .recent_p2p_conversation(&message.chat_id, since)
                .await?;
        }
        if let Some(conversation) = found {
            if message.is_p2p()
                && let (Some(thread), None) = (thread, &conversation.thread_id)
            {
                self.store
                    .set_conversation_thread(&conversation.id, thread)
                    .await?;
            }
            self.store
                .assign_conversation(message_id, &conversation.id)
                .await?;
            return Ok(conversation.id);
        }

        let bot = self.bot_open_id.get().map(String::as_str);
        let text = context::event_text(received, bot);
        let backend = context::split_backend_prefix(&text)
            .0
            .unwrap_or(&self.default_backend);
        let origin = if message.is_p2p() {
            Origin::P2p
        } else if thread.is_some() {
            Origin::ExistingThread
        } else {
            Origin::NewThread
        };
        // 群会话不绑定话题：群里各个话题的提问都进这一个会话
        let (thread, root_message_id) = match thread {
            Some(_) if message.is_p2p() => (thread, root.unwrap_or(message_id)),
            _ => (None, message_id),
        };
        let id = Uuid::now_v7().to_string();
        self.store
            .create_conversation(&NewConversation {
                id: &id,
                chat_id: &message.chat_id,
                chat_type: &message.chat_type,
                origin,
                thread_id: thread,
                root_message_id,
                owner_open_id: &received.sender.sender_id.open_id,
                backend,
                now_ms: now_ms(),
            })
            .await?;
        self.store.assign_conversation(message_id, &id).await?;
        tracing::info!(conversation = %id, ?origin, backend, "新会话");
        if thread.is_none() {
            let longest = self.merge_window(true);
            self.recent.retain(|_, r| r.since.elapsed() <= longest);
            self.recent.insert(
                key,
                Recent {
                    conversation_id: id.clone(),
                    since: Instant::now(),
                    attachment_only: attachment_only(received),
                },
            );
        }
        Ok(id)
    }

    /// 连发合并的窗口，和 actor 里的一致：只收到附件时多等一会儿后面的提问。
    fn merge_window(&self, attachment_only: bool) -> Duration {
        if attachment_only {
            self.debounce * ATTACHMENT_ONLY_FACTOR
        } else {
            self.debounce
        }
    }

    /// 这个人在这个聊天里、窗口期内刚开的会话。收到文字后窗口按正常长度算。
    async fn recent_conversation(
        &mut self,
        key: &(String, String),
        attachment_only: bool,
    ) -> anyhow::Result<Option<Conversation>> {
        let Some(recent) = self.recent.get(key) else {
            return Ok(None);
        };
        if recent.since.elapsed() > self.merge_window(recent.attachment_only) {
            return Ok(None);
        }
        let id = recent.conversation_id.clone();
        let found = self.store.conversation(&id).await?;
        if found.is_some()
            && let Some(recent) = self.recent.get_mut(key)
        {
            recent.attachment_only &= attachment_only;
        }
        Ok(found)
    }

    fn send(&mut self, conversation_id: String, msg: Msg) {
        let Self {
            actors,
            store,
            runner,
            debounce,
            idle_tx,
            ..
        } = self;
        let mut msg = msg;
        // 发送失败只可能是 actor 异常退出（panic）：换一个新的再发一次
        for _ in 0..2 {
            let slot = actors
                .entry(conversation_id.clone())
                .or_insert_with(|| Slot {
                    tx: Actor::spawn(
                        conversation_id.clone(),
                        store.clone(),
                        Arc::clone(runner),
                        *debounce,
                        idle_tx.clone(),
                    ),
                    sent: 0,
                });
            match slot.tx.send(msg) {
                Ok(()) => {
                    slot.sent += 1;
                    return;
                }
                Err(mpsc::error::SendError(returned)) => {
                    tracing::error!(conversation = %conversation_id, "会话 actor 已退出，重新创建");
                    actors.remove(&conversation_id);
                    msg = returned;
                }
            }
        }
    }

    /// actor 报告空闲：发给它的消息它都处理完了才回收，否则等它下一次报告。
    fn retire(&mut self, idle: &Idle) {
        if self
            .actors
            .get(&idle.conversation_id)
            .is_some_and(|slot| slot.sent == idle.epoch)
        {
            self.actors.remove(&idle.conversation_id);
        }
    }
}

/// 只有图片、文件或转发、没有文字的消息：多半后面还跟着一句提问。
fn attachment_only(received: &MessageReceived) -> bool {
    matches!(
        received.message.message_type.as_str(),
        "image" | "file" | "media" | "audio" | "sticker" | "merge_forward"
    )
}
