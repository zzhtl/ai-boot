-- 持久化收件箱：机器人要处理的消息先落库再 ACK，服务重启后从这里恢复。
-- message_id 是飞书的幂等键（同一条消息可能被重复推送），主键冲突即重复投递。
CREATE TABLE turn_inputs (
    message_id     TEXT    NOT NULL PRIMARY KEY,
    chat_id        TEXT    NOT NULL,
    -- 飞书的取值原样存，不加 CHECK：出现新取值时插入失败会导致无限重推
    chat_type      TEXT    NOT NULL,
    sender_open_id TEXT    NOT NULL,
    -- 原始事件 JSON。只有白名单用户发给机器人的消息才会落到这里
    payload        TEXT    NOT NULL,
    status         TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'done')),
    received_at_ms INTEGER NOT NULL,
    handled_at_ms  INTEGER
) STRICT;

-- 启动恢复只扫未处理的那一小部分
CREATE INDEX turn_inputs_pending ON turn_inputs (received_at_ms) WHERE status = 'pending';
