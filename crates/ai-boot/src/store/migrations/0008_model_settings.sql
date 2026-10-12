-- 会话覆盖值为空时沿用服务配置；轮次记录本轮实际采用的强度。
ALTER TABLE conversations ADD COLUMN model_override TEXT;
ALTER TABLE conversations ADD COLUMN effort_override TEXT;
ALTER TABLE turns ADD COLUMN reasoning_effort TEXT;
