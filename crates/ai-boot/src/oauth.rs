//! 以用户身份读云文档：OAuth 2.0 授权码 + PKCE，token 落库，后台续期。
//!
//! - 发起：只给白名单里的人发起；state 一次性、10 分钟过期，PKCE 用 S256。
//! - 回调：state 核对后换 token，再用 token 查「这是谁」，必须就是发起授权的人。
//! - 取用：快过期就先刷新。刷新单飞（同一时刻只刷一次）；refresh_token 每用一次
//!   就轮换、旧的立即作废，新的必须马上落库。被拒（吊销、过期、已用过）就标记为
//!   需要重新授权，由调用方私聊发授权卡片。
//! - 续期：refresh_token 约 7 天过期，后台每隔几天主动刷新一次，最长 365 天。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ai_boot_feishu::api::ApiClient;
use anyhow::Context as _;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse as _, Redirect, Response};
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng as _;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::store::{NewToken, Store, now_ms};
use crate::whitelist::Whitelist;

pub const CALLBACK_PATH: &str = "/oauth/feishu/callback";
const START_PATH: &str = "/oauth/feishu/start";
const STATE_TTL_MS: i64 = 10 * 60 * 1000;
/// 同时进行中的授权上限：发起接口不要求登录，防止被刷满。
const MAX_PENDING: i64 = 50;
/// access_token 剩这么久就先刷新再用。
const ACCESS_MARGIN_MS: i64 = 5 * 60 * 1000;
/// 后台续期：access_token 剩这么久，或者上次换 token 已过这么久，就刷新。
const BACKGROUND_ACCESS_MARGIN_MS: i64 = 10 * 60 * 1000;
const BACKGROUND_REFRESH_AGE_MS: i64 = 3 * 24 * 3600 * 1000;
const BACKGROUND_EVERY: Duration = Duration::from_secs(5 * 60);
/// 同一个人多久最多私聊一次授权卡片。
const PROMPT_EVERY: Duration = Duration::from_secs(6 * 3600);
/// 刷新被拒、只能重新授权的错误码（官方文档「刷新 user_access_token」与
/// 「获取 user_access_token」）：用户无权限、token 不属于本应用、token 无效、
/// 超过 365 天、已吊销、已用过、应用没开刷新开关。
const REAUTH_CODES: [i64; 7] = [20010, 20024, 20026, 20037, 20064, 20073, 20074];
const PKCE_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// 取 token 的结果。
pub enum Access {
    Ready(SecretString),
    /// 没授权过或授权失效：要私聊授权卡片。
    Unauthorized(String),
    /// 暂时取不到（网络、数据库）：这一轮不读文档，下次再试。
    Unavailable(String),
}

pub struct Oauth {
    api: Arc<ApiClient>,
    store: Store,
    whitelist: Arc<Whitelist>,
    app_id: String,
    redirect_uri: Url,
    accounts_url: Url,
    scopes: String,
    refresh_lock: tokio::sync::Mutex<()>,
    prompted: Mutex<HashMap<String, Instant>>,
}

pub struct OauthSettings {
    pub app_id: String,
    pub redirect_uri: Url,
    pub accounts_url: Url,
    pub scopes: Vec<String>,
}

impl Oauth {
    pub fn new(
        api: Arc<ApiClient>,
        store: Store,
        whitelist: Arc<Whitelist>,
        settings: OauthSettings,
    ) -> Self {
        Self {
            api,
            store,
            whitelist,
            app_id: settings.app_id,
            redirect_uri: settings.redirect_uri,
            accounts_url: settings.accounts_url,
            scopes: settings.scopes.join(" "),
            refresh_lock: tokio::sync::Mutex::new(()),
            prompted: Mutex::new(HashMap::new()),
        }
    }

    /// 授权卡片上的按钮链接：指向我们自己的发起接口，点的时候才生成 state，
    /// 卡片放多久都不会过期。
    pub fn start_link(&self, open_id: &str) -> String {
        let mut url = self.redirect_uri.clone();
        url.set_path(START_PATH);
        url.set_query(None);
        url.query_pairs_mut().append_pair("user", open_id);
        url.to_string()
    }

    /// 发起授权，返回飞书授权页的地址。
    pub async fn begin(&self, open_id: &str) -> Result<Url, String> {
        if !self.whitelist.allows(open_id) {
            return Err("这个账号没有使用机器人的权限".to_owned());
        }
        let now = now_ms();
        let pending = self
            .store
            .purge_oauth_states(now)
            .await
            .map_err(|err| format!("{err:#}"))?;
        if pending >= MAX_PENDING {
            return Err("授权请求太多，稍后再试".to_owned());
        }
        let state = random_string(43);
        let verifier = random_string(64);
        self.store
            .create_oauth_state(&state, open_id, &verifier, now + STATE_TTL_MS)
            .await
            .map_err(|err| format!("{err:#}"))?;
        let mut url = self
            .accounts_url
            .join("open-apis/authen/v1/authorize")
            .map_err(|err| format!("授权页地址不合法：{err}"))?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.app_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", self.redirect_uri.as_str())
            .append_pair("scope", &self.scopes)
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge(&verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(url)
    }

    /// 回调：换 token、核对授权人、落库。返回授权人的名字。
    pub async fn finish(&self, state: &str, code: &str) -> Result<String, String> {
        let now = now_ms();
        let (open_id, verifier) = self
            .store
            .take_oauth_state(state, now)
            .await
            .map_err(|err| format!("{err:#}"))?
            .ok_or_else(|| "授权链接已过期或已经用过，请回到飞书重新点「去授权」".to_owned())?;
        let token = self
            .api
            .exchange_code(code, &verifier, self.redirect_uri.as_str())
            .await
            .map_err(|err| format!("换取授权失败：{err}"))?;
        let who = self
            .api
            .user_info(&token.access_token)
            .await
            .map_err(|err| format!("确认授权人失败：{err}"))?;
        if who.open_id != open_id {
            tracing::warn!(expected = %open_id, actual = %who.open_id, "授权人与发起人不一致，已丢弃");
            return Err("完成授权的飞书账号不是发起授权的人，没有保存".to_owned());
        }
        if token.refresh_token.is_none() {
            tracing::warn!(
                "授权结果里没有 refresh_token，两小时后需要重新授权（检查 offline_access）"
            );
        }
        let expires =
            |secs: u64| now + i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
        let access = token.access_token.expose_secret().to_owned();
        let refresh = token
            .refresh_token
            .as_ref()
            .map(|t| t.expose_secret().to_owned());
        self.store
            .save_user_token(&NewToken {
                open_id: &open_id,
                access_token: &access,
                access_expires_at: expires(token.expires_in),
                refresh_token: refresh.as_deref(),
                refresh_expires_at: token.refresh_expires_in.map(expires),
                scopes: &token.scope,
                now_ms: now,
            })
            .await
            .map_err(|err| format!("{err:#}"))?;
        tracing::info!(open_id = %open_id, "云文档授权完成");
        if let Ok(mut prompted) = self.prompted.lock() {
            prompted.remove(&open_id);
        }
        Ok(who.name)
    }

    /// 取某个人可用的 user_access_token。
    pub async fn token(&self, open_id: &str) -> Access {
        if let Ok(Some(stored)) = self.store.user_token(open_id).await
            && !stored.needs_reauth
            && stored.access_expires_at - now_ms() > ACCESS_MARGIN_MS
        {
            return Access::Ready(stored.access_token);
        }
        self.refresh(open_id, false).await
    }

    /// 文档接口说 token 无效：先强刷一次，还不行就只能重新授权。
    pub async fn rejected(&self, open_id: &str) -> Access {
        self.refresh(open_id, true).await
    }

    async fn refresh(&self, open_id: &str, force: bool) -> Access {
        let _single = self.refresh_lock.lock().await;
        let now = now_ms();
        let stored = match self.store.user_token(open_id).await {
            Ok(Some(stored)) => stored,
            Ok(None) => return Access::Unauthorized("还没有授权读取云文档".to_owned()),
            Err(err) => return Access::Unavailable(format!("{err:#}")),
        };
        if stored.needs_reauth {
            return Access::Unauthorized("云文档授权已失效，需要重新授权".to_owned());
        }
        // 排队等锁的时候，别人可能已经刷新过了
        if !force && stored.access_expires_at - now > ACCESS_MARGIN_MS {
            return Access::Ready(stored.access_token);
        }
        let Some(refresh) = stored.refresh_token else {
            return if stored.access_expires_at > now && !force {
                Access::Ready(stored.access_token)
            } else {
                Access::Unauthorized("授权不能续期（缺 offline_access），需要重新授权".to_owned())
            };
        };
        match self.api.refresh_user_token(&refresh).await {
            Ok(token) => {
                let expires =
                    |secs: u64| now + i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX);
                let access = token.access_token.expose_secret().to_owned();
                let new_refresh = token
                    .refresh_token
                    .as_ref()
                    .map(|t| t.expose_secret().to_owned());
                let saved = self
                    .store
                    .rotate_user_token(
                        refresh.expose_secret(),
                        &NewToken {
                            open_id,
                            access_token: &access,
                            access_expires_at: expires(token.expires_in),
                            refresh_token: new_refresh.as_deref(),
                            refresh_expires_at: token.refresh_expires_in.map(expires),
                            scopes: &token.scope,
                            now_ms: now,
                        },
                    )
                    .await;
                match saved {
                    Ok(true) => tracing::debug!(open_id, "用户 token 已刷新"),
                    Ok(false) => tracing::warn!(open_id, "刷新期间库里的 token 已被替换"),
                    // 旧的已经作废、新的没存下：本进程还能用到 access_token 过期
                    Err(err) => tracing::error!(open_id, "刷新后的 token 没能落库：{err:#}"),
                }
                Access::Ready(token.access_token)
            }
            Err(err) if err.code().is_some_and(|code| REAUTH_CODES.contains(&code)) => {
                tracing::warn!(open_id, %err, "刷新被拒，需要重新授权");
                if let Err(err) = self.store.mark_reauth(open_id).await {
                    tracing::error!("{err:#}");
                }
                Access::Unauthorized(format!("云文档授权已失效（{err}），需要重新授权"))
            }
            Err(err) if stored.access_expires_at > now && !force => {
                tracing::warn!(open_id, %err, "刷新失败，先用还没过期的 token");
                Access::Ready(stored.access_token)
            }
            Err(err) => Access::Unavailable(format!("刷新云文档授权失败：{err}")),
        }
    }

    /// 后台续期一轮。
    pub async fn refresh_due(&self) {
        let now = now_ms();
        let due = match self
            .store
            .tokens_due(
                now + BACKGROUND_ACCESS_MARGIN_MS,
                now - BACKGROUND_REFRESH_AGE_MS,
            )
            .await
        {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!("{err:#}");
                return;
            }
        };
        for open_id in due {
            if let Access::Unauthorized(reason) | Access::Unavailable(reason) =
                self.refresh(&open_id, true).await
            {
                tracing::warn!(open_id = %open_id, %reason, "后台续期失败");
            }
        }
        if let Err(err) = self.store.purge_oauth_states(now).await {
            tracing::warn!("{err:#}");
        }
    }

    /// 要不要私聊授权卡片：同一个人一段时间内只发一次。
    pub fn should_prompt(&self, open_id: &str) -> bool {
        let Ok(mut prompted) = self.prompted.lock() else {
            return false;
        };
        let now = Instant::now();
        match prompted.get(open_id) {
            Some(at) if now.duration_since(*at) < PROMPT_EVERY => false,
            _ => {
                prompted.insert(open_id.to_owned(), now);
                true
            }
        }
    }
}

/// 后台续期，直到退出。
pub async fn keep_fresh(oauth: Arc<Oauth>, cancel: CancellationToken) {
    loop {
        oauth.refresh_due().await;
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(BACKGROUND_EVERY) => {}
        }
    }
}

fn random_string(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| char::from(PKCE_CHARS[rng.random_range(0..PKCE_CHARS.len())]))
        .collect()
}

/// PKCE S256：BASE64URL(SHA256(verifier))，不带填充。
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 回调服务：发起授权（跳转到飞书授权页）与授权回调两个路由。
pub async fn serve(
    oauth: Arc<Oauth>,
    listen: SocketAddr,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("OAuth 回调服务监听 {listen} 失败"))?;
    tracing::info!(%listen, "OAuth 回调服务已启动");
    axum::serve(listener, router(oauth))
        .with_graceful_shutdown(cancel.cancelled_owned())
        .await
        .context("OAuth 回调服务异常退出")
}

fn router(oauth: Arc<Oauth>) -> Router {
    Router::new()
        .route(START_PATH, get(start))
        .route(CALLBACK_PATH, get(callback))
        .with_state(oauth)
}

#[derive(Deserialize)]
struct StartQuery {
    user: String,
}

async fn start(State(oauth): State<Arc<Oauth>>, Query(query): Query<StartQuery>) -> Response {
    match oauth.begin(&query.user).await {
        Ok(url) => Redirect::to(url.as_str()).into_response(),
        Err(message) => page(StatusCode::FORBIDDEN, &message),
    }
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(State(oauth): State<Arc<Oauth>>, Query(query): Query<CallbackQuery>) -> Response {
    if let Some(error) = query.error {
        return page(StatusCode::BAD_REQUEST, &format!("授权没有完成（{error}）"));
    }
    let (Some(code), Some(state)) = (query.code, query.state) else {
        return page(StatusCode::BAD_REQUEST, "缺少授权码");
    };
    match oauth.finish(&state, &code).await {
        Ok(name) => page(
            StatusCode::OK,
            &format!(
                "{name}，授权完成。回到飞书继续提问即可，机器人会以你的身份读取群里贴的云文档。"
            ),
        ),
        Err(message) => page(StatusCode::BAD_REQUEST, &message),
    }
}

fn page(status: StatusCode, message: &str) -> Response {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    (
        status,
        Html(format!(
            "<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>ai-boot 授权</title><body><p>{escaped}</p></body></html>"
        )),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const BOSS: &str = "ou_boss";

    struct Fixture {
        oauth: Arc<Oauth>,
        store: Store,
        server: MockServer,
        _dir: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("数据库");
        let base = Url::parse(&format!("{}/", server.uri())).expect("地址");
        let api = Arc::new(
            ApiClient::new(base, "cli_x", SecretString::from("app-secret")).expect("客户端"),
        );
        let oauth = Arc::new(Oauth::new(
            api,
            store.clone(),
            Arc::new(Whitelist::new([BOSS.to_owned()])),
            OauthSettings {
                app_id: "cli_x".into(),
                redirect_uri: Url::parse("http://10.0.0.8:18080/oauth/feishu/callback")
                    .expect("地址"),
                accounts_url: Url::parse("https://accounts.example.com/").expect("地址"),
                scopes: vec!["offline_access".into(), "docx:document:readonly".into()],
            },
        ));
        Fixture {
            oauth,
            store,
            server,
            _dir: dir,
        }
    }

    fn token_response(access: &str, refresh: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "code": 0, "access_token": access, "expires_in": 7200,
            "refresh_token": refresh, "refresh_token_expires_in": 604800,
            "scope": "offline_access docx:document:readonly", "token_type": "Bearer"
        }))
    }

    async fn mount_user(f: &Fixture, access: &str, open_id: &str) {
        Mock::given(method("GET"))
            .and(path("/open-apis/authen/v1/user_info"))
            .and(header("authorization", format!("Bearer {access}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "code": 0, "msg": "ok", "data": {"open_id": open_id, "name": "朱工"}
            })))
            .mount(&f.server)
            .await;
    }

    fn query(url: &Url, key: &str) -> Option<String> {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
    }

    /// 发起授权并完成回调，返回 state。
    async fn authorize(f: &Fixture) {
        let url = f.oauth.begin(BOSS).await.expect("发起");
        let state = query(&url, "state").expect("state");
        Mock::given(method("POST"))
            .and(path("/open-apis/authen/v2/oauth/token"))
            .and(body_partial_json(
                json!({"grant_type": "authorization_code", "code": "c-1"}),
            ))
            .respond_with(token_response("u-1", "r-1"))
            .mount(&f.server)
            .await;
        mount_user(f, "u-1", BOSS).await;
        f.oauth.finish(&state, "c-1").await.expect("完成");
    }

    fn ready(access: Access) -> String {
        match access {
            Access::Ready(token) => token.expose_secret().to_owned(),
            Access::Unauthorized(reason) | Access::Unavailable(reason) => {
                panic!("应当拿到 token：{reason}")
            }
        }
    }

    #[tokio::test]
    async fn the_authorize_url_carries_pkce_and_only_whitelisted_people_can_start() {
        let f = fixture().await;
        assert!(f.oauth.begin("ou_other").await.is_err());
        let url = f.oauth.begin(BOSS).await.expect("发起");
        assert_eq!(url.host_str(), Some("accounts.example.com"));
        assert_eq!(url.path(), "/open-apis/authen/v1/authorize");
        assert_eq!(query(&url, "client_id").as_deref(), Some("cli_x"));
        assert_eq!(query(&url, "response_type").as_deref(), Some("code"));
        assert_eq!(
            query(&url, "redirect_uri").as_deref(),
            Some("http://10.0.0.8:18080/oauth/feishu/callback")
        );
        assert_eq!(
            query(&url, "scope").as_deref(),
            Some("offline_access docx:document:readonly")
        );
        assert_eq!(
            query(&url, "code_challenge_method").as_deref(),
            Some("S256")
        );
        assert_eq!(query(&url, "code_challenge").map(|c| c.len()), Some(43));
        assert_eq!(
            f.oauth.start_link(BOSS),
            "http://10.0.0.8:18080/oauth/feishu/start?user=ou_boss"
        );
    }

    #[tokio::test]
    async fn a_completed_authorization_is_stored_and_used_until_near_expiry() {
        let f = fixture().await;
        authorize(&f).await;
        assert_eq!(ready(f.oauth.token(BOSS).await), "u-1");
        // state 一次性
        let url = f.oauth.begin(BOSS).await.expect("再发起");
        let state = query(&url, "state").expect("state");
        mount_user(&f, "u-1", BOSS).await;
        f.oauth.finish(&state, "c-1").await.expect("第二次授权");
        assert!(
            f.oauth.finish(&state, "c-1").await.is_err(),
            "state 只能用一次"
        );
    }

    #[tokio::test]
    async fn someone_else_completing_the_authorization_is_rejected() {
        let f = fixture().await;
        let url = f.oauth.begin(BOSS).await.expect("发起");
        let state = query(&url, "state").expect("state");
        Mock::given(method("POST"))
            .and(path("/open-apis/authen/v2/oauth/token"))
            .respond_with(token_response("u-evil", "r-evil"))
            .mount(&f.server)
            .await;
        mount_user(&f, "u-evil", "ou_evil").await;
        let err = f.oauth.finish(&state, "c-9").await.expect_err("应当拒绝");
        assert!(err.contains("不是发起授权的人"), "{err}");
        assert!(f.store.user_token(BOSS).await.expect("查询").is_none());
        assert!(f.store.user_token("ou_evil").await.expect("查询").is_none());
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh_and_the_rotation_is_saved() {
        let f = fixture().await;
        // 一个快过期的 token
        f.store
            .save_user_token(&NewToken {
                open_id: BOSS,
                access_token: "u-old",
                access_expires_at: now_ms() + 60_000,
                refresh_token: Some("r-old"),
                refresh_expires_at: Some(now_ms() + 604_800_000),
                scopes: "offline_access",
                now_ms: now_ms(),
            })
            .await
            .expect("保存");
        Mock::given(method("POST"))
            .and(path("/open-apis/authen/v2/oauth/token"))
            .and(body_partial_json(
                json!({"grant_type": "refresh_token", "refresh_token": "r-old"}),
            ))
            .respond_with(token_response("u-new", "r-new"))
            .expect(1)
            .mount(&f.server)
            .await;
        let (a, b, c) = tokio::join!(
            f.oauth.token(BOSS),
            f.oauth.token(BOSS),
            f.oauth.token(BOSS)
        );
        for access in [a, b, c] {
            assert_eq!(ready(access), "u-new");
        }
        let stored = f.store.user_token(BOSS).await.expect("查询").expect("有");
        assert_eq!(
            stored.refresh_token.map(|t| t.expose_secret().to_owned()),
            Some("r-new".to_owned()),
            "新的 refresh_token 必须落库"
        );
    }

    #[tokio::test]
    async fn a_revoked_refresh_token_means_reauthorisation() {
        let f = fixture().await;
        f.store
            .save_user_token(&NewToken {
                open_id: BOSS,
                access_token: "u-old",
                access_expires_at: now_ms() - 1,
                refresh_token: Some("r-used"),
                refresh_expires_at: Some(now_ms() + 1),
                scopes: "offline_access",
                now_ms: now_ms(),
            })
            .await
            .expect("保存");
        Mock::given(method("POST"))
            .and(path("/open-apis/authen/v2/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "code": 20064, "error": "invalid_grant", "error_description": "revoked"
            })))
            .expect(1)
            .mount(&f.server)
            .await;
        assert!(matches!(f.oauth.token(BOSS).await, Access::Unauthorized(_)));
        // 已标记，不再去刷
        assert!(matches!(f.oauth.token(BOSS).await, Access::Unauthorized(_)));
        assert!(
            f.store
                .user_token(BOSS)
                .await
                .expect("查询")
                .expect("有")
                .needs_reauth
        );
        assert!(f.oauth.should_prompt(BOSS));
        assert!(!f.oauth.should_prompt(BOSS), "一段时间内只私聊一次");
    }

    #[tokio::test]
    async fn the_callback_server_redirects_and_reports_in_plain_pages() {
        let f = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("端口");
        let addr = listener.local_addr().expect("地址");
        let app = router(Arc::clone(&f.oauth));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("客户端");

        let started = http
            .get(format!("http://{addr}{START_PATH}?user={BOSS}"))
            .send()
            .await
            .expect("发起");
        assert_eq!(started.status(), 303);
        let location = started
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .expect("跳转地址")
            .to_owned();
        assert!(
            location.starts_with("https://accounts.example.com/open-apis/authen/v1/authorize?")
        );

        let outsider = http
            .get(format!("http://{addr}{START_PATH}?user=ou_other"))
            .send()
            .await
            .expect("外人");
        assert_eq!(outsider.status(), 403);

        let denied = http
            .get(format!(
                "http://{addr}{CALLBACK_PATH}?error=access_denied&state=x"
            ))
            .send()
            .await
            .expect("拒绝");
        assert_eq!(denied.status(), 400);
        let body = denied.text().await.expect("正文");
        assert!(body.contains("授权没有完成"), "{body}");

        let forged = http
            .get(format!(
                "http://{addr}{CALLBACK_PATH}?code=c&state=<script>"
            ))
            .send()
            .await
            .expect("伪造");
        assert_eq!(forged.status(), 400);
        assert!(!forged.text().await.expect("正文").contains("<script>"));
    }

    #[test]
    fn the_pkce_challenge_matches_the_rfc_example() {
        // RFC 7636 附录 B 的例子（期望值用 openssl 独立算过）
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn random_strings_use_only_unreserved_characters() {
        let value = random_string(128);
        assert_eq!(value.len(), 128);
        assert!(value.bytes().all(|b| PKCE_CHARS.contains(&b)));
        assert_ne!(random_string(43), random_string(43));
    }
}
