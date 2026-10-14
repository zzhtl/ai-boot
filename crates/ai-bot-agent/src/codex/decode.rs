//! `codex exec --json` 的 JSONL 事件转成中立事件。

use crate::{Activity, AgentEvent, Draft, FailKind, Outcome, Started, Step, Usage};
use serde_json::{Value, json};

pub struct Decoder {
    model: String,
    text: String,
}

impl Decoder {
    pub fn new(model: String) -> Self {
        Self {
            model,
            text: String::new(),
        }
    }

    pub fn decode_line(&mut self, line: &str) -> Vec<AgentEvent> {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => return vec![AgentEvent::Warning("Codex 输出了非 JSON 事件".into())],
        };
        match string(&value, "type") {
            "thread.started" => vec![AgentEvent::Started(Started {
                session_id: string(&value, "thread_id").into(),
                model: self.model.clone(),
                ..Started::default()
            })],
            "turn.started" => vec![AgentEvent::Activity(Activity::Thinking)],
            "item.started" | "item.updated" | "item.completed" => self.item(&value),
            "turn.completed" => {
                let usage = &value["usage"];
                vec![
                    AgentEvent::Usage(Usage {
                        input_tokens: usage["input_tokens"].as_u64().unwrap_or_default(),
                        output_tokens: usage["output_tokens"].as_u64().unwrap_or_default(),
                        cache_read_tokens: usage["cached_input_tokens"]
                            .as_u64()
                            .unwrap_or_default(),
                        cache_creation_tokens: usage["cache_write_input_tokens"]
                            .as_u64()
                            .unwrap_or_default(),
                        ..Usage::default()
                    }),
                    AgentEvent::Finished(Outcome::Success {
                        structured: serde_json::from_str(&self.text).ok(),
                        text: self.text.clone(),
                        turns: 1,
                    }),
                ]
            }
            "turn.failed" => {
                let reason = string(&value["error"], "message").to_owned();
                vec![AgentEvent::Finished(Outcome::Failed {
                    kind: failure_kind(&reason),
                    reason,
                })]
            }
            "error" => vec![AgentEvent::Warning(string(&value, "message").into())],
            other => vec![AgentEvent::Warning(format!("未知 Codex 事件：{other}"))],
        }
    }

    fn item(&mut self, value: &Value) -> Vec<AgentEvent> {
        let item = &value["item"];
        let completed = string(value, "type") == "item.completed";
        let started = string(value, "type") == "item.started";
        let id = string(item, "id").to_owned();
        match string(item, "type") {
            "agent_message" => {
                let text = string(item, "text").to_owned();
                self.text = text.clone();
                if let Ok(structured) = serde_json::from_str::<Value>(&text) {
                    vec![
                        AgentEvent::Activity(Activity::Concluding),
                        AgentEvent::Draft(Draft {
                            title: structured["title"].as_str().map(str::to_owned),
                            summary: structured["summary"].as_str().map(str::to_owned),
                            commands: structured.get("commands").cloned(),
                        }),
                    ]
                } else {
                    vec![
                        AgentEvent::Activity(Activity::Writing),
                        AgentEvent::Step(Step::Text(text)),
                    ]
                }
            }
            "reasoning" => vec![AgentEvent::Activity(Activity::Thinking)],
            "command_execution" | "mcp_tool_call" | "web_search" | "file_change" => {
                let (tool, input) = match string(item, "type") {
                    "command_execution" => ("Bash".into(), json!({"command": item["command"]})),
                    "mcp_tool_call" => (
                        format!("mcp__{}__{}", string(item, "server"), string(item, "tool")),
                        item["arguments"].clone(),
                    ),
                    "web_search" => ("WebSearch".into(), json!({"query": item["query"]})),
                    _ => ("apply_patch".into(), item["changes"].clone()),
                };
                if started {
                    vec![AgentEvent::Step(Step::ToolCall { id, tool, input })]
                } else if completed {
                    let ok = string(item, "status") != "failed"
                        && item["exit_code"].as_i64().is_none_or(|code| code == 0)
                        && item["error"].is_null()
                        && item["result"]["isError"] != true;
                    let preview = if let Some(output) = item["aggregated_output"].as_str() {
                        output.to_owned()
                    } else if !item["error"].is_null() {
                        item["error"].to_string()
                    } else {
                        String::new()
                    };
                    vec![AgentEvent::Step(Step::ToolDone {
                        id,
                        ok,
                        preview: preview.chars().take(500).collect(),
                    })]
                } else {
                    vec![AgentEvent::Activity(Activity::Streaming)]
                }
            }
            "todo_list" => vec![AgentEvent::Activity(Activity::Thinking)],
            other => vec![AgentEvent::Warning(format!("未知 Codex item：{other}"))],
        }
    }
}

impl crate::process::Decode for Decoder {
    fn line(&mut self, line: &str) -> Vec<AgentEvent> {
        self.decode_line(line)
    }

    fn abnormal_exit(&self, reason: String) -> Outcome {
        let kind = match failure_kind(&reason) {
            FailKind::Api => FailKind::Cli,
            kind => kind,
        };
        Outcome::Failed { kind, reason }
    }
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn failure_kind(reason: &str) -> FailKind {
    let reason = reason.to_ascii_lowercase();
    if reason.contains("no rollout found")
        || (reason.contains("session")
            && (reason.contains("not found") || reason.contains("no saved")))
    {
        FailKind::SessionNotFound
    } else if reason.contains("401")
        || reason.contains("unauthorized")
        || reason.contains("authentication")
    {
        FailKind::Auth
    } else if reason.contains("429")
        || reason.contains("rate limit")
        || reason.contains("usage limit")
        || reason.contains("quota")
    {
        FailKind::RateLimit
    } else {
        FailKind::Api
    }
}
