//! 用户 token 与进行中的授权。

use anyhow::Context as _;
use secrecy::SecretString;

use super::Store;

/// 库里的一组用户 token。
pub struct StoredToken {
    pub access_token: SecretString,
    pub access_expires_at: i64,
    pub refresh_token: Option<SecretString>,
    pub needs_reauth: bool,
}

/// 要保存的一组 token（授权完成或刷新成功）。
pub struct NewToken<'a> {
    pub open_id: &'a str,
    pub access_token: &'a str,
    pub access_expires_at: i64,
    pub refresh_token: Option<&'a str>,
    pub refresh_expires_at: Option<i64>,
    pub scopes: &'a str,
    pub now_ms: i64,
}

type TokenRow = (String, i64, Option<String>, i64);

impl Store {
    pub async fn user_token(&self, open_id: &str) -> anyhow::Result<Option<StoredToken>> {
        let row: Option<TokenRow> = sqlx::query_as(
            "SELECT access_token, access_expires_at, refresh_token, needs_reauth
             FROM user_tokens WHERE open_id = ?",
        )
        .bind(open_id)
        .fetch_optional(&self.pool)
        .await
        .context("查询用户 token 失败")?;
        Ok(row.map(|(access, expires, refresh, reauth)| StoredToken {
            access_token: SecretString::from(access),
            access_expires_at: expires,
            refresh_token: refresh.map(SecretString::from),
            needs_reauth: reauth != 0,
        }))
    }

    /// 授权完成：整组覆盖，清掉「需要重新授权」。
    pub async fn save_user_token(&self, token: &NewToken<'_>) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO user_tokens
                 (open_id, access_token, access_expires_at, refresh_token, refresh_expires_at,
                  scopes, needs_reauth, refreshed_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, 0, ?)
             ON CONFLICT (open_id) DO UPDATE SET
                 access_token = excluded.access_token,
                 access_expires_at = excluded.access_expires_at,
                 refresh_token = excluded.refresh_token,
                 refresh_expires_at = excluded.refresh_expires_at,
                 scopes = excluded.scopes,
                 needs_reauth = 0,
                 refreshed_at_ms = excluded.refreshed_at_ms",
        )
        .bind(token.open_id)
        .bind(token.access_token)
        .bind(token.access_expires_at)
        .bind(token.refresh_token)
        .bind(token.refresh_expires_at)
        .bind(token.scopes)
        .bind(token.now_ms)
        .execute(&self.pool)
        .await
        .context("保存用户 token 失败")?;
        Ok(())
    }

    /// 刷新成功后落库。只有库里还是 `old_refresh` 时才覆盖，返回是否覆盖了——
    /// 旧的 refresh_token 此刻已经作废，新的必须马上存下来。
    pub async fn rotate_user_token(
        &self,
        old_refresh: &str,
        token: &NewToken<'_>,
    ) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "UPDATE user_tokens
             SET access_token = ?, access_expires_at = ?, refresh_token = ?,
                 refresh_expires_at = ?, scopes = ?, needs_reauth = 0, refreshed_at_ms = ?
             WHERE open_id = ? AND refresh_token = ?",
        )
        .bind(token.access_token)
        .bind(token.access_expires_at)
        .bind(token.refresh_token)
        .bind(token.refresh_expires_at)
        .bind(token.scopes)
        .bind(token.now_ms)
        .bind(token.open_id)
        .bind(old_refresh)
        .execute(&self.pool)
        .await
        .context("保存刷新后的用户 token 失败")?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn mark_reauth(&self, open_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE user_tokens SET needs_reauth = 1 WHERE open_id = ?")
            .bind(open_id)
            .execute(&self.pool)
            .await
            .context("标记需要重新授权失败")?;
        Ok(())
    }

    /// 需要主动刷新的：access_token 快过期，或者 refresh_token 用了好几天。
    pub async fn tokens_due(
        &self,
        access_before: i64,
        refreshed_before: i64,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT open_id FROM user_tokens
             WHERE needs_reauth = 0 AND refresh_token IS NOT NULL
               AND (access_expires_at < ? OR refreshed_at_ms < ?)",
        )
        .bind(access_before)
        .bind(refreshed_before)
        .fetch_all(&self.pool)
        .await
        .context("查询待刷新的 token 失败")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn create_oauth_state(
        &self,
        state: &str,
        open_id: &str,
        code_verifier: &str,
        expires_at_ms: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO oauth_states (state, open_id, code_verifier, expires_at_ms)
             VALUES (?, ?, ?, ?)",
        )
        .bind(state)
        .bind(open_id)
        .bind(code_verifier)
        .bind(expires_at_ms)
        .execute(&self.pool)
        .await
        .context("保存授权状态失败")?;
        Ok(())
    }

    /// 取出并作废一个 state（一次性）。过期的当作不存在。返回发起人和 PKCE 原文。
    pub async fn take_oauth_state(
        &self,
        state: &str,
        now_ms: i64,
    ) -> anyhow::Result<Option<(String, String)>> {
        let row: Option<(String, String, i64)> = sqlx::query_as(
            "DELETE FROM oauth_states WHERE state = ?
             RETURNING open_id, code_verifier, expires_at_ms",
        )
        .bind(state)
        .fetch_optional(&self.pool)
        .await
        .context("读取授权状态失败")?;
        Ok(row
            .filter(|(_, _, expires)| *expires > now_ms)
            .map(|(open_id, verifier, _)| (open_id, verifier)))
    }

    /// 清掉过期的 state，返回还剩多少个。
    pub async fn purge_oauth_states(&self, now_ms: i64) -> anyhow::Result<i64> {
        sqlx::query("DELETE FROM oauth_states WHERE expires_at_ms <= ?")
            .bind(now_ms)
            .execute(&self.pool)
            .await
            .context("清理授权状态失败")?;
        let (left,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM oauth_states")
            .fetch_one(&self.pool)
            .await
            .context("统计授权状态失败")?;
        Ok(left)
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;

    fn token<'a>(access: &'a str, refresh: Option<&'a str>, now: i64) -> NewToken<'a> {
        NewToken {
            open_id: "ou_boss",
            access_token: access,
            access_expires_at: now + 7_200_000,
            refresh_token: refresh,
            refresh_expires_at: Some(now + 604_800_000),
            scopes: "offline_access",
            now_ms: now,
        }
    }

    #[tokio::test]
    async fn a_rotation_only_replaces_the_refresh_token_it_was_based_on() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        store
            .save_user_token(&token("a1", Some("r1"), 0))
            .await
            .expect("保存");
        assert!(
            store
                .rotate_user_token("r1", &token("a2", Some("r2"), 10))
                .await
                .expect("轮换")
        );
        // 拿旧的 r1 再来一次（并发刷新的另一方）：不能覆盖已经轮换过的
        assert!(
            !store
                .rotate_user_token("r1", &token("a3", Some("r3"), 20))
                .await
                .expect("轮换")
        );
        let stored = store
            .user_token("ou_boss")
            .await
            .expect("查询")
            .expect("有");
        assert_eq!(stored.access_token.expose_secret(), "a2");
        assert_eq!(
            stored.refresh_token.map(|t| t.expose_secret().to_owned()),
            Some("r2".to_owned())
        );
    }

    #[tokio::test]
    async fn reauthorising_clears_the_flag_and_due_tokens_are_found() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        store
            .save_user_token(&token("a1", Some("r1"), 0))
            .await
            .expect("保存");
        assert_eq!(
            store.tokens_due(1, -1).await.expect("查询"),
            Vec::<String>::new()
        );
        assert_eq!(
            store.tokens_due(7_200_001, -1).await.expect("查询"),
            ["ou_boss"]
        );
        store.mark_reauth("ou_boss").await.expect("标记");
        assert!(
            store
                .tokens_due(i64::MAX, i64::MAX)
                .await
                .expect("查询")
                .is_empty()
        );
        assert!(
            store
                .user_token("ou_boss")
                .await
                .expect("查询")
                .expect("有")
                .needs_reauth
        );
        store
            .save_user_token(&token("a9", Some("r9"), 5))
            .await
            .expect("重新授权");
        assert!(
            !store
                .user_token("ou_boss")
                .await
                .expect("查询")
                .expect("有")
                .needs_reauth
        );
    }

    #[tokio::test]
    async fn a_state_works_once_and_not_after_it_expires() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        store
            .create_oauth_state("s1", "ou_boss", "v1", 100)
            .await
            .expect("保存");
        store
            .create_oauth_state("s2", "ou_boss", "v2", 100)
            .await
            .expect("保存");
        assert_eq!(
            store.take_oauth_state("s1", 50).await.expect("取"),
            Some(("ou_boss".to_owned(), "v1".to_owned()))
        );
        assert_eq!(
            store.take_oauth_state("s1", 50).await.expect("取"),
            None,
            "一次性"
        );
        assert_eq!(
            store.take_oauth_state("s2", 100).await.expect("取"),
            None,
            "过期"
        );
        store
            .create_oauth_state("s3", "ou_boss", "v3", 100)
            .await
            .expect("保存");
        assert_eq!(store.purge_oauth_states(200).await.expect("清理"), 0);
    }
}
