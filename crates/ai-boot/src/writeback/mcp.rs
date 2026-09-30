//! 最小的 MCP stdio 客户端：握手、调工具。写回只要这两件事。
//!
//! MCP 的 stdio 传输是一行一条 JSON-RPC 消息。server 可能夹带通知或反向请求，
//! 通知忽略，请求回「不支持」。每次调用有超时：写操作超时说明「不知道写没写成」，
//! 调用方要按结果不明处理，不能直接重试。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const PROTOCOL_VERSION: &str = "2025-06-18";
/// 传给 server 的环境变量：配置里写的之外，只有这几个。
const PASSTHROUGH: [&str; 3] = ["HOME", "PATH", "LANG"];

/// 一个 MCP server 的启动方式。
#[derive(Debug, Clone)]
pub struct Server {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug)]
pub enum CallError {
    /// 超时：写操作可能已经生效。
    Timeout,
    /// 工具报错或协议错误：没有生效。
    Failed(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("调用超时"),
            Self::Failed(message) => f.write_str(message),
        }
    }
}

pub struct Session {
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Session {
    pub async fn start(server: &Server, timeout: Duration) -> Result<Self, String> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .env_clear()
            .envs(
                PASSTHROUGH
                    .iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
            )
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|err| format!("启动 {} 失败：{err}", server.command.display()))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err("拿不到 MCP server 的标准输入输出".to_owned());
        };
        let mut session = Self {
            _child: child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_id: 1,
        };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "ai-boot", "version": env!("CARGO_PKG_VERSION") },
        });
        session
            .request("initialize", params, timeout)
            .await
            .map_err(|err| format!("MCP 握手失败：{err}"))?;
        session
            .send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await
            .map_err(|err| format!("MCP 握手失败：{err}"))?;
        Ok(session)
    }

    /// 调一个工具，返回它的结构化结果（没有结构化结果时尝试把文字按 JSON 解析）。
    pub async fn call(
        &mut self,
        tool: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": tool, "arguments": arguments }),
                timeout,
            )
            .await?;
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(CallError::Failed(if text.is_empty() {
                format!("{tool} 调用失败")
            } else {
                text
            }));
        }
        if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
            return Ok(structured.clone());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// 列出工具：`(工具名, inputSchema)`。
    pub async fn list_tools(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<(String, Value)>, CallError> {
        let result = self.request("tools/list", json!({}), timeout).await?;
        Ok(result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?.to_owned();
                Some((
                    name,
                    tool.get("inputSchema").cloned().unwrap_or(Value::Null),
                ))
            })
            .collect())
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let work = async {
            self.send(&message).await.map_err(CallError::Failed)?;
            loop {
                let line = self
                    .stdout
                    .next_line()
                    .await
                    .map_err(|err| CallError::Failed(format!("读取 MCP 响应失败：{err}")))?
                    .ok_or_else(|| CallError::Failed("MCP server 提前退出".to_owned()))?;
                let Ok(reply) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if reply.get("id").and_then(Value::as_u64) == Some(id)
                    && reply.get("method").is_none()
                {
                    if let Some(error) = reply.get("error") {
                        let text = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("未知错误");
                        return Err(CallError::Failed(text.to_owned()));
                    }
                    return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
                }
                // server 发来的请求（比如 ping、roots/list）：回「不支持」
                if let (Some(request_id), Some(_)) = (reply.get("id"), reply.get("method")) {
                    let refusal = json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "error": { "code": -32601, "message": "method not supported" },
                    });
                    self.send(&refusal).await.map_err(CallError::Failed)?;
                }
            }
        };
        match tokio::time::timeout(timeout, work).await {
            Ok(result) => result,
            Err(_) => Err(CallError::Timeout),
        }
    }

    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|err| format!("写入 MCP 请求失败：{err}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|err| format!("写入 MCP 请求失败：{err}"))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// 用 Python 写的假 MCP server：记下每次 tools/call，按工具名回预设结果。
    /// `replies` 里的值：`{"result": ...}` 正常返回，`{"error": "..."}` 报错，
    /// `{"hang": true}` 不回（模拟超时）。
    pub fn fake_server(dir: &std::path::Path, replies: &Value) -> Server {
        let script = dir.join("fake_mcp.py");
        let log = dir.join("calls.jsonl");
        std::fs::write(
            &script,
            r#"import json, sys
replies = json.loads(sys.argv[1])
log = open(sys.argv[2], "a")
for line in sys.stdin:
    msg = json.loads(line)
    method, mid = msg.get("method"), msg.get("id")
    if method == "initialize":
        # 先插一条通知和一个反向请求，客户端要能跳过
        print(json.dumps({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"}), flush=True)
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "0"}}}), flush=True)
    elif method == "tools/list":
        tools = replies.get("__tools__", [])
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {"tools": tools}}), flush=True)
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"]["arguments"]
        log.write(json.dumps({"name": name, "arguments": args}, ensure_ascii=False) + "\n")
        log.flush()
        action = args.get("action", "")
        reply = replies.get(name + "." + action, replies.get(name, {"result": {}}))
        if reply.get("hang"):
            continue
        if "error" in reply:
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": reply["error"]}], "isError": True}}), flush=True)
        else:
            print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": {
                "content": [{"type": "text", "text": json.dumps(reply["result"])}],
                "structuredContent": reply["result"]}}), flush=True)
"#,
        )
        .expect("写脚本");
        Server {
            command: PathBuf::from("python3"),
            args: vec![
                script.display().to_string(),
                replies.to_string(),
                log.display().to_string(),
            ],
            env: BTreeMap::new(),
        }
    }

    /// 假 server 收到的全部调用。
    pub fn calls(dir: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(dir.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn a_tool_call_returns_its_structured_result() {
        let dir = tempfile::tempdir().expect("临时目录");
        let server = fake_server(
            dir.path(),
            &json!({"jira_issue.get": {"result": {"key": "ABC-1", "summary": "登录 500"}}}),
        );
        let mut session = Session::start(&server, TIMEOUT).await.expect("握手");
        let result = session
            .call(
                "jira_issue",
                json!({"action": "get", "key": "ABC-1"}),
                TIMEOUT,
            )
            .await
            .expect("调用");
        assert_eq!(result["summary"], "登录 500");
        assert_eq!(
            calls(dir.path()),
            [json!({"name": "jira_issue", "arguments": {"action": "get", "key": "ABC-1"}})]
        );
    }

    #[tokio::test]
    async fn tool_errors_and_timeouts_are_told_apart() {
        let dir = tempfile::tempdir().expect("临时目录");
        let server = fake_server(
            dir.path(),
            &json!({
                "confluence_page.by_title": {"error": "空间 DEV 下没有标题为「x」的页面"},
                "confluence_page.create": {"hang": true}
            }),
        );
        let mut session = Session::start(&server, TIMEOUT).await.expect("握手");
        let err = session
            .call("confluence_page", json!({"action": "by_title"}), TIMEOUT)
            .await
            .expect_err("报错");
        assert!(
            matches!(err, CallError::Failed(ref m) if m.contains("没有标题")),
            "{err}"
        );
        let err = session
            .call(
                "confluence_page",
                json!({"action": "create"}),
                Duration::from_millis(300),
            )
            .await
            .expect_err("超时");
        assert!(matches!(err, CallError::Timeout));
    }

    /// 对真实的 qtmcp 做握手（不调用任何工具，不会登录）。
    /// `AI_BOOT_LIVE_QTMCP=<qtmcp 路径>` 时才跑。
    #[tokio::test]
    async fn the_real_qtmcp_accepts_our_handshake() {
        let Some(program) = std::env::var_os("AI_BOOT_LIVE_QTMCP") else {
            eprintln!("跳过：没有设置 AI_BOOT_LIVE_QTMCP");
            return;
        };
        let server = Server {
            command: PathBuf::from(program),
            args: vec!["--toolsets".into(), "jira,confluence".into()],
            env: BTreeMap::new(),
        };
        Session::start(&server, TIMEOUT).await.expect("握手");
    }

    #[tokio::test]
    async fn a_missing_server_fails_to_start() {
        let server = Server {
            command: PathBuf::from("/nonexistent/qtmcp"),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert!(Session::start(&server, TIMEOUT).await.is_err());
    }
}
