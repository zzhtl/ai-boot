//! 写回执行：假 MCP server 扮演 qtmcp，wiremock 扮演飞书（卡片更新）。

use std::sync::Arc;

use ai_boot_feishu::api::ApiClient;
use secrecy::SecretString;
use serde_json::{Value, json};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::mcp::tests::{calls, fake_server};
use super::*;
use crate::answer::{Confidence, Kind, Section, Status};
use crate::store::{FinishedTurn, NewConversation, NewInput, Origin};

const BOSS: &str = "ou_boss";

fn answer(keys: &[&str]) -> Answer {
    Answer {
        kind: Kind::Diagnosis,
        title: "登录偶发 500".into(),
        status: Status::Answered,
        confidence: Confidence::High,
        summary: "连接池耗尽，3.2.2 已修复".into(),
        corrections: vec![],
        conflicts: vec![],
        sections: vec![Section {
            title: "解决方案".into(),
            body: "把 `maxPoolSize` 调到 50".into(),
            images: vec![],
            charts: vec![],
            diagrams: vec![],
        }],
        open_questions: vec![],
        references: vec![],
        jira_keys: keys.iter().map(|k| (*k).to_owned()).collect(),
        image_keys: Default::default(),
    }
}

struct Env {
    store: Store,
    server: MockServer,
    dir: tempfile::TempDir,
}

/// 一个会话里有一轮成功的闭环方案（t-resolve），卡片是 om_closure。
async fn env() -> Env {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(
            "^/open-apis/auth/v3/tenant_access_token/internal$",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 0, "tenant_access_token": "t-x", "expire": 7200})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex("^/open-apis/im/v1/messages/[^/]+$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "msg": "ok", "data": {}})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path()).await.expect("数据库");
    store
        .create_conversation(&NewConversation {
            id: "c1",
            chat_id: "oc_1",
            chat_type: "group",
            origin: Origin::NewThread,
            thread_id: Some("omt_1"),
            root_message_id: "om_1",
            owner_open_id: BOSS,
            backend: "claude",
            now_ms: 1,
        })
        .await
        .expect("会话");
    store
        .insert_input(&NewInput {
            message_id: "om_1",
            chat_id: "oc_1",
            chat_type: "group",
            sender_open_id: BOSS,
            payload: "{}",
            received_at_ms: 1,
        })
        .await
        .expect("输入");
    store
        .create_turn("c1", "t-ask", "om_1", 1)
        .await
        .expect("提问轮");
    store
        .create_resolve_turn("c1", "01999999-0000-7000-8000-000000000001", 2)
        .await
        .expect("闭环轮");
    Env { store, server, dir }
}

const RESOLVE_TURN: &str = "01999999-0000-7000-8000-000000000001";

async fn finish_resolve(env: &Env, answer: &Answer, targets: &[TargetSpec]) {
    env.store
        .set_turn_card(RESOLVE_TURN, "om_closure")
        .await
        .expect("卡片");
    env.store
        .finish_turn(&FinishedTurn {
            turn_id: RESOLVE_TURN,
            conversation_id: "c1",
            status: TurnStatus::Succeeded,
            model: "opus",
            answer_json: Some(&serde_json::to_string(answer).expect("序列化")),
            error_kind: None,
            error: None,
            tokens: 10,
            tool_calls: 0,
            duration_ms: 1000,
            session_id: None,
            session_tokens: None,
            history_cursor_ms: None,
            now_ms: 3,
        })
        .await
        .expect("结束");
    env.store
        .set_writeback_targets(
            RESOLVE_TURN,
            &serde_json::to_string(targets).expect("序列化"),
        )
        .await
        .expect("目标");
}

fn writer(env: &Env, replies: &Value, confluence: bool) -> Arc<Writer> {
    let base = url::Url::parse(&format!("{}/", env.server.uri())).expect("地址");
    let api = Arc::new(ApiClient::new(base, "cli_x", SecretString::from("s")).expect("客户端"));
    Arc::new(Writer::new(
        api,
        env.store.clone(),
        Settings {
            server: fake_server(env.dir.path(), replies),
            confluence: confluence.then(|| ("DEV".to_owned(), "12345".to_owned())),
        },
        vec!["jira.example.com".into(), "wiki.example.com".into()],
    ))
}

fn hash_of(answer: &Answer) -> String {
    format::content_hash(&format::document(answer, &document_footer(RESOLVE_TURN)))
}

fn jira_target(key: &str) -> TargetSpec {
    TargetSpec {
        target: "jira".into(),
        reference: key.into(),
        label: format!("Jira {key}"),
    }
}

/// 发到闭环卡片的最后一次更新。
async fn last_card(env: &Env) -> String {
    let requests = env.server.received_requests().await.expect("请求记录");
    let patch = requests
        .iter()
        .rev()
        .find(|r| r.method.as_str() == "PATCH")
        .expect("更新过卡片");
    String::from_utf8_lossy(&patch.body).into_owned()
}

#[tokio::test]
async fn targets_are_the_first_valid_jira_keys_with_their_titles_plus_confluence() {
    let env = env().await;
    let writer = writer(
        &env,
        &json!({"jira_issue.get": {"result": {"key": "ABC-1", "summary": "登录 500"}}}),
        true,
    );
    let targets = writer
        .targets(&answer(&[
            "ABC-1", "bad key", "ABC-1", "XYZ-2", "QA-3", "R-4",
        ]))
        .await;
    let labels: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Jira ABC-1 登录 500",
            "Jira XYZ-2 登录 500",
            "Jira QA-3 登录 500",
            "Confluence 空间 DEV"
        ]
    );
    assert_eq!(targets[3].reference, "DEV:12345");
}

#[tokio::test]
async fn only_listed_targets_of_a_finished_closure_with_the_shown_content_are_written() {
    let env = env().await;
    let answer = answer(&["ABC-1"]);
    let writer = writer(&env, &json!({}), false);
    let hash = hash_of(&answer);
    // 闭环方案还没完成
    assert!(
        writer
            .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash, BOSS)
            .await
            .is_err()
    );
    finish_resolve(&env, &answer, &[jira_target("ABC-1")]).await;
    // 普通提问轮不能写回
    assert!(
        writer
            .prepare("t-ask", Target::Jira, "ABC-1", &hash, BOSS)
            .await
            .is_err()
    );
    // 不在清单里的单
    let err = writer
        .prepare(RESOLVE_TURN, Target::Jira, "EVIL-1", &hash, BOSS)
        .await
        .err()
        .expect("拒绝");
    assert!(err.text.contains("不在"), "{err:?}");
    // 内容摘要对不上
    let err = writer
        .prepare(
            RESOLVE_TURN,
            Target::Jira,
            "ABC-1",
            "0000000000000000",
            BOSS,
        )
        .await
        .err()
        .expect("拒绝");
    assert!(err.text.contains("变了"), "{err:?}");
    // 对得上就登记，再点一次不重复执行
    assert!(
        writer
            .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash, BOSS)
            .await
            .is_ok()
    );
    let again = writer
        .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash, BOSS)
        .await
        .err()
        .expect("进行中");
    assert!(again.text.contains("正在写入"), "{again:?}");
}

#[tokio::test]
async fn a_jira_comment_carries_the_marker_and_the_card_shows_the_result() {
    let env = env().await;
    let answer = answer(&["ABC-1"]);
    finish_resolve(&env, &answer, &[jira_target("ABC-1")]).await;
    let writer = writer(
        &env,
        &json!({
            "jira_comment.list": {"result": {"items": [{"id": "1", "body": "别人的评论"}]}},
            "jira_comment.add": {"result": {
                "id": "10001", "self": "https://jira.example.com/rest/api/2/issue/10000/comment/10001"
            }}
        }),
        false,
    );
    let job = writer
        .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash_of(&answer), BOSS)
        .await
        .expect("登记");
    writer.execute(job).await;

    let calls = calls(env.dir.path());
    let add = calls
        .iter()
        .find(|c| c["arguments"]["action"] == "add")
        .expect("写了评论");
    let body = add["arguments"]["body"].as_str().unwrap_or_default();
    assert!(
        body.contains("*结论*：连接池耗尽"),
        "Jira wiki 标记：{body}"
    );
    assert!(body.contains("{{maxPoolSize}}"));
    assert!(body.contains("ai-boot:wb-"), "幂等标记：{body}");
    let rows = env.store.writebacks(RESOLVE_TURN).await.expect("查询");
    assert_eq!(rows[0].status, WritebackStatus::Done);
    assert_eq!(
        rows[0].result_url.as_deref(),
        Some("https://jira.example.com/browse/ABC-1?focusedCommentId=10001")
    );
    let card = last_card(&env).await;
    assert!(card.contains("已写入 Jira ABC-1"), "{card}");
    assert!(card.contains("focusedCommentId=10001"));
}

#[tokio::test]
async fn an_unclear_write_is_retried_without_writing_twice() {
    let env = env().await;
    let answer = answer(&["ABC-1"]);
    finish_resolve(&env, &answer, &[jira_target("ABC-1")]).await;
    let hash = hash_of(&answer);

    // 第一次：写评论超时，结果不明
    let first = writer(
        &env,
        &json!({
            "jira_comment.list": {"result": {"items": []}},
            "jira_comment.add": {"hang": true}
        }),
        false,
    );
    let job = first
        .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash, BOSS)
        .await
        .expect("登记");
    let row_id = job.row.id.clone();
    // 超时时间写死在执行器里，这里直接把结果记成不明，模拟超时之后的状态
    first
        .store
        .finish_writeback(&row_id, WritebackStatus::Unknown, None, Some("写入超时"), 9)
        .await
        .expect("记录");
    drop(job);

    // 其实上次写成了：评论里已经有这条记录的标记
    let marker = marker(&row_id);
    let second = writer(
        &env,
        &json!({
            "jira_comment.list": {"result": {"items": [{"id": "7", "body": format!("…{marker}")}]}},
            "jira_comment.add": {"error": "不该再写一次"}
        }),
        false,
    );
    let job = second
        .prepare(RESOLVE_TURN, Target::Jira, "ABC-1", &hash, BOSS)
        .await
        .expect("重试");
    assert_eq!(job.row.id, row_id, "重试沿用原来的记录和标记");
    second.execute(job).await;
    let calls = calls(env.dir.path());
    assert!(
        !calls.iter().any(|c| c["arguments"]["action"] == "add"),
        "查到标记就不再写：{calls:?}"
    );
    let rows = env.store.writebacks(RESOLVE_TURN).await.expect("查询");
    assert_eq!(rows[0].status, WritebackStatus::Done);
}

#[tokio::test]
async fn a_confluence_page_is_created_once_under_the_configured_parent() {
    let env = env().await;
    let answer = answer(&["ABC-1"]);
    finish_resolve(
        &env,
        &answer,
        &[TargetSpec {
            target: "confluence".into(),
            reference: "DEV:12345".into(),
            label: "Confluence 空间 DEV".into(),
        }],
    )
    .await;
    let writer = writer(
        &env,
        &json!({
            "confluence_page.by_title": {"error": "空间 DEV 下没有标题为「x」的页面"},
            "confluence_page.create": {"result": {
                "id": "999", "title": "t", "url": "https://wiki.example.com/pages/999"
            }}
        }),
        true,
    );
    let job = writer
        .prepare(
            RESOLVE_TURN,
            Target::Confluence,
            "DEV:12345",
            &hash_of(&answer),
            BOSS,
        )
        .await
        .expect("登记");
    writer.execute(job).await;
    let calls = calls(env.dir.path());
    let create = calls
        .iter()
        .find(|c| c["arguments"]["action"] == "create")
        .expect("创建了页面");
    let args = &create["arguments"];
    assert_eq!(args["space"], "DEV");
    assert_eq!(args["parent_id"], "12345");
    let title = args["title"].as_str().unwrap_or_default();
    assert!(title.starts_with("【闭环】登录偶发 500（"), "{title}");
    assert!(title.contains(" ABC-1 "), "{title}");
    let storage = args["storage"].as_str().unwrap_or_default();
    assert!(storage.contains("<strong>结论</strong>"), "{storage}");
    assert!(storage.contains("<code>maxPoolSize</code>"));
    let card = last_card(&env).await;
    assert!(card.contains("已写入 Confluence 空间 DEV"), "{card}");
}
