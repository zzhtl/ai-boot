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

use crate::event::{
    Activity, AgentEvent, Draft, FailKind, McpStatus, Outcome, Started, Step, Usage,
};

/// 工具结果摘要的长度上限（字符）。
const PREVIEW_LIMIT: usize = 400;
/// 模型中间话语的长度上限（字符）。
const TEXT_LIMIT: usize = 1000;
/// PreToolUse hook 拒绝时，CLI 只给一条错误的工具结果，没有 permission_denied
/// 事件。实测格式：`PreToolUse:<工具名> hook error: <原因>`。
const PRE_TOOL_USE_PREFIX: &str = "PreToolUse:";
const HOOK_ERROR_MARK: &str = " hook error: ";
/// 标题和概述里被写成 `\uXXXX` 的汉字达到这么多个，就认为这次答案在转义输出中文。
const ESCAPED_CJK_WARN: usize = 10;
/// 草稿只从答案开头这么多字节里找：模型万一没按顺序写，不至于每个片段都从头扫一遍
/// 整份答案。
const DRAFT_SCAN_BYTES: usize = 16 * 1024;

/// 逐行解码。每行都是自包含的一条事件，只有两件事要跨行记：结构化答案的增量片段
/// （拼起来才看得出哪些字段写完了），和最近一次请求带上的上下文（终态时一起报）。
#[derive(Debug, Default)]
pub struct Decoder {
    /// 正在写的结构化答案。
    answer: Option<PartialAnswer>,
    /// 已经报过的草稿：字段没变就不再报。
    draft: Draft,
    context_tokens: u64,
}

/// `StructuredOutput` 工具调用的内容块：序号和已收到的 JSON 片段。
#[derive(Debug)]
struct PartialAnswer {
    index: u64,
    json: String,
    /// 标题和概述都拿到了。
    head: bool,
    /// 命令也拿到了（或者已经超出扫描范围）：后面的片段不用再拼。
    done: bool,
    /// 已经检查过中文有没有被转义。
    checked: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
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
            Some("assistant") => {
                if let Some(tokens) = context_of(&value) {
                    self.context_tokens = tokens;
                }
                assistant(&value)
            }
            Some("user") => user(&value),
            Some("result") => result(&value, self.context_tokens),
            Some("rate_limit_event") => rate_limit(&value),
            Some("stream_event") => self.stream_event(&value),
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

impl Decoder {
    /// `--include-partial-messages` 的增量事件：完整内容随后还会以 assistant 事件出现，
    /// 这里只取「模型在做什么」，以及结构化答案里已经写完的标题、概述和命令。结构化答案是
    /// 模型调用 `StructuredOutput` 工具写出来的，参数按 JSON 片段一段段流过来。
    fn stream_event(&mut self, value: &Value) -> Vec<AgentEvent> {
        let index = value.pointer("/event/index").and_then(Value::as_u64);
        let mut events = Vec::new();
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
                        self.answer = index.map(|index| PartialAnswer {
                            index,
                            json: String::new(),
                            head: false,
                            done: false,
                            checked: false,
                        });
                        Activity::Concluding
                    }
                    _ => Activity::Streaming,
                }
            }
            Some("content_block_delta") => {
                if let Some(draft) = self.answer_delta(index, value) {
                    events.push(AgentEvent::Draft(draft));
                }
                if let Some(warning) = self.escape_warning() {
                    events.push(AgentEvent::Warning(warning));
                }
                Activity::Streaming
            }
            // 序号只在一条消息内有效
            Some("message_start") => {
                self.answer = None;
                Activity::Streaming
            }
            _ => Activity::Streaming,
        };
        events.insert(0, AgentEvent::Activity(activity));
        events
    }

    /// 结构化答案的一段参数片段。标题、概述或命令刚写完时返回新的草稿。
    fn answer_delta(&mut self, index: Option<u64>, value: &Value) -> Option<Draft> {
        let answer = self.answer.as_mut().filter(|a| Some(a.index) == index)?;
        if answer.done {
            return None;
        }
        answer
            .json
            .push_str(value.pointer("/event/delta/partial_json")?.as_str()?);
        let title = completed_string(&answer.json, "title");
        let summary = completed_string(&answer.json, "summary");
        let commands = completed_raw(&answer.json, "commands")
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .filter(Value::is_array);
        answer.head = title.is_some() && summary.is_some();
        answer.done = (answer.head && commands.is_some()) || answer.json.len() > DRAFT_SCAN_BYTES;
        // 答案被 schema 校验打回、重写时，先保留上一次的
        let draft = Draft {
            title: title.or_else(|| self.draft.title.clone()),
            summary: summary.or_else(|| self.draft.summary.clone()),
            commands: commands.or_else(|| self.draft.commands.clone()),
        };
        if draft == self.draft {
            return None;
        }
        self.draft = draft.clone();
        Some(draft)
    }

    /// 标题和概述写完时看一眼：模型偶尔把整份答案的中文写成 `\uXXXX`，解析出来一样，
    /// 生成的 token 却多两三倍，写结论要多等一两分钟。只报一次，留在日志里看有多常见。
    fn escape_warning(&mut self) -> Option<String> {
        let answer = self.answer.as_mut().filter(|a| a.head && !a.checked)?;
        answer.checked = true;
        let escaped = escaped_cjk(&answer.json);
        (escaped >= ESCAPED_CJK_WARN).then(|| {
            format!("结构化答案的中文被写成了 \\uXXXX 转义（标题和概述里有 {escaped} 个），这一轮写结论会慢两三倍")
        })
    }
}

/// JSON 原文里被写成 `\uXXXX` 的常用汉字个数。成对跳过转义序列，`\\u4e2d`（字面的
/// 反斜杠加 u）不算。
fn escaped_cjk(json: &str) -> usize {
    let bytes = json.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let hex = bytes
            .get(i + 2..i + 6)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match (bytes.get(i + 1), hex) {
            (Some(b'u'), Some(code)) => {
                if (0x4E00..=0x9FFF).contains(&code) {
                    count += 1;
                }
                i += 6;
            }
            _ => i += 2,
        }
    }
    count
}

/// 写到一半的 JSON 对象里，顶层字段 `key` 的字符串值；值的结束引号还没出现（或者
/// 这个字段还没写到、不是字符串）就返回 `None`。
fn completed_string(partial: &str, key: &str) -> Option<String> {
    serde_json::from_str(completed_raw(partial, key)?).ok()
}

/// 写到一半的 JSON 对象里，顶层字段 `key` 已经写完的值（字符串、数组或对象）的原文；
/// 还没写完或者还没写到就返回 `None`。
fn completed_raw<'a>(partial: &'a str, key: &str) -> Option<&'a str> {
    let bytes = partial.as_bytes();
    let mut depth = 0_usize;
    // 顶层下一个字符串是字段名
    let mut expect_key = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => {
                depth += 1;
                expect_key = depth == 1 && bytes[i] == b'{';
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' => {
                expect_key = depth == 1;
                i += 1;
            }
            b'"' => {
                let end = string_end(bytes, i)?;
                if !(depth == 1 && expect_key) {
                    i = end + 1;
                    continue;
                }
                expect_key = false;
                let name: String = serde_json::from_str(&partial[i..=end]).ok()?;
                let colon = skip_space(bytes, end + 1)?;
                if bytes[colon] != b':' {
                    i = colon;
                    continue;
                }
                let start = skip_space(bytes, colon + 1)?;
                if name == key {
                    let end = match bytes[start] {
                        b'"' => string_end(bytes, start)?,
                        b'[' | b'{' => container_end(bytes, start)?,
                        _ => return None,
                    };
                    return Some(&partial[start..=end]);
                }
                i = start;
            }
            _ => i += 1,
        }
    }
    None
}

/// 从 `start` 处的引号开始的字符串在哪里结束（结束引号的下标），还没结束返回 `None`。
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// 从 `start` 处的 `[` 或 `{` 开始的数组、对象在哪里结束（配对的括号的下标），还没
/// 结束返回 `None`。字符串里的括号不算。
fn container_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = string_end(bytes, i)? + 1;
                continue;
            }
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// 跳过空白，返回下一个字符的下标；到末尾了返回 `None`。
fn skip_space(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len()).find(|&i| !bytes[i].is_ascii_whitespace())
}

/// 一次请求带上的上下文：输入加缓存读写。同一次请求的每个内容块都带着同一份用量。
fn context_of(value: &Value) -> Option<u64> {
    let usage = value.pointer("/message/usage")?;
    Some(
        u64_at(usage, "input_tokens")
            + u64_at(usage, "cache_creation_input_tokens")
            + u64_at(usage, "cache_read_input_tokens"),
    )
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
        // 跑得久的命令被 CLI 转到后台，开始和结束各报一次：命令还在跑，不是卡住了
        Some("task_started" | "task_progress" | "task_updated" | "task_notification") => {
            vec![AgentEvent::Activity(Activity::Streaming)]
        }
        // 斜杠命令清单有变；我们禁用了斜杠命令，用不上
        Some("commands_changed") => Vec::new(),
        Some(other) => vec![AgentEvent::Warning(format!(
            "stream-json 出现未知 system 子类型 `{other}`，已忽略"
        ))],
        None => Vec::new(),
    }
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

fn result(value: &Value, context_tokens: u64) -> Vec<AgentEvent> {
    let usage = Usage {
        context_tokens,
        ..usage_of(value)
    };
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
    fn background_commands_count_as_progress_and_command_list_changes_are_silent() {
        for subtype in ["task_started", "task_notification"] {
            let events = decode(&format!(
                r#"{{"type":"system","subtype":"{subtype}","task_id":"b1","description":"git clone"}}"#
            ));
            assert_eq!(
                events,
                vec![AgentEvent::Activity(Activity::Streaming)],
                "{subtype}"
            );
        }
        assert!(
            decode(r#"{"type":"system","subtype":"commands_changed","commands":[]}"#).is_empty()
        );
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
    fn a_field_counts_as_written_only_once_its_closing_quote_arrives() {
        let full = r#"{"kind": "diagnosis", "title": "登录超时 \"1205\"", "status": "partial", "summary": "锁等待\n超时", "sections": [{"title": "x"}]}"#;
        assert_eq!(
            completed_string(full, "title").as_deref(),
            Some("登录超时 \"1205\"")
        );
        assert_eq!(
            completed_string(full, "summary").as_deref(),
            Some("锁等待\n超时")
        );
        // 嵌套对象里的同名字段不算
        let nested = r#"{"sections": [{"title": "段落"}], "title": "顶层"#;
        assert_eq!(completed_string(nested, "title"), None);
        // 每一个前缀都不能报出没写完的值
        for end in 0..full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let partial = &full[..end];
            if let Some(summary) = completed_string(partial, "summary") {
                assert_eq!(summary, "锁等待\n超时", "{partial}");
            }
        }
    }

    #[test]
    fn the_draft_follows_the_structured_answer_as_it_streams() {
        let mut decoder = Decoder::new();
        let mut drafts = Vec::new();
        let start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"StructuredOutput","input":{}}}}"#;
        drafts.extend(decoder.line(start));
        let pieces = [
            r#"{"kind": "diagnosis", "ti"#,
            r#"tle": "登录超时", "status": "par"#,
            r#"tial", "summary": "锁等待"#,
            r#"超时", "sections": []}"#,
        ];
        for piece in pieces {
            let delta = serde_json::json!({
                "type": "stream_event",
                "event": {"type": "content_block_delta", "index": 1,
                          "delta": {"type": "input_json_delta", "partial_json": piece}}
            });
            drafts.extend(decoder.line(&delta.to_string()));
        }
        let drafts: Vec<Draft> = drafts
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Draft(draft) => Some(draft),
                _ => None,
            })
            .collect();
        assert_eq!(
            drafts,
            [
                Draft {
                    title: Some("登录超时".into()),
                    summary: None,
                    commands: None,
                },
                Draft {
                    title: Some("登录超时".into()),
                    summary: Some("锁等待超时".into()),
                    commands: None,
                },
            ]
        );
        // 别的内容块的片段不算
        let other = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"summary\": \"x\"}"}}}"#;
        assert!(
            !decoder
                .line(other)
                .iter()
                .any(|e| matches!(e, AgentEvent::Draft(_)))
        );
    }

    /// 命令数组紧跟概述写出来：整个数组写完才算，字符串里的 `]` 和引号不能让它提前收尾。
    #[test]
    fn the_commands_count_as_written_only_once_the_whole_array_arrives() {
        let full = r#"{"title": "看堆积", "summary": "看 LAG 列", "commands": [{"title": "查看[堆积]", "where": "kafka \"主机\"", "command": "grep ']' a.log", "look": ""}], "sections": []}"#;
        let expected: Value = serde_json::from_str(full).expect("JSON");
        let raw = completed_raw(full, "commands").expect("写完了");
        assert_eq!(
            serde_json::from_str::<Value>(raw).expect("数组"),
            expected["commands"]
        );
        for end in 0..full.len() {
            if !full.is_char_boundary(end) {
                continue;
            }
            let partial = &full[..end];
            if let Some(raw) = completed_raw(partial, "commands") {
                assert_eq!(
                    serde_json::from_str::<Value>(raw).ok().as_ref(),
                    Some(&expected["commands"]),
                    "{partial}"
                );
            }
        }
        // 草稿里带着命令，写完命令之后的片段不再拼
        let events = stream_answer(&[&full[..60], &full[60..]]);
        let last = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Draft(draft) => Some(draft),
                _ => None,
            })
            .next_back()
            .expect("草稿");
        assert_eq!(last.commands.as_ref(), Some(&expected["commands"]));
        assert_eq!(last.summary.as_deref(), Some("看 LAG 列"));
    }

    /// 把一份结构化答案按片段喂给解码器，返回所有事件。
    fn stream_answer(pieces: &[&str]) -> Vec<AgentEvent> {
        let mut decoder = Decoder::new();
        let mut events = decoder.line(r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"StructuredOutput","input":{}}}}"#);
        for piece in pieces {
            let delta = serde_json::json!({
                "type": "stream_event",
                "event": {"type": "content_block_delta", "index": 1,
                          "delta": {"type": "input_json_delta", "partial_json": piece}}
            });
            events.extend(decoder.line(&delta.to_string()));
        }
        events
    }

    #[test]
    fn an_answer_written_with_escaped_chinese_is_reported_once() {
        let warnings = |events: &[AgentEvent]| {
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::Warning(w) if w.contains("转义")))
                .count()
        };
        // 把汉字逐个写成 \uXXXX，模拟模型转义输出
        let escape = |text: &str| -> String {
            text.chars()
                .map(|c| format!("\\u{:04x}", u32::from(c)))
                .collect()
        };
        let escaped = format!(
            r#"{{"title": "{}", "summary": "{}", "#,
            escape("登录超时"),
            escape("连接池耗尽导致登录超时")
        );
        let events = stream_answer(&[&escaped, r#""sections": []}"#]);
        assert_eq!(warnings(&events), 1, "{events:?}");
        // 解析出来的草稿照常是中文
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::Draft(Draft { summary: Some(s), .. }) if s == "连接池耗尽导致登录超时"
        )));

        // 正文里出现字面的反斜杠加 u（JSON 里写作两个反斜杠）不算
        let plain = format!(
            r#"{{"title": "登录超时", "summary": "连接池耗尽导致登录超时，日志里的 {} 不算", "#,
            escape("连接池耗尽导致登录超时").replace('\\', "\\\\")
        );
        assert_eq!(warnings(&stream_answer(&[&plain, "}"])), 0);
    }

    #[test]
    fn the_last_request_context_is_reported_with_the_usage() {
        let mut decoder = Decoder::new();
        for (input, created, read) in [(10, 6_000, 0), (8, 400, 6_010)] {
            let line = serde_json::json!({
                "type": "assistant",
                "message": {"content": [], "usage": {
                    "input_tokens": input,
                    "cache_creation_input_tokens": created,
                    "cache_read_input_tokens": read
                }}
            });
            decoder.line(&line.to_string());
        }
        let events = decoder.line(r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","modelUsage":{"m":{"inputTokens":18,"outputTokens":5}}}"#);
        let Some(AgentEvent::Usage(usage)) = events.first() else {
            panic!("应当先报用量：{events:?}");
        };
        assert_eq!(usage.context_tokens, 6_418);
        assert_eq!(usage.input_tokens, 18);
    }

    #[test]
    fn cost_rounds_instead_of_truncating() {
        assert_eq!(usd_to_micros(0.033_927), 33_927);
        assert_eq!(usd_to_micros(0.000_000_5), 1);
        assert_eq!(usd_to_micros(f64::NAN), 0);
    }
}
