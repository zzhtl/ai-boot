//! 文本类文件：日志、配置、源码、CSV……
//!
//! 先按 UTF-8 解，解不开再按 GB18030（兼容 GBK）试。太长的按日志的看法压缩：
//! 保留开头、结尾和出错的行（带上下文），中间注明省略了多少行。

use std::sync::LazyLock;

use encoding_rs::{Encoding, GB18030};
use regex::Regex;

use super::{Output, TEXT_CHARS, clip_chars};

/// 开头、结尾各保留多少行。
const HEAD_LINES: usize = 40;
const TAIL_LINES: usize = 80;
/// 出错的行前后各带几行。
const CONTEXT_BEFORE: usize = 2;
const CONTEXT_AFTER: usize = 3;

static ERROR_LINE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)(error|exception|fatal|panic|traceback|caused by|严重|错误|异常)").ok()
});

/// 解码成字符串。看起来不是文本（有 NUL、控制字符太多、两种编码都解不开）时返回 `None`。
pub fn decode(bytes: &[u8]) -> Option<(String, &'static str)> {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom..]);
        return Some((text.into_owned(), encoding.name()));
    }
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        return None;
    }
    let (text, encoding) = match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), "UTF-8"),
        Err(_) => {
            let (text, _, had_errors) = GB18030.decode(bytes);
            if had_errors {
                return None;
            }
            (text.into_owned(), "GB18030")
        }
    };
    // 控制字符超过 5% 的不当文本
    let control = text
        .chars()
        .take(8192)
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    (control * 20 < text.chars().take(8192).count().max(1)).then_some((text, encoding))
}

pub fn extract(bytes: &[u8]) -> Output {
    let Some((text, encoding)) = decode(bytes) else {
        return Output {
            note: Some("无法识别的文本编码".to_owned()),
            ..Output::default()
        };
    };
    let (text, condensed) = condense(&text, TEXT_CHARS);
    let mut notes = Vec::new();
    if encoding != "UTF-8" {
        notes.push(format!("按 {encoding} 解码"));
    }
    if condensed {
        notes.push("内容较长，只保留了开头、结尾和出错的行，完整内容见原件".to_owned());
    }
    Output {
        text,
        images: Vec::new(),
        note: (!notes.is_empty()).then(|| notes.join("；")),
    }
}

/// 压缩到 `budget` 个字符以内。返回是否做了压缩。
pub fn condense(text: &str, budget: usize) -> (String, bool) {
    if text.chars().count() <= budget {
        return (text.to_owned(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut keep = vec![false; lines.len()];
    for flag in keep.iter_mut().take(HEAD_LINES) {
        *flag = true;
    }
    for flag in keep.iter_mut().rev().take(TAIL_LINES) {
        *flag = true;
    }
    if let Some(pattern) = ERROR_LINE.as_ref() {
        for (i, line) in lines.iter().enumerate() {
            if pattern.is_match(line) {
                let from = i.saturating_sub(CONTEXT_BEFORE);
                let to = (i + CONTEXT_AFTER).min(lines.len().saturating_sub(1));
                for flag in &mut keep[from..=to] {
                    *flag = true;
                }
            }
        }
    }
    let mut out = String::new();
    let mut skipped = 0;
    for (line, kept) in lines.iter().zip(&keep) {
        if *kept {
            if skipped > 0 {
                out.push_str(&format!("…（省略 {skipped} 行）…\n"));
                skipped = 0;
            }
            out.push_str(line);
            out.push('\n');
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push_str(&format!("…（省略 {skipped} 行）…\n"));
    }
    // 出错的行太多时仍可能超出：按字符截断，末尾说明
    let (clipped, cut) = clip_chars(&out, budget);
    if cut {
        return (format!("{clipped}\n…（后面的内容超出上限，已截断）"), true);
    }
    (clipped, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gbk_logs_are_decoded() {
        let (bytes, _, _) = GB18030.encode("2026-09-28 10:00:01 错误：连接池耗尽");
        let (text, encoding) = decode(&bytes).expect("能解码");
        assert_eq!(text, "2026-09-28 10:00:01 错误：连接池耗尽");
        assert_eq!(encoding, "GB18030");
        let output = extract(&bytes);
        assert_eq!(output.note.as_deref(), Some("按 GB18030 解码"));
    }

    #[test]
    fn a_bom_decides_the_encoding() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "日志".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes).map(|(t, _)| t).as_deref(), Some("日志"));
    }

    #[test]
    fn binary_content_is_not_mistaken_for_text() {
        assert!(decode(&[0x00, 0x01, 0x02]).is_none());
        let noisy: Vec<u8> = (1..32_u8).filter(|b| !matches!(b, 9 | 10 | 13)).collect();
        assert!(decode(&noisy.repeat(100)).is_none());
    }

    #[test]
    fn long_logs_keep_head_tail_and_errors_with_context() {
        let mut log = String::new();
        for i in 0..5000 {
            if i == 2500 {
                log.push_str("java.lang.IllegalStateException: Connection is not available\n");
                log.push_str("\tat com.zaxxer.hikari.pool.HikariPool.getConnection\n");
            } else {
                log.push_str(&format!(
                    "2026-09-28 10:00:{:02} INFO 第 {i} 行 正常\n",
                    i % 60
                ));
            }
        }
        let (out, condensed) = condense(&log, 16 * 1024);
        assert!(condensed);
        assert!(out.chars().count() <= 16 * 1024 + 64);
        assert!(out.contains("第 0 行"), "保留开头");
        assert!(out.contains("第 4999 行"), "保留结尾");
        assert!(out.contains("Connection is not available"), "保留出错的行");
        assert!(out.contains("HikariPool.getConnection"), "带上下文");
        assert!(out.contains("第 2498 行"), "带上文");
        assert!(out.contains("省略"));
        assert!(!out.contains("第 1200 行"), "中间正常的行被省略");
    }

    #[test]
    fn short_text_is_left_alone() {
        assert_eq!(condense("一行", 10), ("一行".to_owned(), false));
    }
}
