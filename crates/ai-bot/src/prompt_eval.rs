//! 手动运行的真实模型评测：使用模拟资料，不连接现场或向飞书发送消息。

use std::path::{Path, PathBuf};
use std::time::Duration;

use ai_bot_agent::codex::{CodexBackend, CodexConfig};
use ai_bot_agent::{AgentBackend, AgentEvent, Effort, SessionRef, TurnRequest};

use crate::answer::{self, Answer, Confidence, Status};

async fn ask(
    backend: &CodexBackend,
    root: &Path,
    name: &str,
    session: SessionRef,
    question: &str,
) -> (String, Answer) {
    let context_dir = root.join("work/context");
    std::fs::create_dir_all(&context_dir).expect("本轮资料目录");
    let seq = i64::try_from(std::fs::read_dir(&context_dir).expect("资料文件").count() + 1)
        .expect("轮次");
    std::fs::write(context_dir.join(format!("turn-{seq}.md")), question).expect("评测资料");
    let question_text = question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(question);
    let context = crate::context::TurnContext {
        question: format!("{question_text}\n仅根据本轮测试资料回答，无需外部查询或实际执行。"),
        ..crate::context::TurnContext::default()
    };
    let request_prompt = super::request(&context, seq);
    assert!(request_prompt.len() < 1024, "本轮请求不应内联资料和规则");
    let mut handle = backend
        .start_turn(TurnRequest {
            session,
            prompt: request_prompt,
            workdir: root.join("work"),
            run_dir: root.join(name),
            images: Vec::new(),
            rules: super::rules(None),
            schema: answer::schema().clone(),
            mcp_servers: Vec::new(),
            model: Some("gpt-6.1-sol".into()),
            effort: Some(Effort::Xhigh),
            timeout: Duration::from_secs(300),
            budget_usd_micros: None,
        })
        .await
        .expect("启动评测");
    let mut session_id = None;
    let mut result = None;
    while let Some(event) = handle.events.recv().await {
        match event {
            AgentEvent::Started(started) => session_id = Some(started.session_id),
            AgentEvent::Finished(outcome) => match answer::reply_of(&outcome) {
                Some(answer::Reply::Structured(answer)) => result = Some(*answer),
                _ => panic!("{name} 未产生结构化答案：{outcome:?}"),
            },
            _ => {}
        }
    }
    let answer = result.expect("最终结果");
    println!(
        "{name}: {}",
        serde_json::to_string(&answer).expect("答案 JSON")
    );
    // 含长技术标识时允许适度超出建议字数，仍防止首屏变成长文。
    assert!(answer.summary.chars().count() <= 240, "{name} 概述过长");
    assert!(answer.open_questions.len() <= 2, "{name} 追问过多");
    for command in &answer.commands {
        assert!(!command.command.contains('\n'), "{name} 命令必须单行");
        assert!(!command.command.contains("$("), "{name} 不应生成命令替换");
    }
    (session_id.expect("会话 ID"), answer)
}

fn kafka_command(answer: &Answer, namespace: &str, server: &str) {
    assert_eq!(answer.status, Status::Answered);
    assert_eq!(answer.commands.len(), 1, "固定参数不应再加查询步骤");
    let command = &answer.commands[0].command;
    for part in [
        "kubectl",
        namespace,
        "exec",
        "kafka-0",
        "--",
        "/usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh",
        "--command-config /usr/local/qingteng/kafka/config/consumer.properties",
        "--bootstrap-server",
        server,
        "--describe",
        "--all-groups",
    ] {
        assert!(command.contains(part), "缺少 {part}：{command}");
    }
    assert!(!command.contains('<'), "固定参数不能占位：{command}");
}

fn eval_backend(root: &Path) -> CodexBackend {
    std::fs::create_dir(root.join("work")).expect("工作目录");
    // 登录凭据只链接，所有评测会话记录放在临时目录，断言失败也会自动清理。
    let login_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".codex"));
    let test_home = root.join("codex");
    std::fs::create_dir(&test_home).expect("评测 Codex 目录");
    let auth = login_home.join("auth.json");
    if auth.is_file() {
        std::os::unix::fs::symlink(auth, test_home.join("auth.json")).expect("复用登录");
    }
    let local_config = std::fs::read_to_string(login_home.join("config.toml"))
        .ok()
        .and_then(|raw| toml::from_str::<toml::Value>(&raw).ok());
    let mut config = toml::Table::new();
    if let Some(sandbox) = local_config.and_then(|config| config.get("sandbox_mode").cloned()) {
        config.insert("sandbox_mode".into(), sandbox);
    }
    config.insert("web_search".into(), "disabled".into());
    std::fs::write(
        test_home.join("config.toml"),
        toml::to_string(&config).expect("评测配置"),
    )
    .expect("本机执行权限");
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
    let skills = test_home.join("skills");
    std::fs::create_dir(&skills).expect("评测技能目录");
    std::os::unix::fs::symlink(
        login_home.join("skills/qt_program/program"),
        skills.join("qt_program"),
    )
    .expect("使用真实 qt_program 技能");
    env.push(("CODEX_HOME".into(), test_home.into_os_string()));
    CodexBackend::new(CodexConfig {
        program: std::env::var_os("AI_BOT_CODEX_BIN")
            .expect("设置 AI_BOT_CODEX_BIN 为 Codex 绝对路径")
            .into(),
        profile: None,
        env,
        stop_grace: Duration::from_secs(5),
    })
}

#[tokio::test]
#[ignore = "调用已登录的本机 Codex 评估实际回复，模型固定 gpt-6.1-sol / xhigh"]
async fn reply_quality_with_codex() {
    let root = tempfile::tempdir().expect("评测目录");
    let backend = eval_backend(root.path());
    let (session, fixed) = ask(
        &backend,
        root.path(),
        "fixed",
        SessionRef::New,
        r#"
查下 3.x k3s 部署 Kafka 消息队列堆积命令。
已读取适用于本现场版本的内部操作文档正文和部署清单：namespace=qtsa；
Kafka 为 StatefulSet，Pod 固定为 kafka-0、kafka-1、kafka-2；本次 kafka-0 正常且只有一个容器。
Service=kafka-svc，端口 9092；在 Kafka Pod 内执行工具。
工具完整路径 /usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh，
认证文件 /usr/local/qingteng/kafka/config/consumer.properties。
文档确认查询全部组用 --command-config、--bootstrap-server、--describe --all-groups，观察 LAG 列。
文档补充：指定消费者组用 --group 代替 --all-groups；队列对应 Topic，要按输出的 TOPIC 列筛选。
本版本查询输出列为 GROUP、TOPIC、PARTITION、CURRENT-OFFSET、LOG-END-OFFSET、LAG 等。
GROUP 和 TOPIC 的实际名称可从主查询结果取；不要把消费者组与 Topic 混为一谈。
目标是在能运行 kubectl 的主机上敲一条命令。
"#,
    )
    .await;
    kafka_command(&fixed, "qtsa", "kafka-svc:9092");
    assert!(
        fixed.summary.contains("**"),
        "首屏应突出重点：{}",
        fixed.summary
    );
    assert!(!fixed.sections.is_empty(), "一般用法询问应有实用的折叠补充");
    let details = fixed
        .sections
        .iter()
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(details.contains("TOPIC") || details.contains("Topic") || details.contains("topic"));
    assert!(
        details.contains("--group")
            || details.contains("grep")
            || details.contains("awk")
            || details.contains("筛选"),
        "应补充相关的参数调整或筛选用法：{details}"
    );

    let (corrected_session, corrected) = ask(
        &backend,
        root.path(),
        "corrected",
        SessionRef::Resume(session),
        r#"
更正：这套现场使用 qtsa-prod，Service 是 kafka-prod-svc:9093，其他参数不变。
这是刚查询的实际输出：namespace qtsa-prod，Pod kafka-0 Running/Ready，
Service kafka-prod-svc 的端口为 9093。请修正上一轮的堆积查询命令。
"#,
    )
    .await;
    kafka_command(&corrected, "qtsa-prod", "kafka-prod-svc:9093");
    assert!(!corrected.corrections.is_empty());

    let (_, command_only) = ask(
        &backend,
        root.path(),
        "command-only",
        SessionRef::Resume(corrected_session),
        "给我刚才修正后的查询命令即可，这次不要补充用法或详细说明。",
    )
    .await;
    kafka_command(&command_only, "qtsa-prod", "kafka-prod-svc:9093");
    assert!(
        command_only.sections.is_empty(),
        "明确只要命令时应尊重用户范围"
    );

    let (_, dynamic) = ask(
        &backend,
        root.path(),
        "dynamic",
        SessionRef::New,
        r#"
查看 api-worker 的最近 100 行日志，给我现场可以按顺序敲的命令。
部署文档已核实 namespace=ops，Deployment=api-worker，Pod 标签 app=api-worker，单容器。
Pod 带随机后缀，每次发布都会变；当前 Pod 名只能由我在现场查询，你无法访问现场。
要查询 Running/Ready 的 Pod，再取其日志；查询输出的名称列是 NAME。
"#,
    )
    .await;
    assert_eq!(dynamic.commands.len(), 2, "必须先查动态名称，再查日志");
    let query = &dynamic.commands[0];
    let logs = &dynamic.commands[1];
    assert!(query.command.contains("get pods"), "{}", query.command);
    assert!(!query.command.contains('<'));
    assert!(logs.command.contains("logs"));
    assert!(logs.command.contains("100"));
    let placeholders = regex::Regex::new(r"<[^<>\n]+>").expect("占位符正则");
    let placeholder = placeholders
        .find(&logs.command)
        .expect("动态 Pod 名")
        .as_str();
    assert!(
        query.look.contains(placeholder),
        "查询结果与 {placeholder} 必须明确对应"
    );
    assert!(query.look.contains("NAME") || query.look.contains("名称"));

    let (_, conflict) = ask(
        &backend,
        root.path(),
        "version",
        SessionRef::New,
        r#"
我要这套 Kafka 的堆积查询命令。
现场确定是 4.x，namespace=ops，Pod kafka-0 正常运行；脚本位置和连接端口尚未查明。
唯一找到的文档仅适用于 3.x，写着 /usr/local/qingteng/kafka/bin/kafka-consumer-groups.sh
和 kafka-svc:9092，文档明确说 4.x 安装目录、认证和端口发生变化。
当前没有 4.x 的相关资料或现场配置结果。
"#,
    )
    .await;
    assert_ne!(conflict.status, Status::Answered);
    assert!(!conflict.open_questions.is_empty() || !conflict.commands.is_empty());
    for command in &conflict.commands {
        assert!(!command.command.contains("kafka-svc:9092"));
        assert!(!command.command.contains("/usr/local/qingteng/kafka/"));
    }

    let (_, uncertain) = ask(
        &backend,
        root.path(),
        "uncertain",
        SessionRef::New,
        r#"
登录偶发 500，根因是什么？目前只有用户口述，没有日志、代码或监控。
同事猜是连接池耗尽，但没有验证过。不能访问现场。
"#,
    )
    .await;
    assert_ne!(uncertain.status, Status::Answered);
    assert_ne!(uncertain.confidence, Confidence::High);
    for command in &uncertain.commands {
        for mutation in ["restart", "delete", "set resources", "apply", "sed -i"] {
            assert!(
                !command.command.contains(mutation),
                "根因不明不能直接修改：{command:?}"
            );
        }
    }

    let (_, cause) = ask(
        &backend,
        root.path(),
        "cause",
        SessionRef::New,
        r#"
只解释启动失败的原因，不需要处理步骤或命令。
已核实的现场：堆栈是 ClassNotFoundException: com.mysql.cj.jdbc.Driver；
应用启动时调用 Class.forName("com.mysql.cj.jdbc.Driver")；
同一版本制品解包清单及实际启动 classpath 均没有包含 MySQL Connector/J 驱动。
"#,
    )
    .await;
    assert_eq!(cause.status, Status::Answered);
    assert!(cause.commands.is_empty());
    assert!(!cause.summary.contains("**解决"));
}

#[tokio::test]
#[ignore = "调用本机 Codex，用测试资料验证通用补充规则，实际回复由模型生成"]
async fn operational_help_is_actionable_and_respects_command_only_requests() {
    let root = tempfile::tempdir().expect("评测目录");
    let backend = eval_backend(root.path());
    let (session, answer) = ask(
        &backend,
        root.path(),
        "operational-help",
        SessionRef::New,
        include_str!("../tests/fixtures/replies/kafka-query.txt"),
    )
    .await;
    kafka_command(&answer, "qtsa", "kafka-svc:9092");
    assert!(answer.summary.contains("**"), "首屏要突出重点");
    let details = answer
        .sections
        .iter()
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        details.contains("--group"),
        "应有具体的参数替换说明：{details}"
    );
    assert!(
        details.contains("grep") || details.contains("awk"),
        "应有可照做的筛选片段：{details}"
    );
    let (_, only_command) = ask(
        &backend,
        root.path(),
        "only-command",
        SessionRef::Resume(session),
        "这次只给刚才的主查询命令，不需要补充用法或详细说明。",
    )
    .await;
    kafka_command(&only_command, "qtsa", "kafka-svc:9092");
    assert!(only_command.sections.is_empty());
}

#[tokio::test]
#[ignore = "调用本机 Codex，验证只问原因时不会追加处理命令"]
async fn cause_only_requests_do_not_turn_into_howto() {
    let root = tempfile::tempdir().expect("评测目录");
    let backend = eval_backend(root.path());
    let (_, answer) = ask(
        &backend,
        root.path(),
        "cause-only",
        SessionRef::New,
        include_str!("../tests/fixtures/replies/cause-only.txt"),
    )
    .await;
    assert_eq!(answer.status, Status::Answered);
    assert!(answer.commands.is_empty());
    assert!(!answer.summary.contains("**解决"));
}

#[tokio::test]
#[ignore = "调用本机 Codex，验证根因未确认时仍给有依据的优先判断、备选和区分方法"]
async fn uncertain_causes_are_ranked_and_actionable() {
    let root = tempfile::tempdir().expect("评测目录");
    let backend = eval_backend(root.path());
    let (_, answer) = ask(
        &backend,
        root.path(),
        "ranked-causes",
        SessionRef::New,
        include_str!("../tests/fixtures/replies/ranked-causes.txt"),
    )
    .await;
    assert_eq!(answer.status, Status::Partial, "候选根因尚未验证");
    assert!(answer.summary.contains("连接"), "先给依据较强的判断");
    assert!(answer.summary.contains("SQL") || answer.summary.contains("查询"));
    let details = answer
        .sections
        .iter()
        .map(|s| s.body.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(details.contains("GC"), "应分析已有线索中的弱候选");
    assert!(
        details.contains("120") || details.contains("毫秒"),
        "说明 GC 证据的局限"
    );
    assert!(
        details.contains("堆栈") || details.contains("请求") || details.contains("时间"),
        "给能区分候选的验证方法"
    );
    for command in &answer.commands {
        for mutation in ["restart", "delete", "set resources", "apply", "sed -i"] {
            assert!(
                !command.command.contains(mutation),
                "不能把猜测直接变成修改"
            );
        }
    }
}
