use ai_boot_agent::codex::decode::Decoder;
use ai_boot_agent::{AgentEvent, FailKind, Outcome, Step};
use serde_json::json;

#[test]
fn recorded_turn_keeps_the_thread_and_structured_answer() {
    let mut decoder = Decoder::new("gpt-6.1-sol".into());
    let events: Vec<_> = include_str!("fixtures/codex/success.jsonl")
        .lines()
        .flat_map(|line| decoder.decode_line(line))
        .collect();
    assert!(
        matches!(&events[0], AgentEvent::Started(s) if s.session_id == "01a11ac5-e33f-7b42-bc98-215a3fda1341")
    );
    assert!(events.iter().any(
        |e| matches!(e, AgentEvent::Usage(u) if u.input_tokens == 16816 && u.output_tokens == 15)
    ));
    assert!(
        matches!(events.last(), Some(AgentEvent::Finished(Outcome::Success { structured: Some(value), .. })) if value == &json!({"reply":"ok"}))
    );
    assert_eq!(events.iter().filter(|e| e.is_terminal()).count(), 1);
}

#[test]
fn commands_mcp_and_failures_keep_progress_and_error_information() {
    let mut decoder = Decoder::new("m".into());
    let call = decoder.decode_line(r#"{"type":"item.started","item":{"id":"t","type":"mcp_tool_call","server":"qtmcp","tool":"jira_issue","arguments":{"action":"get","key":"X-1"}}}"#);
    assert!(
        matches!(&call[0], AgentEvent::Step(Step::ToolCall { tool, input, .. }) if tool == "mcp__qtmcp__jira_issue" && input["key"] == "X-1")
    );
    let done = decoder.decode_line(r#"{"type":"item.completed","item":{"id":"t","type":"mcp_tool_call","status":"failed","error":{"message":"offline"}}}"#);
    assert!(
        matches!(&done[0], AgentEvent::Step(Step::ToolDone { ok:false, preview, .. }) if preview.contains("offline"))
    );
    let failed = decoder
        .decode_line(r#"{"type":"turn.failed","error":{"message":"429 rate limit exceeded"}}"#);
    assert!(matches!(
        &failed[0],
        AgentEvent::Finished(Outcome::Failed {
            kind: FailKind::RateLimit,
            ..
        })
    ));
}

#[test]
fn plain_or_invalid_output_is_never_invented_as_a_structured_answer() {
    let mut decoder = Decoder::new("m".into());
    assert!(matches!(
        &decoder.decode_line("broken")[0],
        AgentEvent::Warning(_)
    ));
    decoder.decode_line(r#"{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"plain answer"}}"#);
    assert!(
        matches!(decoder.decode_line(r#"{"type":"turn.completed","usage":{}}"#).last(), Some(AgentEvent::Finished(Outcome::Success { structured: None, text, .. })) if text == "plain answer")
    );
}
