//! 直接执行编出来的二进制测 hook：CLI 看到的就是这个进程的 stdout 和退出码。

use std::io::Write as _;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

struct Reply {
    code: Option<i32>,
    stdout: String,
}

fn hook(backend: &str, workdir: &std::path::Path, stdin: &[u8]) -> Reply {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-boot"))
        .args(["hook", "--backend", backend, "--workdir"])
        .arg(workdir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 hook");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("写入");
    let output = child.wait_with_output().expect("等待 hook");
    Reply {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).expect("UTF-8"),
    }
}

fn claude_input(event: &str, tool: &str, input: Value) -> Vec<u8> {
    // 字段取自 2.1.283 实测的 hook 输入
    json!({
        "session_id": "s", "transcript_path": "/t", "cwd": "/w", "permission_mode": "default",
        "hook_event_name": event, "tool_name": tool, "tool_input": input,
    })
    .to_string()
    .into_bytes()
}

fn parsed(reply: &Reply) -> Value {
    serde_json::from_str(reply.stdout.trim()).expect("stdout 应当是一条 JSON")
}

#[test]
fn permission_requests_for_mcp_reads_and_writes_are_approved() {
    let dir = tempfile::tempdir().expect("临时目录");
    for (tool, input) in [
        (
            "mcp__qtmcp__jira_issue",
            json!({"action": "get", "key": "XX-1"}),
        ),
        (
            "mcp__qtmcp__confluence_page",
            json!({"action": "update", "page_id": "1"}),
        ),
    ] {
        let reply = hook(
            "claude",
            dir.path(),
            &claude_input("PermissionRequest", tool, input),
        );
        assert_eq!(reply.code, Some(0), "{tool}");
        assert_eq!(
            parsed(&reply),
            json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}}),
            "{tool}"
        );
    }
}

#[test]
fn permission_requests_for_command_tools_are_denied_with_a_reason() {
    let dir = tempfile::tempdir().expect("临时目录");
    let reply = hook(
        "claude",
        dir.path(),
        &claude_input("PermissionRequest", "Bash", json!({"command": "id"})),
    );
    assert_eq!(reply.code, Some(0));
    let out = parsed(&reply);
    assert_eq!(out["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert!(
        out["hookSpecificOutput"]["decision"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("Bash"))
    );
}

#[test]
fn pre_tool_use_denies_command_tools_and_stays_silent_for_the_rest() {
    let dir = tempfile::tempdir().expect("临时目录");
    let command = hook(
        "claude",
        dir.path(),
        &claude_input("PreToolUse", "Bash", json!({"command": "id"})),
    );
    assert_eq!(command.code, Some(0));
    assert_eq!(
        parsed(&command)["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    for (tool, input) in [
        ("mcp__qtmcp__jira_search", json!({"jql": "project = XX"})),
        (
            "mcp__qtmcp__jira_comment",
            json!({"action": "add", "key": "XX-1", "body": "x"}),
        ),
        ("StructuredOutput", json!({"summary": "x"})),
        ("Grep", json!({"pattern": "ERROR"})),
    ] {
        let reply = hook(
            "claude",
            dir.path(),
            &claude_input("PreToolUse", tool, input),
        );
        assert_eq!(reply.code, Some(0), "{tool}");
        assert!(
            reply.stdout.is_empty(),
            "{tool} 应当不表态：{}",
            reply.stdout
        );
    }
}

#[test]
fn reads_outside_the_workdir_are_denied() {
    let dir = tempfile::tempdir().expect("临时目录");
    let reply = hook(
        "claude",
        dir.path(),
        &claude_input("PreToolUse", "Read", json!({"file_path": "/etc/hostname"})),
    );
    assert_eq!(
        parsed(&reply)["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
}

#[test]
fn garbage_input_blocks_instead_of_passing() {
    // 看不懂就拒绝：放行意味着「喂一段垃圾就能绕过策略」
    let dir = tempfile::tempdir().expect("临时目录");
    for garbage in [&b"not json"[..], b"", b"{}", b"{\"tool_name\":\"Bash\"}"] {
        let reply = hook("claude", dir.path(), garbage);
        assert_eq!(
            reply.code,
            Some(2),
            "{:?}",
            String::from_utf8_lossy(garbage)
        );
        assert!(reply.stdout.is_empty());
    }
}

#[test]
fn other_hook_events_are_left_alone() {
    let dir = tempfile::tempdir().expect("临时目录");
    let reply = hook(
        "claude",
        dir.path(),
        &claude_input("PostToolUse", "Read", json!({})),
    );
    assert_eq!(reply.code, Some(0));
    assert!(reply.stdout.is_empty());
}

#[test]
fn codex_payloads_get_the_same_decisions() {
    // Codex 的 hook 输入多了 turn_id/model，permission_mode 在 exec 下是 bypassPermissions
    let dir = tempfile::tempdir().expect("临时目录");
    let input = json!({
        "session_id": "s", "turn_id": "t", "cwd": "/w", "model": "m",
        "permission_mode": "bypassPermissions", "hook_event_name": "PermissionRequest",
        "tool_name": "mcp__qtmcp__gitlab_project", "tool_input": {"action": "create_branch"},
    });
    let reply = hook("codex", dir.path(), input.to_string().as_bytes());
    assert_eq!(
        parsed(&reply)["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
}
