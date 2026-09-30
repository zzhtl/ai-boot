-- 会话：一个话题（私聊里是一串引用追问）对应一个会话，会话绑定一个可续接的 Agent 会话。
CREATE TABLE conversations (
    -- UUID v7，同时是会话工作目录的名字
    id                TEXT    NOT NULL PRIMARY KEY,
    chat_id           TEXT    NOT NULL,
    chat_type         TEXT    NOT NULL,
    -- new_thread：群里顶层消息 @ 机器人，机器人开的新话题，话题里的消息不用 @ 也算追问；
    -- existing_thread：在别人的话题里 @ 机器人，只有 @ 或引用机器人卡片才算追问
    origin            TEXT    NOT NULL CHECK (origin IN ('new_thread', 'existing_thread', 'p2p')),
    -- 机器人开话题时，要等回复成功才知道话题 ID
    thread_id         TEXT,
    -- 拿到 thread_id 之前，靠话题根消息认出追问
    root_message_id   TEXT    NOT NULL,
    owner_open_id     TEXT    NOT NULL,
    backend           TEXT    NOT NULL,
    agent_session_id  TEXT,
    -- 后端报的是会话累计用量，每轮用量按差值算
    session_tokens    INTEGER NOT NULL DEFAULT 0 CHECK (session_tokens >= 0),
    -- 已经交给 Agent 的最新一条话题消息的时间，追问只取这之后的
    history_cursor_ms INTEGER,
    status            TEXT    NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'resolved', 'closed')),
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL
) STRICT;

-- 同一个话题同时只有一个进行中的会话；同时是按话题、按根消息找会话的索引
CREATE UNIQUE INDEX conversations_active_thread ON conversations (thread_id)
    WHERE status = 'active' AND thread_id IS NOT NULL;
CREATE UNIQUE INDEX conversations_active_root ON conversations (root_message_id)
    WHERE status = 'active';

-- 轮次：会话里的一问一答，对应一张卡片。重试复用同一行（attempts 递增）。
CREATE TABLE turns (
    -- UUID v7，写进卡片按钮的回传值
    id              TEXT    NOT NULL PRIMARY KEY,
    conversation_id TEXT    NOT NULL REFERENCES conversations (id),
    seq             INTEGER NOT NULL CHECK (seq >= 1),
    status          TEXT    NOT NULL DEFAULT 'queued'
                    CHECK (status IN ('queued', 'running', 'succeeded', 'failed',
                                      'interrupted', 'timeout', 'budget_exceeded')),
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    card_message_id TEXT,
    -- 本轮提问拍平后的文字，续接失败时拿来给新会话补前情
    question        TEXT,
    model           TEXT,
    answer_json     TEXT,
    -- 失败类别，取值跟着后端走，不加 CHECK
    error_kind      TEXT,
    error           TEXT,
    tokens          INTEGER NOT NULL DEFAULT 0 CHECK (tokens >= 0),
    tool_calls      INTEGER NOT NULL DEFAULT 0 CHECK (tool_calls >= 0),
    duration_ms     INTEGER,
    created_at_ms   INTEGER NOT NULL,
    started_at_ms   INTEGER,
    finished_at_ms  INTEGER,
    UNIQUE (conversation_id, seq)
) STRICT;

-- 卡片回调、引用机器人卡片的追问都按卡片消息找轮次
CREATE UNIQUE INDEX turns_card ON turns (card_message_id) WHERE card_message_id IS NOT NULL;
-- 重启恢复只扫没结束的
CREATE INDEX turns_open ON turns (status) WHERE status IN ('queued', 'running');

-- 收件箱里的消息归到哪个会话、哪一轮。都为空表示还没分派。
ALTER TABLE turn_inputs ADD COLUMN conversation_id TEXT REFERENCES conversations (id);
ALTER TABLE turn_inputs ADD COLUMN turn_id TEXT REFERENCES turns (id);
CREATE INDEX turn_inputs_turn ON turn_inputs (turn_id) WHERE turn_id IS NOT NULL;
