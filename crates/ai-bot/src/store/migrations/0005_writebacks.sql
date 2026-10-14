-- 轮次的类型：普通提问，或者点「已解决」后生成闭环方案的那一轮
ALTER TABLE turns ADD COLUMN kind TEXT NOT NULL DEFAULT 'ask' CHECK (kind IN ('ask', 'resolve'));
-- 闭环方案可以写回的目标（JSON）：生成时由 ai-boot 查好，卡片按它渲染
ALTER TABLE turns ADD COLUMN writeback_targets TEXT;

-- 写回：闭环方案写进 Jira 评论或发布成 Confluence 页面。同一轮对同一个目标只有
-- 一行，重复点击、重试都落在这一行上，靠它和目标里的幂等标记保证只写一次。
CREATE TABLE writebacks (
    -- UUID v7；它的前 12 位是写进 Jira 评论、Confluence 标题的幂等标记
    id            TEXT    NOT NULL PRIMARY KEY,
    turn_id       TEXT    NOT NULL REFERENCES turns (id),
    target        TEXT    NOT NULL CHECK (target IN ('jira', 'confluence')),
    -- Jira 单号，或 Confluence 的「空间:父页面」
    target_ref    TEXT    NOT NULL,
    -- 写入内容的摘要，卡片上的按钮带着它，内容变了就拒绝执行
    content_hash  TEXT    NOT NULL,
    -- unknown：调用超时或进程中断，不知道写没写成；再点一次会先查重
    status        TEXT    NOT NULL CHECK (status IN ('running', 'done', 'failed', 'unknown')),
    result_url    TEXT,
    error         TEXT,
    requested_by  TEXT    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (turn_id, target, target_ref)
) STRICT;
