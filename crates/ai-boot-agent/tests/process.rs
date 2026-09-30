//! 子进程生命周期：用一个假 CLI 脚本走完 ClaudeBackend 的真实启动路径。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ai_boot_agent::claude::{ClaudeBackend, ClaudeConfig};
use ai_boot_agent::{
    AgentBackend, AgentError, AgentEvent, FailKind, Outcome, SessionRef, TurnHandle, TurnRequest,
};

/// 按 FAKE_MODE 模拟 `claude -p` 的几种行为。
const FAKE_CLI: &str = r#"#!/bin/bash
# 信号处理要在输出 init 之前装好：测试一收到 Started 就可能发 SIGINT
case "$FAKE_MODE" in
  graceful) trap 'touch "$FAKE_MARK"; echo "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"terminal_reason\":\"aborted_streaming\",\"total_cost_usd\":0.002,\"modelUsage\":{}}"; exit 0' INT ;;
  stubborn) trap '' INT ;;
esac
input_bytes=$(cat | wc -c)
echo '{"type":"system","subtype":"init","session_id":"s-fake","model":"fake","tools":["Read"],"mcp_servers":[],"plugins":[],"skills":[],"permissionMode":"default","claude_code_version":"0.0.0"}'
case "$FAKE_MODE" in
  ok)
    printf '{"type":"result","subtype":"success","is_error":false,"num_turns":1,"total_cost_usd":0.001,"modelUsage":{},"result":"done","structured_output":{"prompt_bytes":%d}}\n' "$input_bytes"
    ;;
  graceful)
    sleep 30 &
    wait
    ;;
  stubborn)
    sleep 300 &
    # 先写临时文件再改名：测试一看到文件就会去读，不能读到半截
    echo $! > "$FAKE_PIDFILE.tmp" && mv "$FAKE_PIDFILE.tmp" "$FAKE_PIDFILE"
    wait
    ;;
  crash)
    echo "boom: something broke" >&2
    exit 3
    ;;
esac
"#;

struct Fixture {
    dir: tempfile::TempDir,
    program: PathBuf,
}

/// 假 CLI 全进程只写一次。每个测试各写一份会撞上 ETXTBSY：一个线程正在写
/// 脚本时另一个线程 fork，子进程在 exec 前继承了写句柄，这边再去执行脚本就被
/// 内核拒绝（Text file busy）。写入期间其他测试都等在这把锁上，不会有并发 fork。
fn fake_program() -> PathBuf {
    static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
    PROGRAM
        .get_or_init(|| {
            use std::os::unix::fs::PermissionsExt as _;
            let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
            let staging = dir.join(format!("fake-claude.{}.tmp", std::process::id()));
            // 最终文件名固定：改名是原子的，并发的其他测试进程拿到的要么是旧文件、
            // 要么是新文件，都完整且没有打开的写句柄
            let program = dir.join("fake-claude");
            std::fs::write(&staging, FAKE_CLI).expect("写脚本");
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            std::fs::rename(&staging, &program).expect("改名");
            program
        })
        .clone()
}

impl Fixture {
    fn new() -> Self {
        let program = fake_program();
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::create_dir(dir.path().join("work")).expect("工作目录");
        Self { dir, program }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn backend(&self, mode: &str, grace: Duration) -> ClaudeBackend {
        let env: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("FAKE_MODE".into(), mode.into()),
            ("FAKE_MARK".into(), self.path("interrupted").into()),
            ("FAKE_PIDFILE".into(), self.path("child.pid").into()),
        ];
        ClaudeBackend::new(ClaudeConfig {
            program: self.program.clone(),
            hook_program: PathBuf::from("/bin/true"),
            hook_timeout_secs: 10,
            env,
            stop_grace: grace,
        })
    }

    fn request(&self, prompt: String, timeout: Duration) -> TurnRequest {
        TurnRequest {
            session: SessionRef::New,
            prompt,
            workdir: self.path("work"),
            run_dir: self.path("run"),
            images: Vec::new(),
            rules: String::new(),
            schema: serde_json::json!({"type": "object"}),
            mcp_servers: Vec::new(),
            model: None,
            effort: None,
            timeout,
            budget_usd_micros: None,
        }
    }
}

async fn next(handle: &mut TurnHandle) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), handle.events.recv())
        .await
        .expect("等待事件超时")
        .expect("事件流提前结束")
}

/// 读到终态为止，返回途中的全部事件。
async fn until_finished(handle: &mut TurnHandle) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        let event = next(handle).await;
        let done = event.is_terminal();
        events.push(event);
        if done {
            return events;
        }
    }
}

fn outcome(events: &[AgentEvent]) -> &Outcome {
    match events.last() {
        Some(AgentEvent::Finished(outcome)) => outcome,
        other => panic!("没有终态：{other:?}"),
    }
}

fn process_gone(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // 已被杀、尚未回收的僵尸也算没了
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

async fn wait_for(condition: impl Fn() -> bool) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn a_prompt_larger_than_the_pipe_buffer_reaches_the_cli_through_stdin() {
    let f = Fixture::new();
    let prompt = "x".repeat(200_000);
    let mut handle = f
        .backend("ok", Duration::from_secs(5))
        .start_turn(f.request(prompt, Duration::from_secs(30)))
        .await
        .expect("启动");
    assert!(
        handle.session_id.is_some(),
        "Claude 的会话 ID 由我们预先分配"
    );
    let events = until_finished(&mut handle).await;
    let Outcome::Success { structured, .. } = outcome(&events) else {
        panic!("应当成功：{events:?}");
    };
    assert_eq!(
        structured.as_ref(),
        Some(&serde_json::json!({"prompt_bytes": 200_000}))
    );
}

#[tokio::test]
async fn cancelling_asks_the_cli_to_stop_and_keeps_its_final_usage() {
    let f = Fixture::new();
    let mut handle = f
        .backend("graceful", Duration::from_secs(5))
        .start_turn(f.request("hi".into(), Duration::from_secs(30)))
        .await
        .expect("启动");
    assert!(matches!(next(&mut handle).await, AgentEvent::Started(_)));

    let started = Instant::now();
    handle.cancel.cancel();
    let events = until_finished(&mut handle).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    // CLI 自己报的「执行出错」被我们的「中断」取代，但它给的用量要留下
    assert_eq!(outcome(&events), &Outcome::Interrupted);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Usage(_))));
    assert!(f.path("interrupted").exists(), "CLI 应当收到 SIGINT");
}

#[tokio::test]
async fn a_cli_that_ignores_sigint_is_killed_with_its_whole_process_group() {
    let f = Fixture::new();
    let mut handle = f
        .backend("stubborn", Duration::from_millis(500))
        .start_turn(f.request("hi".into(), Duration::from_secs(30)))
        .await
        .expect("启动");
    assert!(matches!(next(&mut handle).await, AgentEvent::Started(_)));
    let pidfile = f.path("child.pid");
    assert!(wait_for(|| pidfile.exists()).await, "子进程没有启动");
    let child: i32 = std::fs::read_to_string(&pidfile)
        .expect("读 pid")
        .trim()
        .parse()
        .expect("pid");

    handle.cancel.cancel();
    let events = until_finished(&mut handle).await;
    assert_eq!(outcome(&events), &Outcome::Interrupted);
    // CLI 拉起的子进程（真实场景里是 MCP server）也要一起清掉
    assert!(
        wait_for(|| process_gone(child)).await,
        "子进程 {child} 还活着"
    );
}

#[tokio::test]
async fn exceeding_the_wall_clock_limit_is_a_timeout() {
    let f = Fixture::new();
    let mut handle = f
        .backend("graceful", Duration::from_secs(5))
        .start_turn(f.request("hi".into(), Duration::from_millis(300)))
        .await
        .expect("启动");
    let events = until_finished(&mut handle).await;
    assert_eq!(outcome(&events), &Outcome::TimedOut);
}

#[tokio::test]
async fn exiting_without_a_result_is_a_cli_failure_carrying_the_stderr_tail() {
    let f = Fixture::new();
    let mut handle = f
        .backend("crash", Duration::from_secs(5))
        .start_turn(f.request("hi".into(), Duration::from_secs(30)))
        .await
        .expect("启动");
    let events = until_finished(&mut handle).await;
    let Outcome::Failed { kind, reason } = outcome(&events) else {
        panic!("应当失败：{events:?}");
    };
    assert_eq!(*kind, FailKind::Cli);
    assert!(
        reason.contains("退出码 3") && reason.contains("boom"),
        "{reason}"
    );
}

#[tokio::test]
async fn dropping_the_event_receiver_stops_the_cli() {
    let f = Fixture::new();
    let mut handle = f
        .backend("graceful", Duration::from_secs(5))
        .start_turn(f.request("hi".into(), Duration::from_secs(30)))
        .await
        .expect("启动");
    assert!(matches!(next(&mut handle).await, AgentEvent::Started(_)));
    drop(handle);
    let mark = f.path("interrupted");
    assert!(
        wait_for(|| mark.exists()).await,
        "没人收事件时 CLI 应当被叫停"
    );
}

#[tokio::test]
async fn setup_errors_are_reported_before_anything_runs() {
    let f = Fixture::new();
    let mut backend_config = ClaudeConfig {
        program: PathBuf::from("/nonexistent/claude"),
        hook_program: PathBuf::from("/bin/true"),
        hook_timeout_secs: 10,
        env: Vec::new(),
        stop_grace: Duration::from_secs(1),
    };
    let missing = ClaudeBackend::new(backend_config.clone())
        .start_turn(f.request("hi".into(), Duration::from_secs(1)))
        .await;
    assert!(
        matches!(missing, Err(AgentError::NotFound { .. })),
        "{missing:?}"
    );

    backend_config.program = f.program.clone();
    let mut request = f.request("hi".into(), Duration::from_secs(1));
    request.workdir = Path::new("/nonexistent/work").to_path_buf();
    let no_workdir = ClaudeBackend::new(backend_config).start_turn(request).await;
    assert!(
        matches!(no_workdir, Err(AgentError::MissingWorkdir(_))),
        "{no_workdir:?}"
    );
}
