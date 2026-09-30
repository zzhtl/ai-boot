//! 真实 claude + 真实 `ai-boot hook` 的全链路检查。
//!
//! 默认跳过（会消耗订阅额度，也依赖本机的登录态和 qtmcp）。手动运行：
//! `AI_BOOT_LIVE_CLAUDE=1 cargo test -p ai-boot --test live_claude -- --nocapture`
//!
//! 只调用 GitLab 的只读 action：GitLab 走 token，不会触发 SSO 登录。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use ai_boot_agent::claude::{ClaudeBackend, ClaudeConfig};
use ai_boot_agent::{AgentBackend, AgentEvent, McpServer, Outcome, SessionRef, Step, TurnRequest};

fn enabled() -> bool {
    matches!(
        std::env::var("AI_BOOT_LIVE_CLAUDE").as_deref(),
        Ok("1" | "true")
    )
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

#[tokio::test]
async fn read_actions_pass_and_escapes_are_blocked_end_to_end() {
    if !enabled() {
        eprintln!("跳过：设置 AI_BOOT_LIVE_CLAUDE=1 才运行");
        return;
    }
    let root = tempfile::tempdir().expect("临时目录");
    let workdir = root.path().join("work");
    std::fs::create_dir(&workdir).expect("工作目录");
    std::fs::write(workdir.join("notes.txt"), "live-check").expect("写文件");

    let env: Vec<(OsString, OsString)> = ["HOME", "PATH", "LANG"]
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|v| (OsString::from(key), v)))
        .collect();
    let backend = ClaudeBackend::new(ClaudeConfig {
        program: home().join(".local/bin/claude"),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_ai-boot")),
        hook_timeout_secs: 10,
        env,
        stop_grace: Duration::from_secs(10),
    });
    let request = TurnRequest {
        session: SessionRef::New,
        prompt: "按顺序真实调用工具，不要跳过：1. 调用 qtmcp 的 gitlab_project，action=search，关键词 qtmcp；\
                 2. 用 Read 读取 /etc/hostname；3. 用 Read 读取 ./notes.txt。\
                 最后按 JSON 输出：summary 一句话，steps 逐条写每一步的结果。"
            .into(),
        workdir,
        run_dir: root.path().join("run"),
        images: Vec::new(),
        rules: "你是全链路检查探针，按用户要求逐步调用工具。".into(),
        schema: serde_json::json!({
            "type": "object", "additionalProperties": false, "required": ["summary", "steps"],
            "properties": {"summary": {"type": "string"}, "steps": {"type": "array", "items": {"type": "string"}}}
        }),
        mcp_servers: vec![McpServer {
            name: "qtmcp".into(),
            command: home().join(".local/bin/qtmcp"),
            args: vec!["--toolsets".into(), "gitlab".into()],
            env: BTreeMap::new(),
        }],
        model: Some("haiku".into()),
        effort: None,
        timeout: Duration::from_secs(240),
        budget_usd_micros: Some(500_000),
    };

    let mut handle = backend.start_turn(request).await.expect("启动");
    let mut events = Vec::new();
    while let Some(event) = handle.events.recv().await {
        eprintln!("{event:?}");
        let done = event.is_terminal();
        events.push(event);
        if done {
            break;
        }
    }

    let started = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Started(s) => Some(s),
            _ => None,
        })
        .expect("Started");
    assert_eq!(
        started.session_id,
        handle.session_id.clone().unwrap_or_default()
    );
    assert!(started.mcp_servers.as_deref().is_some_and(|s| {
        s.iter()
            .any(|m| m.name == "qtmcp" && m.status == "connected")
    }));

    // MCP 调用真的执行了（命令行上的 --allowedTools 预先批准了 qtmcp）
    let search_ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Step(Step::ToolCall { id, tool, input })
                if tool == "mcp__qtmcp__gitlab_project" && input["action"] == "search" =>
            {
                Some(id.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(!search_ids.is_empty(), "模型没有调用 search");
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Step(Step::ToolDone { id, ok: true, .. }) if search_ids.contains(&id.as_str())
    )));

    // 工作目录外的读取被拦下
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::Step(Step::Denied { tool, .. }) if tool == "Read"
    )));
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Finished(Outcome::Success {
            structured: Some(_),
            ..
        }))
    ));
}
