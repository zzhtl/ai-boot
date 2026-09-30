-- 当前 Agent 会话最后一轮结束时带着的上下文（token）。每次请求都要带上整段会话，
-- 超过阈值时下一轮把前几轮的结论收拢成摘要、换新会话。0 表示还不知道（老数据，
-- 或者这个会话还没跑完一轮）。只加列、有默认值，SQLite 不重写表。
ALTER TABLE conversations ADD COLUMN context_tokens INTEGER NOT NULL DEFAULT 0 CHECK (context_tokens >= 0);
