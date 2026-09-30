//! OpenAPI 客户端对着 wiremock 跑：请求形状、token 复用与刷新、限流重试。

use ai_boot_feishu::api::{ApiClient, ApiError, Container, Reply, ResourceKind};
use secrecy::SecretString;
use serde_json::json;
use url::Url;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn server_with_token(expected_token_calls: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
        .and(body_json(
            json!({"app_id": "cli_test", "app_secret": "secret"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "msg": "ok", "tenant_access_token": "t-token", "expire": 7200
        })))
        .expect(expected_token_calls)
        .mount(&server)
        .await;
    server
}

fn client(server: &MockServer) -> ApiClient {
    let base = Url::parse(&format!("{}/", server.uri())).expect("地址");
    ApiClient::new(base, "cli_test", SecretString::from("secret")).expect("客户端")
}

fn ok(data: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"code": 0, "msg": "ok", "data": data}))
}

#[tokio::test]
async fn the_token_is_fetched_once_and_reused() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .and(header("authorization", "Bearer t-token"))
        .respond_with(ok(json!({"items": []})))
        .expect(3)
        .mount(&server)
        .await;
    let api = client(&server);
    for _ in 0..3 {
        api.get_message("om_1").await.expect("取消息");
    }
}

#[tokio::test]
async fn a_reply_carries_the_idempotency_key_and_thread_flag() {
    let server = server_with_token(1).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/messages/om_1/reply"))
        .and(body_json(json!({
            "msg_type": "interactive", "content": "{\"schema\":\"2.0\"}",
            "reply_in_thread": true, "uuid": "u-1"
        })))
        .respond_with(ok(
            json!({"message_id": "om_2", "chat_id": "oc_1", "thread_id": "omt_1"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let sent = client(&server)
        .reply(Reply {
            message_id: "om_1",
            msg_type: "interactive",
            content: "{\"schema\":\"2.0\"}".into(),
            reply_in_thread: true,
            uuid: "u-1".into(),
        })
        .await
        .expect("回复");
    assert_eq!(sent.message_id, "om_2");
    assert_eq!(sent.thread_id.as_deref(), Some("omt_1"));
}

#[tokio::test]
async fn a_card_update_is_a_patch_with_the_card_as_a_string() {
    let server = server_with_token(1).await;
    let card = json!({"schema": "2.0", "body": {"elements": []}});
    Mock::given(method("PATCH"))
        .and(path("/open-apis/im/v1/messages/om_card"))
        .and(body_json(json!({"content": card.to_string()})))
        .respond_with(ok(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .update_card("om_card", &card)
        .await
        .expect("改卡");
}

#[tokio::test]
async fn thread_history_is_listed_newest_first_with_paging() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "thread"))
        .and(query_param("container_id", "omt_1"))
        .and(query_param("sort_type", "ByCreateTimeDesc"))
        .and(query_param("page_size", "50"))
        .respond_with(ok(json!({
            "has_more": true, "page_token": "next",
            "items": [{
                "message_id": "om_9", "msg_type": "text", "create_time": "1790000000000",
                "sender": {"id": "ou_1", "id_type": "open_id", "sender_type": "user"},
                "body": {"content": "{\"text\":\"@_user_1 看下\"}"},
                "mentions": [{"key": "@_user_1", "id": "ou_2", "id_type": "open_id", "name": "张三"}]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let page = client(&server)
        .list_messages(Container::Thread("omt_1"), true, None)
        .await
        .expect("列消息");
    assert!(page.has_more);
    assert_eq!(page.page_token.as_deref(), Some("next"));
    assert_eq!(page.items[0].mentions[0].name, "张三");
    assert_eq!(page.items[0].sender.sender_type, "user");
}

#[tokio::test]
async fn chat_members_follow_the_page_token() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/chats/oc_1/members"))
        .and(query_param("page_token", "p2"))
        .respond_with(ok(
            json!({"has_more": false, "items": [{"member_id": "ou_b", "name": "乙"}]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/chats/oc_1/members"))
        .respond_with(ok(json!({
            "has_more": true, "page_token": "p2", "items": [{"member_id": "ou_a", "name": "甲"}]
        })))
        .mount(&server)
        .await;
    let members = client(&server).chat_members("oc_1").await.expect("群成员");
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["甲", "乙"]);
}

#[tokio::test]
async fn rate_limiting_is_retried_after_the_reset_header() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("x-ogw-ratelimit-reset", "1")
                .set_body_json(json!({"code": 99991400, "msg": "rate limited"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .respond_with(ok(json!({"items": []})))
        .expect(1)
        .mount(&server)
        .await;
    let started = std::time::Instant::now();
    client(&server)
        .get_message("om_1")
        .await
        .expect("重试后成功");
    assert!(started.elapsed() >= std::time::Duration::from_millis(900));
}

#[tokio::test]
async fn an_invalid_token_is_refreshed_once_and_the_call_replayed() {
    let server = server_with_token(2).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 99991663, "msg": "invalid tenant access token"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .respond_with(ok(json!({"items": []})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .get_message("om_1")
        .await
        .expect("刷新后成功");
}

#[tokio::test]
async fn business_errors_surface_with_their_code() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 230002, "msg": "Bot/User can NOT be out of the chat."
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .get_message("om_1")
        .await
        .expect_err("应当失败");
    assert!(matches!(err, ApiError::Api { code: 230_002, .. }), "{err}");
}

#[tokio::test]
async fn ids_that_would_rewrite_the_path_never_reach_the_network() {
    let server = MockServer::start().await;
    let err = client(&server)
        .get_message("../chats/oc_1")
        .await
        .expect_err("应当拒绝");
    assert!(matches!(err, ApiError::InvalidArgument(_)));
    assert!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}

#[tokio::test]
async fn a_resource_downloads_as_raw_bytes() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1/resources/img_v2_abc"))
        .and(query_param("type", "image"))
        .and(header("authorization", "Bearer t-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(vec![0x89, b'P', b'N', b'G']),
        )
        .mount(&server)
        .await;
    let download = client(&server)
        .download_resource("om_1", "img_v2_abc", ResourceKind::Image, 1024)
        .await
        .expect("下载");
    assert_eq!(download.bytes, vec![0x89, b'P', b'N', b'G']);
    assert_eq!(download.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn a_download_error_comes_back_as_a_business_code() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1/resources/file_x"))
        .and(query_param("type", "file"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 234043, "msg": "unsupported message type"
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .download_resource("om_1", "file_x", ResourceKind::File, 1024)
        .await
        .expect_err("应当失败");
    assert_eq!(err.code(), Some(234043), "{err}");
}

#[tokio::test]
async fn a_download_over_the_limit_is_cut_off() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1/resources/file_big"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(vec![7_u8; 4096]),
        )
        .mount(&server)
        .await;
    let err = client(&server)
        .download_resource("om_1", "file_big", ResourceKind::File, 1000)
        .await
        .expect_err("超过上限");
    assert!(matches!(err, ApiError::TooLarge { limit: 1000 }), "{err}");
}

#[tokio::test]
async fn a_download_is_retried_after_a_server_error() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1/resources/file_r"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages/om_1/resources/file_r"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("log line"),
        )
        .mount(&server)
        .await;
    let download = client(&server)
        .download_resource("om_1", "file_r", ResourceKind::File, 1024)
        .await
        .expect("重试后成功");
    assert_eq!(download.bytes, b"log line");
}

#[tokio::test]
async fn the_group_window_is_filtered_by_seconds_and_asks_for_raw_cards() {
    let server = server_with_token(1).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("container_id_type", "chat"))
        .and(query_param("container_id", "oc_1"))
        .and(query_param("start_time", "1790580000"))
        .and(query_param("sort_type", "ByCreateTimeDesc"))
        .and(query_param("card_msg_content_type", "user_card_content"))
        .respond_with(ok(json!({"has_more": false, "items": [
            {"message_id": "om_9", "msg_type": "text", "create_time": "1790580001000",
             "sender": {"id": "ou_1", "sender_type": "user"}, "body": {"content": "{\"text\":\"hi\"}"}}
        ]})))
        .mount(&server)
        .await;
    let page = client(&server)
        .list_chat_since("oc_1", 1_790_580_000, None)
        .await
        .expect("取群消息");
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn an_authorization_code_is_exchanged_without_a_bearer_and_read_from_the_top_level() {
    let server = server_with_token(0).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/authen/v2/oauth/token"))
        .and(body_json(json!({
            "grant_type": "authorization_code", "client_id": "cli_test", "client_secret": "secret",
            "code": "c-1", "code_verifier": "v-1", "redirect_uri": "http://10.0.0.1:18080/cb"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "access_token": "u-access", "expires_in": 7200,
            "refresh_token": "r-1", "refresh_token_expires_in": 604800,
            "scope": "offline_access docx:document:readonly", "token_type": "Bearer"
        })))
        .mount(&server)
        .await;
    let token = client(&server)
        .exchange_code("c-1", "v-1", "http://10.0.0.1:18080/cb")
        .await
        .expect("换 token");
    use secrecy::ExposeSecret as _;
    assert_eq!(token.access_token.expose_secret(), "u-access");
    assert_eq!(token.expires_in, 7200);
    assert_eq!(
        token
            .refresh_token
            .as_ref()
            .map(|t| t.expose_secret().to_owned()),
        Some("r-1".to_owned())
    );
    assert_eq!(token.refresh_expires_in, Some(604_800));
    let requests = server.received_requests().await.expect("请求记录");
    assert!(
        requests
            .iter()
            .all(|r| !r.headers.contains_key("authorization")),
        "换 token 不带任何 Authorization 头"
    );
}

#[tokio::test]
async fn a_rejected_refresh_reports_the_oauth_error() {
    let server = server_with_token(0).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/authen/v2/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "code": 20064, "error": "invalid_grant", "error_description": "The refresh token has been revoked."
        })))
        .mount(&server)
        .await;
    let err = client(&server)
        .refresh_user_token(&SecretString::from("r-old"))
        .await
        .expect_err("应当失败");
    assert_eq!(err.code(), Some(20064));
    assert!(err.to_string().contains("revoked"), "{err}");
}

#[tokio::test]
async fn document_calls_carry_the_user_token() {
    let server = server_with_token(0).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/docs/v1/content"))
        .and(query_param("doc_token", "doxcnAbc"))
        .and(query_param("doc_type", "docx"))
        .and(query_param("content_type", "markdown"))
        .and(header("authorization", "Bearer u-token"))
        .respond_with(ok(json!({"content": "# 复盘\n连接池耗尽"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/authen/v1/user_info"))
        .and(header("authorization", "Bearer u-token"))
        .respond_with(ok(json!({"open_id": "ou_boss", "name": "朱工"})))
        .mount(&server)
        .await;
    let api = client(&server);
    let token = SecretString::from("u-token");
    assert_eq!(
        api.docx_markdown(&token, "doxcnAbc").await.expect("导出"),
        "# 复盘\n连接池耗尽"
    );
    assert_eq!(
        api.user_info(&token).await.expect("用户").open_id,
        "ou_boss"
    );
}

#[tokio::test]
async fn image_blocks_are_collected_across_pages() {
    let server = server_with_token(0).await;
    Mock::given(method("GET"))
        .and(path("/open-apis/docx/v1/documents/doxcnAbc/blocks"))
        .and(query_param("page_token", "p2"))
        .respond_with(ok(json!({"has_more": false, "items": [
            {"block_type": 27, "image": {"token": "boxB", "width": 10, "height": 10}}
        ]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/open-apis/docx/v1/documents/doxcnAbc/blocks"))
        .respond_with(ok(json!({"has_more": true, "page_token": "p2", "items": [
            {"block_type": 2, "text": {"elements": []}},
            {"block_type": 27, "image": {"token": "boxA"}},
            {"block_type": 27, "image": {"token": ""}}
        ]})))
        .mount(&server)
        .await;
    let tokens = client(&server)
        .docx_image_tokens(&SecretString::from("u-token"), "doxcnAbc", 10)
        .await
        .expect("图片块");
    assert_eq!(tokens, ["boxA", "boxB"]);
}

#[tokio::test]
async fn sheet_values_are_read_as_text_for_all_ranges() {
    let server = server_with_token(0).await;
    Mock::given(method("GET"))
        .and(path(
            "/open-apis/sheets/v2/spreadsheets/shtcnX/values_batch_get",
        ))
        .and(query_param("ranges", "s1!A1:AD200,s2!A1:AD200"))
        .and(query_param("valueRenderOption", "ToString"))
        .respond_with(ok(json!({"valueRanges": [
            {"range": "s1!A1:B2", "values": [["主机", "JDK"], ["app-01", 17]]},
            {"range": "s2!A1:A1", "values": [["空"]]}
        ]})))
        .mount(&server)
        .await;
    let ranges = client(&server)
        .sheet_values(
            &SecretString::from("u-token"),
            "shtcnX",
            &["s1!A1:AD200".to_owned(), "s2!A1:AD200".to_owned()],
        )
        .await
        .expect("读表");
    assert_eq!(ranges.len(), 2);
    assert_eq!(ranges[0][1][1], json!(17));
}

#[tokio::test]
async fn a_private_message_goes_by_open_id() {
    let server = server_with_token(1).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/messages"))
        .and(query_param("receive_id_type", "open_id"))
        .and(body_json(json!({
            "receive_id": "ou_boss", "msg_type": "interactive", "content": "{}", "uuid": "u-9"
        })))
        .respond_with(ok(json!({"message_id": "om_dm"})))
        .mount(&server)
        .await;
    let sent = client(&server)
        .send_to_user("ou_boss", "interactive", "{}".to_owned(), "u-9".to_owned())
        .await
        .expect("发送");
    assert_eq!(sent.message_id, "om_dm");
}

#[tokio::test]
async fn emails_resolve_from_either_response_shape() {
    let server = server_with_token(1).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/contact/v3/users/batch_get_id"))
        .and(query_param("user_id_type", "open_id"))
        .respond_with(ok(json!({
            "items": [{"email": "a@example.com", "open_id": "ou_a", "user_id": "u_a"}],
            "user_list": [{"email": "b@example.com", "user_id": "ou_b"}, {"email": "c@example.com"}]
        })))
        .mount(&server)
        .await;
    let mut found = client(&server)
        .open_ids_by_email(&[
            "a@example.com".to_owned(),
            "b@example.com".to_owned(),
            "c@example.com".to_owned(),
        ])
        .await
        .expect("解析");
    found.sort();
    assert_eq!(
        found,
        [
            ("a@example.com".to_owned(), "ou_a".to_owned()),
            ("b@example.com".to_owned(), "ou_b".to_owned())
        ]
    );
}

/// 卡片里的图片要先上传：multipart 表单里带 image_type=message 和图片本身。
#[tokio::test]
async fn an_image_is_uploaded_as_multipart_and_its_key_returned() {
    let server = server_with_token(1).await;
    Mock::given(method("POST"))
        .and(path("/open-apis/im/v1/images"))
        .and(header("authorization", "Bearer t-token"))
        .respond_with(ok(json!({"image_key": "img_v3_abc"})))
        .expect(1)
        .mount(&server)
        .await;
    let api = client(&server);
    let key = api
        .upload_image(b"\x89PNG fake".to_vec())
        .await
        .expect("上传");
    assert_eq!(key, "img_v3_abc");
    let requests = server.received_requests().await.expect("请求记录");
    let upload = requests
        .iter()
        .find(|r| r.url.path() == "/open-apis/im/v1/images")
        .expect("上传请求");
    let content_type = upload
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        content_type.starts_with("multipart/form-data"),
        "{content_type}"
    );
    let body = String::from_utf8_lossy(&upload.body);
    assert!(body.contains("name=\"image_type\""), "{body}");
    assert!(body.contains("message"), "{body}");
    assert!(body.contains("name=\"image\""), "{body}");
    assert!(body.contains("PNG fake"), "{body}");
}
