//! 文本类文件：日志、配置、源码、CSV……
//!
//! UTF-8 和 GB18030（兼容 GBK）都按容错的方式解一遍，取乱码少的：UTF-8 日志里混了一段
//! GBK、末尾截断了半个字，照样当文本读，说明里注明。终端颜色控制符先去掉。
//! 太长的按日志的看法压缩：先给结尾留出预算，再放开头和出错的行（带堆栈，同类的只留
//! 第一次和最后一次），中间注明省略了多少行。

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use encoding_rs::{Encoding, GB18030};
use regex::Regex;

use super::{Output, TEXT_BYTES};

/// 开头、结尾各最多保留多少行，各占预算的几分之一。结尾先留：最后的报错和退出原因在那里。
const HEAD_LINES: usize = 40;
const TAIL_LINES: usize = 80;
const HEAD_SHARE: usize = 6;
const TAIL_SHARE: usize = 3;
/// 出错的行前后各带几行；后面跟着堆栈时带堆栈，不再另带下文。
const CONTEXT_BEFORE: usize = 2;
const CONTEXT_AFTER: usize = 3;
/// 单行的字节上限：一行 JSON 就能占掉好几 KB 预算。
const LINE_BYTES: usize = 1000;
/// 每段堆栈（异常本身、每个 Caused by）保留开头和结尾各几行：业务代码的帧常在第 5～15 层，
/// Python 最内层的帧在最后。一整块最多占预算的几分之一，放不下时每段少留一些。
const STACK_HEAD: usize = 16;
const STACK_TAIL: usize = 4;
const BLOCK_SHARE: usize = 4;
/// 「…（省略 N 行）…」一行最多占的字节。
const GAP_BYTES: usize = 40;
/// 判断是不是文本只看开头这么多。
const SAMPLE: usize = 8192;

/// 英文词的边界按 ASCII 算（`(?-u:\b)`）：Unicode 的词边界遇到中文会让正则退回慢得多的
/// 匹配方式，日志里满是中文。
static ERROR_LINE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)error|exception|fatal|panic|traceback|caused by|严重|错误|异常|失败|超时|拒绝",
        // 英文词整词匹配：failover、request.timeout.ms、timeout=30 这类不算
        r"|(?-u:\b)fail(?:s|ed|ing|ure|ures)?(?-u:\b)|(?-u:\b)timed out(?-u:\b)",
        r"|(?-u:\b)timeout(?:$|[^\w.=-])|(?-u:\b)(?:refused|killed|denied)(?-u:\b)",
        // OOMKilled 之类只认大写：不区分大小写会匹配到 room、zoom
        r"|(?-i:(?-u:\b)OOM)",
    ))
    .ok()
});

/// 接在出错行后面的堆栈：缩进的行（Java 的 `at`、Python 的帧和代码、Go 的文件行、
/// `... N more`），以及不缩进的 Caused by、Python 的 Traceback、Go 的 goroutine 和函数帧。
static STACK_LINE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:\s+\S|Caused by:|Suppressed:|\.\.\. \d+ (?:more|common frames omitted)",
        r"|Traceback \(most recent call last\)|During handling of the above exception",
        r"|The above exception was the direct cause|goroutine \d+ \[|created by |\[signal ",
        r"|[\w.*/()\[\]-]+\(.*\)$)",
    ))
    .ok()
});

/// 堆栈里新起一段的行：每段各留开头结尾，最底下的根因不会被上面的帧挤掉。
static STACK_SEGMENT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^\s*(?:Caused by:|Suppressed:)|^(?:Traceback \(most recent call last\)",
        r"|During handling of the above exception|The above exception was the direct cause",
        r"|goroutine \d+ \[)",
    ))
    .ok()
});

/// 归并同类错误时抹掉的可变部分：UUID、十六进制、数字（时间戳、线程号、IP 都在内）。
static VARIABLE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}|0x[0-9a-f]+",
        r"|(?-u:\b)[0-9a-f]*[0-9][0-9a-f]*(?-u:\b)|[0-9]+",
    ))
    .ok()
});

/// 终端颜色和光标控制：CSI（ESC [ … 字母）和 OSC（ESC ] … BEL 或 ESC \）。
static ANSI: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)").ok());

/// 解码出的文字。
#[derive(Debug)]
pub struct Decoded {
    pub text: String,
    pub encoding: &'static str,
    /// 解不开、替换成 U+FFFD 的地方。
    pub replaced: usize,
}

/// 看开头就能断定不是文本（没有 BOM 却有 NUL）时返回 `false`；其余的要完整解码才知道。
pub fn maybe_text(bytes: &[u8]) -> bool {
    Encoding::for_bom(bytes).is_some() || !bytes[..bytes.len().min(SAMPLE)].contains(&0)
}

/// 解码成字符串，去掉终端颜色控制符。看起来不是文本（有 NUL、乱码或控制字符太多）时
/// 返回 `None`。
pub fn decode(bytes: &[u8]) -> Option<Decoded> {
    if let Some((encoding, bom)) = Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom..]);
        return Some(Decoded {
            text: strip_ansi(&text),
            encoding: encoding.name(),
            replaced: 0,
        });
    }
    if !maybe_text(bytes) {
        return None;
    }
    let (text, encoding, replaced) = match std::str::from_utf8(bytes) {
        Ok(text) => (Cow::Borrowed(text), "UTF-8", 0),
        Err(_) => {
            // 两种都按容错的方式解，取乱码少的。GB18030 里正经编码的 U+FFFD 实际不会出现，
            // 数替换字符就是它解不开的地方
            let utf8 = bytes
                .utf8_chunks()
                .filter(|chunk| !chunk.invalid().is_empty())
                .count();
            let (gb, _) = GB18030.decode_without_bom_handling(bytes);
            let gb_replaced = gb.matches('\u{FFFD}').count();
            if gb_replaced < utf8 {
                (gb, "GB18030", gb_replaced)
            } else {
                (String::from_utf8_lossy(bytes), "UTF-8", utf8)
            }
        }
    };
    // 乱码超过 1% 的不当文本；只有一处的（多半是末尾截断的半个字）不算
    if replaced > 1 && replaced * 100 > text.chars().count() {
        return None;
    }
    let text = strip_ansi(&text);
    // 控制字符超过 5% 的不当文本
    let control = text
        .chars()
        .take(SAMPLE)
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        .count();
    (control * 20 < text.chars().take(SAMPLE).count().max(1)).then_some(Decoded {
        text,
        encoding,
        replaced,
    })
}

fn strip_ansi(text: &str) -> String {
    match ANSI.as_ref() {
        Some(pattern) if text.contains('\x1b') => pattern.replace_all(text, "").into_owned(),
        _ => text.to_owned(),
    }
}

pub fn extract(bytes: &[u8]) -> Result<Output, String> {
    let decoded = decode(bytes).ok_or_else(|| "二进制文件，无法解析".to_owned())?;
    let (text, condensed) = condense(&decoded.text, TEXT_BYTES);
    let mut notes = describe(&decoded);
    if condensed {
        notes.push("内容较长，只保留了开头、结尾和出错的行，完整内容见原件".to_owned());
    }
    Ok(Output {
        text,
        note: (!notes.is_empty()).then(|| notes.join("；")),
        ..Output::default()
    })
}

/// 解码方式的说明：不是 UTF-8、有解不开的地方时注明。
pub fn describe(decoded: &Decoded) -> Vec<String> {
    let mut notes = Vec::new();
    if decoded.encoding != "UTF-8" {
        notes.push(format!("按 {} 解码", decoded.encoding));
    }
    if decoded.replaced > 0 {
        notes.push(format!("有 {} 处无法解码，已替换成 �", decoded.replaced));
    }
    notes
}

/// 有没有出错的行。
pub fn has_errors(text: &str) -> bool {
    ERROR_LINE
        .as_ref()
        .is_some_and(|pattern| text.lines().any(|line| pattern.is_match(line)))
}

/// 压缩到 `budget` 个字节以内（预算至少要有几 KB）。返回是否做了压缩。
pub fn condense(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_owned(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut picker = Picker::new(&lines, budget);
    let tail_budget = picker.used + budget / TAIL_SHARE;
    for i in (0..lines.len()).rev().take(TAIL_LINES) {
        if !picker.take_within(&[i], None, tail_budget) {
            break;
        }
    }
    let head_budget = picker.used + budget / HEAD_SHARE;
    for i in 0..lines.len().min(HEAD_LINES) {
        if !picker.take_within(&[i], None, head_budget) {
            break;
        }
    }
    picker.take_errors();
    let out = picker.render();
    debug_assert!(out.len() <= budget, "{} > {budget}", out.len());
    (out, true)
}

/// 挑出要保留的行，边挑边按渲染后的字节数记账。
struct Picker<'a> {
    lines: &'a [&'a str],
    keep: Vec<bool>,
    /// 接在某一行后面的说明。
    notes: HashMap<usize, String>,
    used: usize,
    budget: usize,
    /// 单行上限：预算小的时候跟着缩。
    line_bytes: usize,
}

impl<'a> Picker<'a> {
    fn new(lines: &'a [&'a str], budget: usize) -> Self {
        Self {
            lines,
            keep: vec![false; lines.len()],
            notes: HashMap::new(),
            // 开头如果被省略，最前面还要多一行「省略」
            used: GAP_BYTES,
            budget,
            line_bytes: LINE_BYTES.min(budget / 8),
        }
    }

    /// 放进输出的样子：太长的行截断并注明。
    fn shown(&self, i: usize) -> Cow<'a, str> {
        let line = self.lines[i];
        if line.len() <= self.line_bytes {
            return Cow::Borrowed(line);
        }
        let mut end = self.line_bytes;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        Cow::Owned(format!(
            "{}…（这一行共 {} 字节，已截断）",
            &line[..end],
            line.len()
        ))
    }

    fn take(&mut self, indices: &[usize], note: Option<(usize, String)>) -> bool {
        self.take_within(indices, note, self.budget)
    }

    /// 保留这些行（升序），`note` 接在某一行后面；总账超过 `limit` 就一行都不留。
    /// 新留下的一段连续行两头都不挨着已留的行时，按多出一行「省略」记账：只会多算不会少算。
    fn take_within(
        &mut self,
        indices: &[usize],
        note: Option<(usize, String)>,
        limit: usize,
    ) -> bool {
        let fresh: Vec<usize> = indices.iter().copied().filter(|&i| !self.keep[i]).collect();
        let mut runs = 0;
        let mut n = 0;
        while n < fresh.len() {
            let start = fresh[n];
            while fresh.get(n + 1) == Some(&(fresh[n] + 1)) {
                n += 1;
            }
            let end = fresh[n];
            n += 1;
            let joined =
                (start > 0 && self.keep[start - 1]) || self.keep.get(end + 1) == Some(&true);
            runs += usize::from(!joined);
        }
        let mut cost: usize = fresh
            .iter()
            .map(|&i| self.shown(i).len() + 1)
            .sum::<usize>()
            + runs * GAP_BYTES;
        if let Some((_, text)) = &note {
            cost += text.len() + 1;
        }
        if self.used + cost > limit.min(self.budget) {
            return false;
        }
        for i in fresh {
            self.keep[i] = true;
        }
        if let Some((at, text)) = note {
            self.notes.insert(at, text);
        }
        self.used += cost;
        true
    }

    /// 出错的行带上前文和堆栈。同类的（抹掉数字、UUID 后相同）只留第一次和最后一次，
    /// 注明一共几次。顺序从最早和最晚的两头往中间排：最早的常是根因，最晚的是最终结果。
    fn take_errors(&mut self) {
        let Some(pattern) = ERROR_LINE.as_ref() else {
            return;
        };
        // 每行只匹配一次：找前后文时还要再看邻近的行是不是出错行
        let errors: Vec<bool> = self.lines.iter().map(|l| pattern.is_match(l)).collect();
        let mut blocks: Vec<Block> = Vec::new();
        let mut kinds: HashMap<String, Vec<usize>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut i = 0;
        while i < self.lines.len() {
            if !errors[i] {
                i += 1;
                continue;
            }
            let block = self.block(i, &errors);
            let next = block.end.max(i + 1);
            let kind = template(self.lines[i]);
            let seen = kinds.entry(kind.clone()).or_default();
            if seen.is_empty() {
                order.push(kind);
            }
            seen.push(blocks.len());
            blocks.push(block);
            // 堆栈里的 Caused by 也像出错的行，已经算进这一块了
            i = next;
        }
        let mut firsts = Vec::new();
        let mut lasts = Vec::new();
        let (mut low, mut high) = (0, order.len());
        while low < high {
            firsts.push(&order[low]);
            low += 1;
            if low < high {
                high -= 1;
                firsts.push(&order[high]);
            }
        }
        for kind in &firsts {
            let seen = &kinds[kind.as_str()];
            let first = &blocks[seen[0]];
            let note = (seen.len() > 2).then(|| {
                let at = first.lines.last().copied().unwrap_or(first.error);
                (at, format!("…（同类错误共出现 {} 次）…", seen.len()))
            });
            self.take_block(first, note);
            if let Some(&last) = seen.last().filter(|_| seen.len() > 1) {
                lasts.push(last);
            }
        }
        for last in lasts {
            self.take_block(&blocks[last], None);
        }
    }

    /// 整块放不下就只留出错的那一行。
    fn take_block(&mut self, block: &Block, note: Option<(usize, String)>) {
        if self.take(&block.lines, note.clone()) {
            return;
        }
        let note = note.map(|(_, text)| (block.error, text));
        self.take(&[block.error], note);
    }

    /// 出错的第 `error` 行和它的前文、堆栈（没有堆栈时带几行下文）。前后文不越过别的
    /// 出错行：一连串同类报错互为上下文的话，归并就白做了。
    fn block(&self, error: usize, errors: &[bool]) -> Block {
        let context = |i: &usize| !errors[*i];
        let from = (error.saturating_sub(CONTEXT_BEFORE)..error)
            .rev()
            .take_while(context)
            .last()
            .unwrap_or(error);
        let end = stack_end(self.lines, error);
        if end == error + 1 {
            let to = (error + 1..=(error + CONTEXT_AFTER).min(self.lines.len() - 1))
                .take_while(context)
                .last()
                .unwrap_or(error);
            return Block {
                error,
                lines: (from..=to).collect(),
                end,
            };
        }
        // 按段切开，每段留开头结尾；整块超过上限就每段少留一些
        let mut segments: Vec<Vec<usize>> = vec![Vec::new()];
        for i in error + 1..end {
            let starts = STACK_SEGMENT
                .as_ref()
                .is_some_and(|p| p.is_match(self.lines[i]));
            match segments.last_mut() {
                Some(segment) if !starts => segment.push(i),
                _ => segments.push(vec![i]),
            }
        }
        let cap = self.budget / BLOCK_SHARE;
        let mut frames = STACK_HEAD + STACK_TAIL;
        loop {
            let mut lines: Vec<usize> = (from..=error).collect();
            for (n, segment) in segments.iter().enumerate() {
                // 第一段的开头是出错的行本身，其余各段的开头是 Caused by 这类
                let (header, body) = match n {
                    0 => (None, segment.as_slice()),
                    _ => (segment.first(), segment.get(1..).unwrap_or_default()),
                };
                lines.extend(header);
                let head = frames * STACK_HEAD / (STACK_HEAD + STACK_TAIL);
                if body.len() <= frames {
                    lines.extend(body);
                } else {
                    lines.extend(&body[..head]);
                    lines.extend(&body[body.len() - (frames - head)..]);
                }
            }
            let bytes: usize = lines.iter().map(|&i| self.shown(i).len() + 1).sum();
            if bytes <= cap || frames == 0 {
                return Block { error, lines, end };
            }
            frames /= 2;
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let mut skipped = 0;
        for i in 0..self.lines.len() {
            if !self.keep[i] {
                skipped += 1;
                continue;
            }
            if skipped > 0 {
                out.push_str(&format!("…（省略 {skipped} 行）…\n"));
                skipped = 0;
            }
            out.push_str(&self.shown(i));
            out.push('\n');
            if let Some(note) = self.notes.get(&i) {
                out.push_str(note);
                out.push('\n');
            }
        }
        if skipped > 0 {
            out.push_str(&format!("…（省略 {skipped} 行）…\n"));
        }
        out
    }
}

/// 一个出错的地方。
struct Block {
    /// 出错的那一行。
    error: usize,
    /// 要保留的行（升序）。
    lines: Vec<usize>,
    /// 堆栈结束后的下一行。
    end: usize,
}

/// 第 `error` 行后面的堆栈到哪一行为止（不含）。空行后面接着堆栈的（Go 的 goroutine、
/// Python 的链式异常）也算；Python 的异常说明在帧的后面，也带上。
fn stack_end(lines: &[&str], error: usize) -> usize {
    let Some(stack) = STACK_LINE.as_ref() else {
        return error + 1;
    };
    let traceback = |line: &str| line.starts_with("Traceback (most recent call last)");
    let mut python = traceback(lines[error]);
    let mut end = error + 1;
    while let Some(line) = lines.get(end) {
        if stack.is_match(line) {
            python |= traceback(line);
        } else if line.trim().is_empty() {
            if !lines.get(end + 1).is_some_and(|next| stack.is_match(next)) {
                break;
            }
        } else if python {
            python = false;
        } else {
            break;
        }
        end += 1;
    }
    end
}

/// 同类错误的归并键。
fn template(line: &str) -> String {
    match VARIABLE.as_ref() {
        Some(pattern) => pattern.replace_all(line.trim(), "#").into_owned(),
        None => line.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gbk_logs_are_decoded() {
        let (bytes, _, _) = GB18030.encode("2026-09-28 10:00:01 错误：连接池耗尽");
        let decoded = decode(&bytes).expect("能解码");
        assert_eq!(decoded.text, "2026-09-28 10:00:01 错误：连接池耗尽");
        assert_eq!(decoded.encoding, "GB18030");
        let output = extract(&bytes).expect("解析");
        assert_eq!(output.note.as_deref(), Some("按 GB18030 解码"));
    }

    #[test]
    fn a_bom_decides_the_encoding() {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "日志".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode(&bytes).map(|d| d.text).as_deref(), Some("日志"));
    }

    #[test]
    fn binary_content_is_not_mistaken_for_text() {
        assert!(decode(&[0x00, 0x01, 0x02]).is_none());
        let noisy: Vec<u8> = (1..32_u8).filter(|b| !matches!(b, 9 | 10 | 13)).collect();
        assert!(decode(&noisy.repeat(100)).is_none());
        // 大半解不开的字节
        let garbage: Vec<u8> = (0..4096_u32).map(|i| (i * 7 % 128 + 128) as u8).collect();
        assert!(decode(&garbage).is_none());
        assert!(extract(&[0x00, 0x01]).is_err_and(|e| e.contains("二进制")));
    }

    fn utf8_log(lines: usize) -> String {
        (0..lines)
            .map(|i| {
                format!(
                    "2026-09-28 10:00:{:02} INFO 第 {i} 行 正常处理请求\n",
                    i % 60
                )
            })
            .collect()
    }

    #[test]
    fn a_gbk_segment_in_a_utf8_log_keeps_it_readable() {
        let mut bytes = utf8_log(200).into_bytes();
        let (gbk, _, _) = GB18030.encode("2026-09-28 10:01:00 ERROR 调用支付网关失败\n");
        bytes.extend_from_slice(&gbk);
        bytes.extend_from_slice(utf8_log(10).as_bytes());
        let decoded = decode(&bytes).expect("仍是文本");
        assert_eq!(decoded.encoding, "UTF-8");
        assert!(decoded.replaced > 0);
        assert!(decoded.text.contains("第 199 行 正常处理请求"));
        let note = extract(&bytes).expect("解析").note.expect("有说明");
        assert!(note.contains("无法解码"), "{note}");
    }

    #[test]
    fn a_gbk_log_with_a_stray_byte_is_still_gbk() {
        let text = "2026-09-28 10:00:01 错误：连接池耗尽，等待 30 秒后重试\n".repeat(20);
        let (gbk, _, _) = GB18030.encode(&text);
        let mut bytes = gbk.into_owned();
        bytes.insert(100, 0xFF);
        let decoded = decode(&bytes).expect("仍是文本");
        assert_eq!(decoded.encoding, "GB18030");
        assert_eq!(decoded.replaced, 1);
        assert!(decoded.text.contains("连接池耗尽"));
    }

    #[test]
    fn a_truncated_last_character_or_a_stray_byte_is_tolerated() {
        // 按字节截断的日志，末尾剩半个汉字；短文件也一样
        let mut bytes = "2026-09-28 ERROR 连接超时".as_bytes().to_vec();
        bytes.truncate(bytes.len() - 1);
        let decoded = decode(&bytes).expect("仍是文本");
        assert_eq!((decoded.encoding, decoded.replaced), ("UTF-8", 1));
        assert!(decoded.text.starts_with("2026-09-28 ERROR 连接"));
        let mut bytes = utf8_log(50).into_bytes();
        // 时间戳里的一个数字坏成 0xFF
        bytes[14] = 0xFF;
        let decoded = decode(&bytes).expect("仍是文本");
        assert_eq!((decoded.encoding, decoded.replaced), ("UTF-8", 1));
    }

    #[test]
    fn terminal_colors_are_stripped_before_judging_and_reading() {
        let log = concat!(
            "\x1b[2m2026-09-28 10:00:01.123\x1b[0;39m \x1b[32m INFO\x1b[0;39m \x1b[35m4242\x1b[0;39m ",
            "\x1b[2m---\x1b[0;39m \x1b[36mc.x.App\x1b[0;39m \x1b[2m:\x1b[0;39m Started\n",
            "\x1b[2m2026-09-28 10:00:02.456\x1b[0;39m \x1b[31mERROR\x1b[0;39m \x1b[35m4242\x1b[0;39m ",
            "\x1b[2m---\x1b[0;39m \x1b[36mc.x.Pay\x1b[0;39m \x1b[2m:\x1b[0;39m 调用失败\n",
            "\x1b]8;;http://x\x07链接\x1b]8;;\x07\n",
        );
        let decoded = decode(log.as_bytes()).expect("彩色日志也是文本");
        assert!(!decoded.text.contains('\x1b'), "{:?}", decoded.text);
        assert!(
            decoded
                .text
                .contains("2026-09-28 10:00:02.456 ERROR 4242 --- c.x.Pay : 调用失败")
        );
        assert!(decoded.text.contains("链接"));
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
        assert!(out.len() <= 16 * 1024);
        assert!(out.contains("第 0 行"), "保留开头");
        assert!(out.contains("第 4999 行"), "保留结尾");
        assert!(out.contains("Connection is not available"), "保留出错的行");
        assert!(out.contains("HikariPool.getConnection"), "带堆栈");
        assert!(out.contains("第 2498 行"), "带上文");
        assert!(out.contains("省略"));
        assert!(!out.contains("第 1200 行"), "中间正常的行被省略");
    }

    #[test]
    fn many_errors_do_not_push_out_the_tail() {
        let mut log = String::from("2026-09-28 10:00:00 INFO 启动 app 3.2.1\n");
        log.push_str(&utf8_log(50));
        for i in 0..3000 {
            log.push_str(&format!(
                "2026-09-28 10:{:02}:{:02} ERROR [exec-{i}] 调用支付网关失败，连接超时 order={i} trace=9f{i:06x}\n",
                i / 60 % 60,
                i % 60
            ));
            log.push_str(&format!(
                "2026-09-28 10:00:00 WARN 第 {i} 次重试 user-{i}\n"
            ));
        }
        log.push_str("2026-09-28 11:00:00 FATAL 连接池耗尽，服务退出\n");
        for i in 0..100 {
            log.push_str(&format!("2026-09-28 11:00:01 INFO 关闭第 {i} 个组件\n"));
        }
        let (out, _) = condense(&log, TEXT_BYTES);
        assert!(out.len() <= TEXT_BYTES);
        assert!(out.contains("启动 app 3.2.1"), "开头");
        assert!(out.contains("关闭第 99 个组件"), "结尾");
        assert!(out.contains("FATAL 连接池耗尽，服务退出"), "最后的致命错误");
        assert!(out.contains("order=0 "), "同类的第一次");
        assert!(out.contains("order=2999 "), "同类的最后一次");
        assert!(out.contains("同类错误共出现 3000 次"), "{out}");
        assert_eq!(out.matches("调用支付网关失败").count(), 2, "同类的只留两次");
    }

    #[test]
    fn deep_stacks_keep_business_frames_and_the_root_cause() {
        let mut log = utf8_log(3000);
        log.push_str("2026-09-28 10:05:00 ERROR c.x.OrderService - 下单失败\n");
        log.push_str("org.springframework.dao.DataAccessResourceFailureException: 取连接失败\n");
        for depth in 0..60 {
            let frame = if depth == 12 {
                "com.example.order.OrderService.place(OrderService.java:88)".to_owned()
            } else {
                format!("org.framework.Layer{depth}.invoke(Layer{depth}.java:{depth})")
            };
            log.push_str(&format!("\tat {frame}\n"));
        }
        log.push_str("Caused by: java.net.SocketTimeoutException: connect timed out\n");
        for depth in 0..40 {
            log.push_str(&format!(
                "\tat java.net.Socket{depth}.connect(Socket.java:{depth})\n"
            ));
        }
        log.push_str("\t... 58 more\n");
        log.push_str(&utf8_log(3000));
        let (out, _) = condense(&log, TEXT_BYTES);
        assert!(out.len() <= TEXT_BYTES);
        assert!(out.contains("下单失败"));
        assert!(
            out.contains("OrderService.place(OrderService.java:88)"),
            "第 12 层的业务帧"
        );
        assert!(
            out.contains("Caused by: java.net.SocketTimeoutException"),
            "根因"
        );
        assert!(out.contains("Socket0.connect"), "根因的第一帧");
        assert!(out.contains("... 58 more"));
        assert!(!out.contains("Layer40.invoke"), "中间的框架帧省略");
    }

    #[test]
    fn python_and_go_stacks_stay_attached() {
        let mut log = utf8_log(3000);
        log.push_str("Traceback (most recent call last):\n");
        for depth in 0..30 {
            log.push_str(&format!(
                "  File \"/app/m{depth}.py\", line {depth}, in f{depth}\n"
            ));
            log.push_str(&format!("    f{}()\n", depth + 1));
        }
        log.push_str("KeyError: 'tenant_id'\n");
        log.push_str(&utf8_log(100));
        log.push_str("panic: runtime error: invalid memory address or nil pointer dereference\n");
        log.push_str("[signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x47e9d6]\n\n");
        log.push_str("goroutine 1 [running]:\nmain.(*Server).handle(0x0, 0xc000010000)\n");
        log.push_str("\t/app/server.go:42 +0x26\nmain.main()\n\t/app/main.go:10 +0x25\n");
        log.push_str(&utf8_log(3000));
        let (out, _) = condense(&log, TEXT_BYTES);
        assert!(out.contains("KeyError: 'tenant_id'"), "异常说明");
        assert!(out.contains("/app/m29.py"), "最内层的帧");
        assert!(out.contains("/app/m0.py"), "最外层的帧");
        assert!(out.contains("/app/server.go:42"), "Go 的帧");
        assert!(out.contains("goroutine 1 [running]"));
    }

    #[test]
    fn huge_lines_are_cut_and_the_output_is_byte_bounded() {
        let json = format!("{{\"payload\":\"{}\"}}", "中".repeat(2000));
        let mut log = String::new();
        for i in 0..100 {
            log.push_str(&format!("2026-09-28 ERROR 回调失败 id={i} body={json}\n"));
        }
        for budget in [2 * 1024, 5000, TEXT_BYTES] {
            let (out, condensed) = condense(&log, budget);
            assert!(condensed);
            assert!(out.len() <= budget, "{} > {budget}", out.len());
            assert!(out.contains("已截断"), "{out}");
        }
        // 纯中文的长日志按字节算，不会超出 prompt 里的字节预算
        let (out, _) = condense(&"一二三四五六七八九十\n".repeat(20_000), TEXT_BYTES);
        assert!(out.len() <= TEXT_BYTES);
    }

    #[test]
    fn the_error_pattern_avoids_obvious_false_positives() {
        let pattern = ERROR_LINE.as_ref().expect("正则");
        for line in [
            "调用支付网关失败，连接超时",
            "Connection refused",
            "Read timed out",
            "Timeout waiting for connection",
            "Container killed: OOMKilled",
            "Permission denied",
            "3 tests failed",
        ] {
            assert!(pattern.is_match(line), "{line}");
        }
        for line in [
            "request.timeout.ms = 30000",
            "timeout=5000",
            "failover to replica",
            "zoom room booked",
            "INFO 处理完成",
        ] {
            assert!(!pattern.is_match(line), "{line}");
        }
    }

    #[test]
    fn short_text_is_left_alone() {
        assert_eq!(condense("一行", 10), ("一行".to_owned(), false));
    }
}
