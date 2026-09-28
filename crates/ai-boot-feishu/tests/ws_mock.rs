//! 长连接客户端对着本地模拟服务端跑：接入点用 wiremock，长连接用
//! tokio-tungstenite 起一个真实的 WebSocket 服务。

use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_boot_feishu::ws::frame::{Frame, FrameKind, Header, METHOD_CONTROL, METHOD_DATA};
use ai_boot_feishu::ws::{DataKind, FrameHandler, WsClient, WsConfig};
use base64::Engine as _;
use futures_util::{SinkExt as _, StreamExt as _};
use secrecy::SecretString;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::{Message, http};
use tokio_util::sync::CancellationToken;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type ServerWs = WebSocketStream<TcpStream>;

const WAIT: Duration = Duration::from_secs(5);

#[derive(Clone)]
enum Behavior {
    Reply(Option<Vec<u8>>),
    Fail,
    Sleep(Duration),
}

struct Recorder {
    calls: mpsc::Sender<(DataKind, Vec<u8>)>,
    behavior: Behavior,
}

#[async_trait::async_trait]
impl FrameHandler for Recorder {
    async fn handle(&self, kind: DataKind, payload: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        let _ = self.calls.send((kind, payload)).await;
        match &self.behavior {
            Behavior::Reply(data) => Ok(data.clone()),
            Behavior::Fail => Err("处理失败".into()),
            Behavior::Sleep(d) => {
                tokio::time::sleep(*d).await;
                Ok(None)
            }
        }
    }
}

struct Harness {
    endpoint: MockServer,
    conns: mpsc::Receiver<ServerWs>,
    calls: mpsc::Receiver<(DataKind, Vec<u8>)>,
    cancel: CancellationToken,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Setup {
    ping_secs: i64,
    behavior: Behavior,
    reject_with_403: bool,
    handler_budget: Duration,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            ping_secs: 100,
            behavior: Behavior::Reply(None),
            reject_with_403: false,
            handler_budget: Duration::from_millis(500),
        }
    }
}

async fn start(setup: Setup) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听");
    let addr = listener.local_addr().expect("地址");
    let (conn_tx, conns) = mpsc::channel(8);
    let reject = setup.reject_with_403;
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            if reject {
                // 回调签名由 tungstenite 规定，错误类型大小不由我们决定
                #[allow(clippy::result_large_err)]
                let refuse = |_: &Request, _: Response| -> Result<Response, ErrorResponse> {
                    Err(http::Response::builder()
                        .status(403)
                        .header("handshake-status", "403")
                        .header("handshake-msg", "forbidden")
                        .body(None)
                        .expect("响应"))
                };
                let _ = tokio_tungstenite::accept_hdr_async(tcp, refuse).await;
                continue;
            }
            if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                let _ = conn_tx.send(ws).await;
            }
        }
    });

    let endpoint = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/callback/ws/endpoint"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "URL": format!("ws://{addr}/ws?device_id=dev-1&service_id=7"),
                "ClientConfig": {
                    "ReconnectCount": -1,
                    "ReconnectInterval": 1,
                    "ReconnectNonce": 1,
                    "PingInterval": setup.ping_secs
                }
            }
        })))
        .mount(&endpoint)
        .await;

    let mut config = WsConfig::new("cli_test", SecretString::from("secret"));
    config.base_url = Url::parse(&format!("{}/", endpoint.uri())).expect("地址");
    config.connect_timeout = Duration::from_secs(2);
    config.handler_budget = setup.handler_budget;
    config.liveness_grace = Duration::from_millis(200);
    config.fatal_backoff = Duration::from_secs(60);

    let (calls_tx, calls) = mpsc::channel(16);
    let handler = Arc::new(Recorder {
        calls: calls_tx,
        behavior: setup.behavior,
    });
    let cancel = CancellationToken::new();
    let client = WsClient::new(config).expect("客户端");
    tokio::spawn(client.run(handler, cancel.clone()));

    Harness {
        endpoint,
        conns,
        calls,
        cancel,
    }
}

async fn next_conn(h: &mut Harness) -> ServerWs {
    tokio::time::timeout(WAIT, h.conns.recv())
        .await
        .expect("等待连接超时")
        .expect("连接通道关闭")
}

/// 读下一个帧；`skip_pings` 为真时跳过客户端心跳。
async fn next_frame(ws: &mut ServerWs, skip_pings: bool) -> Frame {
    loop {
        let message = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("等待帧超时")
            .expect("连接已关闭")
            .expect("读取失败");
        let Message::Binary(bytes) = message else {
            continue;
        };
        let frame = Frame::decode_bytes(&bytes).expect("解码");
        if skip_pings && frame.kind() == FrameKind::Ping {
            continue;
        }
        return frame;
    }
}

async fn send(ws: &mut ServerWs, frame: &Frame) {
    ws.send(Message::Binary(frame.encode_bytes().into()))
        .await
        .expect("发送");
}

fn header(key: &str, value: &str) -> Header {
    Header {
        key: key.into(),
        value: value.into(),
    }
}

fn data_frame(kind: &str, message_id: &str, sum: usize, seq: usize, payload: &[u8]) -> Frame {
    Frame {
        seq_id: 100 + seq as u64,
        log_id: 200,
        service: 7,
        method: METHOD_DATA,
        headers: vec![
            header("type", kind),
            header("message_id", message_id),
            header("sum", &sum.to_string()),
            header("seq", &seq.to_string()),
            header("trace_id", "trace-1"),
        ],
        payload_encoding: None,
        payload_type: None,
        payload: Some(payload.to_vec()),
        log_id_new: None,
    }
}

fn ack_body(frame: &Frame) -> serde_json::Value {
    serde_json::from_slice(frame.payload.as_deref().expect("ACK 负载")).expect("ACK JSON")
}

#[tokio::test]
async fn an_event_is_dispatched_and_acked_quickly() {
    let mut h = start(Setup::default()).await;
    let mut ws = next_conn(&mut h).await;

    // 建连后立刻发出第一个心跳，service 取自 URL 上的 service_id
    let ping = next_frame(&mut ws, false).await;
    assert_eq!(ping.kind(), FrameKind::Ping);
    assert_eq!(ping.method, METHOD_CONTROL);
    assert_eq!(ping.service, 7);

    let sent_at = Instant::now();
    send(&mut ws, &data_frame("event", "m-1", 1, 0, br#"{"k":1}"#)).await;
    let acked = next_frame(&mut ws, true).await;
    let elapsed = sent_at.elapsed();
    assert!(elapsed < Duration::from_millis(100), "ACK 用时 {elapsed:?}");

    assert_eq!(acked.method, METHOD_DATA);
    assert_eq!(acked.seq_id, 100);
    assert_eq!(acked.log_id, 200);
    assert_eq!(acked.header("trace_id"), Some("trace-1"));
    assert!(acked.header("biz_rt").is_some());
    assert_eq!(
        ack_body(&acked),
        serde_json::json!({"code":200,"headers":null,"data":null})
    );

    let (kind, payload) = h.calls.recv().await.expect("处理方被调用");
    assert_eq!(kind, DataKind::Event);
    assert_eq!(payload, br#"{"k":1}"#);
}

#[tokio::test]
async fn split_messages_are_joined_and_only_the_last_part_is_acked() {
    let mut h = start(Setup::default()).await;
    let mut ws = next_conn(&mut h).await;

    send(&mut ws, &data_frame("event", "m-2", 2, 1, b"world")).await;
    send(&mut ws, &data_frame("event", "m-2", 2, 0, b"hello ")).await;

    let (_, payload) = tokio::time::timeout(WAIT, h.calls.recv())
        .await
        .expect("等待处理")
        .expect("处理方被调用");
    assert_eq!(payload, b"hello world");

    let acked = next_frame(&mut ws, true).await;
    assert_eq!(acked.header("seq"), Some("0"), "只 ACK 补齐的那一片");
    // 不应再有第二个 ACK
    let extra = tokio::time::timeout(Duration::from_millis(300), next_frame(&mut ws, true)).await;
    assert!(extra.is_err(), "多出了 ACK：{extra:?}");
}

#[tokio::test]
async fn a_callback_response_is_base64_encoded_into_the_ack() {
    let toast = br#"{"toast":{"type":"info","content":"ok"}}"#.to_vec();
    let mut h = start(Setup {
        behavior: Behavior::Reply(Some(toast.clone())),
        ..Setup::default()
    })
    .await;
    let mut ws = next_conn(&mut h).await;

    send(&mut ws, &data_frame("event", "m-3", 1, 0, b"{}")).await;
    let acked = next_frame(&mut ws, true).await;
    let data = ack_body(&acked)["data"]
        .as_str()
        .expect("data 是字符串")
        .to_owned();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("base64");
    assert_eq!(decoded, toast);
}

#[tokio::test]
async fn a_failing_handler_is_acked_with_500() {
    let mut h = start(Setup {
        behavior: Behavior::Fail,
        ..Setup::default()
    })
    .await;
    let mut ws = next_conn(&mut h).await;

    send(&mut ws, &data_frame("event", "m-4", 1, 0, b"{}")).await;
    assert_eq!(ack_body(&next_frame(&mut ws, true).await)["code"], 500);
}

#[tokio::test]
async fn a_slow_handler_is_acked_within_the_budget() {
    let mut h = start(Setup {
        behavior: Behavior::Sleep(Duration::from_secs(3)),
        handler_budget: Duration::from_millis(200),
        ..Setup::default()
    })
    .await;
    let mut ws = next_conn(&mut h).await;

    let sent_at = Instant::now();
    send(&mut ws, &data_frame("event", "m-5", 1, 0, b"{}")).await;
    let acked = next_frame(&mut ws, true).await;
    assert!(
        sent_at.elapsed() < Duration::from_secs(1),
        "{:?}",
        sent_at.elapsed()
    );
    assert_eq!(ack_body(&acked)["code"], 500);
}

#[tokio::test]
async fn unknown_data_frames_are_neither_dispatched_nor_acked() {
    let mut h = start(Setup::default()).await;
    let mut ws = next_conn(&mut h).await;

    send(&mut ws, &data_frame("mystery", "m-6", 1, 0, b"{}")).await;
    let extra = tokio::time::timeout(Duration::from_millis(300), next_frame(&mut ws, true)).await;
    assert!(extra.is_err());
    assert!(h.calls.try_recv().is_err());
}

#[tokio::test]
async fn a_pong_with_client_config_changes_the_ping_interval() {
    let mut h = start(Setup {
        ping_secs: 100,
        ..Setup::default()
    })
    .await;
    let mut ws = next_conn(&mut h).await;
    assert_eq!(next_frame(&mut ws, false).await.kind(), FrameKind::Ping);

    let pong = Frame {
        headers: vec![header("type", "pong")],
        payload: Some(br#"{"PingInterval":1}"#.to_vec()),
        ..Frame::ping(7)
    };
    let sent_at = Instant::now();
    send(&mut ws, &pong).await;
    assert_eq!(next_frame(&mut ws, false).await.kind(), FrameKind::Ping);
    let gap = sent_at.elapsed();
    assert!(
        gap > Duration::from_millis(800) && gap < Duration::from_millis(1600),
        "新的心跳间隔没有生效：{gap:?}"
    );
}

#[tokio::test]
async fn after_the_server_drops_the_client_refetches_the_endpoint_and_reconnects() {
    let mut h = start(Setup::default()).await;
    let ws = next_conn(&mut h).await;
    drop(ws);

    let mut again = next_conn(&mut h).await;
    assert_eq!(next_frame(&mut again, false).await.kind(), FrameKind::Ping);
    let requests = h.endpoint.received_requests().await.expect("记录请求");
    assert!(requests.len() >= 2, "重连前必须重新获取接入点");
}

#[tokio::test]
async fn a_silent_server_trips_the_liveness_deadline() {
    // 心跳 1 秒、宽限 200ms：约 2.2 秒收不到任何帧就判定连接已死
    let mut h = start(Setup {
        ping_secs: 1,
        ..Setup::default()
    })
    .await;
    let _silent = next_conn(&mut h).await;
    let started = Instant::now();
    let _again = next_conn(&mut h).await;
    let waited = started.elapsed();
    assert!(
        waited > Duration::from_secs(2) && waited < Duration::from_secs(4),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_403_handshake_backs_off_instead_of_hot_looping() {
    let h = start(Setup {
        reject_with_403: true,
        ..Setup::default()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let requests = h.endpoint.received_requests().await.expect("记录请求");
    assert_eq!(requests.len(), 1, "403 之后不应立即重试");
}
