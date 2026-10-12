//! 原生 Codex 工程闭环：真实本地 Git/测试/制品，模拟 GitLab/Jenkins，不访问生产环境。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ai_boot_agent::codex::{CodexBackend, CodexConfig};
use ai_boot_agent::{AgentBackend, AgentEvent, Effort, Outcome, SessionRef, TurnRequest};
use serde_json::{Value, json};

fn run(program: &str, args: &[&str], directory: &Path) -> String {
    let output = Command::new(program)
        .args(args)
        .current_dir(directory)
        .output()
        .expect("执行隔离验证命令");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("命令输出")
        .trim()
        .to_owned()
}

async fn ask(backend: &CodexBackend, root: &Path, seq: u32, question: &str) -> Value {
    let mut handle = backend
        .start_turn(TurnRequest {
            session: SessionRef::New,
            prompt: question.into(),
            workdir: root.join("work"),
            run_dir: root.join(format!("run-{seq}")),
            images: Vec::new(),
            rules: "使用 $qt_program 处理本轮提问，按提供的 JSON Schema 返回飞书结果。相关补充资料按需读取。\n当前是隔离验证，只操作本次临时目录、模拟 qtmcp 与本地 Git remote，不访问其他项目或真实发布环境。".into(),
            schema: serde_json::from_str(include_str!("../src/schemas/answer.json"))
                .expect("生产回复 Schema"),
            mcp_servers: Vec::new(),
            model: Some("gpt-6.1-sol".into()),
            effort: Some(Effort::Xhigh),
            timeout: Duration::from_secs(600),
            budget_usd_micros: None,
        })
        .await
        .expect("启动真实 Codex");
    let mut answer = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Finished(Outcome::Success { structured, .. }) => answer = structured,
            AgentEvent::Finished(outcome) => panic!("第 {seq} 轮：{outcome:?}"),
            _ => {}
        }
    }
    let answer = answer.expect("结构化答案");
    println!("第 {seq} 轮：{} {}", answer["status"], answer["summary"]);
    answer
}

#[tokio::test]
#[ignore = "调用本机 Codex，隔离验证工程授权、版本/MR/制品/部署验收、续接与失败处理"]
async fn engineering_workflow_preserves_version_authorization_and_external_results() {
    let root = tempfile::tempdir().expect("隔离目录");
    let root = root.path();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/engineering_mcp.py");
    run(
        "python3",
        &[
            fixture.to_str().expect("fixture"),
            "--init",
            root.to_str().expect("root"),
        ],
        root,
    );
    let home = root.join("codex");
    let work = root.join("work");
    std::fs::create_dir_all(home.join("skills")).expect("测试技能目录");
    std::fs::create_dir_all(work.join("context")).expect("会话工作目录");
    let login_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".codex"));
    if login_home.join("auth.json").is_file() {
        std::os::unix::fs::symlink(login_home.join("auth.json"), home.join("auth.json"))
            .expect("复用本机登录");
    }
    for (name, path) in [
        ("qt_program", "qt_program/program"),
        ("qt-merge-request", "qt_program/mr"),
    ] {
        std::os::unix::fs::symlink(
            login_home.join("skills").join(path),
            home.join("skills").join(name),
        )
        .expect("加载本次维护的真实 skill");
    }
    let local: toml::Value =
        toml::from_str(&std::fs::read_to_string(login_home.join("config.toml")).expect("本机配置"))
            .expect("本机 TOML");
    let sandbox = local
        .get("sandbox_mode")
        .and_then(toml::Value::as_str)
        .unwrap_or("workspace-write");
    let config = toml::Value::try_from(json!({
        "sandbox_mode": sandbox, "web_search": "disabled",
        "mcp_servers": {"qtmcp": {"command": "/usr/bin/python3", "args": [fixture, root], "default_tools_approval_mode": "approve"}}
    })).expect("隔离配置");
    std::fs::write(
        home.join("config.toml"),
        toml::to_string(&config).expect("TOML"),
    )
    .expect("测试配置");
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
    for (key, value) in [
        ("GIT_AUTHOR_NAME", "Engineering Test"),
        ("GIT_COMMITTER_NAME", "Engineering Test"),
        ("GIT_AUTHOR_EMAIL", "test@example.test"),
        ("GIT_COMMITTER_EMAIL", "test@example.test"),
    ] {
        env.push((key.into(), value.into()));
    }
    let backend = CodexBackend::new(CodexConfig {
        program: std::env::var_os("AI_BOOT_CODEX_BIN")
            .expect("Codex 绝对路径")
            .into(),
        profile: None,
        env,
        stop_grace: Duration::from_secs(5),
    });
    let state = || -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(root.join("state.json")).expect("实际模拟结果"),
        )
        .expect("状态 JSON")
    };
    let dirty = std::fs::read(root.join("seed/README.md")).expect("原有改动");
    ask(&backend, root, 1, "zzhtl：lab/demo 产品 1.2.3 从修改到 MR、打包部署验证大致怎么做？只说明流程，现在不要执行任何修改、MR 或构建部署。").await;
    assert!(state()["writes"].as_array().expect("写操作").is_empty());
    assert!(
        state()["calls"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .all(|call| !matches!(
                call["arguments"]["action"].as_str(),
                Some("create" | "update" | "trigger")
            )),
        "查询不能尝试外部写操作"
    );
    let question = format!(
        "zzhtl：请实际完成 lab/demo 产品 1.2.3 的 LAB-42：修复 app.py 的加法结果，创建任务分支、测试、提交 MR，打包并部署到 isolated-test，实际验证正数和负数输入；各阶段完成再返回结果。项目资料与构建发布方式查 qtmcp 及仓库，已有本地检出 {} 中的未提交改动必须保留。",
        root.join("seed").display()
    );
    std::fs::write(work.join("context/turn-2.md"), &question).expect("任务原文");
    let answer = ask(&backend, root, 2, &question).await;
    assert_eq!(answer["status"], "answered", "{answer}");
    let completed = state();
    assert_eq!(completed["mrs"].as_array().expect("MR").len(), 1);
    assert_eq!(completed["builds"].as_array().expect("构建").len(), 2);
    let sha = completed["mrs"][0]["sha"].as_str().expect("源分支 SHA");
    assert_eq!(
        run(
            "git",
            &["rev-parse", "fix/LAB-42^"],
            &root.join("remote.git")
        ),
        completed["base"].as_str().expect("基线")
    );
    let version: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("deployed/version.json")).expect("实际运行版本"),
    )
    .expect("版本 JSON");
    assert_eq!(version["commit"], sha);
    assert_eq!(version["sha256"], completed["builds"][0]["sha256"]);
    assert_eq!(
        run("python3", &["app.py", "2", "3"], &root.join("deployed")),
        "5"
    );
    assert_eq!(
        run("python3", &["app.py", "-2", "3"], &root.join("deployed")),
        "1"
    );
    assert_eq!(
        std::fs::read(root.join("seed/README.md")).expect("原有改动"),
        dirty
    );
    let records: Vec<_> = std::fs::read_dir(work.join("context/tasks"))
        .expect("多阶段任务记录")
        .collect();
    assert!(!records.is_empty(), "中断前必须有可恢复的任务记录");
    let writes_before = completed["writes"].as_array().expect("操作记录").len();
    let recovered = ask(&backend, root, 3, "zzhtl：继续刚才 LAB-42 的工程任务，请核实之前的 MR、构建部署和实际运行结果，完成剩余步骤。").await;
    assert_eq!(recovered["status"], "answered", "{recovered}");
    assert_eq!(
        state()["writes"].as_array().expect("续接操作").len(),
        writes_before,
        "新会话续接不能重复创建 MR、构建或部署"
    );
    std::fs::write(root.join("fail_next_package"), "test failure").expect("注入构建失败");
    let failed = ask(&backend, root, 4, "zzhtl：LAB-42 请对刚才同一提交重新打包并部署 isolated-test，再实际验证；不要重新修改代码或提交 MR。").await;
    assert_eq!(
        failed["status"], "partial",
        "失败不能标记全部完成：{failed}"
    );
    let failed_state = state();
    assert_eq!(
        failed_state["builds"].as_array().expect("构建记录").len(),
        3,
        "打包失败不能继续部署"
    );
    assert_eq!(failed_state["builds"][2]["result"], "FAILURE");
    println!(
        "已验证：查询无写操作；版本基线正确；真实 commit/push、制品与运行结果一致；新会话续接无重复外部操作；构建失败停止部署。"
    );
}
