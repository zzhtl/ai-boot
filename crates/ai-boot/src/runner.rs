//! 一轮问答的执行：回（或复用）进度卡 → 等会话 actor 的开跑信号 → 采集上下文 →
//! 调 Agent（续接会话；续接不上就新开会话并补前情）→ 节流更新进度 → 换成答案卡 → 落库。
//!
//! 一张卡片只由它所属这一轮的执行任务更新；按钮回调只回 toast。

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ai_boot_agent::{
    Activity, AgentBackend, AgentEvent, Effort, FailKind, McpServer, Outcome, SessionRef, Started,
    Step, TurnHandle, TurnRequest, Usage,
};
use ai_boot_feishu::api::{ApiClient, Reply};
use ai_boot_feishu::event::MessageReceived;
use futures_util::StreamExt as _;
use serde_json::Value;
use tokio::sync::{Semaphore, oneshot, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::alert::Alerts;
use crate::answer::{self, Reply as AnswerReply};
use crate::context;
use crate::oauth::{Access, Oauth};
use crate::prompt::{self, Prior};
use crate::render::card::{self, Footer};
use crate::render::diagram;
use crate::render::markdown::HostAllowlist;
use crate::render::steps;
use crate::store::{
    Conversation, FinishedTurn, InterruptedTurn, Origin, Store, TurnKind, TurnRecord, TurnStatus,
    now_ms,
};
use crate::writeback::{self, Writer};

/// 进度卡最多多久更新一次。单条消息的更新上限是 5 QPS，这里远低于它。
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// 没有新进展时也隔这么久刷新一次用时，让人看得出还在跑。
const HEARTBEAT: Duration = Duration::from_secs(10);
/// 这么久一条输出都没有，就在进度卡上提示可能卡住了。模型等首个 token、工具
/// 慢查询都会安静一阵，所以留得比较宽。
const STALL_AFTER: Duration = Duration::from_secs(90);
/// 进度卡上的阶段。
const PHASE_READING: &str = "📥 正在读取上下文…";
const PHASE_STARTING: &str = "🚀 正在启动 Claude";
const PHASE_THINKING: &str = "💭 思考中";
const PHASE_WRITING: &str = "✍️ 正在输出";
const PHASE_TOOL: &str = "🔧 调用工具中";
const PHASE_CONCLUDING: &str = "📝 正在整理结论";
const DENY_TEXT: &str = "你没有使用这个机器人的权限。";
/// 模型能用的内置工具。
const BUILTIN_TOOLS: [&str; 7] = [
    "Read",
    "Glob",
    "Grep",
    "Bash",
    "WebFetch",
    "WebSearch",
    "StructuredOutput",
];
/// 一个答案最多附几个文件。
const MAX_FILES: usize = 5;
/// 附带的文件大小上限：飞书文件消息最大 30 MB。
const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;
/// 落库的提问文字上限：只用来给续接失败的会话补前情。
const QUESTION_KEEP_CHARS: usize = 4000;
/// 上一轮结束时上下文超过这么多（token），这一轮就把前几轮的结论收拢成摘要、换新
/// 会话。每次请求都要带上整段会话，越长每一步越慢；一轮之内由 CLI 的自动压缩兜底。
const COMPACT_AT_TOKENS: i64 = 120_000;

pub struct Settings {
    pub data_dir: PathBuf,
    /// 附件解析用的外部工具与临时目录。
    pub tools: context::extract::Tools,
    /// 新会话第一轮带上群里最近的消息：最多几条、往前看多久。
    pub window: Option<(usize, Duration)>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub timeout: Duration,
    pub budget_usd_micros: Option<u64>,
    pub mcp_servers: Vec<McpServer>,
    pub link_hosts: Vec<String>,
    /// 环境速查文件，接在规则后面；没配就不带。
    pub knowledge_file: Option<PathBuf>,
}

pub struct Runner {
    api: Arc<ApiClient>,
    store: Store,
    /// 按名字（会话绑定的 `backend`）找后端。
    backends: HashMap<String, Arc<dyn AgentBackend>>,
    settings: Settings,
    slots: Arc<Semaphore>,
    bot_open_id: Arc<OnceLock<String>>,
    /// 以提问人的身份读云文档；没配就不读。
    oauth: Option<Arc<Oauth>>,
    /// 闭环方案写回；没配就只生成方案、不给写回按钮。
    writer: Option<Arc<Writer>>,
    /// 需要人去服务器上处理的故障，私聊告警。
    alerts: Option<Arc<Alerts>>,
}

/// 要执行的一轮。
pub struct TurnSpec {
    pub turn_id: String,
    pub conversation_id: String,
    pub seq: i64,
    /// 重试：复用这一轮原来的卡片。
    pub retry: bool,
    /// 建这一轮时，同一会话里还有一轮没结束。
    pub queued_behind: bool,
    pub kind: TurnKind,
    /// 闭环方案那一轮没有用户消息，卡片回复在被点击的答案卡下面。
    pub reply_to: Option<String>,
}

/// 会话 actor 对一轮的控制。
pub struct TurnControl {
    /// 开跑信号：连发合并的窗口过去、同一会话的前一轮也结束后发出。发送端被
    /// 丢弃等同取消。
    pub go: oneshot::Receiver<()>,
    /// 停止按钮。
    pub cancel: CancellationToken,
}

/// 一次 Agent 执行的结果。
struct Attempt {
    outcome: Outcome,
    model: String,
    tool_calls: usize,
    usage: Option<Usage>,
    self_check: Option<String>,
    /// 后端真正起了会话（收到了 Started）时的会话 ID。
    session_id: Option<String>,
    /// 是按停止按钮停下的。
    stopped: bool,
    /// 从启动进程算起，各阶段开始的时刻：慢的时候看得出慢在哪一段。
    timings: Timings,
}

#[derive(Debug, Default, Clone, Copy)]
struct Timings {
    /// CLI 报告会话已启动（加载完 MCP）。
    started: Option<Duration>,
    /// 模型开始写最终的结构化答案。
    concluding: Option<Duration>,
    /// 结束。
    finished: Option<Duration>,
}

/// 一轮的结局：最终卡片和要落库的内容。
struct Report {
    card: Value,
    status: TurnStatus,
    model: String,
    answer_json: Option<String>,
    error_kind: Option<&'static str>,
    error: Option<String>,
    tokens: i64,
    tool_calls: i64,
    session_id: Option<String>,
    session_tokens: Option<i64>,
    /// 本轮结束时带着的上下文，下一轮据此决定要不要换新会话。
    context_tokens: Option<i64>,
    cursor: Option<i64>,
}

impl Report {
    /// Agent 没有跑起来就结束的一轮。
    fn early(card: Value, status: TurnStatus, error_kind: &'static str, error: String) -> Self {
        Self {
            card,
            status,
            model: String::new(),
            answer_json: None,
            error_kind: Some(error_kind),
            error: Some(error),
            tokens: 0,
            tool_calls: 0,
            session_id: None,
            session_tokens: None,
            context_tokens: None,
            cursor: None,
        }
    }
}

/// 一轮执行时进度卡上的固定部分。
#[derive(Clone, Copy)]
struct Screen<'a> {
    spec: &'a TurnSpec,
    card_id: &'a str,
    preview: &'a str,
    read: &'a [String],
    started_at: Instant,
    stop: &'a CancellationToken,
}

/// 分析过程中进度卡上会变的部分。
#[derive(Default)]
struct Live<'a> {
    steps: &'a [String],
    /// 模型最近一次说明的进展。
    thought: Option<&'a str>,
    /// 正在写的结论里已经写完的概述。
    draft: Option<&'a str>,
    /// 已经写完的关键命令。
    commands: &'a [answer::Command],
    notes: &'a [String],
}

impl Screen<'_> {
    fn progress(&self, live: &Live<'_>, stoppable: bool) -> Value {
        card::progress(&card::Progress {
            question: self.preview,
            read: self.read,
            steps: live.steps,
            thought: live.thought,
            draft: live.draft,
            commands: live.commands,
            elapsed: self.started_at.elapsed(),
            notes: live.notes,
            stop: stoppable.then_some(self.spec.turn_id.as_str()),
        })
    }
}

/// 工作目录里的完整聊天记录：新会话整份重写，追问轮追加。
enum Transcript {
    Replace(String),
    Append(String),
}

impl Runner {
    pub fn new(
        api: Arc<ApiClient>,
        store: Store,
        backends: HashMap<String, Arc<dyn AgentBackend>>,
        settings: Settings,
        max_concurrent: usize,
        bot_open_id: Arc<OnceLock<String>>,
    ) -> Self {
        Self {
            api,
            store,
            backends,
            settings,
            slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
            bot_open_id,
            oauth: None,
            writer: None,
            alerts: None,
        }
    }

    pub fn with_alerts(mut self, alerts: Arc<Alerts>) -> Self {
        self.alerts = Some(alerts);
        self
    }

    pub fn with_writer(mut self, writer: Arc<Writer>) -> Self {
        self.writer = Some(writer);
        self
    }

    pub fn writer(&self) -> Option<&Arc<Writer>> {
        self.writer.as_ref()
    }

    pub fn with_oauth(mut self, oauth: Arc<Oauth>) -> Self {
        self.oauth = Some(oauth);
        self
    }

    /// 执行一轮，直到它结束（完成、失败、被停止或取消）。
    pub async fn run_turn(&self, spec: TurnSpec, mut control: TurnControl) {
        let started_at = Instant::now();
        let conversation = match self.store.conversation(&spec.conversation_id).await {
            Ok(Some(conversation)) => conversation,
            Ok(None) => {
                tracing::error!(turn = %spec.turn_id, "会话不存在");
                return;
            }
            Err(err) => {
                tracing::error!(turn = %spec.turn_id, "{err:#}");
                return;
            }
        };
        let (reply_to, preview) = match spec.kind {
            TurnKind::Ask => match self.first_input(&spec.turn_id).await {
                Ok((first_id, first)) => {
                    let bot = self.bot_open_id.get().map(String::as_str);
                    let text = context::event_text(&first, bot);
                    // 卡片输入框里的补充没有聊天消息可回，回复到那张卡片上
                    let reply_to = if context::is_card_input(&first_id) {
                        first
                            .message
                            .parent_id
                            .clone()
                            .filter(|p| !p.is_empty())
                            .unwrap_or(first_id)
                    } else {
                        first_id
                    };
                    (reply_to, context::split_backend_prefix(&text).1.to_owned())
                }
                Err(err) => {
                    tracing::error!(turn = %spec.turn_id, "{err}");
                    return;
                }
            },
            TurnKind::Resolve => (
                spec.reply_to.clone().unwrap_or_default(),
                "问题已解决，生成闭环方案".to_owned(),
            ),
        };
        let stop = Some(spec.turn_id.as_str());

        let mut notes = Vec::new();
        let mut read = Vec::new();
        if spec.queued_behind {
            notes.push("排队中：这个会话的上一轮结束后开始".to_owned());
        } else if spec.kind == TurnKind::Ask {
            read.push(PHASE_READING.to_owned());
        }
        let opening = card::progress(&card::Progress {
            question: &preview,
            read: &read,
            notes: &notes,
            stop,
            ..card::Progress::default()
        });
        let Some(card_id) = self.open_card(&spec, &reply_to, &opening).await else {
            let report = Report::early(
                Value::Null,
                TurnStatus::Failed,
                "card",
                "回复卡片失败".to_owned(),
            );
            self.record(&spec, &conversation, &report, started_at).await;
            return;
        };

        // 开跑信号：连发合并、同一话题排队
        let go = tokio::select! {
            go = &mut control.go => go.is_ok(),
            () = control.cancel.cancelled() => false,
        };
        if !go {
            self.cancelled(&spec, &conversation, &card_id, started_at)
                .await;
            return;
        }

        // 全局并发
        let permit = match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let waiting = card::progress(&card::Progress {
                    question: &preview,
                    elapsed: started_at.elapsed(),
                    notes: &["排队中，其他会话的问题完成后开始".to_owned()],
                    stop,
                    ..card::Progress::default()
                });
                self.update(&card_id, &waiting).await;
                tokio::select! {
                    permit = Arc::clone(&self.slots).acquire_owned() => match permit {
                        Ok(permit) => permit,
                        // 信号量不会被关闭
                        Err(_) => return,
                    },
                    () = control.cancel.cancelled() => {
                        self.cancelled(&spec, &conversation, &card_id, started_at).await;
                        return;
                    }
                }
            }
        };

        // 排队期间前一轮可能刚落了会话 ID、游标和用量，必须用最新的
        let conversation = match self.store.conversation(&spec.conversation_id).await {
            Ok(Some(latest)) => latest,
            Ok(None) => conversation,
            Err(err) => {
                tracing::warn!("{err:#}，沿用排队前读到的会话状态");
                conversation
            }
        };
        let report = match spec.kind {
            TurnKind::Ask => {
                self.analyse(
                    &spec,
                    &conversation,
                    &card_id,
                    &preview,
                    started_at,
                    &control.cancel,
                )
                .await
            }
            TurnKind::Resolve => {
                self.analyse_resolve(
                    &spec,
                    &conversation,
                    &card_id,
                    &preview,
                    started_at,
                    &control.cancel,
                )
                .await
            }
        };
        drop(permit);
        self.record(&spec, &conversation, &report, started_at).await;
        if !self.update_final(&card_id, &report.card).await {
            // 图表、图片被飞书拒收也不能让进度卡一直挂着：去掉它们再送一次
            let plain = card::without_visuals(&report.card);
            if plain != report.card {
                self.update_final(&card_id, &plain).await;
            }
        }
        if report.status == TurnStatus::Succeeded {
            self.send_files(&card_id, &conversation, report.answer_json.as_deref())
                .await;
        }
        if spec.kind == TurnKind::Ask && report.status == TurnStatus::Succeeded {
            self.mark_superseded(&spec, &conversation, report.answer_json.as_deref())
                .await;
        }
        self.alert(&report).await;
    }

    /// 答案附带的文件（脚本、报告、导出的数据）：上传后作为文件消息回复在答案卡下面。
    /// 发不出去的回一句原因，免得提问人以为没有附件。
    async fn send_files(
        &self,
        card_id: &str,
        conversation: &Conversation,
        answer_json: Option<&str>,
    ) {
        let Some(answer) =
            answer_json.and_then(|json| serde_json::from_str::<answer::Answer>(json).ok())
        else {
            return;
        };
        let workdir = self
            .settings
            .data_dir
            .join("sessions")
            .join(&conversation.id);
        for path in answer.files.iter().take(MAX_FILES) {
            let uploaded = match workdir_file(&workdir, path, MAX_FILE_BYTES).await {
                Ok((name, bytes)) => self
                    .api
                    .upload_file(&name, bytes)
                    .await
                    .map_err(|err| format!("上传失败：{err}")),
                Err(reason) => Err(reason),
            };
            let reply = match &uploaded {
                Ok(file_key) => Reply {
                    message_id: card_id,
                    msg_type: "file",
                    content: serde_json::json!({ "file_key": file_key }).to_string(),
                    reply_in_thread: false,
                    uuid: idempotency_key("file", file_key),
                },
                Err(reason) => {
                    tracing::warn!(%reason, conversation = %conversation.id, "答案附带的文件发不出去");
                    Reply {
                        message_id: card_id,
                        msg_type: "text",
                        content: serde_json::json!({
                            "text": format!("附件 {path} 没有发出：{reason}")
                        })
                        .to_string(),
                        reply_in_thread: false,
                        uuid: idempotency_key("file-error", &format!("{card_id}/{path}")),
                    }
                }
            };
            if let Err(err) = self.api.reply(reply).await {
                tracing::warn!(%err, "回复附件失败");
            }
        }
    }

    /// 这一轮更正了之前的结论：把上一张答案卡重画成「已更正，以最新回复为准」。
    async fn mark_superseded(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        answer_json: Option<&str>,
    ) {
        let corrected = answer_json
            .and_then(|json| serde_json::from_str::<answer::Answer>(json).ok())
            .is_some_and(|answer| !answer.corrections.is_empty());
        if !corrected {
            return;
        }
        // 在哪张卡片上补充、纠正的，就把哪张标成已更正；直接 @ 的算会话里上一张答案
        let followed = match self.followed_card(&spec.turn_id).await {
            Some(card) => self.store.answer_by_card(&card).await.ok().flatten(),
            None => None,
        };
        let previous = match followed {
            Some(followed) => followed,
            None => match self.store.previous_answer(&conversation.id, spec.seq).await {
                Ok(Some(previous)) => previous,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!("{err:#}");
                    return;
                }
            },
        };
        let (Some(card_id), Some(answer)) = (
            previous.card_message_id.as_deref(),
            previous
                .answer_json
                .as_deref()
                .and_then(|json| serde_json::from_str::<answer::Answer>(json).ok()),
        ) else {
            return;
        };
        // 旧卡片上不再放输入框：以最新的回复为准
        let footer = Footer {
            turn: u32::try_from(previous.seq).unwrap_or(u32::MAX),
            elapsed: Duration::from_millis(
                u64::try_from(previous.duration_ms.unwrap_or(0)).unwrap_or(0),
            ),
            ..Footer::default()
        };
        let links = HostAllowlist(&self.settings.link_hosts);
        let corrected_in = u32::try_from(spec.seq).unwrap_or(u32::MAX);
        let card = card::superseded(&answer, &footer, &links, corrected_in);
        self.update(card_id, &card).await;
    }

    /// 这一轮是在哪张机器人卡片上接着问的（引用卡片，或在卡片输入框里补充）。
    async fn followed_card(&self, turn_id: &str) -> Option<String> {
        let (_, first) = self.first_input(turn_id).await.ok()?;
        let parent = first.message.parent_id.filter(|p| !p.is_empty())?;
        match self.store.turn_by_card(&parent).await {
            Ok(Some(_)) => Some(parent),
            _ => None,
        }
    }

    /// 登录失效、运行环境自检不通过：不是重试能解决的，告诉管理员。
    async fn alert(&self, report: &Report) {
        let Some(alerts) = &self.alerts else {
            return;
        };
        let reason = report.error.as_deref().unwrap_or_default();
        match report.error_kind {
            Some("auth") => {
                alerts
                    .notify(
                        "auth",
                        &format!(
                            "Agent 登录失效，分析都会失败，需要在服务器上重新登录（{reason}）"
                        ),
                    )
                    .await;
            }
            Some("self_check") => {
                alerts
                    .notify(
                        "self_check",
                        &format!("运行环境自检不通过，分析都会被取消：{reason}"),
                    )
                    .await;
            }
            _ => {}
        }
    }

    async fn analyse(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        card_id: &str,
        preview: &str,
        started_at: Instant,
        stop: &CancellationToken,
    ) -> Report {
        let retry = Some(spec.turn_id.as_str());
        let Some(backend) = self.backends.get(&conversation.backend).cloned() else {
            let reason = format!("没有配置 {} 后端。", conversation.backend);
            return Report::early(
                card::failure("❌ 后端不可用", &reason, "去掉开头的后端前缀重新提问", None),
                TurnStatus::Failed,
                "backend",
                reason,
            );
        };
        let inputs = match self.inputs(&spec.turn_id).await {
            Ok(inputs) => inputs,
            Err(err) => {
                return Report::early(
                    card::failure("❌ 读取提问失败", &err, "查看服务日志", retry),
                    TurnStatus::Failed,
                    "store",
                    err,
                );
            }
        };
        let workdir = self
            .settings
            .data_dir
            .join("sessions")
            .join(&conversation.id);
        // 会话已经很长：前几轮只带结论，聊天记录和之前的答案都在工作目录里，要细节它自己查
        let compact = conversation.agent_session_id.is_some()
            && conversation.context_tokens >= COMPACT_AT_TOKENS;
        let resume = conversation.agent_session_id.clone().filter(|_| !compact);
        let fresh = conversation.agent_session_id.is_none();
        // 连发并进来的消息（先发截图、再打字提问）也显示在进度卡的「问题」里
        let bot = self.bot_open_id.get().map(String::as_str);
        let joined = inputs
            .iter()
            .map(|received| {
                let text = context::event_text(received, bot);
                context::split_backend_prefix(&text).1.trim().to_owned()
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let preview = if joined.is_empty() {
            preview
        } else {
            joined.as_str()
        };
        let reading = [PHASE_READING.to_owned()];
        let mut screen = Screen {
            spec,
            card_id,
            preview,
            read: &reading,
            started_at,
            stop,
        };
        let gathering = Instant::now();
        let context = self
            .gather(&inputs, &workdir, spec.seq, fresh, conversation, screen)
            .await;
        let gathered = gathering.elapsed();
        let manifest = context.manifest();
        tracing::info!(
            turn = %spec.turn_id,
            elapsed_ms = gathered.as_millis(),
            messages = manifest.messages,
            images = manifest.images,
            files = manifest.files,
            "上下文已读取"
        );
        let read = read_lines(&manifest);
        screen.read = &read;
        // 跟启动 Agent 一起进行：每次更新卡片都要一个来回，不值得让 Agent 等它
        let starting = [PHASE_STARTING.to_owned()];
        let read_card = screen.progress(
            &Live {
                notes: &starting,
                ..Live::default()
            },
            true,
        );

        let question: String = context.question.chars().take(QUESTION_KEEP_CHARS).collect();
        let manifest_json = serde_json::to_string(&manifest).ok();
        if let Err(err) = self
            .store
            .start_turn(&spec.turn_id, &question, manifest_json.as_deref(), now_ms())
            .await
        {
            tracing::warn!("{err:#}");
        }
        let (session, prompt, transcript) = match &resume {
            Some(session_id) => {
                let (prompt, appended) = prompt::follow_up(&context, spec.seq);
                (
                    SessionRef::Resume(session_id.clone()),
                    prompt,
                    Transcript::Append(appended),
                )
            }
            None if !fresh => {
                tracing::info!(
                    conversation = %conversation.id,
                    context_tokens = conversation.context_tokens,
                    "会话上下文已经很长，前几轮收拢成结论摘要，换新会话"
                );
                let prior = self.priors(conversation, spec.seq).await;
                // 本轮新增的照常追加进完整记录
                let (_, appended) = prompt::follow_up(&context, spec.seq);
                (
                    SessionRef::New,
                    prompt::recovered(&context, &prior, prompt::Handoff::Compacted),
                    Transcript::Append(appended),
                )
            }
            None => {
                let prior = self.priors(conversation, spec.seq).await;
                let (prompt, transcript) = prompt::first_turn(&context, &prior);
                (SessionRef::New, prompt, Transcript::Replace(transcript))
            }
        };
        if let Err(err) = write_transcript(&workdir, &transcript) {
            let reason = format!("{err}");
            return Report::early(
                card::failure("❌ 准备工作目录失败", &reason, "查看服务日志", retry),
                TurnStatus::Failed,
                "workdir",
                reason,
            );
        }
        let cursor = context.newest_ms;
        let mut resumed = resume.is_some();
        let images = absolute_images(&workdir, &context);
        let ((), mut attempt) = tokio::join!(
            self.update(card_id, &read_card),
            self.attempt(&*backend, &screen, &workdir, session, prompt, images, 1),
        );

        let lost = matches!(
            attempt.outcome,
            Outcome::Failed {
                kind: FailKind::SessionNotFound,
                ..
            }
        );
        if resumed && lost && !attempt.stopped {
            // 之前发过的聊天记录和附件都在工作目录里（本轮新增的也已经追加进完整记录），
            // 不再从飞书重拉、重发：告诉新会话去哪里查，只给本轮新增的
            tracing::warn!(conversation = %conversation.id, "会话续接不上，新开会话，之前的上下文让它从工作目录读");
            let prior = self.priors(conversation, spec.seq).await;
            let prompt = prompt::recovered(&context, &prior, prompt::Handoff::Lost);
            resumed = false;
            let images = absolute_images(&workdir, &context);
            attempt = self
                .attempt(
                    &*backend,
                    &screen,
                    &workdir,
                    SessionRef::New,
                    prompt,
                    images,
                    2,
                )
                .await;
        }
        // 卡片输入框里的补充在群里看不到原话，答案卡上要写出来
        let asked = inputs
            .first()
            .is_some_and(|r| context::is_card_input(&r.message.message_id))
            .then_some(question.as_str());
        let timings = attempt.timings;
        let report = self
            .report(
                spec,
                conversation,
                &workdir,
                asked,
                attempt,
                resumed,
                cursor,
                started_at.elapsed(),
            )
            .await;
        let millis = |d: Option<Duration>| d.map(|d| d.as_millis()).unwrap_or_default();
        tracing::info!(
            turn = %spec.turn_id,
            gather_ms = gathered.as_millis(),
            cli_started_ms = millis(timings.started),
            concluding_ms = millis(timings.concluding),
            finished_ms = millis(timings.finished),
            context_tokens = report.context_tokens.unwrap_or_default(),
            "本轮各阶段耗时（后三项从启动 Agent 算起）"
        );
        // 答案也留一份在工作目录：换新会话后前几轮只带结论，要看全文去这里查
        if let Some(answer) = report
            .answer_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<answer::Answer>(json).ok())
            && let Err(err) = append_answer(
                &workdir,
                &prompt::answer_record(spec.seq, &question, &answer),
            )
        {
            tracing::warn!(%err, "答案没能写进工作目录");
        }
        report
    }

    /// 生成闭环方案：续接会话，只给要求；续接不上就新开会话、用前几轮的结论补
    /// 前情。成功后查出写回目标，换成闭环卡片。
    async fn analyse_resolve(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        card_id: &str,
        preview: &str,
        started_at: Instant,
        stop: &CancellationToken,
    ) -> Report {
        let Some(backend) = self.backends.get(&conversation.backend).cloned() else {
            let reason = format!("没有配置 {} 后端。", conversation.backend);
            return Report::early(
                card::failure("❌ 后端不可用", &reason, "", None),
                TurnStatus::Failed,
                "backend",
                reason,
            );
        };
        if let Err(err) = self
            .store
            .start_turn(&spec.turn_id, "生成闭环方案", None, now_ms())
            .await
        {
            tracing::warn!("{err:#}");
        }
        let workdir = self
            .settings
            .data_dir
            .join("sessions")
            .join(&conversation.id);
        if let Err(err) = prepare_dir(&workdir) {
            let reason = format!("{err}");
            return Report::early(
                card::failure(
                    "❌ 准备工作目录失败",
                    &reason,
                    "查看服务日志",
                    Some(&spec.turn_id),
                ),
                TurnStatus::Failed,
                "workdir",
                reason,
            );
        }
        let screen = Screen {
            spec,
            card_id,
            preview,
            read: &[],
            started_at,
            stop,
        };
        let (session, prompt, mut resumed) = match &conversation.agent_session_id {
            Some(session_id) => (
                SessionRef::Resume(session_id.clone()),
                prompt::resolve(),
                true,
            ),
            None => (
                SessionRef::New,
                prompt::resolve_fresh(&self.priors(conversation, spec.seq).await),
                false,
            ),
        };
        let mut attempt = self
            .attempt(&*backend, &screen, &workdir, session, prompt, Vec::new(), 1)
            .await;
        let lost = matches!(
            attempt.outcome,
            Outcome::Failed {
                kind: FailKind::SessionNotFound,
                ..
            }
        );
        if resumed && lost && !attempt.stopped {
            tracing::warn!(conversation = %conversation.id, "会话续接不上，用前几轮的结论生成闭环方案");
            let prompt = prompt::resolve_fresh(&self.priors(conversation, spec.seq).await);
            resumed = false;
            attempt = self
                .attempt(
                    &*backend,
                    &screen,
                    &workdir,
                    SessionRef::New,
                    prompt,
                    Vec::new(),
                    2,
                )
                .await;
        }
        let mut report = self
            .report(
                spec,
                conversation,
                &workdir,
                None,
                attempt,
                resumed,
                None,
                started_at.elapsed(),
            )
            .await;
        let answer = report
            .answer_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<answer::Answer>(json).ok());
        if let (TurnStatus::Succeeded, Some(answer)) = (report.status, answer) {
            let targets = match &self.writer {
                Some(writer) => writer.targets(&answer).await,
                None => Vec::new(),
            };
            let targets_json = serde_json::to_string(&targets).unwrap_or_else(|_| "[]".to_owned());
            if let Err(err) = self
                .store
                .set_writeback_targets(&spec.turn_id, &targets_json)
                .await
            {
                tracing::error!("{err:#}");
            }
            let record = TurnRecord {
                id: spec.turn_id.clone(),
                seq: spec.seq,
                kind: TurnKind::Resolve,
                status: TurnStatus::Succeeded,
                card_message_id: Some(card_id.to_owned()),
                answer_json: report.answer_json.clone(),
                writeback_targets: Some(targets_json),
                duration_ms: Some(
                    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX),
                ),
            };
            if let Some(card) = writeback::closure_card(&record, &[], &self.settings.link_hosts) {
                report.card = card;
            }
        }
        report
    }

    /// 采集上下文，同时把读到的消息、图片、文件数一路累加到进度卡上。
    async fn gather(
        &self,
        inputs: &[MessageReceived],
        workdir: &Path,
        seq: i64,
        fresh: bool,
        conversation: &Conversation,
        screen: Screen<'_>,
    ) -> context::TurnContext {
        let (progress, mut seen) = watch::channel(context::Gathering::default());
        let done = CancellationToken::new();
        let collecting = async {
            let context = self
                .collect(inputs, workdir, seq, fresh, conversation, &progress)
                .await;
            done.cancel();
            context
        };
        let showing = async {
            loop {
                tokio::select! {
                    () = done.cancelled() => break,
                    changed = seen.changed() => if changed.is_err() { break },
                }
                let read = [gathering_line(&seen.borrow_and_update())];
                let card = Screen {
                    read: &read,
                    ..screen
                }
                .progress(&Live::default(), true);
                self.update(screen.card_id, &card).await;
                tokio::select! {
                    () = done.cancelled() => break,
                    () = tokio::time::sleep(PROGRESS_INTERVAL) => {}
                }
            }
        };
        let (context, ()) = tokio::join!(collecting, showing);
        context
    }

    /// 采集本轮上下文。附件放在工作目录的 `attachments/<轮次>/` 下，重试时先清掉
    /// 上一次的。新会话带群里最近的消息；群里不在话题里的追问，带上一轮之后群里的新消息
    /// （以前话题里的新消息就是这一层，不开话题后改从群里取）。
    async fn collect(
        &self,
        inputs: &[MessageReceived],
        workdir: &Path,
        seq: i64,
        fresh: bool,
        conversation: &Conversation,
        progress: &watch::Sender<context::Gathering>,
    ) -> context::TurnContext {
        let dir = workdir.join("attachments").join(seq.to_string());
        if let Err(err) = tokio::fs::remove_dir_all(&dir).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%err, dir = %dir.display(), "清理上一次的附件失败");
        }
        let group_follow_up =
            !fresh && conversation.origin != Origin::P2p && conversation.thread_id.is_none();
        // 这个聊天清空过上下文：清空之前的群聊、话题消息都不再带
        let reset = match inputs.first().map(|r| r.message.chat_id.as_str()) {
            Some(chat_id) => match self.store.context_reset(chat_id).await {
                Ok(reset) => reset,
                Err(err) => {
                    tracing::warn!("{err:#}");
                    None
                }
            },
            None => None,
        };
        let window = self
            .settings
            .window
            .filter(|_| fresh || group_follow_up)
            .map(|(count, span)| {
                // 时长为 0 表示不限时间，只按条数
                let mut since = if span.is_zero() {
                    0
                } else {
                    now_ms() / 1000 - i64::try_from(span.as_secs()).unwrap_or(i64::MAX)
                };
                if !fresh && let Some(cursor) = conversation.history_cursor_ms {
                    // 游标那一秒的是上一轮已经交给 Agent 的
                    since = since.max(cursor / 1000 + 1);
                }
                // 清空上下文那一秒及之前的不再带
                if let Some(reset) = reset {
                    since = since.max(reset / 1000 + 1);
                }
                (count, since)
            });
        let since_ms = if fresh {
            None
        } else {
            conversation.history_cursor_ms
        };
        let sources = context::Sources {
            since_ms: match (since_ms, reset) {
                (Some(cursor), Some(reset)) => Some(cursor.max(reset)),
                (cursor, reset) => cursor.or(reset),
            },
            window,
            workspace: context::attach::Workspace {
                root: workdir,
                dir: &dir,
                tools: &self.settings.tools,
            },
            progress,
        };
        let bot = self.bot_open_id.get().map(String::as_str);
        let mut context = context::collect(&self.api, inputs, bot, &sources).await;
        // 引用了机器人的卡片、或者在卡片上补充：告诉 Agent 是针对哪一轮的回答
        if let Some(parent) = inputs
            .first()
            .and_then(|r| r.message.parent_id.as_deref())
            .filter(|p| !p.is_empty())
            && let Ok(Some(turn)) = self.store.turn_by_card(parent).await
            && turn.conversation_id == conversation.id
        {
            context.follows_turn = Some(turn.seq);
            // 引用的卡片就是这个会话里的一轮：会话里有那一轮的答案，不用再给一遍
            if context.quoted_bot_card {
                context.quoted = None;
            }
        }
        if !context.doc_links.is_empty() {
            let asker = inputs
                .first()
                .map(|r| r.sender.sender_id.open_id.as_str())
                .unwrap_or_default();
            self.read_documents(&mut context, asker, &sources.workspace)
                .await;
        }
        context
    }

    /// 以提问人的身份读聊天里贴的云文档。没授权就私聊授权卡片，这一轮照常跑，
    /// 在「没能读取的内容」里注明。
    async fn read_documents(
        &self,
        context: &mut context::TurnContext,
        asker: &str,
        workspace: &context::attach::Workspace<'_>,
    ) {
        let wanted = std::mem::take(&mut context.doc_links);
        let count = wanted.len();
        let Some(oauth) = &self.oauth else {
            context
                .missing
                .push(format!("{count} 篇云文档没有读取：没有配置云文档授权"));
            return;
        };
        let mut access = oauth.token(asker).await;
        for _ in 0..2 {
            let token = match access {
                Access::Ready(token) => token,
                Access::Unauthorized(reason) => {
                    self.prompt_authorization(oauth, asker).await;
                    context.missing.push(format!(
                        "{count} 篇云文档没有读取：{reason}，已私聊发了授权链接"
                    ));
                    return;
                }
                Access::Unavailable(reason) => {
                    context
                        .missing
                        .push(format!("{count} 篇云文档没有读取：{reason}"));
                    return;
                }
            };
            let images_left = context::attach::MAX_IMAGES.saturating_sub(context.images().count());
            let documents =
                context::docs::read(&self.api, &token, &wanted, workspace, images_left).await;
            if !documents.rejected {
                context.add_documents(documents);
                return;
            }
            // 接口说 token 无效：强刷一次再读；还不行就只能重新授权
            access = oauth.rejected(asker).await;
        }
        context
            .missing
            .push(format!("{count} 篇云文档没有读取：授权刷新后仍被拒绝"));
    }

    async fn prompt_authorization(&self, oauth: &Oauth, open_id: &str) {
        if open_id.is_empty() || !oauth.should_prompt(open_id) {
            return;
        }
        let card = card::authorize(&oauth.start_link(open_id));
        if let Err(err) = self
            .api
            .send_to_user(
                open_id,
                "interactive",
                card.to_string(),
                Uuid::now_v7().to_string(),
            )
            .await
        {
            tracing::warn!(%err, "私聊授权卡片失败");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        backend: &dyn AgentBackend,
        screen: &Screen<'_>,
        workdir: &Path,
        session: SessionRef,
        prompt: String,
        images: Vec<PathBuf>,
        n: u32,
    ) -> Attempt {
        let turn_id = &screen.spec.turn_id;
        let run_dir = self
            .settings
            .data_dir
            .join("run")
            .join(format!("{turn_id}-{n}"));
        let request = TurnRequest {
            session,
            prompt,
            workdir: workdir.to_path_buf(),
            run_dir: run_dir.clone(),
            images,
            rules: prompt::rules(self.knowledge().await.as_deref()),
            schema: answer::schema().clone(),
            mcp_servers: self.settings.mcp_servers.clone(),
            model: self.settings.model.clone(),
            effort: self.settings.effort,
            timeout: self.settings.timeout,
            budget_usd_micros: self.settings.budget_usd_micros,
        };
        let attempt = match backend.start_turn(request).await {
            Ok(handle) => {
                tracing::info!(
                    turn = %turn_id,
                    session = handle.session_id.as_deref().unwrap_or_default(),
                    "开始分析"
                );
                self.follow(handle, screen).await
            }
            Err(err) => Attempt {
                outcome: Outcome::Failed {
                    kind: FailKind::Cli,
                    reason: format!("启动失败：{err}"),
                },
                model: self.settings.model.clone().unwrap_or_default(),
                tool_calls: 0,
                usage: None,
                self_check: None,
                session_id: None,
                stopped: false,
                timings: Timings::default(),
            },
        };
        // 本轮生成的 MCP、hook 配置用完即删
        if let Err(err) = tokio::fs::remove_dir_all(&run_dir).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(%err, dir = %run_dir.display(), "清理运行目录失败");
        }
        attempt
    }

    /// 环境速查的内容：每轮读一次，改了文件不用重启。读不到就不带，照常分析。
    async fn knowledge(&self) -> Option<String> {
        let path = self.settings.knowledge_file.as_ref()?;
        match tokio::fs::read_to_string(path).await {
            Ok(text) => Some(text),
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "读不到环境速查，这一轮不带");
                None
            }
        }
    }

    /// 消费事件，节流更新进度卡，直到终态。
    async fn follow(&self, mut handle: TurnHandle, screen: &Screen<'_>) -> Attempt {
        let mcp_names: Vec<&str> = self
            .settings
            .mcp_servers
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let pre_assigned = handle.session_id.clone();
        let spawned = Instant::now();
        let mut result = Attempt {
            outcome: Outcome::Interrupted,
            model: self.settings.model.clone().unwrap_or_default(),
            tool_calls: 0,
            usage: None,
            self_check: None,
            session_id: None,
            stopped: false,
            timings: Timings::default(),
        };
        let mut steps: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut thought: Option<String> = None;
        let mut draft: Option<String> = None;
        let mut draft_commands: Vec<answer::Command> = Vec::new();
        let mut dirty = false;
        let mut phase = PHASE_STARTING;
        let mut last_output = Instant::now();
        let mut last_shown = Instant::now();
        let mut stalled = false;
        let mut ticker = tokio::time::interval(PROGRESS_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                event = handle.events.recv() => {
                    let Some(event) = event else { break };
                    last_output = Instant::now();
                    if stalled {
                        stalled = false;
                        dirty = true;
                    }
                    let next = phase_after(phase, &event);
                    if next != phase {
                        phase = next;
                        dirty = true;
                    }
                    if phase == PHASE_CONCLUDING && result.timings.concluding.is_none() {
                        result.timings.concluding = Some(spawned.elapsed());
                    }
                    match event {
                        AgentEvent::Started(started) => {
                            result.timings.started = Some(spawned.elapsed());
                            tracing::info!(
                                turn = %screen.spec.turn_id,
                                model = %started.model,
                                cli = started.cli_version.as_deref().unwrap_or_default(),
                                permission_mode = started.permission_mode.as_deref().unwrap_or_default(),
                                "Agent 已启动"
                            );
                            if !started.model.is_empty() {
                                result.model = started.model.clone();
                            }
                            result.session_id = Some(started.session_id.clone())
                                .filter(|id| !id.is_empty())
                                .or_else(|| pre_assigned.clone());
                            if let Err(reason) = self_check(&started, &mcp_names) {
                                tracing::error!(%reason, "运行时自检不通过，取消本轮");
                                result.self_check = Some(reason);
                                handle.cancel.cancel();
                            }
                        }
                        AgentEvent::Step(step) => {
                            if matches!(step, ai_boot_agent::Step::ToolCall { .. }) {
                                result.tool_calls += 1;
                            }
                            if let ai_boot_agent::Step::RateLimited { resets_at, .. } = &step {
                                notes.push(rate_limit_note(*resets_at));
                            }
                            // 模型在工具调用之间说的进展，比工具名更能让人判断要不要等
                            if let ai_boot_agent::Step::Text(text) = &step {
                                thought = Some(text.clone());
                                dirty = true;
                            }
                            if let Some(label) = steps::label(&step) {
                                steps.push(label);
                                dirty = true;
                            }
                        }
                        AgentEvent::Draft(ai_boot_agent::Draft { summary, commands, .. }) => {
                            if summary.is_some() {
                                draft = summary;
                                dirty = true;
                            }
                            // 对不上契约的命令不显示，等最终答案
                            if let Some(commands) = commands
                                .and_then(|raw| serde_json::from_value::<Vec<answer::Command>>(raw).ok())
                            {
                                draft_commands = commands;
                                dirty = true;
                            }
                        }
                        AgentEvent::Usage(usage) => result.usage = Some(usage),
                        AgentEvent::Finished(outcome) => {
                            result.outcome = outcome;
                            result.timings.finished = Some(spawned.elapsed());
                            break;
                        }
                        AgentEvent::Warning(message) => {
                            tracing::warn!(%message, "Agent 输出异常");
                        }
                        AgentEvent::Activity(_) => {}
                    }
                }
                () = screen.stop.cancelled(), if !result.stopped => {
                    tracing::info!(turn = %screen.spec.turn_id, "按停止按钮中止本轮");
                    result.stopped = true;
                    handle.cancel.cancel();
                    notes.push("正在停止…".to_owned());
                    dirty = true;
                }
                _ = ticker.tick() => {
                    let quiet = last_output.elapsed();
                    if !stalled && quiet >= STALL_AFTER {
                        stalled = true;
                        tracing::warn!(turn = %screen.spec.turn_id, quiet_secs = quiet.as_secs(), "Agent 长时间没有输出");
                    }
                    if dirty || stalled || last_shown.elapsed() >= HEARTBEAT {
                        dirty = false;
                        last_shown = Instant::now();
                        let mut shown = vec![phase.to_owned()];
                        if stalled {
                            shown.push(format!(
                                "⚠️ 已 {} 没有新进展，可能是模型或工具响应慢，可以停止后重试",
                                card::human_duration(quiet)
                            ));
                        }
                        shown.extend(notes.iter().cloned());
                        let live = Live {
                            steps: &steps,
                            thought: thought.as_deref(),
                            draft: draft.as_deref(),
                            commands: &draft_commands,
                            notes: &shown,
                        };
                        let progress = screen.progress(&live, !result.stopped);
                        self.update(screen.card_id, &progress).await;
                    }
                }
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn report(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        workdir: &Path,
        asked: Option<&str>,
        attempt: Attempt,
        resumed: bool,
        cursor: Option<i64>,
        elapsed: Duration,
    ) -> Report {
        // 后端报的是会话累计用量：续接的会话减去上一轮末的累计值，新会话从 0 算
        let cumulative = attempt
            .usage
            .map(|u| i64::try_from(u.input_tokens + u.output_tokens).unwrap_or(i64::MAX));
        let previous = if resumed {
            conversation.session_tokens
        } else {
            0
        };
        let tokens = match cumulative {
            Some(total) if total >= previous => total - previous,
            Some(total) => total,
            None => 0,
        };
        let started = attempt.session_id.is_some();
        let footer = Footer {
            turn: u32::try_from(spec.seq).unwrap_or(u32::MAX),
            elapsed,
            follow_up: (spec.kind == TurnKind::Ask).then(|| spec.turn_id.clone()),
            asked: asked.map(str::to_owned),
            ..Footer::default()
        };
        let retry = Some(spec.turn_id.as_str());
        let links = HostAllowlist(&self.settings.link_hosts);
        let mut answer_json = None;
        let (card, status, error_kind, error) = if let Some(reason) = &attempt.self_check {
            (
                card::failure(
                    "❌ 运行环境自检未通过",
                    reason,
                    "检查 Agent CLI 与 MCP server 的部署配置",
                    retry,
                ),
                TurnStatus::Failed,
                Some("self_check"),
                Some(reason.clone()),
            )
        } else {
            match &attempt.outcome {
                Outcome::Success { .. } => match answer::reply_of(&attempt.outcome) {
                    Some(AnswerReply::Structured(mut answer)) => {
                        self.prepare_visuals(&mut answer, workdir).await;
                        answer_json = serde_json::to_string(&answer).ok();
                        (
                            card::answer(&answer, &footer, &links),
                            TurnStatus::Succeeded,
                            None,
                            None,
                        )
                    }
                    Some(AnswerReply::Text(text)) => (
                        card::text_reply(&text, &footer, &links),
                        TurnStatus::Succeeded,
                        None,
                        None,
                    ),
                    None => (
                        card::failure("❌ 分析失败", "没有拿到结果", "", retry),
                        TurnStatus::Failed,
                        Some("empty"),
                        Some("没有拿到结果".to_owned()),
                    ),
                },
                Outcome::Failed { kind, reason } => {
                    let (title, hint) = failure_text(*kind);
                    (
                        card::failure(title, reason, hint, retry),
                        TurnStatus::Failed,
                        Some(fail_kind_label(*kind)),
                        Some(reason.clone()),
                    )
                }
                Outcome::Interrupted if attempt.stopped => (
                    card::failure("⏹ 已停止", "这一轮已按要求停止。", "", retry),
                    TurnStatus::Interrupted,
                    Some("stopped"),
                    None,
                ),
                Outcome::Interrupted => (
                    card::failure("⏹ 已中断", "Agent 进程被中断，这一轮没有完成。", "", retry),
                    TurnStatus::Interrupted,
                    Some("interrupted"),
                    None,
                ),
                Outcome::TimedOut => (
                    card::failure(
                        "⏱ 分析超时",
                        &format!(
                            "超过 {} 分钟仍未完成。",
                            self.settings.timeout.as_secs() / 60
                        ),
                        "缩小问题范围后再问",
                        retry,
                    ),
                    TurnStatus::Timeout,
                    Some("timeout"),
                    None,
                ),
                Outcome::BudgetExceeded => (
                    card::failure(
                        "💰 超出单轮预算",
                        "本轮消耗达到了配置的上限，已停止。",
                        "问题较复杂时可以拆成几个小问题",
                        retry,
                    ),
                    TurnStatus::BudgetExceeded,
                    Some("budget"),
                    None,
                ),
            }
        };
        Report {
            card,
            status,
            model: attempt.model,
            answer_json,
            error_kind,
            error,
            tokens,
            tool_calls: i64::try_from(attempt.tool_calls).unwrap_or(i64::MAX),
            session_tokens: cumulative.filter(|_| started),
            context_tokens: match attempt.usage {
                Some(usage) if started && usage.context_tokens > 0 => {
                    Some(i64::try_from(usage.context_tokens).unwrap_or(i64::MAX))
                }
                // 新会话没报上下文（比如中途被停）：旧会话的数不能留着，不然下一轮又换
                _ if started && !resumed => Some(0),
                _ => None,
            },
            session_id: attempt.session_id,
            // Agent 没起来就没看到这些消息，游标不前进
            cursor: cursor.filter(|_| started),
        }
    }

    /// 答案里要展示的截图和流程图先上传到飞书，拿到的 image_key 记进答案（随答案落库，
    /// 旧卡片重画时直接用）。截图只能是这个会话工作目录里的文件；读不到、渲染或上传
    /// 失败的不画，卡片照常出。
    async fn prepare_visuals(&self, answer: &mut answer::Answer, workdir: &Path) {
        enum Source {
            File(String),
            Dot(String),
        }
        let mut wanted: Vec<(String, Source)> = Vec::new();
        for section in &answer.sections {
            for image in &section.images {
                wanted.push((
                    answer::Answer::image_ref(image),
                    Source::File(image.path.clone()),
                ));
            }
            for diagram in &section.diagrams {
                wanted.push((
                    answer::Answer::diagram_ref(diagram),
                    Source::Dot(diagram.dot.clone()),
                ));
            }
        }
        let mut seen = std::collections::HashSet::new();
        wanted.retain(|(key, _)| seen.insert(key.clone()));
        wanted.truncate(card::MAX_IMAGES);
        if wanted.is_empty() {
            return;
        }
        let scratch = &self.settings.tools.scratch;
        let uploaded: Vec<Option<(String, String)>> = futures_util::stream::iter(wanted)
            .map(|(key, source)| async move {
                let bytes = match source {
                    Source::File(path) => attachment_bytes(workdir, &path).await,
                    Source::Dot(dot) => match diagram::render(&dot, scratch).await {
                        Ok(png) => Ok(png),
                        Err(diagram::Failure::NotInstalled) => {
                            Err("没装 graphviz，流程图没有渲染".to_owned())
                        }
                        Err(
                            diagram::Failure::Rejected(reason) | diagram::Failure::Failed(reason),
                        ) => Err(reason),
                    },
                };
                let uploaded = match bytes {
                    Ok(bytes) => self
                        .api
                        .upload_image(bytes)
                        .await
                        .map_err(|err| format!("上传失败：{err}")),
                    Err(reason) => Err(reason),
                };
                match uploaded {
                    Ok(image_key) => Some((key, image_key)),
                    Err(reason) => {
                        tracing::warn!(%key, %reason, "答案里的图没能放进卡片");
                        None
                    }
                }
            })
            .buffer_unordered(3)
            .collect()
            .await;
        answer.image_keys.extend(uploaded.into_iter().flatten());
    }

    /// 回进度卡；重试时复用原来的卡片。返回卡片的消息 ID。
    async fn open_card(&self, spec: &TurnSpec, first_id: &str, card: &Value) -> Option<String> {
        if spec.retry
            && let Ok(Some(turn)) = self.store.turn(&spec.turn_id).await
            && let Some(card_id) = turn.card_message_id
        {
            self.update(&card_id, card).await;
            return Some(card_id);
        }
        // 引用回复落在主消息流里，不开话题；提问本身在话题里时，飞书会把回复留在那个话题
        let reply = Reply {
            message_id: first_id,
            msg_type: "interactive",
            content: card.to_string(),
            reply_in_thread: false,
            uuid: idempotency_key("progress", &spec.turn_id),
        };
        let sent = match self.api.reply(reply).await {
            Ok(sent) => sent,
            Err(err) => {
                tracing::error!(%err, turn = %spec.turn_id, "回复进度卡失败");
                return None;
            }
        };
        if let Err(err) = self
            .store
            .set_turn_card(&spec.turn_id, &sent.message_id)
            .await
        {
            tracing::error!("{err:#}");
        }
        Some(sent.message_id)
    }

    /// 开跑之前被取消（停止按钮）。
    async fn cancelled(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        card_id: &str,
        started_at: Instant,
    ) {
        let report = Report::early(
            card::failure(
                "⏹ 已取消",
                "这一轮在开始前被取消。",
                "",
                Some(&spec.turn_id),
            ),
            TurnStatus::Interrupted,
            "cancelled",
            "开始前被取消".to_owned(),
        );
        self.record(spec, conversation, &report, started_at).await;
        self.update_final(card_id, &report.card).await;
    }

    async fn record(
        &self,
        spec: &TurnSpec,
        conversation: &Conversation,
        report: &Report,
        started_at: Instant,
    ) {
        let finished = FinishedTurn {
            turn_id: &spec.turn_id,
            conversation_id: &conversation.id,
            status: report.status,
            model: &report.model,
            answer_json: report.answer_json.as_deref(),
            error_kind: report.error_kind,
            error: report.error.as_deref(),
            tokens: report.tokens,
            tool_calls: report.tool_calls,
            duration_ms: i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX),
            session_id: report.session_id.as_deref(),
            session_tokens: report.session_tokens,
            history_cursor_ms: report.cursor,
            context_tokens: report.context_tokens,
            now_ms: now_ms(),
        };
        if let Err(err) = self.store.finish_turn(&finished).await {
            tracing::error!(turn = %spec.turn_id, "{err:#}");
        }
    }

    /// 之前成功各轮的问答摘要。
    async fn priors(&self, conversation: &Conversation, seq: i64) -> Vec<Prior> {
        let rows = match self.store.prior_turns(&conversation.id, seq).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!("{err:#}");
                return Vec::new();
            }
        };
        rows.into_iter()
            .filter_map(|row| {
                let answer: answer::Answer =
                    serde_json::from_str(row.answer_json.as_deref()?).ok()?;
                Some(Prior {
                    seq: row.seq,
                    question: row.question.unwrap_or_default(),
                    conclusion: prompt::conclusion_of(&answer),
                })
            })
            .collect()
    }

    async fn first_input(&self, turn_id: &str) -> Result<(String, MessageReceived), String> {
        let ids = self
            .store
            .turn_messages(turn_id)
            .await
            .map_err(|err| format!("{err:#}"))?;
        let first = ids
            .into_iter()
            .next()
            .ok_or_else(|| "这一轮没有消息".to_owned())?;
        let received = self.load(&first).await?;
        Ok((first, received))
    }

    /// 这一轮的全部提问消息（开跑信号之后读，连发合并已经结束）。
    async fn inputs(&self, turn_id: &str) -> Result<Vec<MessageReceived>, String> {
        let ids = self
            .store
            .turn_messages(turn_id)
            .await
            .map_err(|err| format!("{err:#}"))?;
        let mut inputs = Vec::with_capacity(ids.len());
        for id in &ids {
            match self.load(id).await {
                Ok(received) => inputs.push(received),
                Err(err) => tracing::warn!(message_id = %id, "{err}"),
            }
        }
        if inputs.is_empty() {
            return Err("这一轮没有可用的提问".to_owned());
        }
        Ok(inputs)
    }

    pub async fn deny(&self, message_id: &str) {
        let reply = Reply {
            message_id,
            msg_type: "text",
            content: serde_json::json!({ "text": DENY_TEXT }).to_string(),
            reply_in_thread: false,
            uuid: idempotency_key("deny", message_id),
        };
        if let Err(err) = self.api.reply(reply).await {
            tracing::warn!(%err, message_id, "回复「无权限」失败");
        }
    }

    /// 清空上下文之后回一句确认。
    pub async fn confirm_cleared(&self, message_id: &str, turns: u64) {
        let text = if turns == 0 {
            "已清空上下文，之后的提问从这里重新开始。".to_owned()
        } else {
            format!(
                "已清空上下文：删掉了 {turns} 轮问答，以及它们的聊天记录副本、附件和模型会话记录。之后的提问从这里重新开始，不会带上之前的内容。"
            )
        };
        let reply = Reply {
            message_id,
            msg_type: "text",
            content: serde_json::json!({ "text": text }).to_string(),
            reply_in_thread: false,
            uuid: idempotency_key("clear", message_id),
        };
        if let Err(err) = self.api.reply(reply).await {
            tracing::warn!(%err, message_id, "回复「已清空」失败");
        }
    }

    /// 启动时调用：把上次退出时没结束的轮次的卡片改成「已中断」，带重试按钮。
    pub async fn mark_interrupted(&self, turns: Vec<InterruptedTurn>) {
        for turn in turns {
            if let Some(card_id) = turn.card_message_id {
                let card =
                    card::failure("⏹ 已中断", "服务重启，这一轮没有完成。", "", Some(&turn.id));
                self.update_final(&card_id, &card).await;
            }
        }
    }

    async fn load(&self, message_id: &str) -> Result<MessageReceived, String> {
        let payload = self
            .store
            .input_payload(message_id)
            .await
            .map_err(|err| format!("{err:#}"))?
            .ok_or_else(|| "收件箱里没有这条消息".to_owned())?;
        parse_event(&payload)
    }

    async fn update(&self, card_id: &str, card: &Value) {
        if let Err(err) = self.api.update_card(card_id, card).await {
            tracing::warn!(%err, card_id, "更新卡片失败");
        }
    }

    /// 最终结果必须送到：失败了隔几秒再试几次。返回是否送到。
    async fn update_final(&self, card_id: &str, card: &Value) -> bool {
        for attempt in 1..=3 {
            match self.api.update_card(card_id, card).await {
                Ok(()) => return true,
                Err(err) => {
                    tracing::warn!(%err, attempt, card_id, "更新最终卡片失败");
                    tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                }
            }
        }
        tracing::error!(card_id, "最终卡片没能送达");
        false
    }
}

/// 收件箱里的原始事件 JSON → 消息事件。
pub fn parse_event(payload: &str) -> Result<MessageReceived, String> {
    let envelope: ai_boot_feishu::event::Envelope =
        serde_json::from_str(payload).map_err(|err| format!("收件箱里的消息无法解析：{err}"))?;
    serde_json::from_value(envelope.event).map_err(|err| format!("收件箱里的消息无法解析：{err}"))
}

/// 运行时自检：CLI 实际拿到的工具面、MCP 连接和加载的扩展必须与预期一致，
/// 任何一项不符就不跑——配置漂移时宁可不答，也不能带着更大的权限去答。
fn self_check(started: &Started, mcp_names: &[&str]) -> Result<(), String> {
    if let Some(tools) = &started.tools {
        for tool in tools {
            let expected = BUILTIN_TOOLS.contains(&tool.as_str())
                || mcp_names
                    .iter()
                    .any(|name| tool.starts_with(&format!("mcp__{name}__")));
            if !expected {
                return Err(format!("出现了未授权的工具 {tool}"));
            }
        }
    }
    if let Some(servers) = &started.mcp_servers {
        for name in mcp_names {
            match servers.iter().find(|s| s.name == *name) {
                Some(server) if server.status == "connected" => {}
                Some(server) => {
                    return Err(format!("MCP server {name} 的状态是 {}", server.status));
                }
                None => return Err(format!("MCP server {name} 没有加载")),
            }
        }
    }
    if let Some(plugins) = &started.plugins
        && let Some(plugin) = plugins.iter().find(|p| !p.ends_with("@builtin"))
    {
        return Err(format!("加载了非内置插件 {plugin}"));
    }
    if let Some(skills) = &started.skills
        && !skills.is_empty()
    {
        return Err(format!("加载了技能 {}", skills.join("、")));
    }
    if started.permission_mode.as_deref() == Some("bypassPermissions") {
        return Err("权限模式是 bypassPermissions".to_owned());
    }
    Ok(())
}

fn failure_text(kind: FailKind) -> (&'static str, &'static str) {
    match kind {
        FailKind::Auth => ("🔑 Agent 登录失效", "需要在服务器上重新登录"),
        FailKind::RateLimit => ("⏳ 额度受限", "稍后再试"),
        FailKind::Api => ("❌ 模型服务出错", "稍后再试"),
        FailKind::Cli => ("❌ 执行异常退出", "查看服务日志"),
        FailKind::SessionNotFound => ("❌ 会话无法续接", "稍后重试会新开会话"),
        FailKind::SelfCheck | FailKind::Other => ("❌ 分析失败", ""),
    }
}

/// 答案里引用的截图：只认会话工作目录里的图片文件，路径来自模型，是不可信输入。
async fn attachment_bytes(workdir: &Path, path: &str) -> Result<Vec<u8>, String> {
    const MAX_BYTES: u64 = 10 * 1024 * 1024;
    let (_, bytes) = workdir_file(workdir, path, MAX_BYTES).await?;
    if !context::extract::is_image(&bytes) {
        return Err(format!("{path} 不是图片"));
    }
    Ok(bytes)
}

/// 工作目录里的一个普通文件：文件名和内容。路径来自模型，是不可信输入：只认相对路径，
/// 解开符号链接之后也要在工作目录里，空文件和超过上限的不要。
async fn workdir_file(
    workdir: &Path,
    path: &str,
    max_bytes: u64,
) -> Result<(String, Vec<u8>), String> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("不是工作目录里的相对路径：{path}"));
    }
    let root = tokio::fs::canonicalize(workdir)
        .await
        .map_err(|err| format!("工作目录不可用：{err}"))?;
    // 解开符号链接之后仍要在工作目录里
    let file = tokio::fs::canonicalize(root.join(relative))
        .await
        .map_err(|err| format!("找不到 {path}：{err}"))?;
    if !file.starts_with(&root) {
        return Err(format!("{path} 不在工作目录里"));
    }
    let metadata = tokio::fs::metadata(&file)
        .await
        .map_err(|err| format!("读取 {path} 失败：{err}"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max_bytes {
        return Err(format!(
            "{path} 不是 {} MB 以内的非空文件",
            max_bytes / 1024 / 1024
        ));
    }
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|err| format!("读取 {path} 失败：{err}"))?;
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned());
    Ok((name, bytes))
}

/// 采集中的进度卡：读到多少写多少，没有的不写。
fn gathering_line(gathering: &context::Gathering) -> String {
    let mut parts = Vec::new();
    if gathering.messages > 0 {
        parts.push(format!("{} 条消息", gathering.messages));
    }
    if gathering.images_total > 0 {
        parts.push(format!(
            "图片 {}/{}",
            gathering.images_done, gathering.images_total
        ));
    }
    if gathering.files_total > 0 {
        parts.push(format!(
            "文件 {}/{}",
            gathering.files_done, gathering.files_total
        ));
    }
    if parts.is_empty() {
        return PHASE_READING.to_owned();
    }
    format!("📥 正在读取：{}", parts.join(" · "))
}

/// 收到一条事件后模型所处的阶段。工具结果回来之后模型会接着想，先按思考算，
/// 等增量事件说明它在做什么再改。
fn phase_after(phase: &'static str, event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::Started(_) => PHASE_THINKING,
        AgentEvent::Activity(Activity::Thinking) => PHASE_THINKING,
        AgentEvent::Activity(Activity::Writing) => PHASE_WRITING,
        AgentEvent::Activity(Activity::Concluding) => PHASE_CONCLUDING,
        AgentEvent::Step(Step::ToolCall { tool, .. }) if tool != "StructuredOutput" => PHASE_TOOL,
        AgentEvent::Step(Step::ToolDone { .. }) if phase == PHASE_TOOL => PHASE_THINKING,
        _ => phase,
    }
}

/// 进度卡上的「已读取」。
fn read_lines(manifest: &context::Manifest) -> Vec<String> {
    const SHOWN: usize = 5;
    let counts: Vec<String> = [
        (manifest.messages, "条消息"),
        (manifest.images, "张图片"),
        (manifest.files, "个文件"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, unit)| format!("{n} {unit}"))
    .collect();
    // 什么都没读（私聊里的一句话）就不占这一行
    let mut lines = Vec::new();
    if !counts.is_empty() {
        lines.push(format!("📥 已读取：{}", counts.join("、")));
    }
    lines.extend(
        manifest
            .missing
            .iter()
            .take(SHOWN)
            .map(|item| format!("⚠️ {item}")),
    );
    if manifest.missing.len() > SHOWN {
        lines.push(format!(
            "⚠️ 另有 {} 项没读到",
            manifest.missing.len() - SHOWN
        ));
    }
    lines
}

fn absolute_images(workdir: &Path, context: &context::TurnContext) -> Vec<PathBuf> {
    context.images().map(|path| workdir.join(path)).collect()
}

/// 落库用的失败类别。
fn fail_kind_label(kind: FailKind) -> &'static str {
    match kind {
        FailKind::Auth => "auth",
        FailKind::RateLimit => "rate_limit",
        FailKind::Api => "api",
        FailKind::Cli => "cli",
        FailKind::SelfCheck => "self_check",
        FailKind::SessionNotFound => "session_not_found",
        FailKind::Other => "other",
    }
}

fn rate_limit_note(resets_at: Option<i64>) -> String {
    match resets_at {
        Some(at) => format!("额度受限，{} 恢复", context::beijing_time(at * 1000)),
        None => "额度受限".to_owned(),
    }
}

fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// 追加到工作目录里的 `context/answers.md`。
fn append_answer(workdir: &Path, record: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workdir.join("context").join(prompt::ANSWERS_FILE))?
        .write_all(record.as_bytes())
}

fn write_transcript(workdir: &Path, transcript: &Transcript) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let context_dir = workdir.join("context");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&context_dir)?;
    let path = context_dir.join("transcript.md");
    match transcript {
        Transcript::Replace(text) => std::fs::write(path, text),
        Transcript::Append(text) => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(text.as_bytes()),
    }
}

/// 同一轮的同一种回复，重试时用同一个 uuid：飞书保证 1 小时内只成功一次。
fn idempotency_key(kind: &str, id: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        format!("ai-boot/{kind}/{id}").as_bytes(),
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_boot_agent::McpStatus;

    fn started() -> Started {
        Started {
            session_id: "s".into(),
            model: "claude-opus".into(),
            cli_version: Some("2.1.283".into()),
            tools: Some(vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "StructuredOutput".into(),
                "mcp__qtmcp__jira_issue".into(),
            ]),
            mcp_servers: Some(vec![McpStatus {
                name: "qtmcp".into(),
                status: "connected".into(),
            }]),
            plugins: Some(vec!["agents-md@builtin".into(), "telemetry@builtin".into()]),
            skills: Some(vec![]),
            permission_mode: Some("default".into()),
        }
    }

    #[test]
    fn the_expected_surface_passes_the_self_check() {
        assert_eq!(self_check(&started(), &["qtmcp"]), Ok(()));
    }

    #[test]
    fn any_drift_fails_the_self_check() {
        let mut s = started();
        s.tools.as_mut().expect("tools").push("Write".into());
        assert!(self_check(&s, &["qtmcp"]).is_err_and(|e| e.contains("Write")));

        let mut s = started();
        s.mcp_servers = Some(vec![McpStatus {
            name: "qtmcp".into(),
            status: "failed".into(),
        }]);
        assert!(self_check(&s, &["qtmcp"]).is_err_and(|e| e.contains("failed")));

        let mut s = started();
        s.plugins = Some(vec!["rust-analyzer-lsp@claude-plugins-official".into()]);
        assert!(self_check(&s, &["qtmcp"]).is_err());

        let mut s = started();
        s.skills = Some(vec!["deploy".into()]);
        assert!(self_check(&s, &["qtmcp"]).is_err());

        let mut s = started();
        s.permission_mode = Some("bypassPermissions".into());
        assert!(self_check(&s, &["qtmcp"]).is_err());
    }

    #[test]
    fn tools_of_an_unconfigured_mcp_server_are_rejected() {
        let mut s = started();
        s.tools
            .as_mut()
            .expect("tools")
            .push("mcp__context7__query-docs".into());
        assert!(self_check(&s, &["qtmcp"]).is_err());
    }

    #[test]
    fn backends_that_report_less_are_only_checked_on_what_they_report() {
        // Codex 的起始事件没有工具清单等信息
        let s = Started {
            session_id: "t".into(),
            ..Started::default()
        };
        assert_eq!(self_check(&s, &["qtmcp"]), Ok(()));
    }

    #[test]
    fn the_gathering_line_counts_up_what_has_been_read() {
        let mut gathering = context::Gathering::default();
        assert_eq!(gathering_line(&gathering), PHASE_READING);
        gathering.messages = 150;
        assert_eq!(gathering_line(&gathering), "📥 正在读取：150 条消息");
        gathering.images_total = 12;
        gathering.images_done = 3;
        gathering.files_total = 1;
        assert_eq!(
            gathering_line(&gathering),
            "📥 正在读取：150 条消息 · 图片 3/12 · 文件 0/1"
        );
    }

    #[test]
    fn the_read_line_skips_what_was_not_there() {
        let manifest = |messages, images, files| context::Manifest {
            messages,
            images,
            files,
            missing: Vec::new(),
        };
        assert!(read_lines(&manifest(0, 0, 0)).is_empty());
        assert_eq!(
            read_lines(&manifest(500, 2, 0)),
            ["📥 已读取：500 条消息、2 张图片"]
        );
    }

    #[test]
    fn the_phase_follows_what_the_model_is_doing() {
        let call = |tool: &str| {
            AgentEvent::Step(Step::ToolCall {
                id: "t".into(),
                tool: tool.into(),
                input: serde_json::json!({}),
            })
        };
        let done = AgentEvent::Step(Step::ToolDone {
            id: "t".into(),
            ok: true,
            preview: String::new(),
        });
        let mut phase = PHASE_STARTING;
        let mut seen = Vec::new();
        for event in [
            AgentEvent::Started(started()),
            AgentEvent::Activity(Activity::Thinking),
            call("mcp__qtmcp__jira_issue"),
            AgentEvent::Activity(Activity::Streaming),
            done,
            AgentEvent::Activity(Activity::Writing),
            AgentEvent::Activity(Activity::Concluding),
            call("StructuredOutput"),
        ] {
            phase = phase_after(phase, &event);
            seen.push(phase);
        }
        assert_eq!(
            seen,
            [
                PHASE_THINKING,
                PHASE_THINKING,
                PHASE_TOOL,
                PHASE_TOOL,
                PHASE_THINKING,
                PHASE_WRITING,
                PHASE_CONCLUDING,
                PHASE_CONCLUDING,
            ]
        );
    }

    #[test]
    fn idempotency_keys_are_stable_and_short() {
        let key = idempotency_key("progress", "om_1");
        assert_eq!(key, idempotency_key("progress", "om_1"));
        assert_ne!(key, idempotency_key("deny", "om_1"));
        assert!(key.len() <= 50);
    }
}
