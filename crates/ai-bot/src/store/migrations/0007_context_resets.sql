-- 「清空上下文」：清空时删掉这个聊天的全部会话数据，再记下清空的时刻。之后取群聊记录、
-- 话题记录只取这个时刻之后的消息，清空之前的聊天不会再进上下文。每个聊天只留最近一次；
-- 只有聊天 ID 和时间，不含聊天内容。只加表，老版本程序不受影响；退回老版本前要先删掉
-- _sqlx_migrations 里 version=7 那一行。
CREATE TABLE context_resets (
    chat_id     TEXT    NOT NULL PRIMARY KEY,
    -- 清空那条消息在飞书上的创建时间（毫秒）
    reset_at_ms INTEGER NOT NULL CHECK (reset_at_ms >= 0)
) STRICT;
