use ai_boot_feishu::ws::{DataKind, FrameHandler, WsConfig, run_go_sdk};
use secrecy::SecretString;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct Handler(mpsc::Sender<Value>);

#[async_trait::async_trait]
impl FrameHandler for Handler {
    async fn handle(&self, kind: DataKind, payload: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        assert_eq!(kind, DataKind::Event);
        let payload: Value = serde_json::from_slice(&payload).expect("JSON");
        self.0.send(payload.clone()).await.expect("事件接收方");
        match payload["type"].as_str() {
            Some("card") => Ok(Some(
                br#"{"toast":{"type":"info","content":"ok"}}"#.to_vec(),
            )),
            Some("failure") => Err("storage failed".into()),
            _ => Ok(None),
        }
    }
}

#[tokio::test]
async fn go_transport_forwards_events_errors_and_card_acks_and_stops() {
    let root = tempfile::tempdir().expect("目录");
    let program = root.path().join("sdk");
    std::fs::write(
        &program,
        r#"#!/usr/bin/python3
import sys,json,os
assert os.environ['FEISHU_APP_ID']=='cli_test'
assert os.environ['FEISHU_APP_SECRET']=='test-secret'
for i,kind in enumerate(['message','card','failure']):
 print(json.dumps({'id':i,'payload':{'type':kind,'unknown':'preserved'}}),flush=True)
replies={}
for _ in range(3):
 r=json.loads(sys.stdin.readline());replies[r['id']]=r
assert replies[0]['error'] is None
assert replies[1]['payload']['toast']['content']=='ok'
assert replies[2]['error']=='storage failed'
print(json.dumps({'id':4,'payload':{'type':'verified'}}),flush=True)
for line in sys.stdin: pass
"#,
    )
    .expect("模拟 SDK");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).expect("权限");
    let (tx, mut rx) = mpsc::channel(8);
    let cancel = CancellationToken::new();
    let config = WsConfig::new("cli_test", SecretString::from("test-secret"));
    let stop = cancel.clone();
    let task =
        tokio::spawn(
            async move { run_go_sdk(&program, config, Arc::new(Handler(tx)), stop).await },
        );
    for _ in 0..3 {
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("超时")
            .expect("事件");
        assert_eq!(event["unknown"], "preserved");
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("超时"),
        Some(json!({"type":"verified"}))
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("退出超时")
        .expect("任务")
        .expect("停止");
}
