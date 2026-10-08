//! 真实 claude + 真实 `ai-boot hook` 的全链路检查。
//!
//! 默认跳过（会消耗订阅额度，也依赖本机的登录态和 qtmcp）。手动运行：
//! `AI_BOOT_LIVE_CLAUDE=1 cargo test -p ai-boot --test live_claude -- --nocapture`
//!
//! 只调用 GitLab 的只读 action：GitLab 走 token，不会触发 SSO 登录。执行命令和上网的
//! 检查要能访问 GitHub。

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

fn backend() -> ClaudeBackend {
    let env: Vec<(OsString, OsString)> = ["HOME", "PATH", "LANG"]
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|v| (OsString::from(key), v)))
        .collect();
    ClaudeBackend::new(ClaudeConfig {
        program: home().join(".local/bin/claude"),
        hook_program: PathBuf::from(env!("CARGO_BIN_EXE_ai-boot")),
        hook_timeout_secs: 10,
        env,
        stop_grace: Duration::from_secs(10),
    })
}

fn probe_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object", "additionalProperties": false, "required": ["summary", "steps"],
        "properties": {"summary": {"type": "string"}, "steps": {"type": "array", "items": {"type": "string"}}}
    })
}

/// 跑完一轮，返回全部事件和启动时就确定的会话 ID。
async fn run(request: TurnRequest) -> (Vec<AgentEvent>, Option<String>) {
    let mut handle = backend().start_turn(request).await.expect("启动");
    let mut events = Vec::new();
    while let Some(event) = handle.events.recv().await {
        eprintln!("{event:?}");
        let done = event.is_terminal();
        events.push(event);
        if done {
            break;
        }
    }
    (events, handle.session_id)
}

/// 某个工具至少被调用过一次，并且至少有一次成功。
fn succeeded(events: &[AgentEvent], tool: &str) -> bool {
    let ids: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Step(Step::ToolCall { id, tool: t, .. }) if t == tool => Some(id.as_str()),
            _ => None,
        })
        .collect();
    events.iter().any(|e| {
        matches!(e, AgentEvent::Step(Step::ToolDone { id, ok: true, .. }) if ids.contains(&id.as_str()))
    })
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
        schema: probe_schema(),
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

    let (events, session_id) = run(request).await;

    let started = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Started(s) => Some(s),
            _ => None,
        })
        .expect("Started");
    assert_eq!(started.session_id, session_id.unwrap_or_default());
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

/// 执行命令和上网真的能用：clone 下来的仓库落在工作目录里、文件工具读得到，网页和搜索
/// 都有结果，没有一次调用被权限层或 hook 拦下。
#[tokio::test]
async fn commands_and_the_web_work_end_to_end() {
    if !enabled() {
        eprintln!("跳过：设置 AI_BOOT_LIVE_CLAUDE=1 才运行");
        return;
    }
    let root = tempfile::tempdir().expect("临时目录");
    let workdir = root.path().join("work");
    std::fs::create_dir(&workdir).expect("工作目录");
    let request = TurnRequest {
        session: SessionRef::New,
        prompt: "按顺序真实调用工具，每一步单独调用，不要跳过：\
                 1. 用 Bash 执行 git clone --depth 1 https://github.com/octocat/Hello-World repos/hello；\
                 2. 用 Read 读取 repos/hello/README；\
                 3. 用 WebFetch 打开 https://example.com，说出页面标题；\
                 4. 用 WebSearch 搜索 octocat Hello-World。\
                 最后按 JSON 输出：summary 一句话，steps 逐条写每一步的结果。"
            .into(),
        workdir: workdir.clone(),
        run_dir: root.path().join("run"),
        images: Vec::new(),
        rules: "你是全链路检查探针，按用户要求逐步调用工具。".into(),
        schema: probe_schema(),
        mcp_servers: Vec::new(),
        model: Some("haiku".into()),
        effort: None,
        timeout: Duration::from_secs(240),
        budget_usd_micros: Some(500_000),
    };

    let (events, _) = run(request).await;

    for tool in ["Bash", "Read", "WebFetch", "WebSearch"] {
        assert!(succeeded(&events, tool), "{tool} 没有成功执行");
    }
    let denied: Vec<&Step> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Step(step @ Step::Denied { .. }) => Some(step),
            _ => None,
        })
        .collect();
    assert!(denied.is_empty(), "有调用被拦下：{denied:?}");
    assert!(
        workdir.join("repos/hello/README").is_file(),
        "仓库没有 clone 到工作目录里"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Finished(Outcome::Success {
            structured: Some(_),
            ..
        }))
    ));
}

/// 本轮的临时目录（`$TMPDIR`）在工作目录外：模型把附件、源码下载到那里之后要能直接
/// Read，不用再拷进工作目录（线上出现过被 hook 拦下、绕道拷贝）。
#[tokio::test]
async fn files_in_the_turn_tmpdir_are_readable() {
    if !enabled() {
        eprintln!("跳过：设置 AI_BOOT_LIVE_CLAUDE=1 才运行");
        return;
    }
    let root = tempfile::tempdir().expect("临时目录");
    let workdir = root.path().join("work");
    std::fs::create_dir(&workdir).expect("工作目录");
    let request = TurnRequest {
        session: SessionRef::New,
        prompt: "按顺序真实调用工具，每一步单独调用，不要跳过：\
                 1. 用 Bash 执行 echo tmp-check > \"$TMPDIR/a.txt\" && echo \"$TMPDIR/a.txt\"；\
                 2. 用 Read 读取上一步输出的那个绝对路径。\
                 最后按 JSON 输出：summary 一句话，steps 逐条写每一步的结果。"
            .into(),
        workdir,
        run_dir: root.path().join("run"),
        images: Vec::new(),
        rules: "你是全链路检查探针，按用户要求逐步调用工具。".into(),
        schema: probe_schema(),
        mcp_servers: Vec::new(),
        model: Some("haiku".into()),
        effort: None,
        timeout: Duration::from_secs(240),
        budget_usd_micros: Some(500_000),
    };

    let (events, _) = run(request).await;

    assert!(succeeded(&events, "Bash"), "Bash 没有成功执行");
    assert!(succeeded(&events, "Read"), "Read 没有读到 $TMPDIR 里的文件");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Step(Step::Denied { .. }))),
        "有调用被拦下"
    );
    assert!(matches!(
        events.last(),
        Some(AgentEvent::Finished(Outcome::Success {
            structured: Some(_),
            ..
        }))
    ));
}
