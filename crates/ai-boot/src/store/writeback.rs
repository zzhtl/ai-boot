//! 写回记录：同一轮对同一个目标只有一行，重复点击、重试都落在这一行上。

use anyhow::Context as _;

use super::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum WritebackStatus {
    Running,
    Done,
    Failed,
    /// 调用超时或进程中断，不知道写没写成：再点一次会先查重。
    Unknown,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Writeback {
    pub id: String,
    pub target: String,
    pub target_ref: String,
    pub status: WritebackStatus,
    pub result_url: Option<String>,
    pub error: Option<String>,
}

pub struct NewWriteback<'a> {
    /// 新建时用的 ID；已有记录时沿用原来的，幂等标记才不会变。
    pub id: &'a str,
    pub turn_id: &'a str,
    pub target: &'a str,
    pub target_ref: &'a str,
    pub content_hash: &'a str,
    pub requested_by: &'a str,
    pub now_ms: i64,
}

impl Store {
    /// 开始一次写回。返回这一行，以及要不要真的去执行：进行中或已完成的不再
    /// 执行；失败或结果不明的改回进行中重试。
    pub async fn begin_writeback(
        &self,
        new: &NewWriteback<'_>,
    ) -> anyhow::Result<(Writeback, bool)> {
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("开启事务失败")?;
        let existing: Option<Writeback> = sqlx::query_as(
            "SELECT id, target, target_ref, status, result_url, error
             FROM writebacks WHERE turn_id = ? AND target = ? AND target_ref = ?",
        )
        .bind(new.turn_id)
        .bind(new.target)
        .bind(new.target_ref)
        .fetch_optional(&mut *tx)
        .await
        .context("查询写回记录失败")?;
        let outcome = match existing {
            Some(row) if matches!(row.status, WritebackStatus::Running | WritebackStatus::Done) => {
                (row, false)
            }
            Some(row) => {
                sqlx::query(
                    "UPDATE writebacks
                     SET status = 'running', content_hash = ?, error = NULL, updated_at_ms = ?
                     WHERE id = ?",
                )
                .bind(new.content_hash)
                .bind(new.now_ms)
                .bind(&row.id)
                .execute(&mut *tx)
                .await
                .context("更新写回记录失败")?;
                (
                    Writeback {
                        status: WritebackStatus::Running,
                        error: None,
                        ..row
                    },
                    true,
                )
            }
            None => {
                sqlx::query(
                    "INSERT INTO writebacks
                         (id, turn_id, target, target_ref, content_hash, status, requested_by,
                          created_at_ms, updated_at_ms)
                     VALUES (?, ?, ?, ?, ?, 'running', ?, ?, ?)",
                )
                .bind(new.id)
                .bind(new.turn_id)
                .bind(new.target)
                .bind(new.target_ref)
                .bind(new.content_hash)
                .bind(new.requested_by)
                .bind(new.now_ms)
                .bind(new.now_ms)
                .execute(&mut *tx)
                .await
                .context("创建写回记录失败")?;
                (
                    Writeback {
                        id: new.id.to_owned(),
                        target: new.target.to_owned(),
                        target_ref: new.target_ref.to_owned(),
                        status: WritebackStatus::Running,
                        result_url: None,
                        error: None,
                    },
                    true,
                )
            }
        };
        tx.commit().await.context("提交事务失败")?;
        Ok(outcome)
    }

    pub async fn finish_writeback(
        &self,
        id: &str,
        status: WritebackStatus,
        result_url: Option<&str>,
        error: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE writebacks SET status = ?, result_url = ?, error = ?, updated_at_ms = ?
             WHERE id = ?",
        )
        .bind(status)
        .bind(result_url)
        .bind(error)
        .bind(now_ms)
        .bind(id)
        .execute(&self.pool)
        .await
        .context("更新写回记录失败")?;
        Ok(())
    }

    pub async fn writebacks(&self, turn_id: &str) -> anyhow::Result<Vec<Writeback>> {
        sqlx::query_as(
            "SELECT id, target, target_ref, status, result_url, error
             FROM writebacks WHERE turn_id = ? ORDER BY created_at_ms",
        )
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await
        .context("查询写回记录失败")
    }

    /// 启动时调用：上次退出时还在写的，结果不明。返回这些写回所在的轮次。
    pub async fn interrupt_writebacks(&self, now_ms: i64) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "UPDATE writebacks SET status = 'unknown', error = '服务重启，结果不明', updated_at_ms = ?
             WHERE status = 'running'
             RETURNING turn_id",
        )
        .bind(now_ms)
        .fetch_all(&self.pool)
        .await
        .context("更新写回记录失败")?;
        let mut turns: Vec<String> = rows.into_iter().map(|(id,)| id).collect();
        turns.dedup();
        Ok(turns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewConversation, NewInput, Origin};

    async fn store_with_turn() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        store
            .create_conversation(&NewConversation {
                id: "c1",
                chat_id: "oc_1",
                chat_type: "group",
                origin: Origin::NewThread,
                thread_id: None,
                root_message_id: "om_1",
                owner_open_id: "ou_1",
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
                sender_open_id: "ou_1",
                payload: "{}",
                received_at_ms: 1,
            })
            .await
            .expect("输入");
        store
            .create_turn("c1", "t1", "om_1", 1)
            .await
            .expect("轮次");
        (store, dir)
    }

    fn new<'a>(id: &'a str) -> NewWriteback<'a> {
        NewWriteback {
            id,
            turn_id: "t1",
            target: "jira",
            target_ref: "ABC-1",
            content_hash: "h1",
            requested_by: "ou_1",
            now_ms: 5,
        }
    }

    #[tokio::test]
    async fn a_second_click_does_not_run_again_while_running_or_after_success() {
        let (store, _dir) = store_with_turn().await;
        let (row, run) = store.begin_writeback(&new("w1")).await.expect("开始");
        assert!(run);
        let (again, run_again) = store.begin_writeback(&new("w2")).await.expect("再点");
        assert!(!run_again, "进行中不重复执行");
        assert_eq!(again.id, row.id);
        store
            .finish_writeback(&row.id, WritebackStatus::Done, Some("https://x"), None, 6)
            .await
            .expect("完成");
        let (done, run_after) = store.begin_writeback(&new("w3")).await.expect("完成后再点");
        assert!(!run_after);
        assert_eq!(done.result_url.as_deref(), Some("https://x"));
    }

    #[tokio::test]
    async fn an_unclear_outcome_is_retried_under_the_same_id() {
        let (store, _dir) = store_with_turn().await;
        let (row, _) = store.begin_writeback(&new("w1")).await.expect("开始");
        assert_eq!(store.interrupt_writebacks(9).await.expect("重启"), ["t1"]);
        let rows = store.writebacks("t1").await.expect("查询");
        assert_eq!(rows[0].status, WritebackStatus::Unknown);
        let (retry, run) = store.begin_writeback(&new("w-new")).await.expect("重试");
        assert!(run);
        assert_eq!(retry.id, row.id, "沿用原来的 ID，幂等标记不变");
    }
}
