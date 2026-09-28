//! 长连接客户端：取 endpoint → 建连 → 心跳 / 收帧 / ACK → 断线重连。
//!
//! 行为对齐官方 Go SDK（`ws/client*.go`），但有几处是按服务端常驻进程的
//! 需要做的取舍：
//! - 每条数据帧都在独立任务里处理，ACK 经单一 writer 发出，慢处理不会
//!   堵住读循环；处理超出预算就先回 500，不等它。
//! - 读循环有存活截止（`2×PingInterval + 宽限`），收到任何帧就续期——
//!   半开连接会静默挂住几个小时，这是第三方实现里踩过的坑。
//! - 致命错误（鉴权失败、连接数超限、应用配置错）不按 SDK 那样退出，
//!   而是长间隔退避后再试：常驻服务退出了没人拉起，配置修好后应自愈。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt as _, StreamExt as _};
use rand::Rng as _;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::ack::ack;
use super::assembler::Assembler;
use super::frame::{Frame, FrameKind, METHOD_CONTROL, METHOD_DATA, header};

/// 服务端下发配置之前的默认值，与官方 SDK 一致（秒）。
const DEFAULT_PING_SECS: u64 = 120;
const DEFAULT_RECONNECT_NONCE_SECS: u64 = 30;
const DEFAULT_RECONNECT_INTERVAL_SECS: u64 = 120;
/// 分片等待时长。Go SDK 5 秒、Node 10 秒，取宽的一端。
const ASSEMBLER_TTL: Duration = Duration::from_secs(10);
/// 断开时给还没发出去的 ACK 留的时间。
const WRITER_DRAIN: Duration = Duration::from_secs(2);

const ENDPOINT_PATH: &str = "callback/ws/endpoint";

#[derive(Clone)]
pub struct WsConfig {
    pub app_id: String,
    pub app_secret: SecretString,
    /// 开放平台地址，默认 `https://open.feishu.cn/`。
    pub base_url: Url,
    /// 单次建连（含 TLS 与 WebSocket 握手）的超时。接入点的 DNS 池里有
    /// 黑洞 IP，不设超时会卡死在某一次尝试上。
    pub connect_timeout: Duration,
    /// 单条数据帧的处理预算。服务端 3 秒收不到 ACK 就重推，留出余量。
    pub handler_budget: Duration,
    /// 致命错误后的退避。
    pub fatal_backoff: Duration,
    /// 存活截止在 `2×PingInterval` 之外的宽限。
    pub liveness_grace: Duration,
}

impl WsConfig {
    pub fn new(app_id: impl Into<String>, app_secret: SecretString) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret,
            base_url: Url::parse("https://open.feishu.cn/").expect("常量地址必然合法"),
            connect_timeout: Duration::from_secs(10),
            handler_budget: Duration::from_secs(2),
            fatal_backoff: Duration::from_secs(600),
            liveness_grace: Duration::from_secs(5),
        }
    }
}

/// 数据帧的业务类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataKind {
    Event,
    /// `type=card` 的帧。按资料推断回调走的是 `Event`，真收到这种帧时照常
    /// 交给处理方，并在日志里标出来。
    Card,
}

/// 数据帧的处理方。
///
/// 返回 `Ok(Some(bytes))` 时，`bytes`（一段 JSON）会以 base64 放进 ACK 的
/// `data`——卡片回调的 toast / 卡片就是这样回给飞书的。返回 `Err` 会让 ACK
/// 带上 500。
#[async_trait::async_trait]
pub trait FrameHandler: Send + Sync + 'static {
    async fn handle(&self, kind: DataKind, payload: Vec<u8>) -> Result<Option<Vec<u8>>, String>;
}

/// 服务端下发的客户端配置，单位都是秒。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ClientConfig {
    #[serde(default)]
    reconnect_interval: i64,
    #[serde(default)]
    reconnect_nonce: i64,
    #[serde(default)]
    ping_interval: i64,
}

fn positive_secs(secs: i64) -> Option<Duration> {
    u64::try_from(secs)
        .ok()
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
}

/// 重连节奏：第一次在 `[0, nonce)` 里随机等待（避免同一时刻大批客户端一起
/// 重连），之后按固定间隔。成功建连后复位。
#[derive(Debug, Clone)]
struct ReconnectPolicy {
    nonce: Duration,
    interval: Duration,
    attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            nonce: Duration::from_secs(DEFAULT_RECONNECT_NONCE_SECS),
            interval: Duration::from_secs(DEFAULT_RECONNECT_INTERVAL_SECS),
            attempts: 0,
        }
    }
}

impl ReconnectPolicy {
    fn apply(&mut self, config: &ClientConfig) {
        if let Some(nonce) = positive_secs(config.reconnect_nonce) {
            self.nonce = nonce;
        }
        if let Some(interval) = positive_secs(config.reconnect_interval) {
            self.interval = interval;
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = if self.attempts == 0 {
            let nonce_ms = u64::try_from(self.nonce.as_millis()).unwrap_or(u64::MAX);
            if nonce_ms == 0 {
                Duration::ZERO
            } else {
                Duration::from_millis(rand::rng().random_range(0..nonce_ms))
            }
        } else {
            self.interval
        };
        self.attempts = self.attempts.saturating_add(1);
        delay
    }

    fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// 一次连接尝试的结局。
#[derive(Debug)]
enum Outcome {
    Cancelled,
    /// 连上过，之后断了。
    Disconnected(String),
    /// 没连上，可以按正常节奏重试。
    Retryable(String),
    /// 配置或鉴权类错误，按长间隔退避。
    Fatal(String),
}

#[derive(Deserialize)]
struct EndpointResp {
    code: i64,
    #[serde(default)]
    msg: String,
    data: Option<EndpointData>,
}

#[derive(Deserialize)]
struct EndpointData {
    #[serde(rename = "URL")]
    url: Option<String>,
    #[serde(rename = "ClientConfig")]
    client_config: Option<ClientConfig>,
}

struct Endpoint {
    url: Url,
    service_id: i32,
    device_id: String,
    client_config: ClientConfig,
}

/// endpoint 接口里可以重试的业务码：系统繁忙、内部错误。
const RETRYABLE_ENDPOINT_CODES: [i64; 2] = [1, 1_000_040_343];
/// 握手失败时表示「连接数超限」的鉴权码。
const EXCEED_CONN_LIMIT: &str = "1000040350";

pub struct WsClient {
    config: WsConfig,
    http: reqwest::Client,
}

impl WsClient {
    pub fn new(config: WsConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { config, http })
    }

    /// 持续运行直到 `cancel` 被触发。连接问题只记日志并重连，不向上返回。
    pub async fn run(self, handler: Arc<dyn FrameHandler>, cancel: CancellationToken) {
        let mut policy = ReconnectPolicy::default();
        loop {
            let outcome = self.connect_once(&handler, &cancel, &mut policy).await;
            let delay = match outcome {
                Outcome::Cancelled => return,
                Outcome::Disconnected(reason) => {
                    tracing::warn!(%reason, "长连接断开，准备重连");
                    policy.next_delay()
                }
                Outcome::Retryable(reason) => {
                    tracing::warn!(%reason, "长连接建立失败，稍后重试");
                    policy.next_delay()
                }
                Outcome::Fatal(reason) => {
                    tracing::error!(
                        %reason,
                        backoff_secs = self.config.fatal_backoff.as_secs(),
                        "长连接遇到配置或鉴权类错误，长间隔退避后再试"
                    );
                    self.config.fatal_backoff
                }
            };
            tracing::debug!(delay_ms = delay.as_millis(), "等待重连");
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    async fn connect_once(
        &self,
        handler: &Arc<dyn FrameHandler>,
        cancel: &CancellationToken,
        policy: &mut ReconnectPolicy,
    ) -> Outcome {
        // URL 里带有短期票据，每次（重）连都要重新取
        let endpoint = tokio::select! {
            () = cancel.cancelled() => return Outcome::Cancelled,
            got = self.fetch_endpoint() => match got {
                Ok(endpoint) => endpoint,
                Err(outcome) => return outcome,
            },
        };
        policy.apply(&endpoint.client_config);

        let connect = tokio::time::timeout(
            self.config.connect_timeout,
            tokio_tungstenite::connect_async(endpoint.url.as_str()),
        );
        let ws = tokio::select! {
            () = cancel.cancelled() => return Outcome::Cancelled,
            result = connect => match result {
                Err(_) => return Outcome::Retryable("建连超时".into()),
                Ok(Err(err)) => return classify_handshake_error(err),
                Ok(Ok((ws, _response))) => ws,
            },
        };
        tracing::info!(
            device_id = %endpoint.device_id,
            service_id = endpoint.service_id,
            "长连接已建立"
        );
        policy.reset();

        let ping_every = positive_secs(endpoint.client_config.ping_interval)
            .unwrap_or(Duration::from_secs(DEFAULT_PING_SECS));
        self.session(ws, handler, cancel, &endpoint, ping_every, policy)
            .await
    }

    async fn fetch_endpoint(&self) -> Result<Endpoint, Outcome> {
        let url = self
            .config
            .base_url
            .join(ENDPOINT_PATH)
            .map_err(|err| Outcome::Fatal(format!("开放平台地址不合法：{err}")))?;
        let body = serde_json::json!({
            "AppID": self.config.app_id,
            "AppSecret": self.config.app_secret.expose_secret(),
        });
        let response = self
            .http
            .post(url)
            .header("locale", "zh")
            .json(&body)
            .send()
            .await
            .map_err(|err| Outcome::Retryable(format!("获取接入点失败：{err}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Outcome::Retryable(format!("获取接入点返回 HTTP {status}")));
        }
        let parsed: EndpointResp = response
            .json()
            .await
            .map_err(|err| Outcome::Retryable(format!("接入点响应无法解析：{err}")))?;
        if parsed.code != 0 {
            let reason = format!("获取接入点失败：code={} msg={}", parsed.code, parsed.msg);
            return Err(if RETRYABLE_ENDPOINT_CODES.contains(&parsed.code) {
                Outcome::Retryable(reason)
            } else {
                Outcome::Fatal(reason)
            });
        }
        let data = parsed
            .data
            .ok_or_else(|| Outcome::Retryable("接入点响应缺少 data".into()))?;
        let url = data
            .url
            .as_deref()
            .and_then(|raw| Url::parse(raw).ok())
            .ok_or_else(|| Outcome::Retryable("接入点响应缺少合法的 URL".into()))?;

        let query = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        let service_id = query("service_id")
            .and_then(|v| v.parse().ok())
            .unwrap_or_default();
        let device_id = query("device_id").unwrap_or_default();
        Ok(Endpoint {
            url,
            service_id,
            device_id,
            // 系统繁忙时服务端可能不带 ClientConfig，沿用当前值
            client_config: data.client_config.unwrap_or_default(),
        })
    }

    async fn session(
        &self,
        ws: tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        handler: &Arc<dyn FrameHandler>,
        cancel: &CancellationToken,
        endpoint: &Endpoint,
        mut ping_every: Duration,
        policy: &mut ReconnectPolicy,
    ) -> Outcome {
        let (mut sink, mut stream) = ws.split();
        // 心跳和 ACK 都从这里发出：单一写者，读循环永远不会被写阻塞
        let (tx, mut rx) = mpsc::channel::<Message>(256);
        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(err) = sink.send(message).await {
                    tracing::debug!(%err, "写长连接失败");
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let mut assembler = Assembler::new(ASSEMBLER_TTL);
        let mut ping = tokio::time::interval(ping_every);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sweep = tokio::time::interval(ASSEMBLER_TTL / 2);
        let liveness_window = |every: Duration| every * 2 + self.config.liveness_grace;
        let liveness = tokio::time::sleep(liveness_window(ping_every));
        tokio::pin!(liveness);

        let outcome = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Outcome::Cancelled,
                () = &mut liveness => break Outcome::Disconnected("超过存活截止仍未收到任何帧".into()),
                incoming = stream.next() => {
                    let message = match incoming {
                        None => break Outcome::Disconnected("服务端关闭了连接".into()),
                        Some(Err(err)) => break Outcome::Disconnected(format!("读取失败：{err}")),
                        Some(Ok(message)) => message,
                    };
                    liveness
                        .as_mut()
                        .reset(tokio::time::Instant::now() + liveness_window(ping_every));
                    let bytes = match message {
                        Message::Binary(bytes) => bytes,
                        Message::Close(frame) => {
                            break Outcome::Disconnected(format!("服务端发来 Close：{frame:?}"));
                        }
                        // 文本帧官方 SDK 也忽略；WebSocket 层的 ping/pong 由 tungstenite 处理
                        _ => continue,
                    };
                    let frame = match Frame::decode_bytes(&bytes) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::warn!(%err, "无法解码的帧，已忽略");
                            continue;
                        }
                    };
                    match frame.method {
                        METHOD_CONTROL => {
                            if let Some(new_every) = self.on_control(&frame, policy)
                                && new_every != ping_every
                            {
                                tracing::debug!(secs = new_every.as_secs(), "心跳间隔更新");
                                ping_every = new_every;
                                ping = tokio::time::interval_at(
                                    tokio::time::Instant::now() + ping_every,
                                    ping_every,
                                );
                                ping.set_missed_tick_behavior(
                                    tokio::time::MissedTickBehavior::Delay,
                                );
                            }
                        }
                        METHOD_DATA => self.on_data(frame, &mut assembler, handler, &tx),
                        other => tracing::debug!(method = other, "未知 method 的帧，已忽略"),
                    }
                }
                _ = ping.tick() => {
                    let frame = Frame::ping(endpoint.service_id);
                    if tx.send(Message::Binary(frame.encode_bytes().into())).await.is_err() {
                        break Outcome::Disconnected("写通道已关闭".into());
                    }
                }
                _ = sweep.tick() => {
                    let dropped = assembler.sweep(Instant::now());
                    if dropped > 0 {
                        tracing::warn!(dropped, "有分片消息在时限内未补齐，已丢弃");
                    }
                }
            }
        };

        drop(tx);
        // 尚在处理中的帧还持有写端；等一小会儿让已完成的 ACK 发出去
        if tokio::time::timeout(WRITER_DRAIN, writer).await.is_err() {
            tracing::debug!("写任务未在时限内结束");
        }
        outcome
    }

    /// 处理控制帧。pong 里带配置时，返回新的心跳间隔。
    fn on_control(&self, frame: &Frame, policy: &mut ReconnectPolicy) -> Option<Duration> {
        if frame.kind() != FrameKind::Pong {
            return None;
        }
        let payload = frame.payload.as_deref().filter(|p| !p.is_empty())?;
        match serde_json::from_slice::<ClientConfig>(payload) {
            Ok(config) => {
                policy.apply(&config);
                positive_secs(config.ping_interval)
            }
            Err(err) => {
                tracing::debug!(%err, "pong 负载不是客户端配置，已忽略");
                None
            }
        }
    }

    fn on_data(
        &self,
        frame: Frame,
        assembler: &mut Assembler,
        handler: &Arc<dyn FrameHandler>,
        tx: &mpsc::Sender<Message>,
    ) {
        let started = Instant::now();
        let kind = match frame.kind() {
            FrameKind::Event => DataKind::Event,
            FrameKind::Card => {
                tracing::warn!(
                    trace_id = frame.header(header::TRACE_ID).unwrap_or_default(),
                    "收到 type=card 的数据帧，按卡片回调处理"
                );
                DataKind::Card
            }
            other => {
                // 与官方 SDK 一致：不认识的数据帧既不分发也不 ACK
                tracing::warn!(kind = ?other, "未知类型的数据帧，已忽略");
                return;
            }
        };

        let message_id = frame.header(header::MESSAGE_ID).unwrap_or_default();
        let sum = parse_header(&frame, header::SUM).unwrap_or(1);
        let seq = parse_header(&frame, header::SEQ).unwrap_or(0);
        let payload = frame.payload.clone().unwrap_or_default();
        let whole = match assembler.push(message_id, sum, seq, payload, started) {
            Ok(Some(whole)) => whole,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(%err, message_id, "分片不合法，已丢弃");
                return;
            }
        };

        let handler = Arc::clone(handler);
        let tx = tx.clone();
        let budget = self.config.handler_budget;
        tokio::spawn(async move {
            // 处理放在独立任务里：超出预算时先回 ACK，处理本身不被取消
            // （丢弃 JoinHandle 只是分离，任务照常跑完）
            let work = tokio::spawn(async move { handler.handle(kind, whole).await });
            let (code, data) = match tokio::time::timeout(budget, work).await {
                Ok(Ok(Ok(data))) => (200, data),
                Ok(Ok(Err(reason))) => {
                    tracing::warn!(%reason, "数据帧处理失败");
                    (500, None)
                }
                Ok(Err(err)) => {
                    tracing::error!(%err, "数据帧处理任务异常结束");
                    (500, None)
                }
                Err(_) => {
                    tracing::warn!(
                        budget_ms = budget.as_millis(),
                        "数据帧处理超出预算，先回 500"
                    );
                    (500, None)
                }
            };
            let reply = ack(&frame, code, data.as_deref(), started.elapsed());
            if tx
                .send(Message::Binary(reply.encode_bytes().into()))
                .await
                .is_err()
            {
                tracing::debug!("连接已断开，ACK 未发出");
            }
        });
    }
}

fn parse_header(frame: &Frame, key: &str) -> Option<usize> {
    frame.header(key).and_then(|v| v.parse().ok())
}

/// 握手失败的分类，对齐官方 SDK：403 与「连接数超限」不该频繁重试。
fn classify_handshake_error(err: tungstenite::Error) -> Outcome {
    let tungstenite::Error::Http(response) = err else {
        return Outcome::Retryable(format!("握手失败：{err}"));
    };
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let status = header("handshake-status")
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or_else(|| response.status().as_u16());
    let message = header("handshake-msg").unwrap_or_default();
    let auth_code = header("handshake-autherrcode").unwrap_or_default();
    let reason = format!("握手被拒：status={status} msg={message} autherrcode={auth_code}");
    match status {
        403 => Outcome::Fatal(reason),
        514 if auth_code == EXCEED_CONN_LIMIT => Outcome::Fatal(reason),
        _ => Outcome::Retryable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_reconnect_is_jittered_then_the_interval_applies() {
        let mut policy = ReconnectPolicy::default();
        policy.apply(&ClientConfig {
            reconnect_interval: 7,
            reconnect_nonce: 3,
            ping_interval: 0,
        });
        let first = policy.next_delay();
        assert!(first < Duration::from_secs(3), "{first:?}");
        assert_eq!(policy.next_delay(), Duration::from_secs(7));
        assert_eq!(policy.next_delay(), Duration::from_secs(7));
        policy.reset();
        assert!(policy.next_delay() < Duration::from_secs(3));
    }

    #[test]
    fn non_positive_server_values_keep_the_current_ones() {
        let mut policy = ReconnectPolicy::default();
        policy.apply(&ClientConfig {
            reconnect_interval: 0,
            reconnect_nonce: -1,
            ping_interval: 0,
        });
        assert_eq!(
            policy.interval,
            Duration::from_secs(DEFAULT_RECONNECT_INTERVAL_SECS)
        );
        assert_eq!(
            policy.nonce,
            Duration::from_secs(DEFAULT_RECONNECT_NONCE_SECS)
        );
    }

    #[test]
    fn client_config_uses_the_sdk_field_names() {
        let config: ClientConfig = serde_json::from_str(
            r#"{"ReconnectCount":-1,"ReconnectInterval":120,"ReconnectNonce":30,"PingInterval":90}"#,
        )
        .expect("解析");
        assert_eq!(config.ping_interval, 90);
        assert_eq!(config.reconnect_interval, 120);
        assert_eq!(config.reconnect_nonce, 30);
    }
}
