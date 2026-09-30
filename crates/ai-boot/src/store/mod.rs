//! SQLite 存储。单机、单写者；WAL 让读不阻塞写。

mod conversation;
mod tokens;
mod writeback;

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

#[cfg(test)]
pub use conversation::Turn;
pub use conversation::{
    Conversation, FinishedTurn, InterruptedTurn, NewConversation, Origin, TurnKind, TurnRecord,
    TurnStatus,
};
pub use tokens::NewToken;
pub use writeback::{NewWriteback, Writeback, WritebackStatus};

const DB_FILE: &str = "ai-boot.db";

/// 当前时刻的 Unix 毫秒时间戳，库里的时间一律用它。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// 数据库句柄。克隆代价等同克隆一个 `Arc`。
#[derive(Debug, Clone)]
pub struct Store {
    pool: SqlitePool,
}

/// 一条待处理的输入。
pub struct NewInput<'a> {
    pub message_id: &'a str,
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub sender_open_id: &'a str,
    pub payload: &'a str,
    pub received_at_ms: i64,
}

impl Store {
    pub async fn open(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join(DB_FILE);
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // 「先落库再 ACK」要求提交即落盘；消息量很小，每次提交 fsync 不构成负担
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("打开数据库 {} 失败", path.display()))?;
        sqlx::migrate!("src/store/migrations")
            .run(&pool)
            .await
            .context("数据库迁移失败")?;
        Ok(Self { pool })
    }

    /// 落库一条输入。返回 `false` 表示同一条消息之前已经落过（重复推送）。
    pub async fn insert_input(&self, input: &NewInput<'_>) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "INSERT INTO turn_inputs
                 (message_id, chat_id, chat_type, sender_open_id, payload, received_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (message_id) DO NOTHING",
        )
        .bind(input.message_id)
        .bind(input.chat_id)
        .bind(input.chat_type)
        .bind(input.sender_open_id)
        .bind(input.payload)
        .bind(input.received_at_ms)
        .execute(&self.pool)
        .await
        .context("写入 turn_inputs 失败")?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn mark_done(&self, message_id: &str, handled_at_ms: i64) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE turn_inputs SET status = 'done', handled_at_ms = ?
             WHERE message_id = ? AND status = 'pending'",
        )
        .bind(handled_at_ms)
        .bind(message_id)
        .execute(&self.pool)
        .await
        .context("更新 turn_inputs 失败")?;
        Ok(())
    }

    /// 在线备份：`VACUUM INTO` 出一份完整、一致的库。目标文件必须不存在。
    pub async fn backup_into(&self, target: &Path) -> anyhow::Result<()> {
        let target = target
            .to_str()
            .with_context(|| format!("备份路径不是 UTF-8：{}", target.display()))?;
        sqlx::query("VACUUM INTO ?")
            .bind(target)
            .execute(&self.pool)
            .await
            .with_context(|| format!("备份到 {target} 失败"))?;
        Ok(())
    }

    /// 删过数据之后整理数据库文件。被删的内容还留在空闲页和 WAL 里，VACUUM 重写整个
    /// 文件、再把 WAL 截断，才算真的没了。库只有几 MB，一天一次的开销可以忽略。
    pub async fn compact(&self) -> anyhow::Result<()> {
        sqlx::query("VACUUM")
            .execute(&self.pool)
            .await
            .context("整理数据库失败")?;
        sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await
            .context("截断 WAL 失败")?;
        Ok(())
    }

    /// 一条输入的原始事件 JSON。
    pub async fn input_payload(&self, message_id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT payload FROM turn_inputs WHERE message_id = ?")
                .bind(message_id)
                .fetch_optional(&self.pool)
                .await
                .context("查询 turn_inputs 失败")?;
        Ok(row.map(|(payload,)| payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(message_id: &str, at: i64) -> NewInput<'_> {
        NewInput {
            message_id,
            chat_id: "oc_1",
            chat_type: "group",
            sender_open_id: "ou_1",
            payload: "{}",
            received_at_ms: at,
        }
    }

    #[tokio::test]
    async fn a_redelivered_message_is_recognised_as_a_duplicate() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        assert!(store.insert_input(&input("om_1", 1)).await.expect("首次"));
        assert!(!store.insert_input(&input("om_1", 2)).await.expect("重复"));
        assert_eq!(
            store.input_payload("om_1").await.expect("查询").as_deref(),
            Some("{}")
        );
        assert_eq!(store.input_payload("om_x").await.expect("查询"), None);
    }

    #[tokio::test]
    async fn pending_inputs_survive_a_reopen_until_marked_done() {
        let dir = tempfile::tempdir().expect("临时目录");
        {
            let store = Store::open(dir.path()).await.expect("打开");
            store.insert_input(&input("om_2", 20)).await.expect("写入");
            store.insert_input(&input("om_1", 10)).await.expect("写入");
            store.insert_input(&input("om_3", 30)).await.expect("写入");
            store.mark_done("om_3", 31).await.expect("完成");
        }
        let store = Store::open(dir.path()).await.expect("重新打开");
        assert_eq!(
            store.unassigned_inputs().await.expect("查询"),
            vec!["om_1".to_owned(), "om_2".to_owned()]
        );
    }
}
