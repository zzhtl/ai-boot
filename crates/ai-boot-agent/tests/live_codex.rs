//! 本机真实 CLI 验证：设置 AI_BOOT_CODEX_PROFILE、AI_BOOT_CONFLUENCE_PAGE_ID 后运行 --ignored。
use ai_boot_agent::codex::{CodexBackend, CodexConfig};
use ai_boot_agent::{
    AgentBackend, AgentEvent, Effort, McpServer, Outcome, SessionRef, TurnRequest,
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
#[ignore = "调用已登录的本机 Codex 和 qtmcp"]
async fn codex_profile_qtmcp_and_resume() {
    let root = tempfile::tempdir().expect("目录");
    let work = root.path().join("work");
    std::fs::create_dir(&work).expect("工作目录");
    let login_home = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".codex")
        });
    let home = root.path().join("codex");
    std::fs::create_dir(&home).expect("测试配置目录");
    let auth = login_home.join("auth.json");
    if auth.is_file() {
        std::os::unix::fs::symlink(auth, home.join("auth.json")).expect("复用登录");
    }
    let profile = std::env::var("AI_BOOT_CODEX_PROFILE").ok();
    if let Some(profile) = &profile {
        std::fs::write(
            home.join(format!("{profile}.config.toml")),
            "model_reasoning_effort = 'xhigh'\n",
        )
        .expect("测试 profile");
    }
    let mut env: Vec<_> = [
        "HOME",
        "PATH",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ]
    .into_iter()
    .filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value)))
    .collect();
    env.push(("CODEX_HOME".into(), home.into_os_string()));
    let backend = CodexBackend::new(CodexConfig {
        program: std::env::var_os("AI_BOOT_CODEX_BIN")
            .unwrap_or_else(|| "codex".into())
            .into(),
        profile,
        env,
        stop_grace: Duration::from_secs(5),
    });
    let qtmcp = std::env::var_os("AI_BOOT_QTMCP_BIN").expect("AI_BOOT_QTMCP_BIN");
    let config = std::env::var("QTMCP_CONFIG").expect("QTMCP_CONFIG");
    let servers = vec![McpServer {
        name: "qtmcp".into(),
        command: qtmcp.into(),
        args: vec!["--toolsets".into(), "jira,confluence,gitlab,jenkins".into()],
        env: [("QTMCP_CONFIG".into(), config)].into_iter().collect(),
    }];
    let request = |session, prompt: &str, run: &str| TurnRequest {
        session,
        prompt: prompt.into(),
        workdir: work.clone(),
        run_dir: root.path().join(run),
        images: Vec::new(),
        rules: "只做查询，不执行写操作。按 JSON 契约回答。".into(),
        schema: json!({"type":"object","properties":{"marker":{"type":"string"}},"required":["marker"],"additionalProperties":false}),
        mcp_servers: servers.clone(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some(Effort::Xhigh),
        timeout: Duration::from_secs(240),
        budget_usd_micros: None,
    };
    let mut session = None;
    let mut tool_ids = Vec::new();
    let mut tool_succeeded = false;
    let mut success = false;
    let page_id = std::env::var("AI_BOOT_CONFLUENCE_PAGE_ID").expect("AI_BOOT_CONFLUENCE_PAGE_ID");
    let first = format!(
        "记住本会话标记 ai-boot-smoke-7281。调用 qtmcp 的 confluence_page，action=get，page_id={page_id}，max_chars=1000，实际读取正文。成功后 marker 返回该标记。不要使用其他工具。"
    );
    let mut handle = backend
        .start_turn(request(SessionRef::New, &first, "first"))
        .await
        .expect("启动");
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Started(s) => session = Some(s.session_id),
            AgentEvent::Step(ai_boot_agent::Step::ToolCall { id, tool, .. }) => {
                if tool == "mcp__qtmcp__confluence_page" {
                    tool_ids.push(id);
                }
            }
            AgentEvent::Step(ai_boot_agent::Step::ToolDone { id, ok, .. }) => {
                tool_succeeded |= ok && tool_ids.contains(&id);
            }
            AgentEvent::Finished(outcome) => {
                assert!(
                    matches!(&outcome, Outcome::Success { structured: Some(value), .. } if value["marker"] == "ai-boot-smoke-7281"),
                    "{outcome:?}"
                );
                success = true;
            }
            _ => {}
        }
    }
    assert!(
        success && tool_succeeded,
        "必须实际调用 qtmcp 成功并完成回答"
    );
    let second = format!(
        "再次调用 qtmcp 的 confluence_page，action=get，page_id={page_id}，max_chars=1000，重新读取正文。不要使用其他工具。成功后按 ai-boot 标准契约回答，summary 只返回上一轮记住的标记原文，kind 用 other，status 用 answered，confidence 用 high，其余数组为空。"
    );
    let mut next = request(
        SessionRef::Resume(session.expect("会话 ID")),
        &second,
        "second",
    );
    next.schema = serde_json::from_str(include_str!("../../ai-boot/src/schemas/answer.json"))
        .expect("生产契约");
    let mut handle = backend.start_turn(next).await.expect("续接");
    let mut resumed = false;
    tool_ids.clear();
    tool_succeeded = false;
    while let Some(event) = handle.events.recv().await {
        if let AgentEvent::Step(ai_boot_agent::Step::ToolCall {
            ref id, ref tool, ..
        }) = event
            && tool == "mcp__qtmcp__confluence_page"
        {
            tool_ids.push(id.clone());
        }
        if let AgentEvent::Step(ai_boot_agent::Step::ToolDone { ref id, ok, .. }) = event {
            tool_succeeded |= ok && tool_ids.contains(id);
        }
        if let AgentEvent::Finished(outcome) = event {
            assert!(
                matches!(&outcome, Outcome::Success { structured: Some(value), .. } if value["summary"] == "ai-boot-smoke-7281"),
                "{outcome:?}"
            );
            resumed = true;
        }
    }
    assert!(
        resumed && tool_succeeded,
        "续接也必须实际读取 Confluence 正文，不能被审批阻塞"
    );
}
