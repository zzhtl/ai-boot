//! 一个会话的 actor：决定消息进哪一轮、哪一轮什么时候开跑，处理停止和重试。
//!
//! 状态是 `running`（正在跑的一轮）和 `pending`（已经回了卡片、还在收连发消息或
//! 排队的轮次，先进先出）。一轮的第一条消息到了就建轮次、回卡片。同一个人的后续
//! 消息并进他还没开始的那一轮（连发、跑的过程中补充）；别人的提问各自排一轮——
//! 一个群共用一个会话，不能把不相干的问题揉成一轮。都空就是空闲，报告给 registry
//! 等待回收——状态全在数据库里，回收后再来消息重建即可。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::callback::{Action, Target, Toast};
use crate::runner::{Runner, TurnControl, TurnSpec};
use crate::store::{Store, TurnKind, TurnStatus, now_ms};

use super::ATTACHMENT_ONLY_FACTOR;

pub(super) enum Msg {
    Input {
        message_id: String,
        /// 只有附件没有文字：多等一会儿后面的提问。
        attachment_only: bool,
        /// 发消息的人：同一个人的连发并进一轮，不同人的各排一轮。
        sender: String,
    },
    Action {
        action: Action,
        turn_id: String,
        /// 点按钮的人（已过白名单）。
        operator: String,
        reply: oneshot::Sender<Toast>,
    },
}

/// actor 空闲了。`epoch` 是它处理过的 registry 消息数。
pub(super) struct Idle {
    pub conversation_id: String,
    pub epoch: u64,
}

struct Running {
    turn_id: String,
    cancel: CancellationToken,
}

struct Pending {
    turn_id: String,
    cancel: CancellationToken,
    go: oneshot::Sender<()>,
    since: Instant,
    /// 到目前为止只收到附件。
    attachment_only: bool,
    sender: String,
}

pub(super) struct Actor {
    conversation_id: String,
    store: Store,
    runner: Arc<Runner>,
    debounce: Duration,
    idle_tx: mpsc::UnboundedSender<Idle>,
    done_tx: mpsc::UnboundedSender<String>,
    done_rx: mpsc::UnboundedReceiver<String>,
    received: u64,
    notified: Option<u64>,
    running: Option<Running>,
    pending: VecDeque<Pending>,
}

impl Actor {
    pub(super) fn spawn(
        conversation_id: String,
        store: Store,
        runner: Arc<Runner>,
        debounce: Duration,
        idle_tx: mpsc::UnboundedSender<Idle>,
    ) -> mpsc::UnboundedSender<Msg> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let actor = Self {
            conversation_id,
            store,
            runner,
            debounce,
            idle_tx,
            done_tx,
            done_rx,
            received: 0,
            notified: None,
            running: None,
            pending: VecDeque::new(),
        };
        tokio::spawn(actor.run(rx));
        tx
    }

    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Msg>) {
        let mut open = true;
        loop {
            // 前一轮结束、窗口也过了，排在最前面的才开跑
            let start_at = match (&self.running, self.pending.front()) {
                (None, Some(pending)) => Some(pending.since + self.window(pending)),
                _ => None,
            };
            tokio::select! {
                msg = inbox.recv(), if open => match msg {
                    Some(msg) => {
                        self.received += 1;
                        self.handle(msg).await;
                    }
                    None => open = false,
                },
                Some(turn_id) = self.done_rx.recv() => self.finished(&turn_id),
                () = wait_until(start_at) => self.start_pending(),
            }
            if self.running.is_none() && self.pending.is_empty() {
                if !open {
                    break;
                }
                if self.notified != Some(self.received) {
                    self.notified = Some(self.received);
                    let _ = self.idle_tx.send(Idle {
                        conversation_id: self.conversation_id.clone(),
                        epoch: self.received,
                    });
                }
            }
        }
    }

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input {
                message_id,
                attachment_only,
                sender,
            } => self.input(message_id, attachment_only, sender).await,
            Msg::Action {
                action,
                turn_id,
                operator,
                reply,
            } => {
                let toast = match action {
                    Action::Stop => self.stop(&turn_id),
                    Action::Retry => self.retry(&turn_id).await,
                    Action::Resolve => self.resolve(&turn_id).await,
                    Action::Writeback {
                        target,
                        reference,
                        hash,
                    } => {
                        self.writeback(&turn_id, target, &reference, &hash, &operator)
                            .await
                    }
                    // 输入框的补充由 registry 当成一条新消息分派，不会走到这里
                    Action::FollowUp => Toast::info("已收到"),
                };
                let _ = reply.send(toast);
            }
        }
    }

    /// 连发合并的窗口。只发了截图、文件的，提问多半还在打字，多等一会儿；
    /// 等到文字就按正常窗口算，已经过了就立刻开始。
    fn window(&self, pending: &Pending) -> Duration {
        if pending.attachment_only {
            self.debounce * ATTACHMENT_ONLY_FACTOR
        } else {
            self.debounce
        }
    }

    async fn input(&mut self, message_id: String, attachment_only: bool, sender: String) {
        if let Some(pending) = self.pending.iter_mut().find(|p| p.sender == sender) {
            if let Err(err) = self.store.join_turn(&pending.turn_id, &message_id).await {
                tracing::error!(message_id, "{err:#}");
            }
            pending.attachment_only &= attachment_only;
            return;
        }
        let turn_id = Uuid::now_v7().to_string();
        let seq = match self
            .store
            .create_turn(&self.conversation_id, &turn_id, &message_id, now_ms())
            .await
        {
            Ok(seq) => seq,
            Err(err) => {
                // 消息没挂上轮次，重启时会重新分派
                tracing::error!(message_id, "{err:#}");
                return;
            }
        };
        let cancel = CancellationToken::new();
        let (go_tx, go_rx) = oneshot::channel();
        self.launch(
            TurnSpec {
                turn_id: turn_id.clone(),
                conversation_id: self.conversation_id.clone(),
                seq,
                retry: false,
                queued_behind: self.running.is_some() || !self.pending.is_empty(),
                kind: TurnKind::Ask,
                reply_to: None,
            },
            go_rx,
            cancel.clone(),
        );
        self.pending.push_back(Pending {
            turn_id,
            cancel,
            go: go_tx,
            since: Instant::now(),
            attachment_only,
            sender,
        });
    }

    fn start_pending(&mut self) {
        if self.running.is_some() {
            return;
        }
        if let Some(pending) = self.pending.pop_front() {
            // 发送失败说明这一轮的任务已经提前结束（比如卡片没回出去）
            if pending.go.send(()).is_ok() {
                self.running = Some(Running {
                    turn_id: pending.turn_id,
                    cancel: pending.cancel,
                });
            }
        }
    }

    fn finished(&mut self, turn_id: &str) {
        if self.running.as_ref().is_some_and(|r| r.turn_id == turn_id) {
            self.running = None;
        }
        // 排队的任务提前结束了（比如卡片没回出去）
        self.pending.retain(|p| p.turn_id != turn_id);
    }

    fn stop(&mut self, turn_id: &str) -> Toast {
        if let Some(running) = self.running.as_ref().filter(|r| r.turn_id == turn_id) {
            running.cancel.cancel();
            return Toast::success("正在停止，稍等几秒");
        }
        if let Some(index) = self.pending.iter().position(|p| p.turn_id == turn_id) {
            // 从队列里拿掉，之后的新消息进新的一轮
            if let Some(pending) = self.pending.remove(index) {
                pending.cancel.cancel();
            }
            return Toast::success("已取消");
        }
        Toast::info("这一轮已经结束了")
    }

    async fn retry(&mut self, turn_id: &str) -> Toast {
        if self.running.is_some() || !self.pending.is_empty() {
            return Toast::warning("这个会话里还有一轮没结束，结束后再重试");
        }
        let turn = match self.store.turn(turn_id).await {
            Ok(Some(turn)) => turn,
            Ok(None) => return Toast::warning("找不到这一轮"),
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        };
        match self.store.latest_seq(&self.conversation_id).await {
            Ok(latest) if latest == turn.seq => {}
            Ok(_) => return Toast::warning("只能重试最新的一轮"),
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        }
        if !turn.status.is_retryable() {
            return Toast::info("这一轮不需要重试");
        }
        match self.store.requeue_turn(turn_id).await {
            Ok(true) => {}
            Ok(false) => return Toast::info("这一轮已经在重试了"),
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        }
        let cancel = CancellationToken::new();
        let (go_tx, go_rx) = oneshot::channel();
        let _ = go_tx.send(());
        self.launch(
            TurnSpec {
                turn_id: turn.id.clone(),
                conversation_id: self.conversation_id.clone(),
                seq: turn.seq,
                retry: true,
                queued_behind: false,
                kind: turn.kind,
                reply_to: None,
            },
            go_rx,
            cancel.clone(),
        );
        self.running = Some(Running {
            turn_id: turn.id,
            cancel,
        });
        Toast::success("开始重试")
    }

    /// 「已解决」：在最新一轮成功的答案上生成闭环方案。
    async fn resolve(&mut self, turn_id: &str) -> Toast {
        if self.running.is_some() || !self.pending.is_empty() {
            return Toast::warning("这个会话里还有一轮没结束，结束后再点");
        }
        let turn = match self.store.turn(turn_id).await {
            Ok(Some(turn)) => turn,
            Ok(None) => return Toast::warning("找不到这一轮"),
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        };
        if turn.kind != TurnKind::Ask || turn.status != TurnStatus::Succeeded {
            return Toast::warning("只能在成功的答案上生成闭环方案");
        }
        match self.store.latest_seq(&self.conversation_id).await {
            Ok(latest) if latest == turn.seq => {}
            Ok(_) => return Toast::warning("只能在最新一轮的答案上点"),
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        }
        let resolve_id = Uuid::now_v7().to_string();
        let seq = match self
            .store
            .create_resolve_turn(&self.conversation_id, &resolve_id, now_ms())
            .await
        {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!("{err:#}");
                return Toast::error("操作失败，稍后再试");
            }
        };
        let cancel = CancellationToken::new();
        let (go_tx, go_rx) = oneshot::channel();
        let _ = go_tx.send(());
        self.launch(
            TurnSpec {
                turn_id: resolve_id.clone(),
                conversation_id: self.conversation_id.clone(),
                seq,
                retry: false,
                queued_behind: false,
                kind: TurnKind::Resolve,
                reply_to: turn.card_message_id,
            },
            go_rx,
            cancel.clone(),
        );
        self.running = Some(Running {
            turn_id: resolve_id,
            cancel,
        });
        Toast::success("开始生成闭环方案")
    }

    /// 写回：校验、登记后在后台执行，卡片由写回任务更新。
    async fn writeback(
        &self,
        turn_id: &str,
        target: Target,
        reference: &str,
        hash: &str,
        operator: &str,
    ) -> Toast {
        let Some(writer) = self.runner.writer().cloned() else {
            return Toast::warning("没有配置写回");
        };
        match writer
            .prepare(turn_id, target, reference, hash, operator)
            .await
        {
            Ok(job) => {
                tokio::spawn(async move { writer.execute(job).await });
                Toast::success("开始写入，完成后卡片会更新")
            }
            Err(toast) => toast,
        }
    }

    fn launch(&self, spec: TurnSpec, go: oneshot::Receiver<()>, cancel: CancellationToken) {
        let runner = Arc::clone(&self.runner);
        // 用 Drop 报告结束：执行任务 panic 时也要让 actor 知道，否则这个会话会卡住
        let done = DoneGuard {
            tx: self.done_tx.clone(),
            turn_id: spec.turn_id.clone(),
        };
        tokio::spawn(async move {
            let _done = done;
            runner.run_turn(spec, TurnControl { go, cancel }).await;
        });
    }
}

struct DoneGuard {
    tx: mpsc::UnboundedSender<String>,
    turn_id: String,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(std::mem::take(&mut self.turn_id));
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}
