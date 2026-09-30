//! 用真实录制的 stream-json 回放解码链路（样本说明见 `fixtures/claude/README.md`）。
//!
//! 期望值不是从实现里抄的：先用脚本直接从样本里数出来，再写进断言。

use ai_boot_agent::claude::decode::Decoder;
use ai_boot_agent::{Activity, AgentEvent, FailKind, Outcome, Started, Step, Usage};

const RESTRICTED: &str = include_str!("fixtures/claude/restricted-mcp-structured.jsonl");
const HOOK_DENY: &str = include_str!("fixtures/claude/hook-deny.jsonl");
const HOOK_CRASH: &str = include_str!("fixtures/claude/hook-crash.jsonl");
const RESUME: &str = include_str!("fixtures/claude/resume.jsonl");
const SIGINT: &str = include_str!("fixtures/claude/sigint.jsonl");
const BUDGET: &str = include_str!("fixtures/claude/budget-exceeded.jsonl");
const RESUME_MISSING: &str = include_str!("fixtures/claude/resume-missing.jsonl");
const PARTIAL: &str = include_str!("fixtures/claude/partial-messages.jsonl");

const ALL: [(&str, &str); 8] = [
    ("restricted", RESTRICTED),
    ("hook-deny", HOOK_DENY),
    ("hook-crash", HOOK_CRASH),
    ("resume", RESUME),
    ("sigint", SIGINT),
    ("budget", BUDGET),
    ("resume-missing", RESUME_MISSING),
    ("partial", PARTIAL),
];

fn replay(jsonl: &str) -> Vec<AgentEvent> {
    let mut decoder = Decoder::new();
    jsonl.lines().flat_map(|line| decoder.line(line)).collect()
}

fn started(events: &[AgentEvent]) -> &Started {
    match events.first() {
        Some(AgentEvent::Started(started)) => started,
        other => panic!("第一条必须是 Started：{other:?}"),
    }
}

fn steps(events: &[AgentEvent]) -> impl Iterator<Item = &Step> {
    events.iter().filter_map(|e| match e {
        AgentEvent::Step(step) => Some(step),
        _ => None,
    })
}

fn tool_calls(events: &[AgentEvent]) -> Vec<&str> {
    steps(events)
        .filter_map(|s| match s {
            Step::ToolCall { tool, .. } => Some(tool.as_str()),
            _ => None,
        })
        .collect()
}

fn denials(events: &[AgentEvent]) -> Vec<(&str, &str)> {
    steps(events)
        .filter_map(|s| match s {
            Step::Denied { tool, reason, .. } => Some((tool.as_str(), reason.as_str())),
            _ => None,
        })
        .collect()
}

fn usage(events: &[AgentEvent]) -> Usage {
    let all: Vec<Usage> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .collect();
    assert_eq!(all.len(), 1, "用量只信 result，一轮只该有一条");
    all[0]
}

fn outcome(events: &[AgentEvent]) -> &Outcome {
    match events.last() {
        Some(AgentEvent::Finished(outcome)) => outcome,
        other => panic!("最后一条必须是终态：{other:?}"),
    }
}

#[test]
fn the_isolated_session_reports_exactly_the_granted_tool_surface() {
    let events = replay(RESTRICTED);
    let started = started(&events);
    assert_eq!(started.cli_version.as_deref(), Some("2.1.283"));
    // 用户设置里的 bypassPermissions 被 --restricted 忽略了
    assert_eq!(started.permission_mode.as_deref(), Some("default"));
    let tools = started.tools.as_deref().expect("工具清单");
    assert_eq!(tools.len(), 9);
    for tool in [
        "Read",
        "Glob",
        "Grep",
        "StructuredOutput",
        "mcp__qtmcp__gitlab_project",
    ] {
        assert!(tools.iter().any(|t| t == tool), "缺少 {tool}：{tools:?}");
    }
    assert!(!tools.iter().any(|t| t == "Bash" || t == "WebFetch"));
    // 内置插件在 --restricted 下仍然存在；个人技能一个都没加载
    assert_eq!(
        started.plugins.as_deref(),
        Some(
            &[
                "agents-md@builtin".to_owned(),
                "telemetry@builtin".to_owned()
            ][..]
        )
    );
    assert_eq!(started.skills.as_deref(), Some(&[][..]));
    let mcp = started.mcp_servers.as_deref().expect("MCP 状态");
    assert_eq!(mcp.len(), 1);
    assert_eq!(
        (mcp[0].name.as_str(), mcp[0].status.as_str()),
        ("qtmcp", "connected")
    );
}

#[test]
fn restricted_mode_blocks_reads_outside_the_workdir() {
    let events = replay(RESTRICTED);
    assert_eq!(
        tool_calls(&events),
        [
            "Read",
            "Read",
            "mcp__qtmcp__gitlab_project",
            "mcp__qtmcp__gitlab_project",
            "StructuredOutput"
        ]
    );
    let denied = denials(&events);
    assert_eq!(denied.len(), 1);
    assert_eq!(denied[0].0, "Read");
    assert!(denied[0].1.contains("--restricted"), "{}", denied[0].1);
}

#[test]
fn the_structured_answer_comes_from_structured_output() {
    let events = replay(RESTRICTED);
    let Outcome::Success {
        structured, turns, ..
    } = outcome(&events)
    else {
        panic!("应当成功：{:?}", outcome(&events));
    };
    let structured = structured.as_ref().expect("结构化结果");
    assert!(structured["summary"].as_str().is_some());
    assert_eq!(structured["steps"].as_array().map(Vec::len), Some(5));
    assert_eq!(*turns, 6);

    let usage = usage(&events);
    assert_eq!(usage.cost_usd_micros, Some(33_927));
    assert_eq!(usage.output_tokens, 1_676);
}

#[test]
fn both_hook_denial_paths_surface_as_denied_steps() {
    let events = replay(HOOK_DENY);
    let denied = denials(&events);
    assert_eq!(denied.len(), 2, "{denied:?}");
    // PermissionRequest 拒绝：有 permission_denied 事件
    assert_eq!(denied[0].0, "mcp__qtmcp__gitlab_project");
    assert!(denied[0].1.contains("branches"));
    // PreToolUse 拒绝：只能从错误的工具结果里认出来
    assert_eq!(denied[1].0, "mcp__qtmcp__gitlab_project");
    assert_eq!(denied[1].1, "probe pre-denies get");
}

#[test]
fn a_crashed_hook_leaves_the_calls_denied() {
    let events = replay(HOOK_CRASH);
    let denied = denials(&events);
    assert_eq!(denied.len(), 2);
    for (_, reason) in denied {
        assert!(reason.contains("no approval surface"), "{reason}");
    }
    assert!(matches!(outcome(&events), Outcome::Success { .. }));
}

#[test]
fn a_resumed_session_keeps_its_id_and_reports_cumulative_usage() {
    let first = replay(RESTRICTED);
    let resumed = replay(RESUME);
    assert_eq!(started(&resumed).session_id, started(&first).session_id);

    let Outcome::Success { structured, .. } = outcome(&resumed) else {
        panic!("应当成功");
    };
    assert_eq!(
        structured.as_ref(),
        Some(&serde_json::json!({"passphrase": "青藤-7391", "first_turn_steps": 5}))
    );
    // 会话累计值：比第一轮大，每轮用量要自己做差
    let (u1, u2) = (usage(&first), usage(&resumed));
    assert_eq!(u2.cost_usd_micros, Some(61_887));
    assert_eq!(u2.output_tokens, 2_029);
    assert!(u2.output_tokens > u1.output_tokens);
}

#[test]
fn resuming_a_missing_session_is_its_own_failure_kind() {
    // 编排层靠它决定改为新开会话，不能混进 Other
    let events = replay(RESUME_MISSING);
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Started(_))),
        "会话不存在时 CLI 不发 init"
    );
    let Outcome::Failed { kind, reason } = outcome(&events) else {
        panic!("应当失败：{events:?}");
    };
    assert_eq!(*kind, FailKind::SessionNotFound);
    assert!(reason.contains("No conversation found"), "{reason}");
}

#[test]
fn sigint_ends_the_turn_as_interrupted() {
    assert_eq!(outcome(&replay(SIGINT)), &Outcome::Interrupted);
}

#[test]
fn budget_exhaustion_is_its_own_outcome_and_still_reports_spend() {
    let events = replay(BUDGET);
    assert_eq!(outcome(&events), &Outcome::BudgetExceeded);
    let usage = usage(&events);
    assert_eq!(usage.cost_usd_micros, Some(3_750));
    assert_eq!(usage.output_tokens, 175);
}

/// 出现 Warning 说明 CLI 吐了防腐层不认识的东西——升级后第一个要看的信号。
#[test]
fn current_cli_output_is_fully_understood() {
    for (name, jsonl) in ALL {
        let warnings: Vec<_> = replay(jsonl)
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::Warning(message) => Some(message),
                _ => None,
            })
            .collect();
        assert!(
            warnings.is_empty(),
            "{name} 里有看不懂的事件：{warnings:#?}"
        );
    }
}

#[test]
fn exactly_one_terminal_event_and_it_is_last() {
    for (name, jsonl) in ALL {
        let events = replay(jsonl);
        assert_eq!(
            events.iter().filter(|e| e.is_terminal()).count(),
            1,
            "{name}"
        );
        assert!(events.last().is_some_and(AgentEvent::is_terminal), "{name}");
    }
}

#[test]
fn every_tool_call_gets_a_result_unless_the_turn_was_cut_short() {
    // 预算熔断和 SIGINT 会在工具返回前切断执行，这两份不适用
    for (name, jsonl) in ALL
        .iter()
        .filter(|(n, _)| !matches!(*n, "sigint" | "budget"))
    {
        let events = replay(jsonl);
        let done: Vec<&str> = steps(&events)
            .filter_map(|s| match s {
                Step::ToolDone { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        for step in steps(&events) {
            if let Step::ToolCall { id, .. } = step {
                assert!(done.contains(&id.as_str()), "{name}：调用 {id} 没有结果");
            }
        }
    }
}

/// 增量事件只用来看阶段：先思考，再调工具、说话，最后写结构化答案；
/// 工具调用和答案本身仍然以完整的 assistant / result 事件为准。
#[test]
fn partial_messages_reveal_what_the_model_is_doing() {
    let events = replay(PARTIAL);
    let phases: Vec<Activity> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Activity(a) if *a != Activity::Streaming => Some(*a),
            _ => None,
        })
        .fold(Vec::new(), |mut phases, a| {
            if phases.last() != Some(&a) {
                phases.push(a);
            }
            phases
        });
    assert_eq!(phases.first(), Some(&Activity::Thinking), "{phases:?}");
    assert!(phases.contains(&Activity::Writing), "{phases:?}");
    assert_eq!(phases.last(), Some(&Activity::Concluding), "{phases:?}");
    let reads = steps(&events)
        .filter(|s| matches!(s, Step::ToolCall { tool, .. } if tool == "Read"))
        .count();
    assert_eq!(reads, 1, "增量事件不能让工具调用重复计数");
    assert!(matches!(
        outcome(&events),
        Outcome::Success {
            structured: Some(_),
            ..
        }
    ));
}
