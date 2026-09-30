//! 用户授权（OAuth 2.0 授权码 + PKCE）：换 token、刷新 token、取用户信息。
//!
//! token 接口的凭据在请求体里，不带 Authorization 头；响应字段在顶层，不在
//! `data` 里。refresh_token 每用一次就轮换，旧的立即失效，调用方必须马上保存
//! 新的。

use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::Value;

use super::client::{ApiClient, ApiError, Auth, Call};

const TOKEN_PATH: &str = "open-apis/authen/v2/oauth/token";

/// 一组用户 token。
#[derive(Debug, Clone)]
pub struct UserToken {
    pub access_token: SecretString,
    /// 秒。
    pub expires_in: u64,
    /// 授权时带了 `offline_access` 才有。
    pub refresh_token: Option<SecretString>,
    /// 秒。
    pub refresh_expires_in: Option<u64>,
    pub scope: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserInfo {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub open_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub name: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    scope: String,
}

impl ApiClient {
    /// 用授权码换 token。`code_verifier` 是发起授权时生成的 PKCE 原文。
    pub async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<UserToken, ApiError> {
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": self.app_id(),
            "client_secret": self.app_secret().expose_secret(),
            "code": code,
            "code_verifier": code_verifier,
            "redirect_uri": redirect_uri,
        });
        self.token_call(body).await
    }

    /// 刷新。成功后旧的 refresh_token 立即失效。
    pub async fn refresh_user_token(
        &self,
        refresh_token: &SecretString,
    ) -> Result<UserToken, ApiError> {
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": self.app_id(),
            "client_secret": self.app_secret().expose_secret(),
            "refresh_token": refresh_token.expose_secret(),
        });
        self.token_call(body).await
    }

    async fn token_call(&self, body: Value) -> Result<UserToken, ApiError> {
        let value = self
            .call_raw_with(Call::post(TOKEN_PATH, body), Auth::None)
            .await?;
        let parsed: TokenResponse =
            serde_json::from_value(value).map_err(|err| ApiError::Decode(err.to_string()))?;
        if parsed.access_token.is_empty() {
            return Err(ApiError::Decode("响应里没有 access_token".to_owned()));
        }
        Ok(UserToken {
            access_token: SecretString::from(parsed.access_token),
            expires_in: parsed.expires_in,
            refresh_token: parsed
                .refresh_token
                .filter(|t| !t.is_empty())
                .map(SecretString::from),
            refresh_expires_in: parsed.refresh_token_expires_in,
            scope: parsed.scope,
        })
    }

    /// 这个 token 属于谁。
    pub async fn user_info(&self, token: &SecretString) -> Result<UserInfo, ApiError> {
        self.call_as(
            Call::get("open-apis/authen/v1/user_info"),
            Auth::User(token),
        )
        .await
    }
}
