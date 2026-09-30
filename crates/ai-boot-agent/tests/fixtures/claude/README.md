# Claude Code stream-json 样本

`claude -p --output-format stream-json --verbose` 的真实输出，防腐层（`src/claude/decode.rs`）的回归基线。
CLI 的事件形状**不是稳定契约**：升级 claude 后按下面的条件重录一份，跑 `cargo test -p ai-boot-agent --test decode_claude`，
就知道解码要不要跟着改。

录制条件：claude **2.1.283**，`--model haiku`，参数与机器人实际使用的一致：

```
--restricted --strict-mcp-config --disable-slash-commands --permission-mode manual --permission-prompts none
--mcp-config <qtmcp --toolsets gitlab> --settings <PermissionRequest + PreToolUse hook> --tools Read,Glob,Grep
--json-schema <schema>  # sigint 与 budget-exceeded 两份没带
```

prompt 走 stdin。hook 是一个探针脚本：只放行 `gitlab_project.search`，PermissionRequest 拒绝其他 MCP 调用，
PreToolUse 拒绝 `gitlab_project.get`。

| 文件 | 场景 | 看点 |
|---|---|---|
| `restricted-mcp-structured.jsonl` | 读工作目录内外的文件、调 MCP、结构化输出 | `--restricted` 拒绝工作目录外的读取；PermissionRequest 放行；`structured_output` |
| `hook-deny.jsonl` | 两条拒绝路径 | PermissionRequest 拒绝产生 `permission_denied`；PreToolUse 拒绝只体现在错误的工具结果里 |
| `hook-crash.jsonl` | hook 直接 exit 1 | 需要批准的调用被自动拒绝（no approval surface）——失败即拒绝 |
| `resume.jsonl` | `--resume` 续接 `restricted-mcp-structured` 的会话，换一份 schema | 会话 ID 不变、记得上一轮内容；成本与 token 为会话累计值 |
| `sigint.jsonl` | 长文本生成中途发 SIGINT | `error_during_execution` + `terminal_reason: aborted_streaming` |
| `budget-exceeded.jsonl` | `--max-budget-usd 0.001` | `error_max_budget_usd` / `budget_exhausted`；被切断的工具调用没有结果 |
| `partial-messages.jsonl` | 读工作目录里的文件后按 schema 回答（claude **2.1.285**，`--model haiku`，加 `--include-partial-messages`，不挂 MCP 和 hook） | `stream_event` 里 thinking / text / `StructuredOutput` 的 `content_block_start`；`system` 的 `status` 与 `thinking_tokens` 心跳 |
| `resume-missing.jsonl` | `--resume` 一个不存在的会话（只带 `-p --output-format stream-json --verbose`，空配置目录） | 没有 init 事件，直接以 `error_during_execution` 结束，`errors` 里是 `No conversation found with session ID` |

脱敏：录制目录替换为 `/work/probe`，内网项目名替换为 `group/project`。
