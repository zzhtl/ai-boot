//! 大消息的拆包重组。
//!
//! 服务端把超长负载拆成 `sum` 片，每片带同一个 `message_id` 和序号 `seq`。
//! 只有补齐的那一片才会被分发和 ACK；未补齐的在 TTL 内没等到后续分片就丢弃。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 单条消息允许的最大分片数与累计字节数。分片信息来自对端，不设上限
/// 就是一个可以被放大的内存消耗点。
const MAX_PARTS: usize = 64;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    #[error("分片序号 {seq} 超出分片数 {sum}")]
    SeqOutOfRange { seq: usize, sum: usize },
    #[error("分片数 {0} 超过上限")]
    TooManyParts(usize),
    #[error("消息累计 {0} 字节，超过上限")]
    TooLarge(usize),
}

struct Pending {
    parts: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    touched: Instant,
}

impl Pending {
    fn new(sum: usize, now: Instant) -> Self {
        Self {
            parts: vec![None; sum],
            received: 0,
            bytes: 0,
            touched: now,
        }
    }
}

pub struct Assembler {
    ttl: Duration,
    pending: HashMap<String, Pending>,
}

impl Assembler {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            pending: HashMap::new(),
        }
    }

    /// 放入一片。返回 `Some(完整负载)` 表示这一片补齐了整条消息。
    pub fn push(
        &mut self,
        message_id: &str,
        sum: usize,
        seq: usize,
        payload: Vec<u8>,
        now: Instant,
    ) -> Result<Option<Vec<u8>>, AssembleError> {
        if sum <= 1 {
            return Ok(Some(payload));
        }
        if sum > MAX_PARTS {
            return Err(AssembleError::TooManyParts(sum));
        }
        if seq >= sum {
            return Err(AssembleError::SeqOutOfRange { seq, sum });
        }

        let entry = self
            .pending
            .entry(message_id.to_owned())
            .or_insert_with(|| Pending::new(sum, now));
        // 同一 message_id 的分片数变了，说明前面那批已经作废，以新的为准重来
        if entry.parts.len() != sum {
            *entry = Pending::new(sum, now);
        }
        entry.touched = now;

        match entry.parts[seq].replace(payload) {
            // 重复的分片：替换内容，不重复计数
            Some(old) => entry.bytes -= old.len(),
            None => entry.received += 1,
        }
        entry.bytes += entry.parts[seq].as_ref().map_or(0, Vec::len);
        if entry.bytes > MAX_TOTAL_BYTES {
            let bytes = entry.bytes;
            self.pending.remove(message_id);
            return Err(AssembleError::TooLarge(bytes));
        }
        if entry.received < sum {
            return Ok(None);
        }

        let Some(done) = self.pending.remove(message_id) else {
            return Ok(None);
        };
        let mut whole = Vec::with_capacity(done.bytes);
        for part in done.parts.into_iter().flatten() {
            whole.extend_from_slice(&part);
        }
        Ok(Some(whole))
    }

    /// 丢掉超过 TTL 仍未补齐的消息，返回丢弃的条数。
    pub fn sweep(&mut self, now: Instant) -> usize {
        let before = self.pending.len();
        let ttl = self.ttl;
        self.pending
            .retain(|_, p| now.saturating_duration_since(p.touched) < ttl);
        before - self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(10);

    #[test]
    fn a_single_part_passes_straight_through() {
        let mut a = Assembler::new(TTL);
        let got = a.push("m", 1, 0, b"abc".to_vec(), Instant::now());
        assert_eq!(got, Ok(Some(b"abc".to_vec())));
    }

    #[test]
    fn out_of_order_parts_are_joined_by_sequence() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(a.push("m", 3, 2, b"c".to_vec(), now), Ok(None));
        assert_eq!(a.push("m", 3, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(
            a.push("m", 3, 1, b"b".to_vec(), now),
            Ok(Some(b"abc".to_vec()))
        );
        // 补齐后不再残留
        assert_eq!(a.sweep(now + TTL * 2), 0);
    }

    #[test]
    fn a_duplicated_part_does_not_complete_the_message_early() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(a.push("m", 2, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(a.push("m", 2, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(
            a.push("m", 2, 1, b"b".to_vec(), now),
            Ok(Some(b"ab".to_vec()))
        );
    }

    #[test]
    fn incomplete_messages_expire_after_the_ttl() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(a.push("m", 2, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(a.sweep(now + TTL / 2), 0);
        assert_eq!(a.sweep(now + TTL), 1);
        // 过期后迟到的分片不会拼出半截消息
        assert_eq!(a.push("m", 2, 1, b"b".to_vec(), now + TTL), Ok(None));
    }

    #[test]
    fn each_part_refreshes_the_ttl() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(a.push("m", 3, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(a.push("m", 3, 1, b"b".to_vec(), now + TTL / 2), Ok(None));
        assert_eq!(a.sweep(now + TTL), 0);
    }

    #[test]
    fn a_changed_part_count_restarts_the_message() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(a.push("m", 3, 0, b"x".to_vec(), now), Ok(None));
        assert_eq!(a.push("m", 2, 0, b"a".to_vec(), now), Ok(None));
        assert_eq!(
            a.push("m", 2, 1, b"b".to_vec(), now),
            Ok(Some(b"ab".to_vec()))
        );
    }

    #[test]
    fn hostile_part_headers_are_rejected() {
        let mut a = Assembler::new(TTL);
        let now = Instant::now();
        assert_eq!(
            a.push("m", 2, 2, Vec::new(), now),
            Err(AssembleError::SeqOutOfRange { seq: 2, sum: 2 })
        );
        assert_eq!(
            a.push("m", MAX_PARTS + 1, 0, Vec::new(), now),
            Err(AssembleError::TooManyParts(MAX_PARTS + 1))
        );
        let big = vec![0_u8; MAX_TOTAL_BYTES / 2 + 1];
        assert_eq!(a.push("m", 3, 0, big.clone(), now), Ok(None));
        assert!(matches!(
            a.push("m", 3, 1, big, now),
            Err(AssembleError::TooLarge(_))
        ));
    }
}
