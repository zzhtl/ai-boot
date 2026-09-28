//! 长连接的二进制帧（`pbbp2.proto`，proto2）。
//!
//! 字段号与类型对齐官方 Go SDK 生成的 `ws/pbbp2.pb.go`。proto2 的 `required`
//! 语义要保留：`SeqID`/`LogID`/`method` 为 0 时也必须编码进去，服务端按
//! 字段存在性解析。

use prost::Message as _;

/// 控制帧：ping / pong。
pub const METHOD_CONTROL: i32 = 0;
/// 数据帧：事件 / 卡片回调。
pub const METHOD_DATA: i32 = 1;

/// 帧头里用到的 key。
pub mod header {
    pub const TYPE: &str = "type";
    /// 拆包后每一片都继承同一个 message_id。
    pub const MESSAGE_ID: &str = "message_id";
    /// 拆包数，未拆包为 1。
    pub const SUM: &str = "sum";
    /// 包序号，未拆包为 0。
    pub const SEQ: &str = "seq";
    pub const TRACE_ID: &str = "trace_id";
    /// 业务处理时长（毫秒），ACK 时由客户端追加。
    pub const BIZ_RT: &str = "biz_rt";
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Header {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Frame {
    #[prost(uint64, required, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub log_id: u64,
    #[prost(int32, required, tag = "3")]
    pub service: i32,
    #[prost(int32, required, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<Header>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}

/// 帧的业务类别，取自 `type` 头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameKind {
    Ping,
    Pong,
    Event,
    /// 资料推断卡片回调走的是 `type=event`，这个分支留着是为了
    /// 真出现时能在日志里看到，而不是被当成未知帧吞掉。
    Card,
    Other(String),
}

impl Frame {
    pub fn decode_bytes(bytes: &[u8]) -> Result<Self, prost::DecodeError> {
        Self::decode(bytes)
    }

    pub fn encode_bytes(&self) -> Vec<u8> {
        self.encode_to_vec()
    }

    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    pub fn kind(&self) -> FrameKind {
        match self.header(header::TYPE) {
            Some("ping") => FrameKind::Ping,
            Some("pong") => FrameKind::Pong,
            Some("event") => FrameKind::Event,
            Some("card") => FrameKind::Card,
            Some(other) => FrameKind::Other(other.to_owned()),
            None => FrameKind::Other(String::new()),
        }
    }

    /// 客户端心跳。`service` 取自连接 URL 上的 `service_id`。
    pub fn ping(service: i32) -> Self {
        Self {
            seq_id: 0,
            log_id: 0,
            service,
            method: METHOD_CONTROL,
            headers: vec![Header {
                key: header::TYPE.to_owned(),
                value: "ping".to_owned(),
            }],
            payload_encoding: None,
            payload_type: None,
            payload: None,
            log_id_new: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_frame() -> Frame {
        Frame {
            seq_id: 0,
            log_id: 0,
            service: 7,
            method: METHOD_DATA,
            headers: vec![
                Header {
                    key: "type".into(),
                    value: "event".into(),
                },
                Header {
                    key: "message_id".into(),
                    value: "m1".into(),
                },
            ],
            payload_encoding: None,
            payload_type: None,
            payload: Some(b"{}".to_vec()),
            log_id_new: None,
        }
    }

    #[test]
    fn round_trips_through_protobuf() {
        let frame = data_frame();
        let decoded = Frame::decode_bytes(&frame.encode_bytes()).expect("解码");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn zero_valued_required_fields_are_still_on_the_wire() {
        // proto2 required：0 值也要编码。换成 proto3 语义会把它们省掉，
        // 服务端按缺字段处理。字段按 tag 顺序编码，前四个都是 varint 0。
        let bytes = Frame::ping(0).encode_bytes();
        assert!(
            bytes.starts_with(&[0x08, 0, 0x10, 0, 0x18, 0, 0x20, 0]),
            "{bytes:?}"
        );
    }

    #[test]
    fn kind_comes_from_the_type_header() {
        assert_eq!(data_frame().kind(), FrameKind::Event);
        assert_eq!(Frame::ping(1).kind(), FrameKind::Ping);
        let mut unknown = data_frame();
        unknown.headers.clear();
        assert_eq!(unknown.kind(), FrameKind::Other(String::new()));
    }
}
