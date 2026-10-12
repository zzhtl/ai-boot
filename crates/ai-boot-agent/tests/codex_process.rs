use ai_boot_agent::codex::{CodexBackend, CodexConfig};
use ai_boot_agent::{AgentBackend, AgentEvent, Effort, FailKind, Outcome, SessionRef, TurnRequest};
use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

#[tokio::test]
async fn high_effort_overrides_personal_settings_for_explicit_and_default_turns() {
    let root = tempfile::tempdir().expect("临时目录");
    let home = root.path().join(".codex");
    std::fs::create_dir(&home).expect("Codex 配置目录");
    for name in ["config.toml", "ai-boot.config.toml"] {
        std::fs::write(
            home.join(name),
            "model = 'personal-model'\nmodel_reasoning_effort = 'max'\n",
        )
        .expect("个人配置");
    }
    let program = root.path().join("codex");
    std::fs::write(&program, r#"#!/bin/sh
printf '%s\n' "$@" > "$AI_BOOT_CAPTURE"
printf '%s' "$CODEX_HOME" > "$AI_BOOT_CAPTURE.home"
printf '%s\n' '{"type":"thread.started","thread_id":"test-thread"}' '{"type":"turn.completed","usage":{}}'
"#).expect("模拟 CLI");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("权限");
    let captured = root.path().join("args");
    let work = root.path().join("work");
    std::fs::create_dir(&work).expect("工作目录");
    let backend = CodexBackend::new(CodexConfig {
        program,
        profile: Some("ai-boot".into()),
        env: vec![
            ("CODEX_HOME".into(), home.into_os_string()),
            ("AI_BOOT_CAPTURE".into(), captured.as_os_str().into()),
        ],
        stop_grace: Duration::from_secs(1),
    });
    for (run, session, effort) in [
        ("first", SessionRef::New, Some(Effort::High)),
        ("next", SessionRef::Resume("test-thread".into()), Some(Effort::High)),
        ("retry", SessionRef::Resume("test-thread".into()), Some(Effort::High)),
        ("switch-first", SessionRef::New, Some(Effort::Xhigh)),
        (
            "switch-next",
            SessionRef::Resume("test-thread".into()),
            Some(Effort::Xhigh),
        ),
        (
            "switch-retry",
            SessionRef::Resume("test-thread".into()),
            Some(Effort::Xhigh),
        ),
        ("default-first", SessionRef::New, None),
        ("default-next", SessionRef::Resume("test-thread".into()), None),
        ("default-retry", SessionRef::Resume("test-thread".into()), None),
    ] {
        let model = if run.starts_with("switch") {
            "complex-model"
        } else {
            "gpt-6.1-sol"
        };
        let mut handle = backend
            .start_turn(TurnRequest {
                session,
                prompt: "question".into(),
                workdir: work.clone(),
                run_dir: root.path().join(run),
                images: Vec::new(),
                rules: String::new(),
                schema: serde_json::json!({}),
                mcp_servers: Vec::new(),
                model: Some(model.into()),
                effort,
                timeout: Duration::from_secs(5),
                budget_usd_micros: None,
            })
            .await
            .expect("启动");
        let mut success = false;
        while let Some(event) = handle.events.recv().await {
            if let AgentEvent::Finished(outcome) = event {
                assert!(matches!(outcome, Outcome::Success { .. }), "{outcome:?}");
                success = true;
            }
        }
        assert!(success);
        assert_eq!(
            std::fs::read_to_string(root.path().join("args.home")).expect("CODEX_HOME"),
            root.path().join(".codex").to_string_lossy()
        );
        let args = std::fs::read_to_string(&captured).expect("CLI 参数");
        let args: Vec<&str> = args.lines().collect();
        assert!(!args.contains(&"--ignore-user-config"));
        assert!(!args.contains(&"--ignore-rules"));
        for override_key in ["mcp_servers=", "sandbox_mode=", "features.", "web_search="] {
            assert!(!args.iter().any(|arg| arg.starts_with(override_key)));
        }
        assert_eq!(
            args.windows(2)
                .filter(|pair| pair[0] == "-m")
                .collect::<Vec<_>>(),
            vec![&["-m", model][..]]
        );
        let expected = format!(
            "model_reasoning_effort=\"{}\"",
            effort.unwrap_or(Effort::High).as_str()
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-c", expected.as_str()])
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("personal-model") || arg.contains("\"max\""))
        );
        assert_eq!(args.contains(&"resume"), !run.ends_with("first"));
    }
}

#[tokio::test]
async fn dropping_a_finished_turn_allows_the_cli_to_flush_telemetry() {
    let root = tempfile::tempdir().expect("临时目录");
    let program = root.path().join("codex");
    std::fs::write(
        &program,
        r#"#!/bin/sh
trap 'exit 42' INT
printf '%s\n' '{"type":"turn.completed","usage":{}}'
/bin/sleep 0.2
printf 'flushed' > "$AI_BOOT_FLUSHED"
"#,
    )
    .expect("模拟 CLI");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("权限");
    let marker = root.path().join("flushed");
    let backend = CodexBackend::new(CodexConfig {
        program,
        profile: None,
        env: vec![
            ("HOME".into(), root.path().as_os_str().into()),
            ("AI_BOOT_FLUSHED".into(), marker.as_os_str().into()),
        ],
        stop_grace: Duration::from_secs(1),
    });
    let mut handle = backend
        .start_turn(TurnRequest {
            session: SessionRef::New,
            prompt: "test".into(),
            workdir: root.path().to_path_buf(),
            run_dir: root.path().join("run"),
            images: Vec::new(),
            rules: String::new(),
            schema: serde_json::json!({}),
            mcp_servers: Vec::new(),
            model: Some("gpt-6.1-sol".into()),
            effort: Some(Effort::Xhigh),
            timeout: Duration::from_secs(5),
            budget_usd_micros: None,
        })
        .await
        .expect("启动");
    assert!(matches!(
        handle.events.recv().await,
        Some(AgentEvent::Usage(_))
    ));
    assert!(matches!(
        handle.events.recv().await,
        Some(AgentEvent::Finished(Outcome::Success { .. }))
    ));
    drop(handle);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("终态后关闭接收端不能中断 CLI 的日志 flush");
}

#[tokio::test]
async fn missing_rollout_on_stderr_is_a_recoverable_session_failure() {
    let root = tempfile::tempdir().expect("临时目录");
    let program = root.path().join("codex");
    std::fs::write(&program, "#!/bin/sh\nprintf '%s\\n' 'Error: thread/resume: no rollout found for thread id missing' >&2\nexit 1\n").expect("模拟 CLI");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("权限");
    let work = root.path().join("work");
    std::fs::create_dir(&work).expect("工作目录");
    let backend = CodexBackend::new(CodexConfig {
        program,
        profile: None,
        env: vec![("HOME".into(), root.path().as_os_str().into())],
        stop_grace: Duration::from_secs(1),
    });
    let mut handle = backend
        .start_turn(TurnRequest {
            session: SessionRef::Resume("missing".into()),
            prompt: "question".into(),
            workdir: work,
            run_dir: root.path().join("run"),
            images: Vec::new(),
            rules: String::new(),
            schema: serde_json::json!({}),
            mcp_servers: Vec::new(),
            model: None,
            effort: None,
            timeout: Duration::from_secs(5),
            budget_usd_micros: None,
        })
        .await
        .expect("启动");
    let mut outcomes = Vec::new();
    while let Some(event) = handle.events.recv().await {
        if let AgentEvent::Finished(outcome) = event {
            outcomes.push(outcome);
        }
    }
    assert!(
        matches!(&outcomes[..], [Outcome::Failed { kind: FailKind::SessionNotFound, reason }] if reason.contains("no rollout found"))
    );
}
