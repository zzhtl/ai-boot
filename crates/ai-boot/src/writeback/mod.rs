//! 闭环方案写回：Jira 评论、Confluence 页面。由 ai-boot 在白名单用户点按钮
//! 确认后执行，写入的正是卡片上确认过的内容。
//!
//! - 目标由 ai-boot 查好放在卡片上给人确认：Jira 单号来自答案，标题现查；
//!   Confluence 的空间和父页面来自配置。
//! - 按钮带内容摘要，执行前重算，对不上（内容变了）就拒绝。
//! - 同一轮对同一个目标只有一条记录：进行中、已完成的不重复执行。
//! - 写之前先查重：Jira 评论末尾有幂等标记，Confluence 标题里有短 ID。调用
//!   超时记为「结果不明」，再点一次会先查到上次写成的那一份。
//! - 写回全局串行；每写完一次，按库里的状态重画闭环卡片。

pub mod format;
pub mod mcp;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use ai_boot_feishu::api::ApiClient;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::answer::Answer;
use crate::callback::{Target, Toast};
use crate::render::card::{self, Footer, WritebackState, WritebackView};
use crate::render::markdown::HostAllowlist;
use crate::store::{
    NewWriteback, Store, TurnKind, TurnRecord, TurnStatus, Writeback, WritebackStatus, now_ms,
};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const WRITE_TIMEOUT: Duration = Duration::from_secs(120);
/// 卡片上最多给几个 Jira 单。
const MAX_JIRA_TARGETS: usize = 3;

static JIRA_KEY: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9]{1,9}-[0-9]{1,7}$").ok());

/// 写回的配置。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 写回用的 qtmcp（不带只读开关）。
    pub server: mcp::Server,
    /// 发布到 Confluence 的空间和父页面；不配就不提供这个目标。
    pub confluence: Option<(String, String)>,
}

/// 闭环卡片上的一个可写回目标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSpec {
    pub target: String,
    pub reference: String,
    pub label: String,
}

/// 登记好、等着执行的一次写回。
pub struct Job {
    turn_id: String,
    row: Writeback,
    target: Target,
    markdown: String,
    answer: Answer,
}

pub struct Writer {
    api: Arc<ApiClient>,
    store: Store,
    settings: Settings,
    link_hosts: Vec<String>,
    serial: tokio::sync::Mutex<()>,
}

enum Failure {
    /// 没有生效。
    Failed(String),
    /// 不知道写没写成。
    Unknown(String),
}

impl Writer {
    pub fn new(
        api: Arc<ApiClient>,
        store: Store,
        settings: Settings,
        link_hosts: Vec<String>,
    ) -> Self {
        Self {
            api,
            store,
            settings,
            link_hosts,
            serial: tokio::sync::Mutex::new(()),
        }
    }

    /// 闭环方案生成后调用：查出能写回的目标。查不到标题也照样给按钮。
    pub async fn targets(&self, answer: &Answer) -> Vec<TargetSpec> {
        let mut keys: Vec<&str> = Vec::new();
        for key in &answer.jira_keys {
            let valid = JIRA_KEY.as_ref().is_some_and(|re| re.is_match(key));
            if valid && !keys.contains(&key.as_str()) {
                keys.push(key);
            }
        }
        keys.truncate(MAX_JIRA_TARGETS);
        let mut targets = Vec::new();
        if !keys.is_empty() {
            let mut session = match mcp::Session::start(&self.settings.server, START_TIMEOUT).await
            {
                Ok(session) => Some(session),
                Err(err) => {
                    tracing::warn!(%err, "查 Jira 单标题失败");
                    None
                }
            };
            for key in keys {
                let summary = match session.as_mut() {
                    Some(session) => session
                        .call(
                            "jira_issue",
                            json!({ "action": "get", "key": key }),
                            READ_TIMEOUT,
                        )
                        .await
                        .ok()
                        .and_then(|issue| {
                            issue
                                .get("summary")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        }),
                    None => None,
                };
                targets.push(TargetSpec {
                    target: Target::Jira.as_str().to_owned(),
                    reference: key.to_owned(),
                    label: match summary {
                        Some(summary) => format!("Jira {key} {summary}"),
                        None => format!("Jira {key}"),
                    },
                });
            }
        }
        if let Some((space, parent)) = &self.settings.confluence {
            targets.push(TargetSpec {
                target: Target::Confluence.as_str().to_owned(),
                reference: format!("{space}:{parent}"),
                label: format!("Confluence 空间 {space}"),
            });
        }
        targets
    }

    /// 按钮点击：校验并登记。返回要执行的写回，或者直接给用户的回应。
    pub async fn prepare(
        &self,
        turn_id: &str,
        target: Target,
        reference: &str,
        hash: &str,
        requested_by: &str,
    ) -> Result<Job, Toast> {
        let failed = |err: anyhow::Error| {
            tracing::error!("{err:#}");
            Toast::error("操作失败，稍后再试")
        };
        let record = self
            .store
            .turn_record(turn_id)
            .await
            .map_err(failed)?
            .ok_or_else(|| Toast::warning("找不到这一轮"))?;
        if record.kind != TurnKind::Resolve || record.status != TurnStatus::Succeeded {
            return Err(Toast::warning("只有闭环方案可以写回"));
        }
        let targets: Vec<TargetSpec> = record
            .writeback_targets
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default();
        if !targets
            .iter()
            .any(|t| t.target == target.as_str() && t.reference == reference)
        {
            return Err(Toast::warning("这个目标不在可写回的范围里"));
        }
        let answer: Answer = record
            .answer_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .ok_or_else(|| Toast::warning("闭环方案的内容读不出来"))?;
        let markdown = format::document(&answer, &document_footer(turn_id));
        if format::content_hash(&markdown) != hash {
            return Err(Toast::warning("闭环方案的内容已经变了，请重新生成"));
        }
        let (row, run) = self
            .store
            .begin_writeback(&NewWriteback {
                id: &uuid::Uuid::now_v7().to_string(),
                turn_id,
                target: target.as_str(),
                target_ref: reference,
                content_hash: hash,
                requested_by,
                now_ms: now_ms(),
            })
            .await
            .map_err(failed)?;
        if !run {
            return Err(match row.status {
                WritebackStatus::Done => Toast::info("已经写过了"),
                _ => Toast::info("正在写入，请稍候"),
            });
        }
        Ok(Job {
            turn_id: turn_id.to_owned(),
            row,
            target,
            markdown,
            answer,
        })
    }

    /// 执行一次写回，完成后重画闭环卡片。
    pub async fn execute(&self, job: Job) {
        let _serial = self.serial.lock().await;
        let result = match job.target {
            Target::Jira => self.jira(&job).await,
            Target::Confluence => self.confluence(&job).await,
        };
        let (status, url, error) = match result {
            Ok(url) => (WritebackStatus::Done, url, None),
            Err(Failure::Failed(error)) => (WritebackStatus::Failed, None, Some(error)),
            Err(Failure::Unknown(error)) => (WritebackStatus::Unknown, None, Some(error)),
        };
        tracing::info!(target = job.row.target, reference = %job.row.target_ref, ?status, "写回结束");
        if let Err(err) = self
            .store
            .finish_writeback(
                &job.row.id,
                status,
                url.as_deref(),
                error.as_deref(),
                now_ms(),
            )
            .await
        {
            tracing::error!("{err:#}");
        }
        self.refresh_card(&job.turn_id).await;
    }

    async fn jira(&self, job: &Job) -> Result<Option<String>, Failure> {
        let key = &job.row.target_ref;
        let marker = marker(&job.row.id);
        let mut session = mcp::Session::start(&self.settings.server, START_TIMEOUT)
            .await
            .map_err(Failure::Failed)?;
        // 查重：上次可能写成了但没拿到响应
        let existing = session
            .call(
                "jira_comment",
                json!({ "action": "list", "key": key, "limit": 50 }),
                READ_TIMEOUT,
            )
            .await
            .map_err(|err| Failure::Failed(format!("查重失败：{err}")))?;
        if existing.to_string().contains(&marker) {
            return Ok(None);
        }
        let body = format!(
            "{}\n\n{{color:#999999}}{marker}{{color}}",
            format::jira_wiki(&job.markdown)
        );
        match session
            .call(
                "jira_comment",
                json!({ "action": "add", "key": key, "body": body }),
                WRITE_TIMEOUT,
            )
            .await
        {
            Ok(result) => Ok(comment_url(&result, key)),
            Err(mcp::CallError::Timeout) => Err(Failure::Unknown(
                "写入超时，可能已经写入；再点一次会先查重".to_owned(),
            )),
            Err(mcp::CallError::Failed(message)) => Err(Failure::Failed(message)),
        }
    }

    async fn confluence(&self, job: &Job) -> Result<Option<String>, Failure> {
        let Some((space, parent)) = &self.settings.confluence else {
            return Err(Failure::Failed("没有配置 Confluence 的发布位置".to_owned()));
        };
        let title = confluence_title(&job.answer, &job.row.id);
        let mut session = mcp::Session::start(&self.settings.server, START_TIMEOUT)
            .await
            .map_err(Failure::Failed)?;
        // 查重：同一空间里标题唯一，找到就是上次写成的
        match session
            .call(
                "confluence_page",
                json!({ "action": "by_title", "space": space, "title": title, "with_body": false }),
                READ_TIMEOUT,
            )
            .await
        {
            Ok(page) => return Ok(page_url(&page)),
            Err(mcp::CallError::Timeout) => {
                return Err(Failure::Failed("查重超时，没有写入".to_owned()));
            }
            // 找不到（或查询出错）：去创建。标题唯一，万一已存在创建会失败，不会重复
            Err(mcp::CallError::Failed(_)) => {}
        }
        match session
            .call(
                "confluence_page",
                json!({
                    "action": "create",
                    "space": space,
                    "parent_id": parent,
                    "title": title,
                    "storage": format::storage(&job.markdown),
                }),
                WRITE_TIMEOUT,
            )
            .await
        {
            Ok(page) => Ok(page_url(&page)),
            Err(mcp::CallError::Timeout) => Err(Failure::Unknown(
                "发布超时，可能已经发布；再点一次会先查重".to_owned(),
            )),
            Err(mcp::CallError::Failed(message)) => Err(Failure::Failed(message)),
        }
    }

    /// 按库里的状态重画闭环卡片。
    pub async fn refresh_card(&self, turn_id: &str) {
        let Ok(Some(record)) = self.store.turn_record(turn_id).await else {
            return;
        };
        let rows = self.store.writebacks(turn_id).await.unwrap_or_default();
        let (Some(card_id), Some(card)) = (
            record.card_message_id.clone(),
            closure_card(&record, &rows, &self.link_hosts),
        ) else {
            return;
        };
        for attempt in 1..=3 {
            match self.api.update_card(&card_id, &card).await {
                Ok(()) => return,
                Err(err) => {
                    tracing::warn!(%err, attempt, "更新闭环卡片失败");
                    tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                }
            }
        }
    }
}

/// 闭环方案文档的尾注。确定性的：同一轮每次生成的内容一样，摘要才对得上。
fn document_footer(turn_id: &str) -> String {
    format!(
        "本方案由 ai-boot 根据飞书中的讨论生成（{}），结论以文中引用的 Jira、代码与文档为准。",
        short_id(turn_id)
    )
}

/// ID 去掉连字符后的前 12 位。
fn short_id(id: &str) -> String {
    id.chars().filter(|c| *c != '-').take(12).collect()
}

/// Jira 评论末尾的幂等标记。
fn marker(writeback_id: &str) -> String {
    format!("ai-boot:wb-{}", short_id(writeback_id))
}

/// Confluence 标题：同一空间唯一，带日期、单号和短 ID。日期取写回记录 ID
/// （UUID v7）里的时间，重试时不变。
fn confluence_title(answer: &Answer, writeback_id: &str) -> String {
    let date = uuid::Uuid::parse_str(writeback_id)
        .ok()
        .and_then(|id| id.get_timestamp())
        .map(|ts| {
            let (secs, _) = ts.to_unix();
            crate::context::beijing_date(i64::try_from(secs).unwrap_or_default() * 1000)
        })
        .unwrap_or_default();
    let key = answer
        .jira_keys
        .first()
        .map(|k| format!(" {k}"))
        .unwrap_or_default();
    let title: String = answer.title.chars().take(60).collect();
    format!("【闭环】{title}（{date}{key} {}）", short_id(writeback_id))
}

/// 评论接口返回的 `self`（REST 地址）换成页面地址。
fn comment_url(result: &Value, key: &str) -> Option<String> {
    let rest = result.get("self").and_then(Value::as_str)?;
    let base = &rest[..rest.find("/rest/")?];
    let id = result.get("id").and_then(Value::as_str)?;
    Some(format!("{base}/browse/{key}?focusedCommentId={id}"))
}

fn page_url(page: &Value) -> Option<String> {
    page.get("url").and_then(Value::as_str).map(str::to_owned)
}

/// 闭环卡片：答案 + 写回按钮与状态。卡片内容和写回的内容来自同一份答案。
pub fn closure_card(
    record: &TurnRecord,
    rows: &[Writeback],
    link_hosts: &[String],
) -> Option<Value> {
    let answer: Answer = serde_json::from_str(record.answer_json.as_deref()?).ok()?;
    let targets: Vec<TargetSpec> = record
        .writeback_targets
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    let hash = format::content_hash(&format::document(&answer, &document_footer(&record.id)));
    let views: Vec<WritebackView> = targets
        .iter()
        .filter_map(|spec| {
            let target = Target::parse(&spec.target)?;
            let row = rows
                .iter()
                .find(|r| r.target == spec.target && r.target_ref == spec.reference);
            let state = match row {
                None => WritebackState::Ready,
                Some(row) => match row.status {
                    WritebackStatus::Running => WritebackState::Running,
                    WritebackStatus::Done => WritebackState::Done(row.result_url.clone()),
                    WritebackStatus::Failed => {
                        WritebackState::Failed(row.error.clone().unwrap_or_default())
                    }
                    WritebackStatus::Unknown => {
                        WritebackState::Unknown(row.error.clone().unwrap_or_default())
                    }
                },
            };
            Some(WritebackView {
                label: spec.label.clone(),
                action: crate::callback::Action::Writeback {
                    target,
                    reference: spec.reference.clone(),
                    hash: hash.clone(),
                },
                state,
            })
        })
        .collect();
    let footer = Footer {
        turn: u32::try_from(record.seq).unwrap_or(u32::MAX),
        hint: "写回前会弹窗确认",
        elapsed: Duration::from_millis(u64::try_from(record.duration_ms.unwrap_or(0)).unwrap_or(0)),
        ..Footer::default()
    };
    Some(card::closure(
        &answer,
        &footer,
        &HostAllowlist(link_hosts),
        &record.id,
        &views,
    ))
}

#[cfg(test)]
mod tests;
