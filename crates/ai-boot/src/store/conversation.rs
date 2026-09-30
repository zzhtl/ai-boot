//! 会话与轮次。
//!
//! 同一会话的写入都来自它的 actor（串行），跨会话共享的只有收件箱；多条语句的
//! 写入一律用 `BEGIN IMMEDIATE` 开事务，免得读事务升级成写事务时直接撞上 busy。

use std::collections::HashSet;

use anyhow::Context as _;

use super::Store;

const BEGIN_WRITE: &str = "BEGIN IMMEDIATE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum Origin {
    /// 群里的顶层消息 @ 机器人，机器人开了新话题。话题里的消息不用 @ 也算追问。
    NewThread,
    /// 在别人开的话题里 @ 机器人。只有 @ 或引用机器人的卡片才算追问。
    ExistingThread,
    P2p,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum TurnStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Timeout,
    BudgetExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(rename_all = "snake_case")]
pub enum TurnKind {
    /// 普通的一问一答。
    Ask,
    /// 点「已解决」后生成闭环方案。
    Resolve,
}

impl TurnStatus {
    /// 结束了但没拿到答案，可以重试。
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Interrupted | Self::Timeout | Self::BudgetExceeded
        )
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Conversation {
    pub id: String,
    pub origin: Origin,
    pub thread_id: Option<String>,
    pub backend: String,
    pub agent_session_id: Option<String>,
    pub session_tokens: i64,
    pub history_cursor_ms: Option<i64>,
    /// 当前 Agent 会话最后一轮结束时带着的上下文（token），0 表示还不知道。
    pub context_tokens: i64,
}

macro_rules! conversation_columns {
    ($prefix:literal) => {
        concat!(
            $prefix,
            "id, ",
            $prefix,
            "origin, ",
            $prefix,
            "thread_id, ",
            $prefix,
            "backend, ",
            $prefix,
            "agent_session_id, ",
            $prefix,
            "session_tokens, ",
            $prefix,
            "history_cursor_ms, ",
            $prefix,
            "context_tokens"
        )
    };
}

pub struct NewConversation<'a> {
    pub id: &'a str,
    pub chat_id: &'a str,
    pub chat_type: &'a str,
    pub origin: Origin,
    pub thread_id: Option<&'a str>,
    pub root_message_id: &'a str,
    pub owner_open_id: &'a str,
    pub backend: &'a str,
    pub now_ms: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Turn {
    pub id: String,
    pub conversation_id: String,
    pub seq: i64,
    pub kind: TurnKind,
    pub status: TurnStatus,
    pub card_message_id: Option<String>,
}

/// 重画闭环卡片要用的一轮的全部结果。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TurnRecord {
    pub id: String,
    pub seq: i64,
    pub kind: TurnKind,
    pub status: TurnStatus,
    pub card_message_id: Option<String>,
    pub answer_json: Option<String>,
    pub writeback_targets: Option<String>,
    pub duration_ms: Option<i64>,
}

/// 收件箱里的一条消息，以及它被分到的会话。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Input {
    pub payload: String,
    pub conversation_id: Option<String>,
}

/// 之前某一轮的问答，续接失败时给新会话补前情。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PriorTurn {
    pub seq: i64,
    pub question: Option<String>,
    pub answer_json: Option<String>,
}

/// 一轮结束时要落库的全部内容。
pub struct FinishedTurn<'a> {
    pub turn_id: &'a str,
    pub conversation_id: &'a str,
    pub status: TurnStatus,
    pub model: &'a str,
    pub answer_json: Option<&'a str>,
    pub error_kind: Option<&'a str>,
    pub error: Option<&'a str>,
    pub tokens: i64,
    pub tool_calls: i64,
    pub duration_ms: i64,
    /// 本轮确认可续接的会话 ID（后端真正起了会话才有）。
    pub session_id: Option<&'a str>,
    /// 会话累计用量；后端没报时为 `None`，保留原值。
    pub session_tokens: Option<i64>,
    pub history_cursor_ms: Option<i64>,
    /// 本轮结束时带着的上下文；后端没报时为 `None`，保留原值。
    pub context_tokens: Option<i64>,
    pub now_ms: i64,
}

/// 重启时被中断的一轮。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InterruptedTurn {
    pub id: String,
    pub card_message_id: Option<String>,
}

impl Store {
    /// 按话题或话题根消息找进行中的会话。
    pub async fn find_conversation(
        &self,
        thread_id: Option<&str>,
        root_message_id: Option<&str>,
    ) -> anyhow::Result<Option<Conversation>> {
        if thread_id.is_none() && root_message_id.is_none() {
            return Ok(None);
        }
        sqlx::query_as(concat!(
            "SELECT ",
            conversation_columns!(""),
            " FROM conversations
             WHERE status = 'active' AND (thread_id = ? OR root_message_id = ?)
             ORDER BY created_at_ms DESC
             LIMIT 1"
        ))
        .bind(thread_id)
        .bind(root_message_id)
        .fetch_optional(&self.pool)
        .await
        .context("查询会话失败")
    }

    /// 群里进行中的会话。一个群一个会话：同一个群的提问续接同一个 Agent 会话，
    /// 聊天记录和附件只发新增的部分。
    pub async fn chat_conversation(&self, chat_id: &str) -> anyhow::Result<Option<Conversation>> {
        sqlx::query_as(concat!(
            "SELECT ",
            conversation_columns!(""),
            " FROM conversations
             WHERE chat_id = ? AND origin != 'p2p' AND status = 'active'
             ORDER BY created_at_ms DESC
             LIMIT 1"
        ))
        .bind(chat_id)
        .fetch_optional(&self.pool)
        .await
        .context("按群查询会话失败")
    }

    /// 某张机器人卡片属于哪一轮。
    pub async fn turn_by_card(&self, card_message_id: &str) -> anyhow::Result<Option<Turn>> {
        sqlx::query_as(
            "SELECT id, conversation_id, seq, kind, status, card_message_id
             FROM turns WHERE card_message_id = ?",
        )
        .bind(card_message_id)
        .fetch_optional(&self.pool)
        .await
        .context("按卡片查询轮次失败")
    }

    /// 某张卡片上的答案（成功的提问轮）。在卡片上补充、纠正后结论变了，要把它标成已更正。
    pub async fn answer_by_card(
        &self,
        card_message_id: &str,
    ) -> anyhow::Result<Option<TurnRecord>> {
        sqlx::query_as(
            "SELECT id, seq, kind, status, card_message_id, answer_json, writeback_targets,
                    duration_ms
             FROM turns
             WHERE card_message_id = ? AND kind = 'ask' AND status = 'succeeded'
               AND answer_json IS NOT NULL",
        )
        .bind(card_message_id)
        .fetch_optional(&self.pool)
        .await
        .context("按卡片查询答案失败")
    }

    /// 某张机器人卡片所在的进行中会话（引用卡片追问时用）。
    pub async fn conversation_by_card(
        &self,
        card_message_id: &str,
    ) -> anyhow::Result<Option<Conversation>> {
        sqlx::query_as(concat!(
            "SELECT ",
            conversation_columns!("c."),
            " FROM turns t JOIN conversations c ON c.id = t.conversation_id
             WHERE t.card_message_id = ? AND c.status = 'active'"
        ))
        .bind(card_message_id)
        .fetch_optional(&self.pool)
        .await
        .context("按卡片查询会话失败")
    }

    pub async fn conversation(&self, id: &str) -> anyhow::Result<Option<Conversation>> {
        sqlx::query_as(concat!(
            "SELECT ",
            conversation_columns!(""),
            " FROM conversations WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("查询会话失败")
    }

    pub async fn create_conversation(&self, new: &NewConversation<'_>) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO conversations
                 (id, chat_id, chat_type, origin, thread_id, root_message_id, owner_open_id,
                  backend, created_at_ms, updated_at_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new.id)
        .bind(new.chat_id)
        .bind(new.chat_type)
        .bind(new.origin)
        .bind(new.thread_id)
        .bind(new.root_message_id)
        .bind(new.owner_open_id)
        .bind(new.backend)
        .bind(new.now_ms)
        .bind(new.now_ms)
        .execute(&self.pool)
        .await
        .context("创建会话失败")?;
        Ok(())
    }

    /// 机器人开的话题，回复成功后补上话题 ID。已经有了就不动。
    pub async fn set_conversation_thread(
        &self,
        conversation_id: &str,
        thread_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE conversations SET thread_id = ?
             WHERE id = ? AND thread_id IS NULL",
        )
        .bind(thread_id)
        .bind(conversation_id)
        .execute(&self.pool)
        .await
        .context("记录话题 ID 失败")?;
        Ok(())
    }

    pub async fn input(&self, message_id: &str) -> anyhow::Result<Option<Input>> {
        sqlx::query_as("SELECT payload, conversation_id FROM turn_inputs WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await
            .context("查询 turn_inputs 失败")
    }

    pub async fn assign_conversation(
        &self,
        message_id: &str,
        conversation_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE turn_inputs SET conversation_id = ? WHERE message_id = ?")
            .bind(conversation_id)
            .bind(message_id)
            .execute(&self.pool)
            .await
            .context("分派消息失败")?;
        Ok(())
    }

    /// 新开一轮并把第一条消息归进去，返回轮次序号。
    pub async fn create_turn(
        &self,
        conversation_id: &str,
        turn_id: &str,
        first_message_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<i64> {
        let mut tx = self
            .pool
            .begin_with(BEGIN_WRITE)
            .await
            .context("开启事务失败")?;
        let (seq,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_one(&mut *tx)
                .await
                .context("取轮次序号失败")?;
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, seq, created_at_ms) VALUES (?, ?, ?, ?)",
        )
        .bind(turn_id)
        .bind(conversation_id)
        .bind(seq)
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .context("创建轮次失败")?;
        sqlx::query("UPDATE turn_inputs SET turn_id = ?, conversation_id = ? WHERE message_id = ?")
            .bind(turn_id)
            .bind(conversation_id)
            .bind(first_message_id)
            .execute(&mut *tx)
            .await
            .context("分派消息失败")?;
        tx.commit().await.context("提交事务失败")?;
        Ok(seq)
    }

    /// 连发的后续消息并进还没开跑的一轮。
    pub async fn join_turn(&self, turn_id: &str, message_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE turn_inputs SET turn_id = ? WHERE message_id = ?")
            .bind(turn_id)
            .bind(message_id)
            .execute(&self.pool)
            .await
            .context("分派消息失败")?;
        Ok(())
    }

    /// 一轮包含的消息，按接收顺序。
    pub async fn turn_messages(&self, turn_id: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT message_id FROM turn_inputs
             WHERE turn_id = ?
             ORDER BY received_at_ms, message_id",
        )
        .bind(turn_id)
        .fetch_all(&self.pool)
        .await
        .context("查询轮次消息失败")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn turn(&self, turn_id: &str) -> anyhow::Result<Option<Turn>> {
        sqlx::query_as(
            "SELECT id, conversation_id, seq, kind, status, card_message_id
             FROM turns WHERE id = ?",
        )
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await
        .context("查询轮次失败")
    }

    pub async fn turn_record(&self, turn_id: &str) -> anyhow::Result<Option<TurnRecord>> {
        sqlx::query_as(
            "SELECT id, seq, kind, status, card_message_id, answer_json, writeback_targets,
                    duration_ms
             FROM turns WHERE id = ?",
        )
        .bind(turn_id)
        .fetch_optional(&self.pool)
        .await
        .context("查询轮次失败")
    }

    /// 某一轮之前、这个会话里最近一张给出了答案的卡片。结论被后面的轮次更正时，
    /// 要回头把它标成「已更正」。
    pub async fn previous_answer(
        &self,
        conversation_id: &str,
        before_seq: i64,
    ) -> anyhow::Result<Option<TurnRecord>> {
        sqlx::query_as(
            "SELECT id, seq, kind, status, card_message_id, answer_json, writeback_targets,
                    duration_ms
             FROM turns
             WHERE conversation_id = ? AND seq < ? AND kind = 'ask' AND status = 'succeeded'
               AND answer_json IS NOT NULL AND card_message_id IS NOT NULL
             ORDER BY seq DESC
             LIMIT 1",
        )
        .bind(conversation_id)
        .bind(before_seq)
        .fetch_optional(&self.pool)
        .await
        .context("查询上一张答案卡失败")
    }

    /// 新开「生成闭环方案」的一轮：没有用户消息，接在最新一轮后面。
    pub async fn create_resolve_turn(
        &self,
        conversation_id: &str,
        turn_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<i64> {
        let (seq,): (i64,) = sqlx::query_as(
            "INSERT INTO turns (id, conversation_id, seq, kind, created_at_ms)
             SELECT ?, ?, COALESCE(MAX(seq), 0) + 1, 'resolve', ? FROM turns WHERE conversation_id = ?
             RETURNING seq",
        )
        .bind(turn_id)
        .bind(conversation_id)
        .bind(now_ms)
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await
        .context("创建闭环轮次失败")?;
        Ok(seq)
    }

    pub async fn set_writeback_targets(&self, turn_id: &str, targets: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE turns SET writeback_targets = ? WHERE id = ?")
            .bind(targets)
            .bind(turn_id)
            .execute(&self.pool)
            .await
            .context("记录写回目标失败")?;
        Ok(())
    }

    /// 会话里最新一轮的序号，还没有轮次时为 0。
    pub async fn latest_seq(&self, conversation_id: &str) -> anyhow::Result<i64> {
        let (seq,): (i64,) =
            sqlx::query_as("SELECT COALESCE(MAX(seq), 0) FROM turns WHERE conversation_id = ?")
                .bind(conversation_id)
                .fetch_one(&self.pool)
                .await
                .context("查询轮次失败")?;
        Ok(seq)
    }

    pub async fn set_turn_card(&self, turn_id: &str, card_message_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE turns SET card_message_id = ? WHERE id = ?")
            .bind(card_message_id)
            .bind(turn_id)
            .execute(&self.pool)
            .await
            .context("记录卡片失败")?;
        Ok(())
    }

    pub async fn start_turn(
        &self,
        turn_id: &str,
        question: &str,
        manifest: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE turns
             SET status = 'running', attempts = attempts + 1, question = ?,
                 context_manifest = ?, started_at_ms = ?
             WHERE id = ?",
        )
        .bind(question)
        .bind(manifest)
        .bind(now_ms)
        .bind(turn_id)
        .execute(&self.pool)
        .await
        .context("更新轮次状态失败")?;
        Ok(())
    }

    /// 把没拿到答案的一轮放回队列重跑。返回 `false` 表示状态不对（比如已在重跑）。
    pub async fn requeue_turn(&self, turn_id: &str) -> anyhow::Result<bool> {
        let done = sqlx::query(
            "UPDATE turns SET status = 'queued', error_kind = NULL, error = NULL
             WHERE id = ? AND status IN ('failed', 'interrupted', 'timeout', 'budget_exceeded')",
        )
        .bind(turn_id)
        .execute(&self.pool)
        .await
        .context("更新轮次状态失败")?;
        Ok(done.rows_affected() == 1)
    }

    /// 一轮结束：记结果、收件箱里的消息标记完成、更新会话的续接信息。
    pub async fn finish_turn(&self, turn: &FinishedTurn<'_>) -> anyhow::Result<()> {
        let mut tx = self
            .pool
            .begin_with(BEGIN_WRITE)
            .await
            .context("开启事务失败")?;
        sqlx::query(
            "UPDATE turns
             SET status = ?, model = ?, answer_json = ?, error_kind = ?, error = ?,
                 tokens = tokens + ?, tool_calls = tool_calls + ?, duration_ms = ?,
                 finished_at_ms = ?
             WHERE id = ?",
        )
        .bind(turn.status)
        .bind(turn.model)
        .bind(turn.answer_json)
        .bind(turn.error_kind)
        .bind(turn.error)
        .bind(turn.tokens)
        .bind(turn.tool_calls)
        .bind(turn.duration_ms)
        .bind(turn.now_ms)
        .bind(turn.turn_id)
        .execute(&mut *tx)
        .await
        .context("更新轮次失败")?;
        sqlx::query(
            "UPDATE turn_inputs SET status = 'done', handled_at_ms = ?
             WHERE turn_id = ? AND status = 'pending'",
        )
        .bind(turn.now_ms)
        .bind(turn.turn_id)
        .execute(&mut *tx)
        .await
        .context("更新收件箱失败")?;
        sqlx::query(
            "UPDATE conversations
             SET agent_session_id = COALESCE(?, agent_session_id),
                 session_tokens = COALESCE(?, session_tokens),
                 history_cursor_ms = COALESCE(?, history_cursor_ms),
                 context_tokens = COALESCE(?, context_tokens),
                 updated_at_ms = ?
             WHERE id = ?",
        )
        .bind(turn.session_id)
        .bind(turn.session_tokens)
        .bind(turn.history_cursor_ms)
        .bind(turn.context_tokens)
        .bind(turn.now_ms)
        .bind(turn.conversation_id)
        .execute(&mut *tx)
        .await
        .context("更新会话失败")?;
        tx.commit().await.context("提交事务失败")?;
        Ok(())
    }

    /// 某一轮之前成功的各轮，按顺序。
    pub async fn prior_turns(
        &self,
        conversation_id: &str,
        before_seq: i64,
    ) -> anyhow::Result<Vec<PriorTurn>> {
        sqlx::query_as(
            "SELECT seq, question, answer_json FROM turns
             WHERE conversation_id = ? AND seq < ? AND status = 'succeeded'
             ORDER BY seq",
        )
        .bind(conversation_id)
        .bind(before_seq)
        .fetch_all(&self.pool)
        .await
        .context("查询历史轮次失败")
    }

    /// 启动时调用：上次退出时没结束的轮次一律记为中断，它们的消息标记完成
    /// （要重跑由用户点重试）。
    pub async fn interrupt_open_turns(&self, now_ms: i64) -> anyhow::Result<Vec<InterruptedTurn>> {
        let mut tx = self
            .pool
            .begin_with(BEGIN_WRITE)
            .await
            .context("开启事务失败")?;
        let turns: Vec<InterruptedTurn> = sqlx::query_as(
            "UPDATE turns
             SET status = 'interrupted', error_kind = 'restart', error = '服务重启，本轮没有完成',
                 finished_at_ms = ?
             WHERE status IN ('queued', 'running')
             RETURNING id, card_message_id",
        )
        .bind(now_ms)
        .fetch_all(&mut *tx)
        .await
        .context("中断未完成的轮次失败")?;
        // 此时所有轮次都已结束，挂在轮次上还没完成的消息就是被中断的这些
        sqlx::query(
            "UPDATE turn_inputs SET status = 'done', handled_at_ms = ?
             WHERE status = 'pending' AND turn_id IS NOT NULL",
        )
        .bind(now_ms)
        .execute(&mut *tx)
        .await
        .context("更新收件箱失败")?;
        tx.commit().await.context("提交事务失败")?;
        Ok(turns)
    }

    /// 创建超过保留期的会话。有轮次在跑、或刚收到消息（还没来得及建轮次）的先不动，
    /// 下一次维护再删。
    pub async fn expired_conversations(
        &self,
        created_before: i64,
        quiet_since: i64,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT c.id FROM conversations c
             WHERE c.created_at_ms < ?
               AND NOT EXISTS (SELECT 1 FROM turn_inputs i
                               WHERE i.conversation_id = c.id AND i.received_at_ms >= ?)
               AND NOT EXISTS (SELECT 1 FROM turns t
                               WHERE t.conversation_id = c.id AND t.status IN ('queued', 'running'))",
        )
        .bind(created_before)
        .bind(quiet_since)
        .fetch_all(&self.pool)
        .await
        .context("查询过期会话失败")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 库里现有的全部会话。清扫残留目录时，它们的目录不算残留。
    pub async fn conversation_ids(&self) -> anyhow::Result<HashSet<String>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM conversations")
            .fetch_all(&self.pool)
            .await
            .context("查询会话失败")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// 删掉一个会话在库里的全部数据：写回记录、原始消息、轮次（提问、答案、读取清单）
    /// 和会话本身。之后引用它的卡片追问会开新会话。
    pub async fn purge_conversation(&self, id: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await.context("开启事务失败")?;
        for statement in [
            "DELETE FROM writebacks
             WHERE turn_id IN (SELECT id FROM turns WHERE conversation_id = ?)",
            "DELETE FROM turn_inputs
             WHERE conversation_id = ?1
                OR turn_id IN (SELECT id FROM turns WHERE conversation_id = ?1)",
            "DELETE FROM turns WHERE conversation_id = ?",
            "DELETE FROM conversations WHERE id = ?",
        ] {
            sqlx::query(statement)
                .bind(id)
                .execute(&mut *tx)
                .await
                .context("删除过期会话失败")?;
        }
        tx.commit().await.context("删除过期会话失败")?;
        Ok(())
    }

    /// 不属于任何会话的旧数据：没分到会话的原始消息（白名单外的私聊、解析不了的）、
    /// 过期的授权请求、太久没续期（早已失效）的用户授权。返回删掉的行数。
    pub async fn purge_orphans(&self, cutoff: i64, now_ms: i64) -> anyhow::Result<u64> {
        let mut deleted = 0;
        for (statement, value) in [
            (
                "DELETE FROM turn_inputs WHERE conversation_id IS NULL AND received_at_ms < ?",
                cutoff,
            ),
            ("DELETE FROM oauth_states WHERE expires_at_ms < ?", now_ms),
            ("DELETE FROM user_tokens WHERE refreshed_at_ms < ?", cutoff),
        ] {
            deleted += sqlx::query(statement)
                .bind(value)
                .execute(&self.pool)
                .await
                .context("清理旧数据失败")?
                .rows_affected();
        }
        Ok(deleted)
    }

    /// 还没分到轮次的消息，按接收时间排序。
    pub async fn unassigned_inputs(&self) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT message_id FROM turn_inputs
             WHERE status = 'pending' AND turn_id IS NULL
             ORDER BY received_at_ms",
        )
        .fetch_all(&self.pool)
        .await
        .context("查询待处理输入失败")?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }
}

#[cfg(test)]
impl Store {
    /// 全部轮次，按创建顺序。
    pub async fn all_turns(&self) -> Vec<Turn> {
        sqlx::query_as(
            "SELECT id, conversation_id, seq, kind, status, card_message_id
             FROM turns ORDER BY created_at_ms, seq",
        )
        .fetch_all(&self.pool)
        .await
        .expect("查询轮次")
    }

    /// 一轮记下的用量：（token 数，工具调用次数）。
    pub async fn turn_usage(&self, turn_id: &str) -> (i64, i64) {
        sqlx::query_as("SELECT tokens, tool_calls FROM turns WHERE id = ?")
            .bind(turn_id)
            .fetch_one(&self.pool)
            .await
            .expect("查询用量")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewInput;

    async fn store() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).await.expect("打开");
        (store, dir)
    }

    async fn input(store: &Store, message_id: &str, at: i64) {
        store
            .insert_input(&NewInput {
                message_id,
                chat_id: "oc_1",
                chat_type: "group",
                sender_open_id: "ou_1",
                payload: "{}",
                received_at_ms: at,
            })
            .await
            .expect("写入");
    }

    async fn conversation(store: &Store, id: &str, root: &str, thread: Option<&str>) {
        store
            .create_conversation(&NewConversation {
                id,
                chat_id: "oc_1",
                chat_type: "group",
                origin: Origin::NewThread,
                thread_id: thread,
                root_message_id: root,
                owner_open_id: "ou_1",
                backend: "claude",
                now_ms: 1,
            })
            .await
            .expect("创建会话");
    }

    fn finished<'a>(turn_id: &'a str, status: TurnStatus) -> FinishedTurn<'a> {
        FinishedTurn {
            turn_id,
            conversation_id: "c1",
            status,
            model: "opus",
            answer_json: None,
            error_kind: None,
            error: None,
            tokens: 100,
            tool_calls: 2,
            duration_ms: 10,
            session_id: Some("s-1"),
            session_tokens: Some(100),
            history_cursor_ms: Some(50),
            context_tokens: Some(9_000),
            now_ms: 99,
        }
    }

    #[tokio::test]
    async fn a_conversation_is_found_by_root_before_its_thread_is_known() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_root", None).await;
        let found = store
            .find_conversation(Some("omt_new"), Some("om_root"))
            .await
            .expect("查询")
            .expect("按根消息找到");
        assert_eq!(found.id, "c1");
        assert_eq!(found.origin, Origin::NewThread);

        store
            .set_conversation_thread("c1", "omt_1")
            .await
            .expect("记录话题");
        store
            .set_conversation_thread("c1", "omt_other")
            .await
            .expect("已有话题时不覆盖");
        let found = store
            .find_conversation(Some("omt_1"), None)
            .await
            .expect("查询")
            .expect("按话题找到");
        assert_eq!(found.thread_id.as_deref(), Some("omt_1"));
        assert!(
            store
                .find_conversation(Some("omt_x"), Some("om_x"))
                .await
                .expect("查询")
                .is_none()
        );
        assert!(
            store
                .find_conversation(None, None)
                .await
                .expect("查询")
                .is_none()
        );
    }

    #[tokio::test]
    async fn one_thread_has_at_most_one_active_conversation() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_1", Some("omt_1")).await;
        let second = store
            .create_conversation(&NewConversation {
                id: "c2",
                chat_id: "oc_1",
                chat_type: "group",
                origin: Origin::ExistingThread,
                thread_id: Some("omt_1"),
                root_message_id: "om_2",
                owner_open_id: "ou_1",
                backend: "claude",
                now_ms: 2,
            })
            .await;
        assert!(
            second.is_err(),
            "同一话题的第二个进行中会话必须被数据库拒绝"
        );
    }

    #[tokio::test]
    async fn turns_are_numbered_per_conversation_and_collect_their_inputs() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_1", None).await;
        for (id, at) in [("om_1", 10), ("om_2", 11), ("om_3", 30)] {
            input(&store, id, at).await;
        }
        let seq = store
            .create_turn("c1", "t1", "om_1", 10)
            .await
            .expect("第一轮");
        assert_eq!(seq, 1);
        store.join_turn("t1", "om_2").await.expect("并入");
        assert_eq!(
            store.turn_messages("t1").await.expect("查询"),
            ["om_1", "om_2"]
        );
        assert_eq!(store.unassigned_inputs().await.expect("查询"), ["om_3"]);

        let seq = store
            .create_turn("c1", "t2", "om_3", 30)
            .await
            .expect("第二轮");
        assert_eq!(seq, 2);
        assert_eq!(store.latest_seq("c1").await.expect("查询"), 2);
        assert!(store.unassigned_inputs().await.expect("查询").is_empty());
    }

    #[tokio::test]
    async fn finishing_a_turn_records_the_session_and_closes_its_inputs() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_1", None).await;
        input(&store, "om_1", 10).await;
        store
            .create_turn("c1", "t1", "om_1", 10)
            .await
            .expect("建轮次");
        store
            .start_turn("t1", "为什么 500", None, 11)
            .await
            .expect("开跑");
        store.set_turn_card("t1", "om_card").await.expect("记卡片");
        store
            .finish_turn(&FinishedTurn {
                answer_json: Some("{}"),
                ..finished("t1", TurnStatus::Succeeded)
            })
            .await
            .expect("结束");

        let turn = store.turn("t1").await.expect("查询").expect("存在");
        assert_eq!(turn.status, TurnStatus::Succeeded);
        let conv = store.conversation("c1").await.expect("查询").expect("存在");
        assert_eq!(conv.agent_session_id.as_deref(), Some("s-1"));
        assert_eq!(conv.session_tokens, 100);
        assert_eq!(conv.history_cursor_ms, Some(50));
        assert_eq!(conv.context_tokens, 9_000);
        let by_card = store
            .conversation_by_card("om_card")
            .await
            .expect("查询")
            .expect("按卡片找到");
        assert_eq!(by_card.id, "c1");
        let prior = store.prior_turns("c1", 2).await.expect("查询");
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].question.as_deref(), Some("为什么 500"));
    }

    #[tokio::test]
    async fn only_turns_without_an_answer_can_be_requeued_and_only_once() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_1", None).await;
        input(&store, "om_1", 10).await;
        store
            .create_turn("c1", "t1", "om_1", 10)
            .await
            .expect("建轮次");
        assert!(!store.requeue_turn("t1").await.expect("排队中不能重试"));
        store
            .finish_turn(&finished("t1", TurnStatus::Interrupted))
            .await
            .expect("结束");
        assert!(store.requeue_turn("t1").await.expect("重试"));
        assert!(!store.requeue_turn("t1").await.expect("重复点击"));
        let turn = store.turn("t1").await.expect("查询").expect("存在");
        assert_eq!(turn.status, TurnStatus::Queued);
    }

    #[tokio::test]
    async fn a_restart_interrupts_open_turns_and_leaves_unassigned_inputs_for_redispatch() {
        let (store, _dir) = store().await;
        conversation(&store, "c1", "om_1", None).await;
        for (id, at) in [("om_1", 10), ("om_2", 20), ("om_3", 30)] {
            input(&store, id, at).await;
        }
        store
            .create_turn("c1", "t1", "om_1", 10)
            .await
            .expect("建轮次");
        store.start_turn("t1", "q", None, 11).await.expect("开跑");
        store.set_turn_card("t1", "om_card").await.expect("记卡片");
        store
            .create_turn("c1", "t2", "om_2", 20)
            .await
            .expect("排队的一轮");

        let interrupted = store.interrupt_open_turns(40).await.expect("恢复");
        let mut ids: Vec<(&str, Option<&str>)> = interrupted
            .iter()
            .map(|t| (t.id.as_str(), t.card_message_id.as_deref()))
            .collect();
        ids.sort();
        assert_eq!(ids, [("t1", Some("om_card")), ("t2", None)]);
        assert_eq!(
            store.unassigned_inputs().await.expect("查询"),
            ["om_3"],
            "挂在被中断轮次上的消息不再重派"
        );
        let turn = store.turn("t1").await.expect("查询").expect("存在");
        assert!(turn.status.is_retryable());
        assert!(
            store
                .interrupt_open_turns(50)
                .await
                .expect("再次恢复")
                .is_empty()
        );
    }
}
