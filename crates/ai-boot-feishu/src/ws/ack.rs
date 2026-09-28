//! 数据帧的 ACK。
//!
//! 与官方 SDK 一致：原样回显收到的帧（SeqID、LogID、service、method 和全部
//! 头，`instance_id` 也要带回去），追加 `biz_rt` 头，负载换成
//! `{"code":…,"headers":null,"data":base64(JSON)|null}`。`data` 是 base64 而
//! 不是原始 JSON——卡片回调的 toast/卡片就放在这里。

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Serialize;

use super::frame::{Frame, Header, header};

#[derive(Serialize)]
struct AckBody {
    code: u16,
    headers: Option<()>,
    data: Option<String>,
}

pub fn ack(original: &Frame, code: u16, data: Option<&[u8]>, biz_rt: Duration) -> Frame {
    let body = AckBody {
        code,
        headers: None,
        data: data.map(|bytes| BASE64.encode(bytes)),
    };
    // 这个结构只有数字和字符串，序列化不会失败；万一失败也要回一个合法的 ACK
    let payload = serde_json::to_vec(&body)
        .unwrap_or_else(|_| br#"{"code":500,"headers":null,"data":null}"#.to_vec());

    let mut frame = original.clone();
    frame.headers.push(Header {
        key: header::BIZ_RT.to_owned(),
        value: biz_rt.as_millis().to_string(),
    });
    frame.payload = Some(payload);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::frame::METHOD_DATA;

    fn event_frame() -> Frame {
        Frame {
            seq_id: 11,
            log_id: 22,
            service: 7,
            method: METHOD_DATA,
            headers: vec![
                Header {
                    key: "type".into(),
                    value: "event".into(),
                },
                Header {
                    key: "instance_id".into(),
                    value: "i-1".into(),
                },
            ],
            payload_encoding: None,
            payload_type: None,
            payload: Some(br#"{"schema":"2.0"}"#.to_vec()),
            log_id_new: Some("n".into()),
        }
    }

    #[test]
    fn echoes_the_frame_and_appends_biz_rt() {
        let original = event_frame();
        let acked = ack(&original, 200, None, Duration::from_millis(42));
        assert_eq!(acked.seq_id, 11);
        assert_eq!(acked.log_id, 22);
        assert_eq!(acked.service, 7);
        assert_eq!(acked.method, METHOD_DATA);
        assert_eq!(acked.header("instance_id"), Some("i-1"));
        assert_eq!(acked.header("biz_rt"), Some("42"));
        assert_eq!(acked.log_id_new.as_deref(), Some("n"));
    }

    #[test]
    fn an_event_ack_has_null_data() {
        let acked = ack(&event_frame(), 200, None, Duration::ZERO);
        let body: serde_json::Value =
            serde_json::from_slice(&acked.payload.expect("负载")).expect("JSON");
        assert_eq!(
            body,
            serde_json::json!({"code":200,"headers":null,"data":null})
        );
    }

    #[test]
    fn callback_data_is_base64_of_the_json() {
        // 官方文档里的例子：{"toast":{"type":"info","content":"ok"}}
        let data = br#"{"toast":{"type":"info","content":"ok"}}"#;
        let acked = ack(&event_frame(), 200, Some(data), Duration::ZERO);
        let body: serde_json::Value =
            serde_json::from_slice(&acked.payload.expect("负载")).expect("JSON");
        assert_eq!(
            body["data"],
            "eyJ0b2FzdCI6eyJ0eXBlIjoiaW5mbyIsImNvbnRlbnQiOiJvayJ9fQ=="
        );
    }
}
