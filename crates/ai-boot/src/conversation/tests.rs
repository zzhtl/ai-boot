//! 会话场景：wiremock 扮演飞书，假后端按剧本吐事件，走真实的 registry、actor、
//! runner 和 SQLite。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use ai_boot_agent::{
    AgentBackend, AgentError, AgentEvent, BackendInfo, BackendKind, FailKind, McpServer, McpStatus,
    Outcome, SessionRef, Started, Step, TurnHandle, TurnRequest, Usage,
};
use ai_boot_feishu::api::ApiClient;
use secrecy::SecretString;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::{Job, Registry};
use crate::callback::{Action, Toast};
use crate::runner::{Runner, Settings};
use crate::store::{NewInput, Store, Turn, TurnStatus};

const BOSS: &str = "ou_boss";
const BOT: &str = "ou_bot";
const DEBOUNCE: Duration = Duration::from_millis(300);

/// 剧本里的一拍。
#[derive(Clone)]
enum Beat {
    Event(AgentEvent),
    /// 等测试放行（或被取消）：让「前一轮还在跑」这类状态可控。
    Gate(Arc<tokio::sync::Notify>),
    /// 一直等到被取消（停止按钮），然后以中断结束。
    UntilCancelled,
}

/// 每次 `start_turn` 取下一段剧本；记下收到的请求。
struct FakeBackend {
    scripts: Mutex<VecDeque<Vec<Beat>>>,
    requests: Mutex<Vec<TurnRequest>>,
}

impl FakeBackend {
    fn requests(&self) -> Vec<TurnRequest> {
        self.requests.lock().expect("锁").clone()
    }
}

#[async_trait::async_trait]
impl AgentBackend for FakeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnHandle, AgentError> {
        self.requests.lock().expect("锁").push(request);
        let script = self
            .scripts
            .lock()
            .expect("锁")
            .pop_front()
            .expect("剧本不够用了");
        let (tx, rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let watch = cancel.clone();
        tokio::spawn(async move {
            for beat in script {
                match beat {
                    Beat::Event(event) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        if watch.is_cancelled() {
                            let _ = tx.send(AgentEvent::Finished(Outcome::Interrupted)).await;
                            return;
                        }
                        let _ = tx.send(event).await;
                    }
                    Beat::Gate(gate) => {
                        tokio::select! {
                            () = gate.notified() => {}
                            () = watch.cancelled() => {
                                let _ = tx.send(AgentEvent::Finished(Outcome::Interrupted)).await;
                                return;
                            }
                        }
                    }
                    Beat::UntilCancelled => {
                        watch.cancelled().await;
                        let _ = tx.send(AgentEvent::Finished(Outcome::Interrupted)).await;
                        return;
                    }
                }
            }
        });
        Ok(TurnHandle {
            events: rx,
            cancel,
            session_id: None,
        })
    }

    async fn preflight(&self) -> Result<BackendInfo, AgentError> {
        Ok(BackendInfo {
            kind: BackendKind::Claude,
            version: "test".into(),
            recorded_version: None,
        })
    }
}

fn started(session: &str) -> Beat {
    started_with(session, &["Read", "Glob", "Grep", "StructuredOutput"])
}

fn started_with(session: &str, tools: &[&str]) -> Beat {
    Beat::Event(AgentEvent::Started(Started {
        session_id: session.into(),
        model: "claude-opus".into(),
        cli_version: Some("2.1.283".into()),
        tools: Some(tools.iter().map(|t| (*t).to_owned()).collect()),
        mcp_servers: Some(vec![McpStatus {
            name: "qtmcp".into(),
            status: "connected".into(),
        }]),
        plugins: Some(vec!["telemetry@builtin".into()]),
        skills: Some(vec![]),
        permission_mode: Some("default".into()),
    }))
}

fn usage(total_in: u64, total_out: u64) -> Beat {
    Beat::Event(AgentEvent::Usage(Usage {
        input_tokens: total_in,
        output_tokens: total_out,
        ..Usage::default()
    }))
}

/// 最后一次请求带着这么多上下文。
fn long_context(tokens: u64) -> Beat {
    Beat::Event(AgentEvent::Usage(Usage {
        context_tokens: tokens,
        ..Usage::default()
    }))
}

fn answered(title: &str) -> Beat {
    Beat::Event(AgentEvent::Finished(Outcome::Success {
        structured: Some(answer_json(title)),
        text: String::new(),
        turns: 3,
    }))
}

/// 追问后结论变了：答案里带更正。
fn corrected(title: &str, correction: &str) -> Beat {
    let mut answer = answer_json(title);
    answer["corrections"] = json!([correction]);
    Beat::Event(AgentEvent::Finished(Outcome::Success {
        structured: Some(answer),
        text: String::new(),
        turns: 3,
    }))
}

fn failed(kind: FailKind, reason: &str) -> Beat {
    Beat::Event(AgentEvent::Finished(Outcome::Failed {
        kind,
        reason: reason.into(),
    }))
}

fn answer_json(title: &str) -> Value {
    json!({
        "kind": "diagnosis", "title": title, "status": "answered",
        "confidence": "high", "summary": "连接池耗尽，见 ABC-12。", "conflicts": [],
        "open_questions": [], "jira_keys": ["ABC-12"],
        "sections": [
            {"title": "根因", "body": "细节".repeat(200), "expanded": true},
            {"title": "解决方案", "body": "调大连接池".repeat(100), "expanded": false},
            {"title": "复测", "body": "压测通过".repeat(100), "expanded": false}
        ],
        "references": [{"kind": "jira", "title": "ABC-12", "url": "https://jira.example.com/browse/ABC-12"}]
    })
}

/// 回复接口：每次回一个新的卡片消息 ID，话题固定为 `omt_1`。
struct ReplyResponder(AtomicUsize);

impl Respond for ReplyResponder {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "msg": "ok",
            "data": {"message_id": format!("om_card_{n}"), "thread_id": "omt_1"}
        }))
    }
}

struct World {
    server: MockServer,
    store: Store,
    runner: Arc<Runner>,
    backend: Arc<FakeBackend>,
    jobs: mpsc::Sender<Job>,
    _dir: tempfile::TempDir,
}

async fn world(scripts: Vec<Vec<Beat>>) -> World {
    world_with(scripts, DEBOUNCE).await
}

async fn world_with(scripts: Vec<Vec<Beat>>, debounce: Duration) -> World {
    world_custom(scripts, debounce, None).await
}

async fn world_custom(
    scripts: Vec<Vec<Beat>>,
    debounce: Duration,
    window: Option<(usize, Duration)>,
) -> World {
    world_full(scripts, debounce, window, false, None).await
}

/// `docs`：接上以用户身份读云文档（授权回调地址是假的，测试里不走浏览器）。
async fn world_full(
    scripts: Vec<Vec<Beat>>,
    debounce: Duration,
    window: Option<(usize, Duration)>,
    docs: bool,
    writer: Option<Value>,
) -> World {
    let server = MockServer::start().await;
    let ok = |data: Value| {
        ResponseTemplate::new(200).set_body_json(json!({"code": 0, "msg": "ok", "data": data}))
    };
    Mock::given(method("POST"))
        .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 0, "tenant_access_token": "t-x", "expire": 7200})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/open-apis/im/v1/messages/[^/]+/reply$"))
        .respond_with(ReplyResponder(AtomicUsize::new(0)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/chats/oc_1/members"))
        .respond_with(ok(json!({"has_more": false, "items": [
            {"member_id": BOSS, "name": "朱工"}, {"member_id": "ou_qa", "name": "测试小王"}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .respond_with(ok(json!({"has_more": false, "items": [
            {"message_id": "om_bot", "msg_type": "interactive", "create_time": "1790584790000",
             "sender": {"id": "cli_app", "sender_type": "app"}, "body": {"content": "{}"}},
            {"message_id": "om_qa", "msg_type": "text", "create_time": "1790584700000",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"text\":\"我在 3.2 上复现了，单号 ABC-12\"}"}}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"^/open-apis/im/v1/messages/[^/]+$"))
        .respond_with(ok(json!({})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path()).await.expect("数据库");
    let base = url::Url::parse(&format!("{}/", server.uri())).expect("地址");
    let api = Arc::new(ApiClient::new(base, "cli_x", SecretString::from("s")).expect("客户端"));
    let backend = Arc::new(FakeBackend {
        scripts: Mutex::new(scripts.into()),
        requests: Mutex::new(Vec::new()),
    });
    let bot = Arc::new(OnceLock::new());
    let _ = bot.set(BOT.to_owned());
    let runner = Runner::new(
        Arc::clone(&api),
        store.clone(),
        HashMap::from([(
            "claude".to_owned(),
            Arc::clone(&backend) as Arc<dyn AgentBackend>,
        )]),
        Settings {
            data_dir: dir.path().to_path_buf(),
            tools: crate::context::extract::Tools {
                office_legacy: false,
                scratch: dir.path().join("tmp"),
                program: "ai-boot".into(),
            },
            window,
            model: Some("opus".into()),
            effort: None,
            timeout: Duration::from_secs(60),
            budget_usd_micros: None,
            mcp_servers: vec![McpServer {
                name: "qtmcp".into(),
                command: "/opt/ai-boot/bin/qtmcp".into(),
                args: vec![],
                env: Default::default(),
            }],
            link_hosts: vec!["jira.example.com".into()],
        },
        1,
        Arc::clone(&bot),
    );
    let runner = if docs {
        let oauth = crate::oauth::Oauth::new(
            Arc::clone(&api),
            store.clone(),
            Arc::new(crate::whitelist::Whitelist::new([BOSS.to_owned()])),
            crate::oauth::OauthSettings {
                app_id: "cli_x".into(),
                redirect_uri: url::Url::parse("http://10.0.0.8:18080/oauth/feishu/callback")
                    .expect("地址"),
                accounts_url: url::Url::parse("https://accounts.example.com/").expect("地址"),
                scopes: vec!["offline_access".into()],
            },
        );
        runner.with_oauth(Arc::new(oauth))
    } else {
        runner
    };
    let runner = match writer {
        Some(replies) => runner.with_writer(Arc::new(crate::writeback::Writer::new(
            Arc::clone(&api),
            store.clone(),
            crate::writeback::Settings {
                server: crate::writeback::mcp::tests::fake_server(dir.path(), &replies),
                confluence: Some(("DEV".to_owned(), "12345".to_owned())),
            },
            vec!["jira.example.com".into()],
        ))),
        None => runner,
    };
    let runner = Arc::new(runner);
    let (jobs, jobs_rx) = mpsc::channel(64);
    let registry = Registry::new(
        store.clone(),
        Arc::clone(&runner),
        bot,
        "claude".into(),
        debounce,
    )
    .with_eraser(Arc::new(crate::maintenance::Maintenance {
        store: store.clone(),
        data_dir: dir.path().to_path_buf(),
        agent_records: None,
    }));
    tokio::spawn(registry.run(jobs_rx));
    World {
        server,
        store,
        runner,
        backend,
        jobs,
        _dir: dir,
    }
}

/// 白名单用户发来一条消息：先落收件箱，再交给会话调度（ingest 的做法）。
/// `extra` 并进 message（话题、引用等）。
async fn say(w: &World, message_id: &str, text: &str, extra: Value) {
    store_input(w, message_id, text, extra).await;
    w.jobs
        .send(Job::Input {
            message_id: message_id.to_owned(),
        })
        .await
        .expect("投递");
}

async fn store_input(w: &World, message_id: &str, text: &str, extra: Value) {
    let content = json!({"text": format!("@_user_1 {text}")}).to_string();
    store_raw(w, message_id, "text", &content, extra).await;
}

/// 任意类型的消息（图片、合并转发……）。
async fn store_raw(w: &World, message_id: &str, message_type: &str, content: &str, extra: Value) {
    store_raw_as(w, message_id, BOSS, message_type, content, extra).await;
}

/// 群里另一个人说的话。
async fn say_as(w: &World, message_id: &str, sender: &str, text: &str) {
    let content = json!({"text": format!("@_user_1 {text}")}).to_string();
    store_raw_as(w, message_id, sender, "text", &content, json!({})).await;
    w.jobs
        .send(Job::Input {
            message_id: message_id.to_owned(),
        })
        .await
        .expect("投递");
}

/// 在答案卡的输入框里补充。
async fn supplement(w: &World, turn_id: &str, card: &str, text: &str) -> Toast {
    let (reply, answer) = oneshot::channel();
    w.jobs
        .send(Job::FollowUp {
            turn_id: turn_id.to_owned(),
            card_message_id: card.to_owned(),
            chat_id: "oc_1".to_owned(),
            text: text.to_owned(),
            operator: BOSS.to_owned(),
            reply,
        })
        .await
        .expect("投递");
    tokio::time::timeout(Duration::from_secs(5), answer)
        .await
        .expect("应答超时")
        .expect("应答")
}

async fn store_raw_as(
    w: &World,
    message_id: &str,
    sender: &str,
    message_type: &str,
    content: &str,
    extra: Value,
) {
    let mut message = json!({
        "message_id": message_id, "chat_id": "oc_1", "chat_type": "group",
        "message_type": message_type, "create_time": "1790584800000",
        "content": content,
        "mentions": [{"key": "@_user_1", "id": {"open_id": BOT}, "name": "排查助手"}]
    });
    if let (Some(message), Some(extra)) = (message.as_object_mut(), extra.as_object()) {
        message.extend(extra.clone());
    }
    let payload = json!({
        "schema": "2.0",
        "header": {"event_id": message_id, "event_type": "im.message.receive_v1"},
        "event": {
            "sender": {"sender_id": {"open_id": sender}, "sender_type": "user"},
            "message": message
        }
    })
    .to_string();
    w.store
        .insert_input(&NewInput {
            message_id,
            chat_id: "oc_1",
            chat_type: "group",
            sender_open_id: sender,
            payload: &payload,
            received_at_ms: crate::store::now_ms(),
        })
        .await
        .expect("落库");
}

/// 在机器人开的话题（`omt_1`，根消息 `root`）里接着说。
fn in_thread(root: &str) -> Value {
    json!({"thread_id": "omt_1", "root_id": root})
}

/// 点重试。一轮的最终卡片发出后，actor 要再过几毫秒才收到「结束」，
/// 这期间点重试会被告知还有一轮没结束——人点不了这么快，测试要等一下。
async fn press_retry(w: &World, turn_id: &str) -> Toast {
    for _ in 0..100 {
        let toast = press(w, Action::Retry, turn_id).await;
        if !toast.text.contains("还有一轮没结束") {
            return toast;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("一直有一轮没结束");
}

/// 等 actor 收到上一轮的「结束」再点（理由同 `press_retry`）。
async fn press_when_idle(w: &World, action: Action, turn_id: &str) -> Toast {
    for _ in 0..100 {
        let toast = press(w, action.clone(), turn_id).await;
        if !toast.text.contains("还有一轮没结束") {
            return toast;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("一直有一轮没结束");
}

async fn press(w: &World, action: Action, turn_id: &str) -> Toast {
    let (reply, answer) = oneshot::channel();
    w.jobs
        .send(Job::Action {
            action,
            turn_id: turn_id.to_owned(),
            operator: BOSS.to_owned(),
            reply,
        })
        .await
        .expect("投递");
    tokio::time::timeout(Duration::from_secs(5), answer)
        .await
        .expect("按钮应当很快得到应答")
        .expect("应答")
}

/// 轮询数据库，直到条件成立。
async fn until(w: &World, what: &str, done: impl Fn(&[Turn]) -> bool) -> Vec<Turn> {
    for _ in 0..500 {
        let turns = w.store.all_turns().await;
        if done(&turns) {
            return turns;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("等不到：{what}；当前 {:?}", w.store.all_turns().await);
}

fn all_finished(count: usize) -> impl Fn(&[Turn]) -> bool {
    move |turns| {
        turns.len() == count
            && turns
                .iter()
                .all(|t| !matches!(t.status, TurnStatus::Queued | TurnStatus::Running))
    }
}

/// 发到某张卡片的最后一次更新。
async fn last_card(w: &World, card_id: &str) -> Value {
    let suffix = format!("/open-apis/im/v1/messages/{card_id}");
    // 最终卡片在落库之后才发，稍等它送达
    for _ in 0..100 {
        let requests = w.server.received_requests().await.expect("请求记录");
        if let Some(patch) = requests
            .iter()
            .rev()
            .find(|r| r.method.as_str() == "PATCH" && r.url.path() == suffix)
        {
            let body: Value = serde_json::from_slice(&patch.body).expect("JSON");
            let card: Value =
                serde_json::from_str(body["content"].as_str().expect("content")).expect("卡片");
            if card["header"]["title"]["content"] != "⏳ 分析中" {
                return card;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("卡片 {card_id} 没有收到最终更新");
}

/// 等某张卡片的最后一次更新满足条件：旧卡片置灰这类更新在落库之后才异步发出。
async fn card_until(w: &World, card_id: &str, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..100 {
        let card = last_card(w, card_id).await;
        if done(&card) {
            return card;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    last_card(w, card_id).await
}

/// 某张卡片收到过几次更新。
async fn card_patches(w: &World, card_id: &str) -> usize {
    w.server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| r.method.as_str() == "PATCH" && r.url.path().ends_with(card_id))
        .count()
}

async fn replies(w: &World) -> Vec<Value> {
    w.server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/reply"))
        .map(|r| serde_json::from_slice(&r.body).expect("JSON"))
        .collect()
}

fn button(card: &Value) -> Option<(Action, String)> {
    card["body"]["elements"]
        .as_array()?
        .iter()
        .find(|e| e["tag"] == "button")
        .and_then(|b| Action::parse(&b["behaviors"][0]["value"]))
}

#[tokio::test]
async fn a_question_ends_as_a_folded_answer_card() {
    let w = world(vec![vec![
        started("s-1"),
        Beat::Event(AgentEvent::Step(Step::ToolCall {
            id: "t1".into(),
            tool: "mcp__qtmcp__jira_issue".into(),
            input: json!({"action": "get", "key": "ABC-12"}),
        })),
        usage(1000, 234),
        answered("<at id=all>登录偶发 500</at>"),
    ]])
    .await;
    say(&w, "om_q", "登录偶发 500 是什么原因", json!({})).await;
    let turns = until(&w, "一轮结束", all_finished(1)).await;
    assert_eq!(turns[0].status, TurnStatus::Succeeded);

    // 交给 Agent 的输入：分层 prompt、schema、规则、完整记录
    let request = w.backend.requests().pop().expect("启动过一轮");
    assert_eq!(request.session, SessionRef::New);
    assert!(request.prompt.contains("登录偶发 500 是什么原因"));
    assert!(!request.prompt.contains("@_user_1"), "@机器人 要去掉");
    assert!(!request.prompt.contains("提问人"), "提问人不算上下文");
    assert_eq!(request.schema, *crate::answer::schema());
    assert_eq!(request.rules, crate::prompt::RULES);
    assert!(!request.run_dir.exists(), "本轮的 MCP、hook 配置用完即删");
    let transcript = std::fs::read_to_string(request.workdir.join("context/transcript.md"))
        .expect("完整记录落盘");
    assert!(transcript.contains("登录偶发 500"));

    // 最终卡片：折叠面板、尾注，且没有标签注入；参考来源不上卡片
    let card = last_card(&w, "om_card_1").await;
    let text = card.to_string();
    assert_eq!(card["header"]["template"], "blue");
    assert!(!text.contains("<at"), "{text}");
    assert!(text.contains("collapsible_panel"));
    assert!(!text.contains("参考来源"), "{text}");
    assert!(
        text.contains("\"tag\":\"input\""),
        "卡片底部有补充用的输入框：{text}"
    );
    assert!(!text.contains("tokens"), "用量不上卡片：{text}");
    let turn = &w.store.all_turns().await[0];
    assert_eq!(
        w.store.turn_usage(&turn.id).await,
        (1234, 1),
        "用量记在库里"
    );
    assert_eq!(button(&card), None, "答案卡上没有按钮");

    // 引用回复在主消息流里，不开话题；收件箱清空
    let replies = replies(&w).await;
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["reply_in_thread"], false);
    let conversation = w
        .store
        .conversation(&turns[0].conversation_id)
        .await
        .expect("查询")
        .expect("会话");
    assert_eq!(conversation.thread_id, None);
    assert_eq!(conversation.agent_session_id.as_deref(), Some("s-1"));
    assert!(w.store.unassigned_inputs().await.expect("查询").is_empty());
}

#[tokio::test]
async fn quick_successive_messages_are_answered_in_one_turn() {
    // 窗口放宽：机器负载高时落库也要时间，测试不能赌 300ms
    let w = world_with(
        vec![vec![started("s-1"), answered("合并")]],
        Duration::from_secs(2),
    )
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    say(
        &w,
        "om_2",
        "日志里有 Connection is not available",
        in_thread("om_1"),
    )
    .await;
    say(&w, "om_3", "版本是 3.2.1", in_thread("om_1")).await;
    let turns = until(&w, "一轮结束", all_finished(1)).await;
    assert_eq!(turns.len(), 1, "三条连发只跑一轮");
    let requests = w.backend.requests();
    assert_eq!(requests.len(), 1);
    let prompt = &requests[0].prompt;
    for part in ["登录报 500", "Connection is not available", "3.2.1"] {
        assert!(prompt.contains(part), "缺 {part}：{prompt}");
    }
    assert_eq!(replies(&w).await.len(), 1, "只回一张卡片");
}

#[tokio::test]
async fn a_follow_up_resumes_the_session_and_counts_only_its_own_tokens() {
    let w = world(vec![
        vec![started("s-1"), usage(1000, 200), answered("第一轮")],
        vec![started("s-1"), usage(1500, 500), answered("第二轮")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    say(&w, "om_2", "那 3.3 有没有这个问题", in_thread("om_1")).await;
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(turns[0].conversation_id, turns[1].conversation_id);
    assert_eq!(turns[1].seq, 2);

    let requests = w.backend.requests();
    assert_eq!(requests[1].session, SessionRef::Resume("s-1".into()));
    assert!(requests[1].prompt.starts_with("# 本轮提问"));
    assert_eq!(
        requests[0].workdir, requests[1].workdir,
        "续接要求同一个工作目录"
    );
    let transcript =
        std::fs::read_to_string(requests[1].workdir.join("context/transcript.md")).expect("记录");
    assert!(transcript.contains("第 2 轮提问"));

    let card = last_card(&w, "om_card_2").await;
    let text = card.to_string();
    assert!(text.contains("第 2 轮"));
    // 会话累计 2000，上一轮末 1200：本轮 800
    let turns = w.store.all_turns().await;
    assert_eq!(w.store.turn_usage(&turns[1].id).await.0, 800);
}

/// 私聊不开话题：引用机器人的卡片回复就是追问，续接同一个 Agent 会话；上一轮结束两小时内
/// 直接新发的也续接（同一件事一问再问，不用从零查起），隔了两小时以上才新开会话。
#[tokio::test]
async fn in_private_chat_a_new_message_follows_up_within_two_hours_and_starts_over_after() {
    let w = world(vec![
        vec![started("s-1"), answered("第一轮")],
        vec![started("s-1"), answered("追问")],
        vec![started("s-1"), answered("接着问")],
        vec![started("s-2"), answered("新问题")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({"chat_type": "p2p"})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    let quote = json!({"chat_type": "p2p", "parent_id": "om_card_1", "root_id": "om_1"});
    say(&w, "om_2", "那 3.3 有没有这个问题", quote).await;
    until(&w, "追问结束", all_finished(2)).await;
    say(&w, "om_3", "改完要重启吗", json!({"chat_type": "p2p"})).await;
    let turns = until(&w, "接着问结束", all_finished(3)).await;
    w.store
        .age_conversation(&turns[0].conversation_id, 2 * 60 * 60 * 1000 + 1000)
        .await;
    say(&w, "om_4", "导出为什么慢", json!({"chat_type": "p2p"})).await;
    let turns = until(&w, "新问题结束", all_finished(4)).await;

    assert_eq!(turns[0].conversation_id, turns[1].conversation_id);
    assert_eq!(turns[1].seq, 2);
    assert_eq!(
        turns[0].conversation_id, turns[2].conversation_id,
        "两小时内续接"
    );
    assert_eq!(turns[2].seq, 3);
    assert_ne!(
        turns[0].conversation_id, turns[3].conversation_id,
        "隔久了新开"
    );
    let requests = w.backend.requests();
    assert_eq!(requests[1].session, SessionRef::Resume("s-1".into()));
    assert_eq!(requests[2].session, SessionRef::Resume("s-1".into()));
    assert!(
        requests[2].prompt.contains("改完要重启吗") && !requests[2].prompt.contains("登录报 500"),
        "续接只带本轮提问：{}",
        requests[2].prompt
    );
    assert_eq!(requests[3].session, SessionRef::New);
    assert!(
        replies(&w)
            .await
            .iter()
            .all(|r| r["reply_in_thread"] == false),
        "私聊也不开话题"
    );
    assert!(
        last_card(&w, "om_card_2")
            .await
            .to_string()
            .contains("\"tag\":\"input\"")
    );
}

/// 引用卡片纠正后结论变了：新卡片写明更正了什么，旧卡片置灰并注明以最新回复为准；
/// 结论没变的追问不去动旧卡片。
#[tokio::test]
async fn a_correction_marks_the_earlier_answer_as_superseded() {
    let w = world(vec![
        vec![started("s-1"), answered("第一轮")],
        vec![started("s-1"), answered("补充后结论没变")],
        vec![
            started("s-1"),
            corrected(
                "更正后",
                "原来说是连接池耗尽 → 实际是 DNS 解析超时（见 dns.log）",
            ),
        ],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    let quote_1 = json!({"parent_id": "om_card_1", "root_id": "om_1"});
    say(&w, "om_2", "补充一下，是 3.2.1 版本", quote_1).await;
    until(&w, "第二轮结束", all_finished(2)).await;
    let patched_before = card_patches(&w, "om_card_1").await;

    let quote_2 = json!({"parent_id": "om_card_2", "root_id": "om_1"});
    say(&w, "om_3", "不对，连接池监控是正常的", quote_2).await;
    until(&w, "第三轮结束", all_finished(3)).await;

    let newest = last_card(&w, "om_card_3").await.to_string();
    assert!(newest.contains("本轮更正"), "{newest}");
    assert!(newest.contains("DNS 解析超时"), "{newest}");
    // 更正的是上一张答案卡（第 2 轮）；结论没变的那次追问没动第 1 张
    let old = card_until(&w, "om_card_2", |card| card["header"]["template"] == "grey").await;
    assert_eq!(old["header"]["template"], "grey", "{old}");
    assert!(old.to_string().contains("这条结论已在第 3 轮更正"), "{old}");
    assert_eq!(card_patches(&w, "om_card_1").await, patched_before);
}

#[tokio::test]
async fn stop_ends_a_running_turn_and_offers_a_retry() {
    let w = world(vec![vec![
        started("s-1"),
        Beat::Event(AgentEvent::Step(Step::ToolCall {
            id: "t1".into(),
            tool: "mcp__qtmcp__jira_search".into(),
            input: json!({"jql": "text ~ \"500\""}),
        })),
        Beat::UntilCancelled,
    ]])
    .await;
    say(&w, "om_1", "查一下", json!({})).await;
    let turns = until(&w, "开始跑", |t| {
        t.first().is_some_and(|t| t.status == TurnStatus::Running)
    })
    .await;
    // 等 Agent 真正起来，再按停止
    until(&w, "Agent 起来", |_| !w.backend.requests().is_empty()).await;
    let toast = press(&w, Action::Stop, &turns[0].id).await;
    assert!(toast.text.contains("正在停止"), "{toast:?}");

    let turns = until(&w, "停下", all_finished(1)).await;
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    let card = last_card(&w, "om_card_1").await;
    assert!(card.to_string().contains("已停止"));
    assert_eq!(button(&card), Some((Action::Retry, turns[0].id.clone())));

    let again = press(&w, Action::Stop, &turns[0].id).await;
    assert!(again.text.contains("已经结束"), "{again:?}");
}

#[tokio::test]
async fn retry_reruns_the_latest_turn_on_the_same_card() {
    let w = world(vec![
        vec![started("s-1"), failed(FailKind::Api, "overloaded")],
        vec![started("s-1"), answered("重试成功")],
    ])
    .await;
    say(&w, "om_1", "查一下", json!({})).await;
    let turns = until(&w, "第一次失败", all_finished(1)).await;
    assert_eq!(turns[0].status, TurnStatus::Failed);
    let card = last_card(&w, "om_card_1").await;
    assert_eq!(button(&card), Some((Action::Retry, turns[0].id.clone())));

    let toast = press_retry(&w, &turns[0].id).await;
    assert_eq!(toast.text, "开始重试");
    until(&w, "重试成功", |t| {
        t.len() == 1 && t[0].status == TurnStatus::Succeeded
    })
    .await;
    let card = last_card(&w, "om_card_1").await;
    assert_eq!(card["header"]["template"], "blue", "{card}");
    assert_eq!(replies(&w).await.len(), 1, "重试复用原来的卡片");

    let again = press_retry(&w, &turns[0].id).await;
    assert!(again.text.contains("不需要重试"), "{again:?}");
}

#[tokio::test]
async fn only_the_latest_turn_can_be_retried() {
    let w = world(vec![
        vec![started("s-1"), failed(FailKind::Api, "overloaded")],
        vec![started("s-1"), failed(FailKind::Api, "overloaded")],
    ])
    .await;
    say(&w, "om_1", "第一问", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    say(&w, "om_2", "第二问", in_thread("om_1")).await;
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    let toast = press_retry(&w, &turns[0].id).await;
    assert!(toast.text.contains("最新"), "{toast:?}");
}

#[tokio::test]
async fn a_lost_session_is_replaced_by_a_new_one_with_the_earlier_conclusions() {
    let w = world_custom(
        vec![
            vec![started("s-1"), answered("连接池耗尽")],
            // 续接时会话文件已经没了：没有 Started，直接失败
            vec![failed(
                FailKind::SessionNotFound,
                "No conversation found with session ID: s-1",
            )],
            vec![started("s-2"), answered("新会话里的答案")],
        ],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    say(&w, "om_2", "怎么修", in_thread("om_1")).await;
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(turns[1].status, TurnStatus::Succeeded);

    let requests = w.backend.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].session, SessionRef::Resume("s-1".into()));
    assert_eq!(requests[2].session, SessionRef::New);
    assert!(requests[2].prompt.contains("# 前情"));
    assert!(
        requests[2].prompt.contains("连接池耗尽"),
        "带上前几轮的结论"
    );
    assert!(requests[2].prompt.contains("怎么修"));
    // 之前的上下文不重拉、不重发：告诉新会话去工作目录里查
    assert!(requests[2].prompt.contains("context/transcript.md"));
    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    assert_eq!(calls.len(), 2, "续接失败不重新拉群聊记录：{calls:?}");
    let transcript =
        std::fs::read_to_string(requests[2].workdir.join("context/transcript.md")).expect("记录");
    assert!(
        transcript.contains("登录报 500") && transcript.contains("怎么修"),
        "完整记录只追加不覆盖：{transcript}"
    );
    let conversation = w
        .store
        .conversation(&turns[1].conversation_id)
        .await
        .expect("查询")
        .expect("会话");
    assert_eq!(conversation.agent_session_id.as_deref(), Some("s-2"));
}

/// 一个群一个会话：不同人的提问续接同一个 Agent 会话，但各自排一轮、各回一张卡片，
/// 不会揉成一轮；前一轮在跑时同一个人再补充，并进他自己排着的那一轮。
#[tokio::test]
async fn different_people_in_a_group_share_the_session_but_not_a_turn() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let w = world(vec![
        vec![
            started("s-1"),
            Beat::Gate(Arc::clone(&gate)),
            answered("第一个"),
        ],
        vec![started("s-1"), answered("第二个")],
        vec![started("s-1"), answered("第三个")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮开跑", |_| !w.backend.requests().is_empty()).await;
    say_as(&w, "om_2", "ou_qa", "订单页打不开").await;
    until(&w, "测试同学的提问回了卡片", |t| {
        t.len() == 2 && t[1].card_message_id.is_some()
    })
    .await;
    say(&w, "om_3", "补充：是 3.2.1 版本", json!({})).await;
    say(&w, "om_4", "日志里有 HikariPool", json!({})).await;
    until(&w, "三轮都排上", |t| {
        t.len() == 3 && t.iter().all(|x| x.card_message_id.is_some())
    })
    .await;
    gate.notify_one();
    let turns = until(&w, "三轮都结束", all_finished(3)).await;
    assert!(
        turns
            .iter()
            .all(|t| t.conversation_id == turns[0].conversation_id)
    );
    let requests = w.backend.requests();
    assert!(requests[1].prompt.contains("订单页打不开"));
    assert!(
        !requests[1].prompt.contains("3.2.1"),
        "别人的补充不进这一轮"
    );
    assert!(requests[2].prompt.contains("3.2.1") && requests[2].prompt.contains("HikariPool"));
    assert!(
        requests[1..]
            .iter()
            .all(|r| r.session == SessionRef::Resume("s-1".into())),
        "都续接同一个会话"
    );
    assert_eq!(replies(&w).await.len(), 3, "三轮三张卡片");
}

/// 在答案卡的输入框里补充：接着这个会话问，回复到那张卡片上，卡片顶上写出追问的话；
/// 结论变了就把那张卡片标成已更正。
#[tokio::test]
async fn a_supplement_typed_on_the_card_follows_up_in_the_same_session() {
    let w = world(vec![
        vec![started("s-1"), answered("连接池耗尽")],
        vec![
            started("s-1"),
            corrected("DNS 解析超时", "原来说是连接池耗尽 → 实际是 DNS 解析超时"),
        ],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    let turns = until(&w, "第一轮结束", all_finished(1)).await;
    assert_eq!(
        supplement(&w, &turns[0].id, "om_card_1", "  ").await.text,
        "内容是空的"
    );
    let toast = supplement(&w, &turns[0].id, "om_card_1", "连接池监控是正常的").await;
    assert_eq!(toast.text, "收到，正在分析");
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(turns[1].conversation_id, turns[0].conversation_id);

    let requests = w.backend.requests();
    assert_eq!(requests[1].session, SessionRef::Resume("s-1".into()));
    assert!(
        requests[1].prompt.contains("（针对第 1 轮的回答）"),
        "{}",
        requests[1].prompt
    );
    assert!(requests[1].prompt.contains("连接池监控是正常的"));
    let reply_paths: Vec<String> = w
        .server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/reply"))
        .map(|r| r.url.path().to_owned())
        .collect();
    assert_eq!(
        reply_paths.last().map(String::as_str),
        Some("/open-apis/im/v1/messages/om_card_1/reply"),
        "回复到那张卡片上：{reply_paths:?}"
    );
    let card = last_card(&w, "om_card_2").await.to_string();
    assert!(
        card.contains("追问：") && card.contains("连接池监控是正常的"),
        "{card}"
    );
    let old = card_until(&w, "om_card_1", |c| c["header"]["template"] == "grey").await;
    assert!(old.to_string().contains("这条结论已在第 2 轮更正"), "{old}");
    assert!(
        !old.to_string().contains("\"tag\":\"input\""),
        "旧卡片上不再放输入框"
    );
}

#[tokio::test]
async fn after_a_restart_open_turns_are_marked_interrupted_and_can_be_retried() {
    let w = world(vec![vec![started("s-2"), answered("重启后重试")]]).await;
    // 上次进程留下的状态：一轮跑到一半，卡片已经发出
    store_input(&w, "om_1", "查一下", json!({})).await;
    w.store
        .create_conversation(&crate::store::NewConversation {
            id: "c1",
            chat_id: "oc_1",
            chat_type: "group",
            origin: crate::store::Origin::NewThread,
            thread_id: Some("omt_1"),
            root_message_id: "om_1",
            owner_open_id: BOSS,
            backend: "claude",
            now_ms: 1,
        })
        .await
        .expect("会话");
    w.store
        .create_turn("c1", "t1", "om_1", 1)
        .await
        .expect("轮次");
    w.store
        .start_turn("t1", "查一下", None, 2)
        .await
        .expect("开跑");
    w.store
        .set_turn_card("t1", "om_card_9")
        .await
        .expect("卡片");

    // 启动时的恢复步骤（main 里的顺序）
    let interrupted = w
        .store
        .interrupt_open_turns(crate::store::now_ms())
        .await
        .expect("恢复");
    assert_eq!(interrupted.len(), 1);
    w.runner.mark_interrupted(interrupted).await;
    let card = last_card(&w, "om_card_9").await;
    assert!(card.to_string().contains("服务重启"), "{card}");
    assert_eq!(button(&card), Some((Action::Retry, "t1".to_owned())));
    assert!(
        w.store.unassigned_inputs().await.expect("查询").is_empty(),
        "被中断那一轮的消息不会自动重跑"
    );

    let toast = press_retry(&w, "t1").await;
    assert_eq!(toast.text, "开始重试", "{toast:?}");
    until(&w, "重试成功", |t| t[0].status == TurnStatus::Succeeded).await;
    let card = last_card(&w, "om_card_9").await;
    assert_eq!(card["header"]["template"], "blue", "{card}");
    assert!(replies(&w).await.is_empty(), "重试复用重启前的卡片");
}

#[tokio::test]
async fn a_follow_up_during_a_running_turn_waits_for_it_then_resumes() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let w = world(vec![
        vec![
            started("s-1"),
            Beat::Gate(Arc::clone(&gate)),
            answered("第一轮"),
        ],
        vec![started("s-1"), answered("第二轮")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮开跑", |_| !w.backend.requests().is_empty()).await;
    say(&w, "om_2", "顺便看下 3.3", in_thread("om_1")).await;

    // 第二轮先回一张排队的卡片，等第一轮结束才开跑
    let queued = until(&w, "第二轮回了卡片", |t| {
        t.len() == 2 && t[1].card_message_id.is_some()
    })
    .await;
    assert_eq!(queued[1].status, TurnStatus::Queued);
    let replies = replies(&w).await;
    assert!(
        replies[1]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("排队中"),
        "{replies:?}"
    );
    tokio::time::sleep(DEBOUNCE * 2).await;
    assert_eq!(
        w.backend.requests().len(),
        1,
        "第一轮没结束，第二轮不能开跑"
    );

    gate.notify_one();
    let turns = until(&w, "两轮都结束", all_finished(2)).await;
    assert!(turns.iter().all(|t| t.status == TurnStatus::Succeeded));
    let requests = w.backend.requests();
    assert_eq!(requests[1].session, SessionRef::Resume("s-1".into()));
    assert!(requests[1].prompt.contains("顺便看下 3.3"));
}

#[tokio::test]
async fn stopping_a_queued_turn_cancels_it_without_touching_the_running_one() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let w = world(vec![vec![
        started("s-1"),
        Beat::Gate(Arc::clone(&gate)),
        answered("第一轮"),
    ]])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮开跑", |_| !w.backend.requests().is_empty()).await;
    say(&w, "om_2", "算了不用看了", in_thread("om_1")).await;
    let queued = until(&w, "第二轮回了卡片", |t| {
        t.len() == 2 && t[1].card_message_id.is_some()
    })
    .await;

    let toast = press(&w, Action::Stop, &queued[1].id).await;
    assert_eq!(toast.text, "已取消");
    gate.notify_one();
    let turns = until(&w, "两轮都结束", all_finished(2)).await;
    assert_eq!(turns[0].status, TurnStatus::Succeeded);
    assert_eq!(turns[1].status, TurnStatus::Interrupted);
    let card = last_card(&w, "om_card_2").await;
    assert!(card.to_string().contains("已取消"), "{card}");
    assert_eq!(w.backend.requests().len(), 1, "被取消的一轮没有启动 Agent");
}

/// 一个群一个会话：话题里的提问也进这个群的会话，会话不绑定某个话题。
#[tokio::test]
async fn a_question_in_a_thread_joins_the_group_session() {
    let w = world(vec![
        vec![started("s-1"), answered("第一个问题")],
        vec![started("s-1"), answered("话题里的问题")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    say(
        &w,
        "om_5",
        "帮忙看下这个话题",
        json!({"thread_id": "omt_9", "root_id": "om_r9"}),
    )
    .await;
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(turns[0].conversation_id, turns[1].conversation_id);
    let conversation = w
        .store
        .conversation(&turns[0].conversation_id)
        .await
        .expect("查询")
        .expect("会话");
    assert_eq!(conversation.thread_id, None, "群会话不绑定话题");
    assert_eq!(
        w.backend.requests()[1].session,
        SessionRef::Resume("s-1".into())
    );
}

#[tokio::test]
async fn a_backend_prefix_binds_the_conversation_to_that_backend() {
    let w = world(vec![]).await;
    say(&w, "om_1", "/codex 登录报 500", json!({})).await;
    let turns = until(&w, "结束", all_finished(1)).await;
    let conversation = w
        .store
        .conversation(&turns[0].conversation_id)
        .await
        .expect("查询")
        .expect("会话");
    assert_eq!(conversation.backend, "codex");
    let card = last_card(&w, "om_card_1").await;
    assert!(card.to_string().contains("没有配置 codex 后端"), "{card}");
    assert!(w.backend.requests().is_empty());
}

#[tokio::test]
async fn an_unexpected_tool_surface_cancels_the_turn() {
    let w = world(vec![vec![
        started_with("s-1", &["Read", "Write"]),
        Beat::Event(AgentEvent::Step(Step::Text("本不该走到这里".into()))),
        Beat::UntilCancelled,
    ]])
    .await;
    say(&w, "om_1", "查一下", json!({})).await;
    until(&w, "结束", all_finished(1)).await;
    let card = last_card(&w, "om_card_1").await;
    assert_eq!(card["header"]["template"], "red");
    let text = card.to_string();
    assert!(text.contains("自检") && text.contains("Write"), "{text}");
}

#[tokio::test]
async fn an_auth_failure_says_how_to_recover() {
    let w = world(vec![vec![
        started("s-1"),
        failed(FailKind::Auth, "Invalid API key · Please run /login"),
    ]])
    .await;
    say(&w, "om_1", "查一下", json!({})).await;
    until(&w, "结束", all_finished(1)).await;
    let text = last_card(&w, "om_card_1").await.to_string();
    assert!(text.contains("登录失效"), "{text}");
}

#[tokio::test]
async fn an_outsider_in_private_chat_is_told_once_by_text() {
    let w = world(vec![]).await;
    w.jobs
        .send(Job::Deny {
            message_id: "om_x".into(),
        })
        .await
        .expect("投递");
    for _ in 0..100 {
        let replies = replies(&w).await;
        if let Some(reply) = replies.first() {
            assert_eq!(reply["msg_type"], "text");
            assert!(
                reply["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("没有使用")
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("没有回复");
}

/// 一张小 PNG。
fn png() -> Vec<u8> {
    let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        8,
        8,
        image::Rgb([200, 30, 30]),
    ));
    let mut out = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("编码");
    out
}

async fn mount_resource(w: &World, message_id: &str, key: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/open-apis/im/v1/messages/{message_id}/resources/{key}"
        )))
        .respond_with(response)
        .mount(&w.server)
        .await;
}

fn bytes(content_type: &str, body: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", content_type)
        .set_body_bytes(body)
}

async fn say_raw(w: &World, message_id: &str, message_type: &str, content: Value, extra: Value) {
    store_raw(w, message_id, message_type, &content.to_string(), extra).await;
    w.jobs
        .send(Job::Input {
            message_id: message_id.to_owned(),
        })
        .await
        .expect("投递");
}

fn ok_items(items: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "code": 0, "msg": "ok", "data": {"has_more": false, "items": items}
    }))
}

#[tokio::test]
async fn images_files_forwards_and_the_group_window_reach_the_agent_in_their_layers() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("有附件的答案")]],
        Duration::from_secs(1),
        Some((30, Duration::from_secs(7200))),
    )
    .await;
    let thread = json!({"thread_id": "omt_9", "root_id": "om_r"});
    // 话题里：测试同学发了日志
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "thread"))
        .respond_with(ok_items(json!([
            {"message_id": "om_log", "msg_type": "file", "create_time": "1790584700000",
             "thread_id": "omt_9",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"file_key\":\"file_log\",\"file_name\":\"app.log\"}"}}
        ])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    // 群里最近：有人说发了版，还贴了一张截图
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ok_items(json!([
            {"message_id": "om_w2", "msg_type": "image", "create_time": "1790584600000",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"image_key\":\"img_w\"}"}},
            {"message_id": "om_w1", "msg_type": "text", "create_time": "1790584500000",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"text\":\"昨天发版了 3.2.1\"}"}}
        ])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    // 提问附带的合并转发：一句话和一张取不到的图
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_fwd"))
        .respond_with(ok_items(json!([
            {"message_id": "om_fwd", "msg_type": "merge_forward", "create_time": "1790584000000",
             "sender": {"id": BOSS, "sender_type": "user"},
             "body": {"content": "{\"content\":\"Merged and Forwarded Message\"}"}},
            {"message_id": "c1", "msg_type": "text", "create_time": "1790583000000",
             "upper_message_id": "om_fwd",
             "sender": {"id": "ou_field", "sender_type": "user"},
             "body": {"content": "{\"text\":\"现场：登录 500，单号 XYZ-7\"}"}},
            {"message_id": "c2", "msg_type": "image", "create_time": "1790583001000",
             "upper_message_id": "om_fwd",
             "sender": {"id": "ou_field", "sender_type": "user"},
             "body": {"content": "{\"image_key\":\"img_fwd\"}"}}
        ])))
        .mount(&w.server)
        .await;
    mount_resource(&w, "om_img", "img_q", bytes("image/png", png())).await;
    mount_resource(&w, "om_w2", "img_w", bytes("image/png", png())).await;
    let log = format!(
        "{}2026-09-28 10:00:00 ERROR HikariPool - Connection is not available\n",
        "2026-09-28 09:59:59 INFO ok\n".repeat(10)
    );
    mount_resource(
        &w,
        "om_log",
        "file_log",
        bytes("text/plain", log.into_bytes()),
    )
    .await;
    mount_resource(
        &w,
        "om_fwd",
        "img_fwd",
        ResponseTemplate::new(400).set_body_json(json!({"code": 234043, "msg": "unsupported"})),
    )
    .await;

    say(&w, "om_txt", "为什么登录 500", thread.clone()).await;
    say_raw(
        &w,
        "om_img",
        "image",
        json!({"image_key": "img_q"}),
        thread.clone(),
    )
    .await;
    say_raw(
        &w,
        "om_fwd",
        "merge_forward",
        json!({"content": "Merged and Forwarded Message"}),
        thread,
    )
    .await;
    let turns = until(&w, "一轮结束", all_finished(1)).await;
    assert_eq!(turns[0].status, TurnStatus::Succeeded);

    let request = w.backend.requests().pop().expect("启动过");
    let prompt = &request.prompt;
    let t1 = prompt.find("# 本轮提问").expect("T1");
    let t2 = prompt.find("# 群聊记录（T2").expect("T2");
    let thread = prompt.find("## 话题里的消息").expect("话题");
    let window = prompt.find("## 群里最近的消息").expect("群消息");
    let shared = prompt
        .find("## 群聊里分享的文件、文档与图片")
        .expect("群里的文件");
    let t3 = prompt.find("# 识别到的 Jira 单（T3").expect("T3");
    assert!(
        t1 < t2 && t2 < thread && thread < window && window < shared && shared < t3,
        "{prompt}"
    );
    let slice = |from: usize, to: usize| &prompt[from..to];
    // T1：文字、转发记录、提问附带的图片
    assert!(slice(t1, t2).contains("为什么登录 500"));
    assert!(slice(t1, t2).contains("提问附带的转发记录"));
    assert!(slice(t1, t2).contains("现场：登录 500"));
    assert!(slice(t1, t2).contains("提问附带的图片"));
    // T2：群里最近的消息、话题里的日志（解析后的文字）和群里的截图
    assert!(slice(window, shared).contains("昨天发版了 3.2.1"));
    assert!(slice(shared, t3).contains("app.log"));
    assert!(slice(shared, t3).contains("Connection is not available"));
    // T3：转发记录里的单号也识别出来
    assert!(prompt[t3..].contains("- XYZ-7"));
    // 读不到的内容
    assert!(prompt.contains("合并转发里的图片取不到"), "{prompt}");

    // 两张图交给后端，都在工作目录里、确实存在
    assert_eq!(request.images.len(), 2, "{:?}", request.images);
    for image in &request.images {
        assert!(image.starts_with(&request.workdir), "{}", image.display());
        assert!(image.exists(), "{}", image.display());
    }
    assert!(request.workdir.join("attachments/1").read_dir().is_ok());

    // 进度卡上写明读了什么、缺了什么
    let requests = w.server.received_requests().await.expect("请求记录");
    let read_card = requests
        .iter()
        .filter(|r| r.method.as_str() == "PATCH")
        .map(|r| String::from_utf8_lossy(&r.body).to_string())
        .find(|body| body.contains("已读取"))
        .expect("进度卡上有「已读取」");
    assert!(read_card.contains("2 张图片"), "{read_card}");
    assert!(read_card.contains("1 个文件"), "{read_card}");
    assert!(read_card.contains("⚠️"), "{read_card}");
}

#[tokio::test]
async fn every_turn_in_a_group_reads_the_new_group_messages() {
    let w = world_custom(
        vec![
            vec![started("s-1"), answered("第一轮")],
            vec![started("s-1"), answered("第二轮")],
        ],
        DEBOUNCE,
        Some((30, Duration::from_secs(7200))),
    )
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    say(&w, "om_2", "怎么修", in_thread("om_1")).await;
    until(&w, "第二轮结束", all_finished(2)).await;
    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    // 从哪个时刻起取见 a_group_follow_up_reads_only_what_was_said_since_the_last_turn
    assert_eq!(calls.len(), 2, "每轮都取群里的新消息：{calls:?}");
}

fn chat_text(id: &str, at_ms: &str, text: &str) -> Value {
    json!({"message_id": id, "msg_type": "text", "create_time": at_ms,
           "sender": {"id": "ou_qa", "sender_type": "user"},
           "body": {"content": json!({"text": text}).to_string()}})
}

fn chat_calls(requests: &[wiremock::Request]) -> Vec<String> {
    requests
        .iter()
        .filter(|r| {
            r.url.path() == "/open-apis/im/v1/messages"
                && r.url
                    .query()
                    .is_some_and(|q| q.contains("container_id_type=chat"))
        })
        .map(|r| r.url.query().unwrap_or_default().to_owned())
        .collect()
}

/// 群聊上下文按条数封顶：一页 50 条，不够就翻页，凑够为止；时长 0 表示不限时间。
#[tokio::test]
async fn the_group_window_pages_until_it_has_enough_messages() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("答案")]],
        DEBOUNCE,
        Some((3, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .and(query_param("page_token", "p2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "msg": "ok", "data": {"has_more": true, "page_token": "p3", "items": [
                chat_text("om_w2", "1790584200000", "第二条"),
                chat_text("om_w1", "1790584100000", "第一条"),
            ]}
        })))
        .with_priority(1)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "msg": "ok", "data": {"has_more": true, "page_token": "p2", "items": [
                chat_text("om_w4", "1790584400000", "第四条"),
                chat_text("om_w3", "1790584300000", "第三条"),
            ]}
        })))
        .with_priority(2)
        .mount(&w.server)
        .await;
    say(&w, "om_1", "看下上面的讨论", json!({})).await;
    until(&w, "结束", all_finished(1)).await;

    let prompt = &w.backend.requests()[0].prompt;
    for kept in ["第四条", "第三条", "第二条"] {
        assert!(prompt.contains(kept), "缺 {kept}：{prompt}");
    }
    assert!(!prompt.contains("第一条"), "超出条数的不要：{prompt}");
    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    assert_eq!(calls.len(), 2, "凑够 3 条就不再翻第三页：{calls:?}");
    assert!(calls[0].contains("start_time=0"), "{calls:?}");
}

/// 答案里引用的截图上传后显示在对应段落里，图表直接画出来；工作目录外的路径不上传。
#[tokio::test]
async fn screenshots_and_charts_in_the_answer_show_up_in_the_card() {
    let mut answer = answer_json("看截图就明白了");
    answer["sections"][0]["images"] = json!([
        // 附件存成 webp 还是 png 看哪个更小，两个都写上，只有一个存在
        {"path": "attachments/1/00-image.webp", "caption": "报错截图"},
        {"path": "attachments/1/00-image.png", "caption": "报错截图"},
        {"path": "../../../etc/hostname", "caption": "越界"},
        {"path": "/etc/hostname", "caption": "绝对路径"}
    ]);
    answer["sections"][1]["charts"] = json!([{
        "title": "各版本失败次数", "type": "bar",
        "categories": ["3.2.0", "3.2.1"], "series": [{"name": "失败", "values": [2, 17]}]
    }]);
    let w = world(vec![vec![
        started("s-1"),
        Beat::Event(AgentEvent::Finished(Outcome::Success {
            structured: Some(answer),
            text: String::new(),
            turns: 3,
        })),
    ]])
    .await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/images"))
        .respond_with(ok_items_data(json!({"image_key": "img_v3_shot"})))
        .mount(&w.server)
        .await;
    mount_resource(&w, "om_img", "img_1", bytes("image/png", png())).await;
    say_raw(
        &w,
        "om_img",
        "image",
        json!({"image_key": "img_1"}),
        json!({}),
    )
    .await;
    until(&w, "结束", all_finished(1)).await;

    let card = last_card(&w, "om_card_1").await;
    let text = card.to_string();
    assert!(text.contains("\"img_key\":\"img_v3_shot\""), "{text}");
    assert!(text.contains("报错截图"), "{text}");
    assert!(text.contains("\"tag\":\"chart\""), "{text}");
    assert!(text.contains("各版本失败次数"), "{text}");
    let uploads = w
        .server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| r.url.path() == "/open-apis/im/v1/images")
        .count();
    assert_eq!(uploads, 1, "只上传工作目录里真实存在的那张");
    let turn = &w.store.all_turns().await[0];
    let record = w
        .store
        .turn_record(&turn.id)
        .await
        .expect("查询")
        .expect("有记录");
    assert!(
        record
            .answer_json
            .unwrap_or_default()
            .contains("img_v3_shot"),
        "image_key 随答案落库，旧卡片重画时不用再传"
    );
}

/// 提问里说了「最近 N 条」，群聊记录就只取 N 条，prompt 里注明范围。
#[tokio::test]
async fn an_explicit_count_in_the_question_narrows_the_group_window() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("答案")]],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "msg": "ok", "data": {"has_more": true, "page_token": "p2", "items": [
                chat_text("om_w4", "1790584400000", "第四条"),
                chat_text("om_w3", "1790584300000", "第三条"),
                chat_text("om_w2", "1790584200000", "第二条"),
            ]}
        })))
        .with_priority(1)
        .mount(&w.server)
        .await;
    say(&w, "om_1", "总结下最近2条消息", json!({})).await;
    until(&w, "结束", all_finished(1)).await;

    let prompt = &w.backend.requests()[0].prompt;
    assert!(prompt.contains("只看最近 2 条消息"), "{prompt}");
    assert!(
        prompt.contains("第四条") && prompt.contains("第三条"),
        "{prompt}"
    );
    assert!(!prompt.contains("第二条"), "超出提问范围的不要：{prompt}");
    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    assert_eq!(calls.len(), 1, "够了就不再翻页：{calls:?}");
}

/// 先发截图、再打字提问：本来各开一个会话，提问那一轮就看不到截图；
/// 同一个人在合并窗口内连发的并进同一轮，只回一张卡片。
#[tokio::test]
async fn a_screenshot_followed_by_the_question_is_answered_in_one_turn() {
    let w = world_with(
        vec![vec![started("s-1"), answered("看了截图")]],
        Duration::from_millis(500),
    )
    .await;
    mount_resource(&w, "om_img", "img_1", bytes("image/png", png())).await;
    say_raw(
        &w,
        "om_img",
        "image",
        json!({"image_key": "img_1"}),
        json!({}),
    )
    .await;
    // 超过文字的窗口（0.5 秒），但还在只有附件时的窗口（2 秒）里
    tokio::time::sleep(Duration::from_secs(1)).await;
    say(&w, "om_txt", "这个报错什么原因", json!({})).await;
    let turns = until(&w, "一轮结束", all_finished(1)).await;
    assert_eq!(turns.len(), 1, "{turns:?}");

    let requests = w.backend.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].prompt.contains("这个报错什么原因"));
    assert_eq!(requests[0].images.len(), 1, "提问那一轮带着截图");
    assert_eq!(replies(&w).await.len(), 1, "只回一张卡片");
}

/// 进群、改群名这类系统提示不是谁说的话，不放进聊天记录；空的层也不出现。
#[tokio::test]
async fn system_notices_stay_out_of_the_chat_record() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("答案")]],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ok_items(json!([
            {"message_id": "om_sys", "msg_type": "system", "create_time": "1790584200000",
             "sender": {"id": "", "sender_type": ""},
             "body": {"content": "{\"template\":\"{from_user} 邀请 {to_chatters} 加入群聊\"}"}},
            chat_text("om_w1", "1790584100000", "昨天发版了")
        ])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    say(&w, "om_1", "上面说了什么", json!({})).await;
    until(&w, "结束", all_finished(1)).await;

    let prompt = &w.backend.requests()[0].prompt;
    assert!(prompt.contains("昨天发版了"), "{prompt}");
    assert!(!prompt.contains("系统消息"), "{prompt}");
    assert!(!prompt.contains("（无）"), "空的层不出现：{prompt}");
}

/// 群聊记录是背景：里面提到的单不进 T2（T2 要求先读，几百条消息里的单都读一遍太慢），
/// 只收提问本身的。
#[tokio::test]
async fn jira_keys_from_the_group_window_are_not_pre_read() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("答案")]],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ok_items(json!([chat_text(
            "om_w1",
            "1790584100000",
            "昨天 ABC-99 也挂了"
        )])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    say(&w, "om_1", "看下 QA-1 为什么失败", json!({})).await;
    until(&w, "结束", all_finished(1)).await;

    let prompt = &w.backend.requests()[0].prompt;
    let t2 = prompt
        .split("# 识别到的 Jira 单")
        .nth(1)
        .and_then(|rest| rest.split("\n# ").next())
        .expect("T2");
    assert!(t2.contains("QA-1"), "{t2}");
    assert!(!t2.contains("ABC-99"), "{t2}");
    assert!(prompt.contains("昨天 ABC-99 也挂了"), "群聊记录本身照样给");
}

/// 群里不开话题：引用卡片追问时，只取上一轮之后群里的新消息。
#[tokio::test]
async fn a_group_follow_up_reads_only_what_was_said_since_the_last_turn() {
    let w = world_custom(
        vec![
            vec![started("s-1"), answered("第一轮")],
            vec![started("s-1"), answered("第二轮")],
        ],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    let quote =
        json!({"parent_id": "om_card_1", "root_id": "om_1", "create_time": "1790588400000"});
    say(&w, "om_2", "怎么修", quote).await;
    let turns = until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(turns[0].conversation_id, turns[1].conversation_id);

    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    assert_eq!(calls.len(), 2, "追问也取群里的新消息：{calls:?}");
    assert!(calls[0].contains("start_time=0"), "{calls:?}");
    // 第一轮的提问发在 1790584800（秒），游标之后一秒起
    assert!(calls[1].contains("start_time=1790584801"), "{calls:?}");
}

/// 等到某次进度卡更新里出现 `text`，返回那张卡片。
async fn progress_with(w: &World, text: &str) -> String {
    for _ in 0..200 {
        let requests = w.server.received_requests().await.expect("请求记录");
        if let Some(body) = requests
            .iter()
            .filter(|r| r.method.as_str() == "PATCH")
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .find(|body| body.contains(text))
        {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("进度卡上一直没有出现「{text}」");
}

/// 分析中：模型说的进展和写到一半的结论先露在进度卡上，不用干等整份答案。
#[tokio::test]
async fn the_progress_card_shows_what_the_model_found_and_the_conclusion_as_it_is_written() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let w = world(vec![vec![
        started("s-1"),
        Beat::Event(AgentEvent::Step(Step::Text(
            "已定位到代码：登录会按主键更新\n用户表那一行".into(),
        ))),
        Beat::Gate(Arc::clone(&gate)),
        Beat::Event(AgentEvent::Draft(ai_boot_agent::Draft {
            title: Some("登录锁等待超时".into()),
            summary: Some("用户表那一行的锁被别的事务占住".into()),
        })),
        Beat::Gate(Arc::clone(&gate)),
        answered("登录锁等待超时"),
    ]])
    .await;
    say(&w, "om_1", "看下这个报错", json!({})).await;
    let thought = progress_with(&w, "已定位到代码").await;
    assert!(
        thought.contains("登录会按主键更新 用户表那一行"),
        "多行压成一行：{thought}"
    );
    gate.notify_one();
    let draft = progress_with(&w, "用户表那一行的锁被别的事务占住").await;
    assert!(draft.contains("结论（还在写详情）"), "{draft}");
    gate.notify_one();
    until(&w, "结束", all_finished(1)).await;
}

/// 会话上下文已经很长：下一轮把前几轮收拢成结论、换新会话，之前的答案全文留在
/// 工作目录里，之后续接新会话。
#[tokio::test]
async fn a_long_session_is_compacted_into_conclusions_for_the_next_turn() {
    let w = world(vec![
        vec![started("s-1"), long_context(130_000), answered("第一轮")],
        vec![started("s-2"), long_context(20_000), answered("第二轮")],
        vec![started("s-2"), answered("第三轮")],
    ])
    .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    let quote =
        json!({"parent_id": "om_card_1", "root_id": "om_1", "create_time": "1790588400000"});
    say(&w, "om_2", "怎么修", quote).await;
    until(&w, "第二轮结束", all_finished(2)).await;
    let quote =
        json!({"parent_id": "om_card_2", "root_id": "om_1", "create_time": "1790588500000"});
    say(&w, "om_3", "修完要重启吗", quote).await;
    until(&w, "第三轮结束", all_finished(3)).await;

    let requests = w.backend.requests();
    assert_eq!(requests[1].session, SessionRef::New, "上下文太长就换新会话");
    let prompt = &requests[1].prompt;
    assert!(
        prompt.starts_with("# 前情（之前的会话上下文太长"),
        "{prompt}"
    );
    assert!(
        prompt.contains("第一轮：连接池耗尽"),
        "前几轮的结论要带上：{prompt}"
    );
    assert!(prompt.contains("怎么修"));
    // 第一轮的完整答案留在工作目录里，新会话要细节去这里查
    let answers = std::fs::read_to_string(requests[0].workdir.join("context/answers.md"))
        .expect("answers.md");
    assert!(answers.contains("## 第 1 轮：第一轮"), "{answers}");
    assert!(answers.contains("调大连接池"));
    // 新会话不长，接着续接它
    assert_eq!(requests[2].session, SessionRef::Resume("s-2".into()));
}

/// 引用的卡片所在的会话已经清掉了：卡片上的旧结论作为提问引用的内容交给模型，
/// 不然「这个还是不行」里的「这个」无从知道。
#[tokio::test]
async fn quoting_a_purged_card_hands_its_old_answer_to_the_model() {
    let w = world(vec![vec![started("s-1"), answered("接着排查")]]).await;
    let card = json!({"schema": "2.0", "header": {"title": {"tag": "plain_text", "content": "登录锁等待超时"}},
                      "body": {"elements": [{"tag": "markdown", "content": "旧结论：用户表那一行的锁被占住"}]}});
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_gone"))
        .respond_with(ok_items(json!([
            {"message_id": "om_gone", "msg_type": "interactive", "create_time": "1790500000000",
             "sender": {"id": "cli_x", "sender_type": "app"},
             "body": {"content": card.to_string()}}
        ])))
        .mount(&w.server)
        .await;
    say(
        &w,
        "om_1",
        "这个还是不行",
        json!({"parent_id": "om_gone", "root_id": "om_gone"}),
    )
    .await;
    until(&w, "结束", all_finished(1)).await;
    let prompt = &w.backend.requests()[0].prompt;
    let quoted = prompt.find("## 提问引用的消息").expect("有引用");
    assert!(
        prompt[quoted..].contains("机器人（之前的回答）"),
        "{prompt}"
    );
    assert!(
        prompt[quoted..].contains("旧结论：用户表那一行的锁被占住"),
        "{prompt}"
    );
}

/// 提问之后、开跑之前群里又来了消息：这一轮已经带上了，下一轮不能再发一遍。
#[tokio::test]
async fn messages_that_arrived_after_the_question_are_not_sent_again() {
    let w = world_custom(
        vec![
            vec![started("s-1"), answered("第一轮")],
            vec![started("s-1"), answered("第二轮")],
        ],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .and(query_param("start_time", "0"))
        .respond_with(ok_items(json!([chat_text(
            "om_late",
            "1790584900000",
            "我这边也复现了"
        )])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    say(&w, "om_1", "登录报 500", json!({})).await;
    until(&w, "第一轮结束", all_finished(1)).await;
    assert!(w.backend.requests()[0].prompt.contains("我这边也复现了"));
    let quote =
        json!({"parent_id": "om_card_1", "root_id": "om_1", "create_time": "1790588400000"});
    say(&w, "om_2", "怎么修", quote).await;
    until(&w, "第二轮结束", all_finished(2)).await;
    let calls = chat_calls(&w.server.received_requests().await.expect("请求记录"));
    // 游标跟到这一轮带上的最新一条群消息（1790584900 秒），不是停在提问时间
    assert!(calls[1].contains("start_time=1790584901"), "{calls:?}");
}

/// 群里有人把客户那边的聊天记录合并转发进来，再 @ 机器人：转发的内容要展开。
#[tokio::test]
async fn a_forward_shared_in_the_group_is_expanded_into_the_group_records() {
    let w = world_custom(
        vec![vec![started("s-1"), answered("看了转发")]],
        DEBOUNCE,
        Some((30, Duration::ZERO)),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .respond_with(ok_items(json!([
            {"message_id": "om_wf", "msg_type": "merge_forward", "create_time": "1790584700000",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"content\":\"Merged and Forwarded Message\"}"}}
        ])))
        .with_priority(1)
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_wf"))
        .respond_with(ok_items(json!([
            {"message_id": "om_wf", "msg_type": "merge_forward", "create_time": "1790584700000",
             "sender": {"id": "ou_qa", "sender_type": "user"},
             "body": {"content": "{\"content\":\"Merged and Forwarded Message\"}"}},
            {"message_id": "k1", "msg_type": "text", "create_time": "1790583000000",
             "upper_message_id": "om_wf",
             "sender": {"id": "ou_customer", "sender_type": "user"},
             "body": {"content": "{\"text\":\"客户：升级到 3.2.1 后登录一直转圈\"}"}}
        ])))
        .mount(&w.server)
        .await;
    say(&w, "om_1", "看下上面转发的", json!({})).await;
    until(&w, "结束", all_finished(1)).await;

    let prompt = &w.backend.requests()[0].prompt;
    let t2 = prompt.find("# 群聊记录（T2").expect("T2");
    let forward = prompt.find("在群里转发的聊天记录").expect("转发展开了");
    assert!(t2 < forward, "{prompt}");
    assert!(prompt[forward..].contains("升级到 3.2.1 后登录一直转圈"));
    assert!(
        !prompt.contains("## 话题里的消息"),
        "不在话题里就不写话题的标题：{prompt}"
    );
}

const DOC_LINK: &str = "https://x.feishu.cn/docx/doxcnAbcdefghijklmnop";

async fn mount_doc(w: &World) {
    Mock::given(method("POST"))
        .and(path("/open-apis/drive/v1/metas/batch_query"))
        .respond_with(ok_items_data(json!({"metas": [
            {"doc_token": "doxcnAbcdefghijklmnop", "doc_type": "docx", "title": "故障复盘"}
        ]})))
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/docs/v1/content"))
        .respond_with(ok_items_data(
            json!({"content": "# 故障复盘\n根因：连接池耗尽（见 QA-77）"}),
        ))
        .mount(&w.server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/open-apis/docx/v1/documents/doxcnAbcdefghijklmnop/blocks",
        ))
        .respond_with(ok_items_data(json!({"has_more": false, "items": []})))
        .mount(&w.server)
        .await;
}

fn ok_items_data(data: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"code": 0, "msg": "ok", "data": data}))
}

async fn direct_messages(w: &World) -> Vec<Value> {
    w.server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| {
            r.method.as_str() == "POST"
                && r.url.path() == "/open-apis/im/v1/messages"
                && r.url
                    .query()
                    .is_some_and(|q| q.contains("receive_id_type=open_id"))
        })
        .map(|r| serde_json::from_slice(&r.body).expect("JSON"))
        .collect()
}

#[tokio::test]
async fn a_linked_document_is_read_with_the_askers_authorisation() {
    let w = world_full(
        vec![vec![started("s-1"), answered("看过文档的答案")]],
        DEBOUNCE,
        None,
        true,
        None,
    )
    .await;
    let now = crate::store::now_ms();
    w.store
        .save_user_token(&crate::store::NewToken {
            open_id: BOSS,
            access_token: "u-valid",
            access_expires_at: now + 3_600_000,
            refresh_token: Some("r-1"),
            refresh_expires_at: Some(now + 604_800_000),
            scopes: "offline_access",
            now_ms: now,
        })
        .await
        .expect("授权");
    mount_doc(&w).await;
    say(
        &w,
        "om_1",
        &format!("看下 {DOC_LINK} 里说的问题"),
        json!({}),
    )
    .await;
    until(&w, "结束", all_finished(1)).await;

    let request = w.backend.requests().pop().expect("启动过");
    let t2 = request.prompt.find("# 识别到的 Jira 单").expect("T2");
    let t1 = &request.prompt[..t2];
    assert!(t1.contains("## 提问附带的文件与文档"), "{}", request.prompt);
    assert!(t1.contains("故障复盘"));
    assert!(t1.contains("连接池耗尽"));
    assert!(request.prompt.contains("- QA-77"), "文档里的单号也识别出来");
    let requests = w.server.received_requests().await.expect("请求记录");
    let doc_call = requests
        .iter()
        .find(|r| r.url.path() == "/open-apis/docs/v1/content")
        .expect("读过文档");
    assert_eq!(
        doc_call
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer u-valid"),
        "用提问人的 token 读"
    );
    assert!(direct_messages(&w).await.is_empty());
}

#[tokio::test]
async fn without_authorisation_the_turn_goes_on_and_the_asker_gets_one_card() {
    let w = world_full(
        vec![
            vec![started("s-1"), answered("第一轮")],
            vec![started("s-1"), answered("第二轮")],
        ],
        DEBOUNCE,
        None,
        true,
        None,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/messages"))
        .respond_with(ok_items_data(json!({"message_id": "om_dm"})))
        .mount(&w.server)
        .await;
    say(&w, "om_1", &format!("看下 {DOC_LINK}"), json!({})).await;
    let turns = until(&w, "第一轮结束", all_finished(1)).await;
    assert_eq!(turns[0].status, TurnStatus::Succeeded, "没授权也照常回答");
    let prompt = &w.backend.requests()[0].prompt;
    assert!(prompt.contains("没能读取的内容"));
    assert!(prompt.contains("已私聊发了授权链接"), "{prompt}");

    let sent = direct_messages(&w).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["receive_id"], BOSS);
    assert!(
        sent[0]["content"]
            .as_str()
            .unwrap_or_default()
            .contains("/oauth/feishu/start?user=ou_boss")
    );

    say(&w, "om_2", &format!("再看下 {DOC_LINK}"), in_thread("om_1")).await;
    until(&w, "第二轮结束", all_finished(2)).await;
    assert_eq!(direct_messages(&w).await.len(), 1, "不重复私聊");
}

/// 卡片上某个按钮的回传值。
fn button_value(card: &Value, id: &str) -> Value {
    card["body"]["elements"]
        .as_array()
        .expect("elements")
        .iter()
        .find(|e| e["tag"] == "button" && e["element_id"] == id)
        .map(|b| b["behaviors"][0]["value"].clone())
        .unwrap_or_else(|| panic!("没有按钮 {id}：{card}"))
}

async fn press_value(w: &World, value: &Value) -> Toast {
    let (action, turn_id) = Action::parse(value).expect("认得出的按钮");
    for _ in 0..100 {
        let toast = press(w, action.clone(), &turn_id).await;
        if !toast.text.contains("还有一轮没结束") {
            return toast;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("一直有一轮没结束");
}

#[tokio::test]
async fn a_resolved_question_becomes_a_closure_plan_that_is_written_back_once() {
    let w = world_full(
        vec![
            vec![started("s-1"), answered("登录偶发 500")],
            vec![started("s-1"), answered("闭环：登录偶发 500")],
        ],
        DEBOUNCE,
        None,
        false,
        Some(json!({
            "jira_issue.get": {"result": {"key": "ABC-12", "summary": "登录 500"}},
            "jira_comment.list": {"result": {"items": []}},
            "jira_comment.add": {"result": {
                "id": "10001", "self": "https://jira.example.com/rest/api/2/issue/1/comment/10001"
            }}
        })),
    )
    .await;
    say(&w, "om_1", "登录偶发 500 是什么原因", json!({})).await;
    let first = until(&w, "回答完", all_finished(1)).await[0].id.clone();

    // 新答案卡不再带「已解决」；早先发出的卡片上还有，点了照样走这条流程
    let toast = press_when_idle(&w, Action::Resolve, &first).await;
    assert_eq!(toast.text, "开始生成闭环方案", "{toast:?}");
    let turns = until(&w, "闭环方案生成完", all_finished(2)).await;
    assert_eq!(turns[1].kind, crate::store::TurnKind::Resolve);
    assert_eq!(turns[1].status, TurnStatus::Succeeded);

    // 闭环轮次续接原来的会话，只给要求
    let request = w.backend.requests().pop().expect("启动过");
    assert_eq!(request.session, SessionRef::Resume("s-1".into()));
    assert!(request.prompt.starts_with("# 问题已确认解决"));
    // 闭环卡片回在答案卡下面，绿色，带写回按钮
    let replies = replies(&w).await;
    assert_eq!(replies.len(), 2);
    let closure = last_card(&w, "om_card_2").await;
    assert_eq!(closure["header"]["template"], "green", "{closure}");
    let text = closure.to_string();
    assert!(text.contains("闭环方案"));
    assert!(text.contains("写入 Jira ABC-12 登录 500"), "{text}");
    assert!(text.contains("写入 Confluence 空间 DEV"));

    // 写回 Jira，只写一次
    let writeback = button_value(&closure, "writeback_0");
    let toast = press_value(&w, &writeback).await;
    assert!(toast.text.contains("开始写入"), "{toast:?}");
    let mut written = false;
    for _ in 0..200 {
        let rows = w.store.writebacks(&turns[1].id).await.expect("查询");
        if rows
            .first()
            .is_some_and(|r| r.status == crate::store::WritebackStatus::Done)
        {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(written, "写回没有完成");
    let again = press_value(&w, &writeback).await;
    assert!(again.text.contains("已经写过了"), "{again:?}");
    let adds = crate::writeback::mcp::tests::calls(w._dir.path())
        .iter()
        .filter(|c| c["arguments"]["action"] == "add")
        .count();
    assert_eq!(adds, 1);
    // 卡片更新成已写入
    let mut updated = false;
    for _ in 0..100 {
        let card = w
            .server
            .received_requests()
            .await
            .expect("请求记录")
            .iter()
            .rev()
            .find(|r| r.method.as_str() == "PATCH" && r.url.path().ends_with("om_card_2"))
            .map(|r| String::from_utf8_lossy(&r.body).to_string())
            .unwrap_or_default();
        if card.contains("已写入 Jira ABC-12") {
            updated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(updated, "闭环卡片应当显示已写入");

    // 旧的答案卡上再点「已解决」：已经不是最新一轮
    let stale = press(&w, Action::Resolve, &first).await;
    assert!(stale.text.contains("最新"), "{stale:?}");
}

#[tokio::test]
async fn clearing_the_context_stops_the_chat_erases_its_data_and_starts_over() {
    let w = world_custom(
        vec![
            vec![started("s-1"), Beat::UntilCancelled],
            vec![started("s-2"), answered("重新开始后的回答")],
        ],
        DEBOUNCE,
        Some((500, Duration::ZERO)),
    )
    .await;
    say(&w, "om_q1", "看下这个报错", json!({})).await;
    let turns = until(&w, "第一轮开跑", |turns| {
        turns.iter().any(|t| t.status == TurnStatus::Running)
    })
    .await;
    let old_conversation = turns[0].conversation_id.clone();
    let workdir = w._dir.path().join("sessions").join(&old_conversation);
    assert!(workdir.is_dir(), "第一轮的工作目录应当已经建好");

    // 在跑的那一轮被停下，这个群的全部数据删光，回一句确认
    say(
        &w,
        "om_clear",
        "清空上下文，重新开始",
        json!({"create_time": "1790584900000"}),
    )
    .await;
    for _ in 0..250 {
        if replies(&w).await.iter().any(|r| {
            r["content"]
                .as_str()
                .is_some_and(|c| c.contains("已清空上下文"))
        }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let confirm = replies(&w)
        .await
        .into_iter()
        .find(|r| {
            r["content"]
                .as_str()
                .is_some_and(|c| c.contains("已清空上下文"))
        })
        .expect("应当回复已清空");
    assert!(
        confirm["content"]
            .as_str()
            .is_some_and(|c| c.contains("1 轮")),
        "{confirm}"
    );
    assert!(w.store.all_turns().await.is_empty(), "轮次应当删光");
    assert!(
        w.store
            .chat_conversation_ids("oc_1")
            .await
            .expect("查询")
            .is_empty()
    );
    for message_id in ["om_q1", "om_clear"] {
        assert!(
            w.store.input(message_id).await.expect("查询").is_none(),
            "{message_id} 应当从收件箱删掉"
        );
    }
    assert!(!workdir.exists(), "工作目录应当删掉");
    assert_eq!(
        w.store.context_reset("oc_1").await.expect("查询"),
        Some(1_790_584_900_000)
    );

    // 再问：新会话，群聊记录只取清空之后的
    say(
        &w,
        "om_q2",
        "新的问题",
        json!({"create_time": "1790585000000"}),
    )
    .await;
    let turns = until(&w, "清空后的那一轮结束", all_finished(1)).await;
    assert_ne!(turns[0].conversation_id, old_conversation);
    let requests = w.backend.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].session, SessionRef::New);
    assert!(!requests[1].prompt.contains("看下这个报错"));
    let window_since: Vec<String> = w
        .server
        .received_requests()
        .await
        .expect("请求记录")
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/open-apis/im/v1/messages")
        .filter_map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "start_time")
                .map(|(_, v)| v.into_owned())
        })
        .collect();
    assert_eq!(
        window_since.last().map(String::as_str),
        Some("1790584901"),
        "{window_since:?}"
    );
}

#[tokio::test]
async fn files_named_in_the_answer_are_sent_under_the_card_and_escapes_are_refused() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut answer = answer_json("给出检查脚本");
    answer["files"] = json!(["out/check.sh", "../../etc/passwd"]);
    let w = world(vec![vec![
        started("s-1"),
        Beat::Gate(Arc::clone(&gate)),
        Beat::Event(AgentEvent::Finished(Outcome::Success {
            structured: Some(answer),
            text: String::new(),
            turns: 2,
        })),
    ]])
    .await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/files"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"code": 0, "msg": "ok", "data": {"file_key": "file_v3_check"}}),
            ),
        )
        .expect(1)
        .mount(&w.server)
        .await;
    say(&w, "om_q1", "给我一个检查脚本", json!({})).await;
    let turns = until(&w, "开跑", |turns| {
        turns.iter().any(|t| t.status == TurnStatus::Running)
    })
    .await;
    // Agent 在工作目录里写好脚本，再交答案
    let out = w
        ._dir
        .path()
        .join("sessions")
        .join(&turns[0].conversation_id)
        .join("out");
    std::fs::create_dir_all(&out).expect("目录");
    std::fs::write(out.join("check.sh"), "#!/bin/sh\nss -s\n").expect("脚本");
    gate.notify_one();
    until(&w, "结束", all_finished(1)).await;

    let mut sent = Vec::new();
    for _ in 0..100 {
        sent = w
            .server
            .received_requests()
            .await
            .expect("请求记录")
            .into_iter()
            .filter(|r| {
                r.method.as_str() == "POST"
                    && r.url.path() == "/open-apis/im/v1/messages/om_card_1/reply"
            })
            .map(|r| serde_json::from_slice::<Value>(&r.body).expect("JSON"))
            .collect();
        if sent.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        sent.iter().any(|r| r["msg_type"] == "file"
            && r["content"]
                .as_str()
                .is_some_and(|c| c.contains("file_v3_check"))),
        "脚本应当作为文件回复在答案卡下面：{sent:?}"
    );
    assert!(
        sent.iter().any(|r| r["msg_type"] == "text"
            && r["content"]
                .as_str()
                .is_some_and(|c| c.contains("../../etc/passwd") && c.contains("没有发出"))),
        "越界的路径要拒绝并说明：{sent:?}"
    );
}

#[tokio::test]
async fn clear_typed_into_the_card_input_really_clears() {
    let w = world(vec![vec![started("s-1"), answered("第一轮的结论")]]).await;
    say(&w, "om_q1", "看下这个报错", json!({})).await;
    let turns = until(&w, "第一轮结束", all_finished(1)).await;
    let toast = supplement(&w, &turns[0].id, "om_card_1", "清空上下文").await;
    assert!(toast.text.contains("正在清空"), "{}", toast.text);
    let mut confirmed = false;
    for _ in 0..250 {
        confirmed = w
            .server
            .received_requests()
            .await
            .expect("请求记录")
            .iter()
            .any(|r| {
                r.url.path() == "/open-apis/im/v1/messages/om_card_1/reply"
                    && String::from_utf8_lossy(&r.body).contains("已清空上下文")
            });
        if confirmed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(confirmed, "确认要回复在那张卡片下面");
    assert!(w.store.all_turns().await.is_empty());
    assert_eq!(w.backend.requests().len(), 1, "清空不能交给模型当追问");
}
