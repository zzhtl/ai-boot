//! Claude Code `stream-json` 的防腐层（从 ai-task-exec 迁来，映射到中立事件）。
//!
//! 约束只有两条：
//! 1. **任何输入都不能让执行挂掉**：不认识的事件、解析不了的行、缺字段，一律
//!    降级成 [`AgentEvent::Warning`]。CLI 升级加了新事件，不该让分析失败。
//! 2. **只信 `result` 事件里的用量**：逐条 `assistant` 事件里的 usage 是流式
//!    快照，和最终值差两个数量级（ai-task 实测）。token 取 `modelUsage`，
//!    预算熔断时 `usage` 整个是 0，只有它还是对的。
//!
//! 回归基线是 `tests/fixtures/claude/` 下按机器人真实参数录制的样本；升级 CLI
//! 后重录一份，跑 `decode_claude` 测试就知道这一层要不要改。

use serde_json::Value;

use crate::event::{Activity, AgentEvent, FailKind, McpStatus, Outcome, Started, Step, Usage};

/// 工具结果摘要的长度上限（字符）。
const PREVIEW_LIMIT: usize = 400;
/// 模型中间话语的长度上限（字符）。
const TEXT_LIMIT: usize = 1000;
/// PreToolUse hook 拒绝时，CLI 只给一条错误的工具结果，没有 permission_denied
/// 事件。实测格式：`PreToolUse:<工具名> hook error: <原因>`。
const PRE_TOOL_USE_PREFIX: &str = "PreToolUse:";
const HOOK_ERROR_MARK: &str = " hook error: ";

/// 逐行解码。无状态：每行都是自包含的一条事件。
#[derive(Debug, Default)]
pub struct Decoder;

impl Decoder {
    pub fn new() -> Self {
        Self
    }

    /// 解一行。一行可能产出零到多条事件（一条 assistant 事件里可以有多个内容块）。
    pub fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                // 不带原文：这一行里可能是聊天内容，告警会进日志
                return vec![AgentEvent::Warning(format!(
                    "stream-json 解析失败（{err}），这一行 {} 字节",
                    line.len()
                ))];
            }
        };
        match value.get("type").and_then(Value::as_str) {
            Some("system") => system(&value),
            Some("assistant") => assistant(&value),
            Some("user") => user(&value),
            Some("result") => result(&value),
            Some("rate_limit_event") => rate_limit(&value),
            Some("stream_event") => stream_event(&value),
            Some(other) => vec![AgentEvent::Warning(format!(
                "stream-json 出现未知事件类型 `{other}`，已忽略"
            ))],
            None => vec![AgentEvent::Warning(format!(
                "stream-json 事件缺少 type 字段，这一行 {} 字节",
                line.len()
            ))],
        }
    }
}

impl crate::process::Decode for Decoder {
    fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        Decoder::line(self, line)
    }
}

fn system(value: &Value) -> Vec<AgentEvent> {
    match value.get("subtype").and_then(Value::as_str) {
        Some("init") => vec![AgentEvent::Started(Started {
            session_id: string_at(value, "session_id").unwrap_or_default(),
            model: string_at(value, "model").unwrap_or_default(),
            cli_version: string_at(value, "claude_code_version"),
            tools: string_list(value.get("tools")),
            mcp_servers: value
                .get("mcp_servers")
                .and_then(Value::as_array)
                .map(|servers| {
                    servers
                        .iter()
                        .map(|s| McpStatus {
                            name: string_at(s, "name").unwrap_or_default(),
                            status: string_at(s, "status").unwrap_or_default(),
                        })
                        .collect()
                }),
            // 插件是对象（name/path/source），技能按字符串或对象兼容处理
            plugins: named_list(value.get("plugins"), "source"),
            skills: named_list(value.get("skills"), "name"),
            permission_mode: string_at(value, "permissionMode"),
        })],
        Some("permission_denied") => vec![AgentEvent::Step(Step::Denied {
            id: string_at(value, "tool_use_id").unwrap_or_default(),
            tool: string_at(value, "tool_name").unwrap_or_default(),
            reason: string_at(value, "decision_reason")
                .or_else(|| string_at(value, "message"))
                .unwrap_or_else(|| "未给出原因".into()),
        })],
        Some("api_retry") => vec![AgentEvent::Step(Step::Retry {
            attempt: value
                .get("attempt")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            error: string_at(value, "error").unwrap_or_default(),
        })],
        // 思考 token 的进度心跳，一次执行能刷出几十条
        Some("thinking_tokens") => vec![AgentEvent::Activity(Activity::Thinking)],
        // 每次向 API 发请求前的状态（requesting），只说明还活着
        Some("status") => vec![AgentEvent::Activity(Activity::Streaming)],
        Some(other) => vec![AgentEvent::Warning(format!(
            "stream-json 出现未知 system 子类型 `{other}`，已忽略"
        ))],
        None => Vec::new(),
    }
}

/// `--include-partial-messages` 的增量事件：完整内容随后还会以 assistant 事件出现，
/// 这里只取「模型在做什么」。结构化答案是模型调用 `StructuredOutput` 工具写出来的。
fn stream_event(value: &Value) -> Vec<AgentEvent> {
    let activity = match value.pointer("/event/type").and_then(Value::as_str) {
        Some("content_block_start") => {
            let block = value.pointer("/event/content_block");
            match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                Some("thinking" | "redacted_thinking") => Activity::Thinking,
                Some("text") => Activity::Writing,
                Some("tool_use")
                    if block.and_then(|b| string_at(b, "name")).as_deref()
                        == Some("StructuredOutput") =>
                {
                    Activity::Concluding
                }
                _ => Activity::Streaming,
            }
        }
        _ => Activity::Streaming,
    };
    vec![AgentEvent::Activity(activity)]
}

fn assistant(value: &Value) -> Vec<AgentEvent> {
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => string_at(block, "text")
                .filter(|text| !text.trim().is_empty())
                .map(|text| AgentEvent::Step(Step::Text(truncate(&text, TEXT_LIMIT)))),
            Some("tool_use") => Some(AgentEvent::Step(Step::ToolCall {
                id: string_at(block, "id").unwrap_or_default(),
                tool: string_at(block, "name").unwrap_or_default(),
                input: block.get("input").cloned().unwrap_or(Value::Null),
            })),
            // thinking / redacted_thinking：非 summarized 模式下只有签名，没有可读内容
            _ => None,
        })
        .collect()
}

fn user(value: &Value) -> Vec<AgentEvent> {
    // `.message` 有时是对象有时是裸字符串，后者不携带工具结果
    let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut events = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = string_at(block, "tool_use_id").unwrap_or_default();
        let ok = !block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = content_text(block.get("content"));
        if !ok && let Some((tool, reason)) = pre_tool_use_denial(&text) {
            events.push(AgentEvent::Step(Step::Denied {
                id: id.clone(),
                tool,
                reason,
            }));
        }
        events.push(AgentEvent::Step(Step::ToolDone {
            id,
            ok,
            preview: truncate(&text, PREVIEW_LIMIT),
        }));
    }
    events
}

/// 从 `PreToolUse:<工具> hook error: <原因>` 里拆出工具名和原因。
fn pre_tool_use_denial(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix(PRE_TOOL_USE_PREFIX)?;
    let (tool, reason) = rest.split_once(HOOK_ERROR_MARK)?;
    Some((tool.trim().to_owned(), reason.trim().to_owned()))
}

fn rate_limit(value: &Value) -> Vec<AgentEvent> {
    let status = value
        .pointer("/rate_limit_info/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // allowed 是常态心跳，只有真被限了才值得报
    if status == "allowed" {
        return Vec::new();
    }
    vec![AgentEvent::Step(Step::RateLimited {
        status: status.to_owned(),
        resets_at: value
            .pointer("/rate_limit_info/resetsAt")
            .and_then(Value::as_i64),
    })]
}

fn result(value: &Value) -> Vec<AgentEvent> {
    let usage = usage_of(value);
    let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
    let terminal = value
        .get("terminal_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_error = value
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let outcome = if subtype == "error_max_budget_usd" || terminal == "budget_exhausted" {
        Outcome::BudgetExceeded
    } else if terminal == "aborted_streaming" {
        // SIGINT 或外部中断：实测给出 error_during_execution + aborted_streaming
        Outcome::Interrupted
    } else if is_error || subtype.starts_with("error") {
        let reason = failure_reason(value, subtype, terminal);
        Outcome::Failed {
            kind: classify(value, &reason),
            reason,
        }
    } else {
        Outcome::Success {
            structured: value
                .get("structured_output")
                .filter(|v| !v.is_null())
                .cloned(),
            text: string_at(value, "result").unwrap_or_default(),
            turns: value
                .get("num_turns")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
        }
    };
    vec![AgentEvent::Usage(usage), AgentEvent::Finished(outcome)]
}

fn failure_reason(value: &Value, subtype: &str, terminal: &str) -> String {
    if let Some(text) = string_at(value, "result").filter(|s| !s.is_empty()) {
        return text;
    }
    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        let joined: Vec<&str> = errors.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            return joined.join("；");
        }
    }
    if let Some(status) = value.get("api_error_status").filter(|v| !v.is_null()) {
        return format!("API 错误：{status}");
    }
    format!("CLI 以 `{subtype}` 结束（terminal_reason={terminal}）")
}

/// 失败归类，决定卡片上的提示。只看状态码和少数稳定关键词，拿不准就归 Other。
fn classify(value: &Value, reason: &str) -> FailKind {
    // 实测：--resume 的会话不存在时，没有 init 事件，直接以这句错误结束
    if reason.contains("No conversation found with session ID") {
        return FailKind::SessionNotFound;
    }
    let status = value.get("api_error_status").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    });
    match status {
        Some(401 | 403) => return FailKind::Auth,
        Some(429) => return FailKind::RateLimit,
        Some(500..=599) => return FailKind::Api,
        _ => {}
    }
    let lower = reason.to_ascii_lowercase();
    if ["authentication", "oauth", "/login", "invalid api key"]
        .iter()
        .any(|k| lower.contains(k))
    {
        FailKind::Auth
    } else if ["rate limit", "usage limit"]
        .iter()
        .any(|k| lower.contains(k))
    {
        FailKind::RateLimit
    } else {
        FailKind::Other
    }
}

fn usage_of(value: &Value) -> Usage {
    let mut usage = Usage {
        cost_usd_micros: value
            .get("total_cost_usd")
            .and_then(Value::as_f64)
            .map(usd_to_micros),
        ..Usage::default()
    };
    if let Some(models) = value.get("modelUsage").and_then(Value::as_object) {
        for entry in models.values() {
            usage.input_tokens += u64_at(entry, "inputTokens");
            usage.output_tokens += u64_at(entry, "outputTokens");
            usage.cache_read_tokens += u64_at(entry, "cacheReadInputTokens");
            usage.cache_creation_tokens += u64_at(entry, "cacheCreationInputTokens");
        }
    }
    usage
}

/// `tool_result.content` 可能是字符串，也可能是内容块数组。
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| string_at(b, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

/// 元素可能是字符串，也可能是带 `key` 字段的对象。
fn named_list(value: Option<&Value>, key: &str) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .or_else(|| string_at(item, key))
                    .or_else(|| string_at(item, "name"))
            })
            .collect()
    })
}

fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// 美元转微美元。四舍五入：截断会让每次都少算一点。
fn usd_to_micros(usd: f64) -> i64 {
    if !usd.is_finite() {
        return 0;
    }
    let micros = (usd * 1_000_000.0).round();
    // 已排除 NaN/无穷；夹到 i64 范围内再转换
    micros.clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

/// 按字符截断，不按字节——按字节切会把多字节字符劈开。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…（已截断）")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(line: &str) -> Vec<AgentEvent> {
        Decoder::new().line(line)
    }

    #[test]
    fn unknown_event_types_degrade_to_a_warning() {
        let events = decode(r#"{"type":"telemetry_v2","payload":{}}"#);
        assert!(matches!(events.as_slice(), [AgentEvent::Warning(_)]));
    }

    #[test]
    fn malformed_lines_do_not_echo_their_content() {
        // 告警会进日志，里面不能有原文（可能是聊天内容）
        for junk in [
            format!("not json 机密内容 {}", "x".repeat(5_000)),
            r#"{"no_type":"机密内容"}"#.to_owned(),
        ] {
            let events = decode(&junk);
            let [AgentEvent::Warning(message)] = events.as_slice() else {
                panic!("应当是 Warning");
            };
            assert!(!message.contains("机密内容"), "{message}");
            assert!(message.len() < 200, "{}", message.len());
        }
    }

    #[test]
    fn a_pre_tool_use_denial_is_recognised_from_the_tool_result() {
        let events = decode(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,
                "content":"PreToolUse:mcp__qtmcp__jira_issue hook error: 只读模式：jira_issue.delete 不在放行范围内"}]}}"#,
        );
        assert!(matches!(
            events.as_slice(),
            [
                AgentEvent::Step(Step::Denied { id, tool, reason }),
                AgentEvent::Step(Step::ToolDone { ok: false, .. })
            ] if id == "t1" && tool == "mcp__qtmcp__jira_issue" && reason.contains("jira_issue.delete")
        ));
    }

    #[test]
    fn throttling_is_reported_with_the_reset_time() {
        assert!(
            decode(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#)
                .is_empty()
        );
        let events = decode(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790595000}}"#,
        );
        assert_eq!(
            events,
            vec![AgentEvent::Step(Step::RateLimited {
                status: "rejected".into(),
                resets_at: Some(1_790_595_000)
            })]
        );
    }

    #[test]
    fn failures_are_classified_for_the_card() {
        let failed = |line: &str| match decode(line).pop() {
            Some(AgentEvent::Finished(Outcome::Failed { kind, .. })) => kind,
            other => panic!("应当失败：{other:?}"),
        };
        assert_eq!(
            failed(
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":401}"#
            ),
            FailKind::Auth
        );
        assert_eq!(
            failed(
                r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Please run /login"}"#
            ),
            FailKind::Auth
        );
        assert_eq!(
            failed(
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":529}"#
            ),
            FailKind::Api
        );
    }

    #[test]
    fn cost_rounds_instead_of_truncating() {
        assert_eq!(usd_to_micros(0.033_927), 33_927);
        assert_eq!(usd_to_micros(0.000_000_5), 1);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
