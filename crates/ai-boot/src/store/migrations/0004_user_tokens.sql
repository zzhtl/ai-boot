-- 以用户身份读云文档的 token。refresh_token 一次性、每用一次就轮换，刷新后必须
-- 立即落库；整个库在 0700 的数据目录里。
CREATE TABLE user_tokens (
    open_id            TEXT    NOT NULL PRIMARY KEY,
    access_token       TEXT    NOT NULL,
    access_expires_at  INTEGER NOT NULL,
    -- 没有 offline_access 时为空，access_token 过期后只能重新授权
    refresh_token      TEXT,
    refresh_expires_at INTEGER,
    scopes             TEXT    NOT NULL,
    -- 刷新被拒（吊销、过期、已用过）：需要重新授权
    needs_reauth       INTEGER NOT NULL DEFAULT 0 CHECK (needs_reauth IN (0, 1)),
    -- 上次换到新 token 的时间：refresh_token 约 7 天过期，隔几天主动续一次
    refreshed_at_ms    INTEGER NOT NULL
) STRICT;

-- 进行中的授权：state 一次性、10 分钟过期；code_verifier 是 PKCE 原文
CREATE TABLE oauth_states (
    state         TEXT    NOT NULL PRIMARY KEY,
    open_id       TEXT    NOT NULL,
    code_verifier TEXT    NOT NULL,
    expires_at_ms INTEGER NOT NULL
) STRICT;
