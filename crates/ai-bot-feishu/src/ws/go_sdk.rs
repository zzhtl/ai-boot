//! 官方 Go SDK 子进程与既有 FrameHandler 的桥接；不新增网络监听端口。

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use secrecy::ExposeSecret as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;

use super::{DataKind, FrameHandler, WsConfig};

#[derive(Deserialize)]
struct Request {
    id: u64,
    payload: Value,
}

#[derive(Serialize)]
struct Response {
    id: u64,
    payload: Option<Value>,
    error: Option<String>,
}

pub async fn run_go_sdk(
    program: &Path,
    config: WsConfig,
    handler: Arc<dyn FrameHandler>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let mut command = tokio::process::Command::new(program);
    command.env_clear();
    for name in [
        "HOME",
        "PATH",
        "LANG",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .env("FEISHU_APP_ID", &config.app_id)
        .env("FEISHU_APP_SECRET", config.app_secret.expose_secret())
        .env(
            "FEISHU_BASE_URL",
            config.base_url.as_str().trim_end_matches('/'),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Go SDK 缺少 stdout"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Go SDK 缺少 stdin"))?;
    let (tx, mut rx) = mpsc::channel::<Response>(256);
    let mut writer = tokio::spawn(async move {
        while let Some(reply) = rx.recv().await {
            let mut bytes = serde_json::to_vec(&reply).map_err(io::Error::other)?;
            bytes.push(b'\n');
            stdin.write_all(&bytes).await?;
        }
        Ok::<_, io::Error>(())
    });
    let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(8 * 1024 * 1024));
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            status = child.wait() => {
                break Err(io::Error::other(format!("Go SDK 已退出：{status:?}")));
            }
            result = &mut writer => {
                break Err(io::Error::other(format!("Go SDK ACK 管道关闭：{result:?}")));
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            line = lines.next() => {
                let line = match line {
                    Some(Ok(line)) => line,
                    Some(Err(err)) => break Err(io::Error::other(err)),
                    None => break Err(io::Error::other("Go SDK 输出管道已关闭")),
                };
                let request: Request = match serde_json::from_str(&line) {
                    Ok(request) => request,
                    Err(err) => break Err(io::Error::other(format!("Go SDK 协议无效：{err}"))),
                };
                let handler = Arc::clone(&handler);
                let tx = tx.clone();
                let budget = config.handler_budget;
                tasks.spawn(async move {
                    let handled = tokio::time::timeout(budget, handler.handle(
                        DataKind::Event, request.payload.to_string().into_bytes(),
                    )).await;
                    let response = match handled {
                        Ok(Ok(payload)) => Response { id: request.id,
                            payload: payload.and_then(|body| serde_json::from_slice(&body).ok()), error: None },
                        Ok(Err(error)) => Response { id: request.id, payload: None, error: Some(error) },
                        Err(_) => Response { id: request.id, payload: None, error: Some("事件处理超时".into()) },
                    };
                    let _ = tx.send(response).await;
                });
            }
        }
    };
    tasks.shutdown().await;
    writer.abort();
    // stdin 关闭后 Go SDK 会取消 context；保留几秒，让 SDK 主动关闭 WebSocket。
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        child.start_kill()?;
        child.wait().await?;
    }
    result
}
