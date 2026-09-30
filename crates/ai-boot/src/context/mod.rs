//! 上下文采集：本轮提问、它引用的消息、合并转发、所在话题、群里最近的消息，
//! 以及其中的图片、文件和云文档链接，按优先级分层（见方案「上下文采集与优先级」）。
//! 云文档要用提问人的授权读，由调用方拿到 token 后再读（`docs`）。

pub mod attach;
pub mod docs;
pub mod extract;
pub mod flatten;
pub mod links;
mod merge;

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ai_boot_feishu::api::{ApiClient, Container, MessageItem};
use ai_boot_feishu::event::MessageReceived;
use regex::Regex;
use serde::Serialize;
use tokio::sync::watch;

use crate::config::MAX_WINDOW_MESSAGES;
use attach::{Attachment, Layer, Wanted, Workspace};
use flatten::{Flat, Mention, flatten};

/// 话题里最多取多少条消息。
const THREAD_LIMIT: usize = 500;
/// 卡片输入框里的补充没有对应的聊天消息，落进收件箱时用这个前缀的 ID，引用的是那张卡片。
pub const CARD_INPUT_PREFIX: &str = "cardinput_";
/// 卡片输入框里的一次补充最多收多少字。
pub const CARD_INPUT_CHARS: usize = 2000;

/// 是不是卡片输入框里的补充（而不是真实的聊天消息）。
pub fn is_card_input(message_id: &str) -> bool {
    message_id.starts_with(CARD_INPUT_PREFIX)
}
/// 进群退群、改群名这类系统提示：没有人说话，放进聊天记录只是噪音。
const SYSTEM: &str = "system";
/// 识别出的 Jira 单号最多列多少个。
const JIRA_KEY_LIMIT: usize = 10;
/// 话题里的合并转发最多展开几条（从新到旧）。
const THREAD_FORWARDS: usize = 3;

static JIRA_KEY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\b[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}\b").ok());

/// 「最近 20 条」「最新的二十条消息」「近两百条聊天记录」「last 50 messages」。
/// 「条」后面必须是消息类的词或者句子结束，免得「最近 20 条日志」也被当成要少看聊天记录。
static REQUESTED_MESSAGES: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?:最近|最新|最后|近)的?\s*([0-9]{1,4}|[零〇一二两三四五六七八九十百千]{1,8})\s*条",
        r"(?:的?(?:消息|聊天|记录|群消息|群聊|讨论|对话|发言|内容)|\s*(?:$|[\s，。,.；;！!？?、）)]))",
        r"|(?i:last\s+([0-9]{1,4})\s+messages?)",
    ))
    .ok()
});

/// 一条聊天记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub at_ms: i64,
    pub sender: String,
    pub text: String,
}

/// 一段展开的合并转发。
#[derive(Debug, Clone)]
pub struct ForwardBlock {
    pub layer: Layer,
    pub title: String,
    pub lines: Vec<Line>,
    pub truncated: bool,
}

/// 一轮提问的上下文。
#[derive(Debug, Clone, Default)]
pub struct TurnContext {
    /// T1：提问原文（已去掉 @机器人和后端前缀）。连发的几条按顺序拼在一起。
    pub question: String,
    /// T1：提问引用的那条消息。
    pub quoted: Option<Line>,
    /// 展开的合并转发：提问带的在 T1，话题里的在 T4。
    pub forwards: Vec<ForwardBlock>,
    /// T2：识别出的 Jira 单号，提问里的排在前面。
    pub jira_keys: Vec<String>,
    /// T4：所在话题的记录（按时间正序，不含提问本身和机器人的消息）。追问轮只有
    /// 上一轮之后的新消息。
    pub thread: Vec<Line>,
    /// 话题超过上限、只取了最近的一部分。
    pub thread_truncated: bool,
    /// T4：群里最近的消息（新会话才取）。
    pub window: Vec<Line>,
    /// 图片和文件：提问带的在 T1，话题和群里分享的在 T3。
    pub attachments: Vec<Attachment>,
    /// 读不到的内容，每条一句话说明原因。
    pub missing: Vec<String>,
    /// 聊天里贴的云文档链接（还没读）：提问里的在前。
    pub doc_links: Vec<docs::WantedDoc>,
    /// 本轮看到的最新一条消息（含提问本身）的时间，是下一轮追问的游标。
    pub newest_ms: Option<i64>,
    /// 提问里说了只看最近几条消息。
    pub requested_messages: Option<usize>,
    /// 这一轮是接着第几轮的卡片问的（引用卡片或在卡片上补充）。
    pub follows_turn: Option<i64>,
}

/// 本轮读了什么、没读到什么。落库，也显示在进度卡片上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub messages: usize,
    pub images: usize,
    pub files: usize,
    pub missing: Vec<String>,
}

impl TurnContext {
    pub fn manifest(&self) -> Manifest {
        let forwarded: usize = self.forwards.iter().map(|f| f.lines.len()).sum();
        Manifest {
            messages: self.thread.len()
                + self.window.len()
                + forwarded
                + usize::from(self.quoted.is_some()),
            images: self.attachments.iter().map(|a| a.images.len()).sum(),
            files: self.attachments.iter().filter(|a| a.text.is_some()).count(),
            missing: self.missing.clone(),
        }
    }

    /// 读完的云文档并进来；文档里提到的 Jira 单号接在后面。
    pub fn add_documents(&mut self, documents: docs::Docs) {
        let found = jira_keys(
            documents
                .attachments
                .iter()
                .filter_map(|a| a.text.as_deref()),
        );
        for key in found {
            if self.jira_keys.len() < JIRA_KEY_LIMIT && !self.jira_keys.contains(&key) {
                self.jira_keys.push(key);
            }
        }
        self.attachments.extend(documents.attachments);
        self.missing.extend(documents.missing);
    }

    /// 交给后端的图片（相对工作目录），提问带的在前。
    pub fn images(&self) -> impl Iterator<Item = &std::path::Path> {
        self.attachments
            .iter()
            .flat_map(|a| a.images.iter().map(std::path::PathBuf::as_path))
    }
}

/// 采集的范围。
pub struct Sources<'a> {
    /// 上一轮的游标：给了就只取话题里在它之后的消息。
    pub since_ms: Option<i64>,
    /// 群里最近的消息：最多几条、从哪个时刻（Unix 秒）起。不取为 `None`。
    /// 提问里说了「最近 N 条」时条数按提问来。
    pub window: Option<(usize, i64)>,
    pub workspace: Workspace<'a>,
    /// 边读边累加的进度。
    pub progress: &'a watch::Sender<Gathering>,
}

/// 采集进度：进度卡上边读边累加，让人知道不是卡住了。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Gathering {
    /// 已取到的聊天记录条数。
    pub messages: usize,
    pub images_done: usize,
    pub images_total: usize,
    pub files_done: usize,
    pub files_total: usize,
}

/// 能选后端的前缀：会话第一条消息以它开头时，这个会话固定用对应的后端。
const BACKEND_PREFIXES: [&str; 2] = ["claude", "codex"];

/// 拆出开头的 `/claude`、`/codex`。
pub fn split_backend_prefix(text: &str) -> (Option<&'static str>, &str) {
    let trimmed = text.trim_start();
    for name in BACKEND_PREFIXES {
        if let Some(rest) = trimmed.strip_prefix('/').and_then(|t| t.strip_prefix(name))
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            return (Some(name), rest.trim_start());
        }
    }
    (None, text)
}

fn event_flat(received: &MessageReceived, bot_open_id: Option<&str>) -> Flat {
    let message = &received.message;
    let mentions: Vec<Mention> = message
        .mentions
        .iter()
        .map(|m| Mention {
            key: m.key.clone(),
            open_id: m.id.open_id.clone(),
            name: m.name.clone(),
        })
        .collect();
    flatten(
        &message.message_type,
        &message.content,
        &mentions,
        bot_open_id,
    )
}

/// 一条消息事件拍平后的文字（去掉 @机器人），不调任何接口。
pub fn event_text(received: &MessageReceived, bot_open_id: Option<&str>) -> String {
    event_flat(received, bot_open_id).text
}

fn item_flat(item: &MessageItem, bot_open_id: Option<&str>) -> Flat {
    if item.deleted {
        return Flat {
            text: "[已撤回]".to_owned(),
            ..Flat::default()
        };
    }
    let mentions: Vec<Mention> = item
        .mentions
        .iter()
        .map(|m| Mention {
            key: m.key.clone(),
            open_id: m.id.clone(),
            name: m.name.clone(),
        })
        .collect();
    let content = item.body.as_ref().map(|b| b.content.as_str()).unwrap_or("");
    flatten(&item.msg_type, content, &mentions, bot_open_id)
}

/// 采集一轮的上下文。`inputs` 是这一轮的提问消息（连发的几条）。
///
/// 群成员、话题、引用的消息、群里最近的消息互不依赖，同时取；取到多少就往
/// `sources.progress` 里累加多少，进度卡边读边显示。
pub async fn collect(
    api: &ApiClient,
    inputs: &[MessageReceived],
    bot_open_id: Option<&str>,
    sources: &Sources<'_>,
) -> TurnContext {
    let Some(first) = inputs.first() else {
        return TurnContext::default();
    };
    let message = &first.message;
    let progress = sources.progress;

    // 提问文字先拍平（不调接口）：提问里说了「最近 N 条」，就按它决定取多少
    let flats: Vec<Flat> = inputs
        .iter()
        .map(|received| event_flat(received, bot_open_id))
        .collect();
    let question = flats
        .iter()
        .map(|flat| split_backend_prefix(&flat.text).1.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let requested = requested_messages(&question);

    let input_ids: HashSet<&str> = inputs
        .iter()
        .map(|r| r.message.message_id.as_str())
        .collect();
    let thread_id = message.thread();
    // 卡片输入框里的补充引用的是机器人自己的卡片，没有内容可取
    let parent = message
        .parent_id
        .as_deref()
        .filter(|p| thread_id.is_none() && !p.is_empty() && !is_card_input(&message.message_id));
    // 引用的那条单独放在提问里，群聊记录里不再重复
    let mut skip_ids = input_ids.clone();
    skip_ids.extend(parent);
    let window = sources
        .window
        .filter(|_| !message.is_p2p())
        .map(|(count, since_secs)| (requested.unwrap_or(count), since_secs));
    let thread_limit = requested.unwrap_or(THREAD_LIMIT).min(THREAD_LIMIT);
    let started = std::time::Instant::now();
    let (names, thread, quoted, window) = tokio::join!(
        member_names(api, &message.chat_id),
        async {
            match thread_id {
                Some(id) => {
                    Some(thread_messages(api, id, sources.since_ms, thread_limit, progress).await)
                }
                None => None,
            }
        },
        async {
            match parent {
                Some(parent) => quoted_item(api, parent).await,
                None => Ok(None),
            }
        },
        async {
            match window {
                Some(window) => Some(window_items(api, message, &skip_ids, window, progress).await),
                None => None,
            }
        },
    );
    let name_of = |open_id: &str| {
        names
            .get(open_id)
            .cloned()
            .unwrap_or_else(|| short_id(open_id))
    };
    let mut context = TurnContext {
        question,
        requested_messages: requested,
        ..TurnContext::default()
    };
    let mut wanted: Vec<Wanted> = Vec::new();

    // T1：提问本身，以及它带的图片、文件、转发
    for (received, flat) in inputs.iter().zip(flats) {
        let id = received.message.message_id.as_str();
        want(
            &mut wanted,
            id,
            flat.attachments,
            Layer::Question,
            "提问附带",
            false,
        );
        if flat.forwarded {
            forward(
                api,
                id,
                Layer::Question,
                "提问附带的转发记录",
                bot_open_id,
                &name_of,
                &mut context,
                &mut wanted,
            )
            .await;
        }
    }

    let mut newest = inputs
        .iter()
        .filter_map(|r| r.message.create_time.parse::<i64>().ok())
        .max();

    if let Some((items, truncated)) = thread {
        // T2：话题记录和话题里分享的附件
        newest = newest.max(
            items
                .iter()
                .filter_map(|item| item.create_time.parse::<i64>().ok())
                .max(),
        );
        context.thread_truncated = truncated;
        let mut items: Vec<&MessageItem> = items
            .iter()
            .filter(|item| {
                !input_ids.contains(item.message_id.as_str())
                    && item.sender.sender_type != "app"
                    && item.msg_type != SYSTEM
            })
            .collect();
        items.sort_by_key(|item| item.create_time.parse::<i64>().unwrap_or_default());
        let mut forwards_left = THREAD_FORWARDS;
        // 从新到旧看附件和转发：数量有上限时，新的更要紧
        for item in items.iter().rev() {
            let flat = item_flat(item, bot_open_id);
            let sender = name_of(&item.sender.id);
            let origin = format!("{sender} 在话题里发的");
            want(
                &mut wanted,
                &item.message_id,
                flat.attachments,
                Layer::Shared,
                &origin,
                false,
            );
            if flat.forwarded && forwards_left > 0 {
                forwards_left -= 1;
                forward(
                    api,
                    &item.message_id,
                    Layer::Shared,
                    &format!("{sender} 在话题里转发的聊天记录"),
                    bot_open_id,
                    &name_of,
                    &mut context,
                    &mut wanted,
                )
                .await;
            }
        }
        context.thread = items
            .iter()
            .map(|item| line_of(item, bot_open_id, &name_of))
            .collect();
    }
    match quoted {
        Ok(Some(item)) => {
            quote(api, &item, bot_open_id, &name_of, &mut context, &mut wanted).await;
        }
        Ok(None) => {}
        Err(err) => context.missing.push(format!("提问引用的消息：{err}")),
    }
    if let Some((items, error)) = window {
        if let Some(err) = error {
            context.missing.push(format!("群里最近的消息：{err}"));
        }
        window_lines(&items, bot_open_id, &name_of, &mut context, &mut wanted);
    }

    let fetched_ms = started.elapsed().as_millis();
    let fetched = attach::fetch(api, wanted, &sources.workspace, progress).await;
    // 采集慢的时候看得出慢在拉消息还是下附件
    tracing::info!(
        messages_ms = fetched_ms,
        attachments_ms = started.elapsed().as_millis() - fetched_ms,
        attachments = fetched.attachments.len(),
        "上下文采集完成"
    );
    context.attachments = fetched.attachments;
    context.missing.extend(fetched.missing);
    context.doc_links = doc_links(&context);

    let question_files = context
        .attachments
        .iter()
        .filter(|a| a.layer == Layer::Question)
        .filter_map(|a| a.text.as_deref());
    let forwarded = |layer: Layer| {
        context
            .forwards
            .iter()
            .filter(move |f| f.layer == layer)
            .flat_map(|f| f.lines.iter().rev().map(|l| l.text.as_str()))
    };
    // Jira 单要求 Agent 先读一遍，所以只收提问本身和所在话题里的：群里几百条消息里
    // 随口提到的单也塞进来，会让它把每张单都查一遍，问什么都要跑好几分钟
    context.jira_keys = jira_keys(
        std::iter::once(context.question.as_str())
            .chain(context.quoted.iter().map(|l| l.text.as_str()))
            .chain(forwarded(Layer::Question))
            .chain(question_files)
            .chain(context.thread.iter().rev().map(|l| l.text.as_str())),
    );
    context.newest_ms = newest;
    context
}

/// 聊天里的云文档链接：提问、引用、提问带的转发里的算 T1，其余算 T3（新的在前）。
fn doc_links(context: &TurnContext) -> Vec<docs::WantedDoc> {
    let mut found: Vec<docs::WantedDoc> = Vec::new();
    let mut add = |text: &str, layer: Layer, origin: &str| {
        for link in links::find(text) {
            if !found.iter().any(|w| w.link.token == link.token) {
                found.push(docs::WantedDoc {
                    link,
                    layer,
                    origin: origin.to_owned(),
                });
            }
        }
    };
    add(&context.question, Layer::Question, "提问里的链接");
    if let Some(quoted) = &context.quoted {
        add(&quoted.text, Layer::Question, "提问引用的消息里的链接");
    }
    for block in context
        .forwards
        .iter()
        .filter(|b| b.layer == Layer::Question)
    {
        for line in &block.lines {
            add(&line.text, Layer::Question, "转发记录里的链接");
        }
    }
    for line in context.thread.iter().rev() {
        add(
            &line.text,
            Layer::Shared,
            &format!("{} 在话题里贴的", line.sender),
        );
    }
    for block in context.forwards.iter().filter(|b| b.layer == Layer::Shared) {
        for line in &block.lines {
            add(&line.text, Layer::Shared, &block.title);
        }
    }
    for line in context.window.iter().rev() {
        add(
            &line.text,
            Layer::Shared,
            &format!("{} 在群里贴的", line.sender),
        );
    }
    found
}

fn want(
    wanted: &mut Vec<Wanted>,
    message_id: &str,
    attachments: Vec<flatten::AttachmentRef>,
    layer: Layer,
    origin: &str,
    forwarded: bool,
) {
    wanted.extend(attachments.into_iter().map(|reference| Wanted {
        message_id: message_id.to_owned(),
        reference,
        layer,
        origin: origin.to_owned(),
        forwarded,
    }));
}

/// 展开一条合并转发。里面的附件用外层消息的 ID 下载（官方说不支持，尽力而为）。
#[allow(clippy::too_many_arguments)]
async fn forward(
    api: &ApiClient,
    message_id: &str,
    layer: Layer,
    title: &str,
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
    context: &mut TurnContext,
    wanted: &mut Vec<Wanted>,
) {
    match merge::expand(api, message_id, bot_open_id, name_of).await {
        Ok(expanded) => {
            for (reference, sender) in expanded.attachments {
                want(
                    wanted,
                    message_id,
                    vec![reference],
                    layer,
                    &format!("转发记录里 {sender} 发的"),
                    true,
                );
            }
            context.forwards.push(ForwardBlock {
                layer,
                title: title.to_owned(),
                lines: expanded.lines,
                truncated: expanded.truncated,
            });
        }
        Err(err) => {
            tracing::warn!(%err, "展开合并转发失败");
            context.missing.push(format!("{title}：{err}"));
        }
    }
}

/// 提问引用的那条消息。引用的是机器人自己的卡片（引用卡片追问）时没有内容可取。
async fn quoted_item(api: &ApiClient, parent: &str) -> Result<Option<MessageItem>, String> {
    let items = api.get_message(parent).await.map_err(|err| {
        tracing::warn!(%err, "取被引用的消息失败");
        err.to_string()
    })?;
    Ok(items
        .into_iter()
        .find(|item| item.message_id == parent && item.sender.sender_type != "app"))
}

/// 不在话题里时，提问引用的那条消息（及其附件、转发）也属于 T1。
async fn quote(
    api: &ApiClient,
    item: &MessageItem,
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
    context: &mut TurnContext,
    wanted: &mut Vec<Wanted>,
) {
    let flat = item_flat(item, bot_open_id);
    want(
        wanted,
        &item.message_id,
        flat.attachments,
        Layer::Question,
        "提问引用的消息",
        false,
    );
    if flat.forwarded {
        forward(
            api,
            &item.message_id,
            Layer::Question,
            "提问引用的转发记录",
            bot_open_id,
            name_of,
            context,
            wanted,
        )
        .await;
    }
    context.quoted = Some(Line {
        at_ms: item.create_time.parse().unwrap_or_default(),
        sender: name_of(&item.sender.id),
        text: flat.text,
    });
}

/// 群里最近的消息（按时间正序）：只看主聊天流，本话题的消息已经在话题记录里了。
/// 从新到旧翻页，凑够 `count` 条为止，一页最多 50 条。中途出错时前面取到的照用，
/// 错误原样带回。
async fn window_items(
    api: &ApiClient,
    message: &ai_boot_feishu::event::Message,
    input_ids: &HashSet<&str>,
    (count, since_secs): (usize, i64),
    progress: &watch::Sender<Gathering>,
) -> (Vec<MessageItem>, Option<String>) {
    let own_thread = message.thread();
    let keep = |item: &MessageItem| {
        !input_ids.contains(item.message_id.as_str())
            && item.sender.sender_type != "app"
            && item.msg_type != SYSTEM
            && (own_thread.is_none() || item.thread_id.as_deref() != own_thread)
    };
    let mut fetched: Vec<MessageItem> = Vec::new();
    let mut token: Option<String> = None;
    let mut error = None;
    loop {
        let page = match api
            .list_chat_since(&message.chat_id, since_secs, token.as_deref())
            .await
        {
            Ok(page) => page,
            Err(err) => {
                tracing::warn!(%err, "取群里最近的消息失败");
                error = Some(err.to_string());
                break;
            }
        };
        let before = fetched.len();
        fetched.extend(page.items.into_iter().filter(|item| keep(item)));
        fetched.truncate(count);
        let added = fetched.len() - before;
        progress.send_modify(|g| g.messages += added);
        if fetched.len() >= count {
            break;
        }
        match page.page_token.filter(|t| page.has_more && !t.is_empty()) {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    fetched.reverse();
    (fetched, error)
}

/// 群里最近的消息进 T2；附件从新到旧排，数量有上限时新的更要紧。
fn window_lines(
    items: &[MessageItem],
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
    context: &mut TurnContext,
    wanted: &mut Vec<Wanted>,
) {
    for item in items.iter().rev() {
        let flat = item_flat(item, bot_open_id);
        let origin = format!("{} 在群里发的", name_of(&item.sender.id));
        want(
            wanted,
            &item.message_id,
            flat.attachments,
            Layer::Shared,
            &origin,
            false,
        );
    }
    context.window = items
        .iter()
        .map(|item| line_of(item, bot_open_id, name_of))
        .collect();
}

async fn member_names(api: &ApiClient, chat_id: &str) -> HashMap<String, String> {
    match api.chat_members(chat_id).await {
        Ok(members) => members.into_iter().map(|m| (m.member_id, m.name)).collect(),
        Err(err) => {
            tracing::warn!(%err, "取群成员失败，聊天记录里的人名用 ID 代替");
            HashMap::new()
        }
    }
}

/// 从新到旧翻页，取到游标（不含）或 `limit` 条为止。返回是否被截断。
/// 话题容器不支持按时间过滤，只能边翻边比。
async fn thread_messages(
    api: &ApiClient,
    thread_id: &str,
    since_ms: Option<i64>,
    limit: usize,
    progress: &watch::Sender<Gathering>,
) -> (Vec<MessageItem>, bool) {
    let after_cursor = |item: &MessageItem| {
        since_ms.is_none_or(|since| item.create_time.parse::<i64>().is_ok_and(|at| at > since))
    };
    let mut items = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = match api
            .list_messages(Container::Thread(thread_id), true, token.as_deref())
            .await
        {
            Ok(page) => page,
            Err(err) => {
                tracing::warn!(%err, "取话题记录失败，只用已取到的部分");
                return (items, true);
            }
        };
        let reached_cursor = page.items.iter().any(|item| !after_cursor(item));
        let before = items.len();
        items.extend(page.items.into_iter().filter(|item| after_cursor(item)));
        let truncated = items.len() >= limit;
        items.truncate(limit);
        let added = items.len() - before;
        progress.send_modify(|g| g.messages += added);
        if truncated {
            return (items, true);
        }
        if reached_cursor {
            return (items, false);
        }
        match page.page_token.filter(|t| page.has_more && !t.is_empty()) {
            Some(next) => token = Some(next),
            None => return (items, false),
        }
    }
}

fn line_of(
    item: &MessageItem,
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
) -> Line {
    Line {
        at_ms: item.create_time.parse().unwrap_or_default(),
        sender: name_of(&item.sender.id),
        text: item_flat(item, bot_open_id).text,
    }
}

/// open_id 没有对应姓名时，用末尾几位区分不同的人。
fn short_id(open_id: &str) -> String {
    let tail: String = open_id
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("成员{tail}")
}

/// 提问里明确要看的最近消息条数，不超过 `MAX_WINDOW_MESSAGES`。
fn requested_messages(question: &str) -> Option<usize> {
    let captures = REQUESTED_MESSAGES.as_ref()?.captures(question)?;
    let number = captures.get(1).or_else(|| captures.get(2))?.as_str();
    let count = number
        .parse::<usize>()
        .ok()
        .or_else(|| chinese_number(number))?;
    (count > 0).then(|| count.min(MAX_WINDOW_MESSAGES))
}

/// 一万以内的中文数字：「二十」「十五」「一百零五」「两百」。
fn chinese_number(text: &str) -> Option<usize> {
    let (mut total, mut digit) = (0_usize, 0_usize);
    for c in text.chars() {
        let value = match c {
            '零' | '〇' => 0,
            '一' => 1,
            '二' | '两' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            '十' | '百' | '千' => {
                let unit = match c {
                    '十' => 10,
                    '百' => 100,
                    _ => 1000,
                };
                // 「十五」的十前面没有数字，按一十算
                total += digit.max(1) * unit;
                digit = 0;
                continue;
            }
            _ => return None,
        };
        digit = value;
    }
    Some(total + digit)
}

/// 按出现顺序去重，最多 `JIRA_KEY_LIMIT` 个。
fn jira_keys<'a>(texts: impl Iterator<Item = &'a str>) -> Vec<String> {
    let Some(pattern) = JIRA_KEY.as_ref() else {
        return Vec::new();
    };
    let mut keys: Vec<String> = Vec::new();
    for text in texts {
        for found in pattern.find_iter(text) {
            let key = found.as_str().to_owned();
            if !keys.contains(&key) {
                keys.push(key);
                if keys.len() == JIRA_KEY_LIMIT {
                    return keys;
                }
            }
        }
    }
    keys
}

/// 北京时间 `MM-DD HH:MM`。中国没有夏令时，固定 UTC+8。
pub fn beijing_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000) + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (_, month, day) = civil_from_days(days);
    format!(
        "{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// 北京时间的日期 `YYYY-MM-DD`。
pub fn beijing_date(ms: i64) -> String {
    let days = (ms.div_euclid(1000) + 8 * 3600).div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// 自 1970-01-01 起的天数转公历日期（Howard Hinnant 的算法）。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beijing_time_is_utc_plus_eight() {
        // 2026-09-28T08:39:52Z = 北京时间 09-28 16:39
        assert_eq!(beijing_time(1_790_584_792_000), "09-28 16:39");
        // 跨天：2026-12-31T20:00:00Z = 北京时间 01-01 04:00
        assert_eq!(beijing_time(1_798_747_200_000), "01-01 04:00");
    }

    #[test]
    fn beijing_date_rolls_over_at_local_midnight() {
        // 2026-12-31T16:00:00Z 是北京时间 2027-01-01 00:00
        assert_eq!(beijing_date(1_798_732_800_000), "2027-01-01");
        assert_eq!(beijing_date(1_798_732_799_000), "2026-12-31");
    }

    #[test]
    fn an_explicit_message_count_in_the_question_narrows_the_window() {
        let cases = [
            ("总结下最近20条消息", Some(20)),
            ("看下最近 30 条", Some(30)),
            ("最新的二十条聊天记录里谁提过回滚", Some(20)),
            ("近两百条群消息", Some(200)),
            ("最近一百零五条的讨论", Some(105)),
            ("最近十五条", Some(15)),
            ("最后 50 条记录说了什么？", Some(50)),
            ("summarize the last 40 messages", Some(40)),
            ("最近两千条消息", Some(MAX_WINDOW_MESSAGES)),
            // 不是在说聊天记录的条数
            ("最近20条日志里的报错", None),
            ("最近两个版本有什么改动", None),
            ("最近 0 条", None),
            ("登录偶发 500", None),
        ];
        for (question, expected) in cases {
            assert_eq!(requested_messages(question), expected, "{question}");
        }
    }

    #[test]
    fn jira_keys_keep_first_seen_order_without_duplicates() {
        let keys = jira_keys(
            [
                "看下 ABC-12 和 XYZ-3",
                "又提到 ABC-12，还有 QA2-100",
                "not-a-key ab-1",
            ]
            .into_iter(),
        );
        assert_eq!(keys, ["ABC-12", "XYZ-3", "QA2-100"]);
    }

    #[test]
    fn a_backend_prefix_is_split_off_only_as_a_whole_word() {
        assert_eq!(
            split_backend_prefix("  /codex 为什么 500"),
            (Some("codex"), "为什么 500")
        );
        assert_eq!(split_backend_prefix("/claude"), (Some("claude"), ""));
        assert_eq!(
            split_backend_prefix("/claudexyz 问题"),
            (None, "/claudexyz 问题")
        );
        assert_eq!(
            split_backend_prefix("问一下 /codex"),
            (None, "问一下 /codex")
        );
    }

    #[test]
    fn unknown_senders_are_still_distinguishable() {
        assert_eq!(short_id("ou_1234567890abcdef"), "成员abcdef");
    }
}
