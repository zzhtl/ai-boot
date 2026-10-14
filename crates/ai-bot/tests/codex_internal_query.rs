//! 原生 Codex：内网足够时直接回答；有公共证据缺口时才查 Web，保留准确性。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ai_bot_agent::codex::{CodexBackend, CodexConfig};
use ai_bot_agent::{AgentBackend, AgentEvent, Effort, Outcome, SessionRef, Step, TurnRequest};
use pulldown_cmark::{Event, Parser};
use serde_json::{Value, json};

fn summary_visible_chars(summary: &str) -> usize {
    Parser::new(summary)
        .map(|event| match event {
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                text.chars().count()
            }
            Event::SoftBreak | Event::HardBreak => 1,
            _ => 0,
        })
        .sum()
}

#[test]
fn linked_summary_counts_visible_text_and_preserves_the_limit() {
    let summary = format!(
        "**{}** [依据](https://example.test/{})",
        "字".repeat(237),
        "long-url/".repeat(100)
    );
    assert!(summary.chars().count() > 240);
    assert_eq!(summary_visible_chars(&summary), 240);
    assert!(summary_visible_chars(&"字".repeat(241)) > 240);
}

#[test]
fn bare_urls_and_inline_code_still_count_as_visible_text() {
    let url = format!("https://example.test/{}", "a".repeat(300));
    assert_eq!(summary_visible_chars(&url), url.chars().count());
    assert_eq!(
        summary_visible_chars("等待 `120 秒`。"),
        "等待 120 秒。".chars().count()
    );
}

async fn query(
    backend: &CodexBackend,
    root: &Path,
    seq: u32,
    question: &str,
) -> (Value, Vec<String>, Duration) {
    let started = Instant::now();
    let mut handle = backend.start_turn(TurnRequest {
        session: SessionRef::New,
        prompt: question.into(),
        workdir: root.join("work"),
        run_dir: root.join(format!("run-{seq}")),
        images: Vec::new(),
        rules: "使用 $qt_program 处理本轮提问，按提供的 JSON Schema 返回飞书结果。相关补充资料按需读取。".into(),
        schema: serde_json::from_str(include_str!("../src/schemas/answer.json")).expect("生产 Schema"),
        mcp_servers: Vec::new(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some(Effort::Xhigh),
        timeout: Duration::from_secs(300),
        budget_usd_micros: None,
    }).await.expect("启动 Codex");
    let mut answer = None;
    let mut tools = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Step(Step::ToolCall { tool, input, .. }) => {
                if tool == "WebSearch" {
                    assert!(
                        !input.to_string().contains("INTERNAL-ONLY-739"),
                        "外网查询不能携带内网标识"
                    );
                    let calls = std::fs::read_to_string(root.join("calls.jsonl"))
                        .expect("外网前已有内网查询");
                    assert!(
                        calls.lines().any(|line| {
                            let call: Value = serde_json::from_str(line).expect("查询记录");
                            call["tool"] == "confluence_page" && call["args"]["page_id"] == "202"
                        }),
                        "应先读内网正文确认公共证据缺口"
                    );
                }
                tools.push(tool);
            }
            AgentEvent::Finished(Outcome::Success { structured, .. }) => answer = structured,
            AgentEvent::Finished(outcome) => panic!("{outcome:?}"),
            _ => {}
        }
    }
    let answer = answer.expect("结构化答案");
    let elapsed = started.elapsed();
    println!(
        "第 {seq} 轮：{:.1}s；{} 次工具调用；{}",
        elapsed.as_secs_f64(),
        tools.len(),
        answer["summary"]
    );
    let summary = answer["summary"].as_str().expect("概述");
    assert!(
        summary_visible_chars(summary) <= 240,
        "首屏正文过长：{summary}"
    );
    (answer, tools, elapsed)
}

#[tokio::test]
#[ignore = "调用本机 Codex 与公开 Web；内网使用隔离 MCP，验证查询优先级与必要证据"]
async fn internal_evidence_stops_extra_search_and_public_gaps_use_web() {
    let root = tempfile::tempdir().expect("隔离目录");
    let root = root.path();
    let home = root.join("codex");
    std::fs::create_dir_all(home.join("skills")).expect("技能目录");
    std::fs::create_dir(root.join("work")).expect("工作目录");
    let login_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".codex"));
    if login_home.join("auth.json").is_file() {
        std::os::unix::fs::symlink(login_home.join("auth.json"), home.join("auth.json"))
            .expect("复用本机登录");
    }
    std::os::unix::fs::symlink(
        login_home.join("skills/qt_program/program"),
        home.join("skills/qt_program"),
    )
    .expect("使用真实 qt_program");
    let local: toml::Value =
        toml::from_str(&std::fs::read_to_string(login_home.join("config.toml")).expect("本机配置"))
            .expect("本机 TOML");
    let sandbox = local
        .get("sandbox_mode")
        .cloned()
        .unwrap_or_else(|| "workspace-write".into());
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/internal_query_mcp.py");
    let config = toml::Value::try_from(json!({
        "sandbox_mode": sandbox, "web_search": "live",
        "mcp_servers": {"qtmcp": {"command": "/usr/bin/python3", "args": [fixture, root.join("calls.jsonl")], "default_tools_approval_mode": "approve"}}
    })).expect("隔离配置");
    std::fs::write(
        home.join("config.toml"),
        toml::to_string(&config).expect("TOML"),
    )
    .expect("保存配置");
    let mut env: Vec<_> = [
        "HOME",
        "PATH",
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
        program: std::env::var_os("AI_BOT_CODEX_BIN")
            .expect("Codex 绝对路径")
            .into(),
        profile: None,
        env,
        stop_grace: Duration::from_secs(5),
    });
    let (answer, tools, _) = query(
        &backend,
        root,
        1,
        "zzhtl，查下 3.x K3s 的 Kafka 消息堆积怎么查？",
    )
    .await;
    assert_eq!(answer["status"], "answered");
    assert!(
        !tools.iter().any(|tool| tool == "WebSearch"),
        "内网证据足够时不应查外网"
    );
    let calls: Vec<Value> = std::fs::read_to_string(root.join("calls.jsonl"))
        .expect("内网查询记录")
        .lines()
        .map(|line| serde_json::from_str(line).expect("查询 JSON"))
        .collect();
    assert!(
        calls.iter().any(|call| call["tool"] == "confluence_page"),
        "不能只凭搜索摘要回答"
    );
    assert!(
        calls.len() <= 4,
        "简单且完整的内网证据不应反复拉取：{calls:?}"
    );
    for call in &calls {
        if call["tool"] == "confluence_search" {
            assert!(
                call["args"]["limit"]
                    .as_u64()
                    .is_some_and(|limit| limit <= 8),
                "首轮应收敛搜索结果"
            );
        }
    }
    assert_eq!(answer["commands"].as_array().expect("命令").len(), 1);
    let command = answer["commands"][0]["command"].as_str().expect("完整命令");
    for part in [
        "kubectl exec -n qtsa kafka-0 --",
        "/usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh",
        "--bootstrap-server kafka-svc:9092",
        "--command-config /usr/local/qingteng/kafka/config/consumer.properties",
        "--describe",
        "--all-groups",
    ] {
        assert!(command.contains(part), "缺少已查证参数 {part}：{command}");
    }
    assert!(
        !command.contains("kafka-old"),
        "不能采用搜索摘要中的过期参数"
    );
    let (public, tools, _) = query(&backend, root, 2, "zzhtl，内网网关事件 INTERNAL-ONLY-739 返回 HTTP 429，响应头是 Retry-After: 120。这两个值的标准含义是什么，应等多久？仅解释含义，不修改现场。").await;
    assert_eq!(public["status"], "answered");
    assert!(
        tools.iter().any(|tool| tool == "WebSearch"),
        "内网缺少必要的公共规范时应补查外网"
    );
    let text = public.to_string().replace("**", "");
    assert!(
        text.contains("120 秒")
            || text.contains("120秒")
            || text.contains("2 分钟")
            || text.contains("两分钟"),
        "重试等待时间不正确：{public}"
    );
    assert!(public["commands"].as_array().expect("命令").is_empty());
}
