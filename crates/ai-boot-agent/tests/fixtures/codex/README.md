# Codex JSONL 样本

`success.jsonl` 录制于 Codex CLI 0.161.0（2026-10-08），模型为 `gpt-6.1-sol`、推理强度 high。
只要求模型输出 `{"reply":"ok"}`，使用 `exec --json --output-schema`，不调用工具。
样本不包含登录凭据或业务内容。升级 CLI 后重新核对事件结构，并运行解码测试和
`AI_BOOT_CODEX_PROFILE=ai-boot` 的 live_codex 测试；后者实际查询 qtmcp、验证续接和正式回复契约。
