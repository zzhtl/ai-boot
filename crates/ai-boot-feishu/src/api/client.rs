//! OpenAPI 调用：tenant_access_token 缓存与统一的重试策略。
//!
//! 重试只覆盖不改变语义的情况：限流（按 `x-ogw-ratelimit-reset` 等待）、
//! 5xx、连接或超时错误、token 失效（刷新后重放一次）。请求在超时前可能已经
//! 到达服务端，所以有副作用的接口必须自带幂等键（例如回复消息的 `uuid`）。

use std::time::Duration;

use rand::Rng as _;
use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::time::Instant;
use url::Url;

const MAX_ATTEMPTS: u32 = 3;
/// token 失效类的业务码，遇到时强制刷新 token 再重放一次。
const TOKEN_INVALID_CODES: [i64; 3] = [99_991_663, 99_991_664, 99_991_671];
const RATE_LIMITED_CODE: i64 = 99_991_400;
/// 离过期还剩多久时刷新。飞书在剩余不足 30 分钟时才会签发新 token，
/// 旧 token 在过期前一直有效，所以提前几分钟即可。
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(300);
const RATE_LIMIT_WAIT_CAP: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApiError {
    #[error("飞书接口返回错误 code={code}：{msg}")]
    Api { code: i64, msg: String },
    #[error("飞书接口返回 HTTP {status}")]
    Http { status: u16 },
    #[error("请求飞书接口失败：{0}")]
    Transport(#[source] reqwest::Error),
    #[error("飞书接口响应无法解析：{0}")]
    Decode(String),
    #[error("请求参数不合法：{0}")]
    InvalidArgument(String),
}

impl ApiError {
    /// 飞书业务错误码（仅 `Api` 变体有）。
    pub fn code(&self) -> Option<i64> {
        match self {
            Self::Api { code, .. } => Some(*code),
            _ => None,
        }
    }
}

struct CachedToken {
    value: SecretString,
    refresh_at: Instant,
}

#[derive(Deserialize)]
struct TokenResp {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    tenant_access_token: String,
    #[serde(default)]
    expire: u64,
}

/// 一次接口调用的描述。
pub(crate) struct Call {
    method: Method,
    path: String,
    query: Vec<(&'static str, String)>,
    body: Option<Value>,
}

impl Call {
    pub(crate) fn get(path: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            path: path.into(),
            query: Vec::new(),
            body: None,
        }
    }

    pub(crate) fn post(path: impl Into<String>, body: Value) -> Self {
        Self {
            method: Method::POST,
            path: path.into(),
            query: Vec::new(),
            body: Some(body),
        }
    }

    pub(crate) fn query(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.query.push((key, value.into()));
        self
    }
}

/// 拼进 URL 路径的 ID 只允许这些字符：来自事件的值不应该能改写路径。
pub(crate) fn path_segment(value: &str) -> Result<&str, ApiError> {
    let ok = !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(value)
    } else {
        Err(ApiError::InvalidArgument(format!("非法的 ID：{value:?}")))
    }
}

pub struct ApiClient {
    http: reqwest::Client,
    base: Url,
    app_id: String,
    app_secret: SecretString,
    token: Mutex<Option<CachedToken>>,
}

impl ApiClient {
    /// `base` 是开放平台地址，例如 `https://open.feishu.cn/`（末尾的 `/` 必须有）。
    pub fn new(
        base: Url,
        app_id: impl Into<String>,
        app_secret: SecretString,
    ) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(ApiError::Transport)?;
        Ok(Self {
            http,
            base,
            app_id: app_id.into(),
            app_secret,
            token: Mutex::new(None),
        })
    }

    fn url(&self, path: &str) -> Result<Url, ApiError> {
        self.base
            .join(path)
            .map_err(|err| ApiError::InvalidArgument(format!("接口路径不合法：{err}")))
    }

    /// 取可用的 tenant_access_token。持锁刷新，并发调用只会触发一次刷新。
    async fn tenant_token(&self) -> Result<SecretString, ApiError> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref()
            && Instant::now() < token.refresh_at
        {
            return Ok(token.value.clone());
        }
        let fresh = self.fetch_token().await?;
        let value = fresh.value.clone();
        *cached = Some(fresh);
        Ok(value)
    }

    async fn invalidate_token(&self) {
        *self.token.lock().await = None;
    }

    async fn fetch_token(&self) -> Result<CachedToken, ApiError> {
        let url = self.url("open-apis/auth/v3/tenant_access_token/internal")?;
        let body = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret.expose_secret(),
        });
        let response = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(ApiError::Transport)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(ApiError::Transport)?;
        let Ok(parsed) = serde_json::from_slice::<TokenResp>(&bytes) else {
            return Err(ApiError::Http {
                status: status.as_u16(),
            });
        };
        if parsed.code != 0 {
            return Err(ApiError::Api {
                code: parsed.code,
                msg: parsed.msg,
            });
        }
        let refresh_in = Duration::from_secs(parsed.expire)
            .saturating_sub(TOKEN_REFRESH_MARGIN)
            .max(Duration::from_secs(60));
        Ok(CachedToken {
            value: SecretString::from(parsed.tenant_access_token),
            refresh_at: Instant::now() + refresh_in,
        })
    }

    /// 调用接口并把 `data` 反序列化为 `T`。
    pub(crate) async fn call<T: DeserializeOwned>(&self, call: Call) -> Result<T, ApiError> {
        let mut value = self.call_raw(call).await?;
        let data = value
            .get_mut("data")
            .map(Value::take)
            .unwrap_or(Value::Null);
        serde_json::from_value(data).map_err(|err| ApiError::Decode(err.to_string()))
    }

    /// 调用接口，返回整个响应体（`code` 已确认为 0）。少数接口的数据不在
    /// `data` 下，由调用方自行取。
    pub(crate) async fn call_raw(&self, call: Call) -> Result<Value, ApiError> {
        let url = self.url(&call.path)?;
        let mut token_refreshed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let token = self.tenant_token().await?;
            let mut request = self
                .http
                .request(call.method.clone(), url.clone())
                .bearer_auth(token.expose_secret());
            if !call.query.is_empty() {
                request = request.query(&call.query);
            }
            if let Some(body) = &call.body {
                request = request.json(body);
            }

            let response = match request.send().await {
                Ok(response) => response,
                Err(err) if attempt < MAX_ATTEMPTS && is_transient(&err) => {
                    tracing::debug!(%err, attempt, path = %call.path, "请求失败，重试");
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(err) => return Err(ApiError::Transport(err)),
            };
            let status = response.status();
            let reset = response
                .headers()
                .get("x-ogw-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(err) if attempt < MAX_ATTEMPTS && is_transient(&err) => {
                    tokio::time::sleep(backoff(attempt)).await;
                    continue;
                }
                Err(err) => return Err(ApiError::Transport(err)),
            };
            let envelope = serde_json::from_slice::<Value>(&bytes).ok();
            let code = envelope
                .as_ref()
                .and_then(|v| v.get("code"))
                .and_then(Value::as_i64);

            let rate_limited =
                status == StatusCode::TOO_MANY_REQUESTS || code == Some(RATE_LIMITED_CODE);
            if rate_limited && attempt < MAX_ATTEMPTS {
                let wait = reset
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| backoff(attempt))
                    .min(RATE_LIMIT_WAIT_CAP);
                tracing::warn!(path = %call.path, wait_ms = wait.as_millis(), "触发限流，等待后重试");
                tokio::time::sleep(wait).await;
                continue;
            }
            if let Some(code) = code
                && TOKEN_INVALID_CODES.contains(&code)
                && !token_refreshed
            {
                token_refreshed = true;
                self.invalidate_token().await;
                continue;
            }
            if status.is_server_error() && attempt < MAX_ATTEMPTS {
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }

            return match (envelope, code) {
                (Some(value), Some(0)) => Ok(value),
                (Some(value), Some(code)) => Err(ApiError::Api {
                    code,
                    msg: value
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                _ => Err(ApiError::Http {
                    status: status.as_u16(),
                }),
            };
        }
    }
}

fn is_transient(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

/// 指数退避加抖动：约 200ms、400ms、800ms。
fn backoff(attempt: u32) -> Duration {
    let base = 200_u64.saturating_mul(1 << attempt.saturating_sub(1).min(5));
    Duration::from_millis(base + rand::rng().random_range(0..100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_that_could_rewrite_the_path_are_rejected() {
        assert!(path_segment("om_dc13264520392913993dd051dba21dcf").is_ok());
        for bad in ["", "../x", "a/b", "a?b", "a b", "a%2F"] {
            assert!(path_segment(bad).is_err(), "{bad}");
        }
    }
}
