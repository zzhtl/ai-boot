//! 真实 Codex：本机能力、实时搜索、网页抓取、shell 和 OTEL 的端到端验证。

use std::path::PathBuf;
use std::time::Duration;

use ai_boot_agent::codex::{CodexBackend, CodexConfig};
use ai_boot_agent::{AgentBackend, AgentEvent, Effort, Outcome, SessionRef, Step, TurnRequest};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
#[ignore = "调用本机 Codex；使用临时配置、模拟 skill/MCP 和 OTLP 接收端"]
async fn profile_keeps_local_capabilities_and_exports_binary_logs() {
    let collector = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/logs"))
        .respond_with(
            ResponseTemplate::new(200).insert_header("Content-Type", "application/x-protobuf"),
        )
        .mount(&collector)
        .await;
    let root = tempfile::tempdir().expect("测试目录");
    let home = root.path().join("codex");
    let work = root.path().join("work");
    let skill = home.join("skills/ai-boot-local-probe");
    std::fs::create_dir_all(&skill).expect("skill 目录");
    std::fs::create_dir(&work).expect("工作目录");
    let login_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".codex"));
    let auth = login_home.join("auth.json");
    if auth.is_file() {
        std::os::unix::fs::symlink(auth, home.join("auth.json")).expect("复用登录");
    }
    std::fs::write(
        home.join("AGENTS.md"),
        "回复 JSON 的 agents 字段必须填写 local-agents-ready。\n",
    )
    .expect("全局 AGENTS");
    std::fs::write(skill.join("SKILL.md"), "---\nname: ai-boot-local-probe\ndescription: 验证 ai-boot 的本机技能加载，用户明确提到该技能时使用。\n---\n回复 JSON 的 skill 字段填写 local-skill-ready。\n")
        .expect("本机 skill");
    let mcp = root.path().join("mcp.py");
    std::fs::write(&mcp, r#"import json,sys
for line in sys.stdin:
    r=json.loads(line)
    if 'id' not in r:
        continue
    method=r.get('method')
    if method=='initialize':
        result={'protocolVersion':r['params']['protocolVersion'],'capabilities':{'tools':{}},'serverInfo':{'name':'probe','version':'1'}}
    elif method=='tools/list':
        result={'tools':[{'name':'get_marker','description':'返回本机配置加载验证标记','inputSchema':{'type':'object','properties':{},'additionalProperties':False},'annotations':{'readOnlyHint':True}}]}
    elif method=='tools/call':
        result={'content':[{'type':'text','text':'local-mcp-ready'}],'isError':False}
    else:
        result={}
    print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':result}),flush=True)
"#).expect("模拟 MCP");
    let mut config = toml::Table::new();
    // 验证终端已有的执行能力，不引入与实际服务不同的额外沙箱。
    let local_config = std::fs::read_to_string(login_home.join("config.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok());
    let sandbox = local_config
        .as_ref()
        .and_then(|config| config.get("sandbox_mode"))
        .cloned()
        .unwrap_or_else(|| "workspace-write".into());
    println!("验证使用本机 sandbox_mode={sandbox}");
    config.insert("sandbox_mode".into(), sandbox);
    config.insert("web_search".into(), "disabled".into());
    config.insert(
        "sandbox_workspace_write".into(),
        toml::Value::try_from(json!({"network_access":true})).expect("网络配置"),
    );
    config.insert("otel".into(), toml::Value::try_from(json!({
        "environment": "ai-boot-test", "log_user_prompt": false,
        "exporter": {"otlp-http": {"endpoint": format!("{}/v1/logs", collector.uri()), "protocol": "binary"}}
    })).expect("OTEL TOML"));
    config.insert("mcp_servers".into(), toml::Value::try_from(json!({
        "probe": {"command": "/usr/bin/python3", "args": [mcp], "default_tools_approval_mode": "approve"}
    })).expect("MCP TOML"));
    std::fs::write(
        home.join("config.toml"),
        toml::to_string(&config).expect("配置 TOML"),
    )
    .expect("本机配置");
    std::fs::write(
        home.join("ai-boot.config.toml"),
        "model_reasoning_effort = 'xhigh'\nweb_search = 'live'\n",
    )
    .expect("profile");
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
            .expect("Codex 绝对路径")
            .into(),
        profile: Some("ai-boot".into()),
        env,
        stop_grace: Duration::from_secs(5),
    });
    let mut handle = backend.start_turn(TurnRequest {
        session: SessionRef::New,
        prompt: "使用 $ai-boot-local-probe 技能，调用 probe 的 get_marker 工具。再实际使用原生 Web 搜索查找 RFC 2606 的 RFC Editor 官方页面，打开正文核实来源。用 shell 的 curl 下载 https://www.rfc-editor.org/rfc/rfc2606.html 到当前目录 out/rfc2606.html，并通过 shell 计算下载文件的 SHA-256。按 schema 返回：skill 来自技能，agents 来自全局 AGENTS.md，mcp 来自工具实际返回值，source_url 为搜索到的官网地址，page_sha256 为文件的 SHA-256 小写十六进制值。不要猜测结果；只在当前目录创建本次验证文件。".into(),
        workdir: work.clone(),
        run_dir: root.path().join("run"),
        images: Vec::new(),
        rules: "这是配置和工具能力验证任务，允许查询公开网页、调用验证工具及在工作目录创建验证文件。".into(),
        schema: json!({"type":"object", "properties":{"skill":{"type":"string"},"agents":{"type":"string"},"mcp":{"type":"string"},"source_url":{"type":"string"},"page_sha256":{"type":"string"}},"required":["skill","agents","mcp","source_url","page_sha256"],"additionalProperties":false}),
        mcp_servers: Vec::new(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some(Effort::Xhigh),
        timeout: Duration::from_secs(300),
        budget_usd_micros: None,
    }).await.expect("启动");
    let mut result = None;
    let mut tool_ids = Vec::new();
    let mut tool_ok = false;
    let mut web_ids = Vec::new();
    let mut shell_ids = Vec::new();
    let mut web_ok = false;
    let mut shell_ok = false;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Step(Step::ToolCall { id, tool, .. })
                if tool == "mcp__probe__get_marker" =>
            {
                tool_ids.push(id)
            }
            AgentEvent::Step(Step::ToolCall { id, tool, .. }) if tool == "WebSearch" => {
                web_ids.push(id)
            }
            AgentEvent::Step(Step::ToolCall { id, tool, .. }) if tool == "Bash" => {
                shell_ids.push(id)
            }
            AgentEvent::Step(Step::ToolDone { id, ok, .. }) => {
                tool_ok |= ok && tool_ids.contains(&id);
                web_ok |= ok && web_ids.contains(&id);
                shell_ok |= ok && shell_ids.contains(&id);
            }
            AgentEvent::Finished(Outcome::Success { structured, .. }) => result = structured,
            AgentEvent::Finished(outcome) => panic!("{outcome:?}"),
            _ => {}
        }
    }
    let result = result.expect("结构化验证结果");
    assert_eq!(result["skill"], "local-skill-ready");
    assert_eq!(result["agents"], "local-agents-ready");
    assert_eq!(result["mcp"], "local-mcp-ready");
    assert!(tool_ok, "必须实际调用本机配置的 MCP");
    assert!(web_ok, "必须实际使用原生 Web 搜索");
    assert!(
        shell_ok,
        "必须实际执行 shell 命令；检测到的命令 ID：{shell_ids:?}；结果：{result}"
    );
    let page = std::fs::read(work.join("out/rfc2606.html")).expect("shell 下载的网页文件");
    assert!(String::from_utf8_lossy(&page).contains("Reserved Top Level DNS Names"));
    let hash: String = Sha256::digest(&page)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(result["page_sha256"], hash);
    assert!(
        result["source_url"]
            .as_str()
            .is_some_and(|url| url.contains("rfc-editor.org"))
    );
    println!(
        "原生 Web 搜索成功，shell 下载 {} 字节网页，SHA-256 校验一致",
        page.len()
    );
    let requests = collector.received_requests().await.expect("OTLP 请求");
    assert!(!requests.is_empty(), "本机 OTEL 配置未生效");
    let mut payloads = Vec::new();
    for request in &requests {
        assert_eq!(
            request
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/x-protobuf")
        );
        assert!(!request.body.is_empty());
        payloads.extend_from_slice(&request.body);
        if let Ok(endpoint) = std::env::var("AI_BOOT_OTEL_FORWARD_ENDPOINT") {
            let response = reqwest::Client::new()
                .post(&endpoint)
                .header("Content-Type", "application/x-protobuf")
                .body(request.body.clone())
                .send()
                .await
                .expect("转发真实 OTLP 包");
            assert!(
                response.status().is_success(),
                "采集端返回 {}",
                response.status()
            );
            println!(
                "OTLP binary: {} bytes -> {}",
                request.body.len(),
                response.status()
            );
        }
    }
    let text = String::from_utf8_lossy(&payloads);
    assert!(text.contains("codex.conversation_starts"));
    assert!(text.contains("gpt-6.1-sol"));
    assert!(text.contains("xhigh"));
    println!(
        "本机 AGENTS、skill、MCP 已加载，收到 {} 批二进制 OTLP 日志",
        requests.len()
    );
}
