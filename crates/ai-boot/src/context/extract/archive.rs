//! 压缩包：zip、tar、tar.gz、gz 解到工作目录里。模型没有 shell，客户打包发来的日志
//! 不解开它就看不到：解开的文件留给它用 Grep、Read 查，prompt 里放清单和最相关的几个
//! 文本文件（有报错的日志优先）。
//!
//! 压缩包是不可信输入：条目数、文件数、单个和总的解压量边解边数，不信头里写的大小，
//! 压缩比离谱的当炸弹停下；只解普通文件，绝对路径、`..`、链接、设备文件都跳过；里面
//! 再套的压缩包只列出来，不再展开；图片解到磁盘上，只列出来。

use std::cell::Cell;
use std::fs::{File, OpenOptions};
use std::io::{self, Cursor, Read, Write as _};
use std::path::{Path, PathBuf};

use encoding_rs::GB18030;
use flate2::read::MultiGzDecoder;
use zip::ZipArchive;

use super::{Output, TEXT_BYTES, blocking, clip_bytes, image, sanitize, text};

/// 最多看多少个条目、解出多少个文件。
const MAX_ENTRIES: usize = 500;
const MAX_FILES: usize = 50;
/// 单个文件、整个压缩包解压后的上限。
const ENTRY_LIMIT: u64 = 20 * 1024 * 1024;
const TOTAL_LIMIT: u64 = 100 * 1024 * 1024;
/// 解压量超过这么多、又超过压缩包本身这么多倍的当压缩炸弹：日志一般压到十分之一左右。
const BOMB_FLOOR: u64 = 8 * 1024 * 1024;
const BOMB_RATIO: u64 = 200;
/// 清单最多列几行；每个文本文件至少要分到多少字节才放进来。
const LISTED: usize = 60;
const MIN_SLICE: usize = 2 * 1024;
/// 每段文本前面「### 路径（说明）」一行，除路径外最多占的字节。
const HEADING_BYTES: usize = 200;

/// 能解开的格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Zip,
    Tar,
    /// gzip：里面是 tar 的当 tar.gz 解开，否则是单个压缩过的文件。
    Gzip,
}

/// tar 的头：第 257 字节起是 `ustar`（POSIX 和 GNU 格式都是）。
pub(super) fn is_tar(bytes: &[u8]) -> bool {
    bytes.get(257..262) == Some(b"ustar")
}

/// 解到原件旁边的 `<名字>.d/` 里。`name` 是聊天里的文件名，单个 gz 解出的文件用它
/// 去掉 `.gz` 命名。
pub(super) async fn extract(
    format: Format,
    path: &Path,
    name: &str,
    bytes: Vec<u8>,
    out_dir: &Path,
) -> Result<Output, String> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");
    let dir_name = format!("{}.d", stem(file_name));
    let dir = out_dir.join(&dir_name);
    let single = sanitize(stem(name));
    blocking(move || unpack(format, &bytes, &dir, &dir_name, &single)).await
}

/// 去掉压缩包的扩展名。
fn stem(file_name: &str) -> &str {
    let lower = file_name.to_ascii_lowercase();
    [".tar.gz", ".tgz", ".tar", ".gz", ".zip"]
        .iter()
        .find(|suffix| lower.len() > suffix.len() && lower.ends_with(*suffix))
        .map_or(file_name, |suffix| {
            &file_name[..file_name.len() - suffix.len()]
        })
}

fn unpack(
    format: Format,
    bytes: &[u8],
    dir: &Path,
    dir_name: &str,
    single: &str,
) -> Result<Output, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("创建解压目录失败：{err}"))?;
    let spent = Cell::new(0);
    let packed = bytes.len() as u64;
    let mut unpacker = Unpacker::new(dir, &spent, packed);
    match format {
        Format::Zip => unpacker.zip(bytes)?,
        Format::Tar => unpacker.tar(Meter::new(bytes, &spent, packed))?,
        Format::Gzip => {
            let mut stream = Meter::new(MultiGzDecoder::new(bytes), &spent, packed);
            let mut head = Vec::with_capacity(512);
            (&mut stream)
                .take(512)
                .read_to_end(&mut head)
                .map_err(|err| format!("压缩包无法读取：{err}"))?;
            let tar = is_tar(&head);
            let stream = Cursor::new(head).chain(stream);
            if tar {
                unpacker.tar(stream)?;
            } else {
                unpacker.scanned = 1;
                if let Err(reason) = unpacker.file(single, 0, stream) {
                    unpacker.stopped = Some(reason);
                }
            }
        }
    }
    unpacker.finish(dir_name)
}

/// 数解压出来的字节（跳过不读的也算，都要解压一遍），超过总量或压缩比离谱时报错，
/// 让读的一方停下。
struct Meter<'a, R> {
    inner: R,
    spent: &'a Cell<u64>,
    packed: u64,
}

impl<'a, R> Meter<'a, R> {
    fn new(inner: R, spent: &'a Cell<u64>, packed: u64) -> Self {
        Self {
            inner,
            spent,
            packed,
        }
    }
}

impl<R: Read> Read for Meter<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        let spent = self.spent.get() + n as u64;
        self.spent.set(spent);
        if over(spent, self.packed).is_some() {
            return Err(io::Error::other("解压量超出上限"));
        }
        Ok(n)
    }
}

/// 解压量超限的原因。
fn over(spent: u64, packed: u64) -> Option<String> {
    if spent > TOTAL_LIMIT {
        return Some(format!("解压后超过 {} MB，后面的没有解", TOTAL_LIMIT >> 20));
    }
    if spent > BOMB_FLOOR && spent / BOMB_RATIO > packed {
        return Some(format!(
            "压缩比超过 {BOMB_RATIO} 倍，疑似压缩炸弹，停止解压"
        ));
    }
    None
}

/// 解出来的一个文件。
struct Entry {
    /// 相对解压目录。
    path: PathBuf,
    size: u64,
    image: bool,
    /// 超过单个文件的上限，或者读到一半出错：只有前面一部分。
    partial: bool,
}

struct Unpacker<'a> {
    dir: &'a Path,
    spent: &'a Cell<u64>,
    packed: u64,
    scanned: usize,
    files: Vec<Entry>,
    /// 只列不解的：里面再套的压缩包（路径、头里写的大小）。
    nested: Vec<(PathBuf, u64)>,
    /// 跳过的：链接和设备文件、路径不安全的、清洗后重名的、读不出来的（加密、
    /// 不支持的压缩方式）、超过文件数上限的。
    special: usize,
    unsafe_paths: usize,
    duplicates: usize,
    failed: usize,
    beyond: usize,
    /// 为什么没有解完。
    stopped: Option<String>,
}

impl<'a> Unpacker<'a> {
    fn new(dir: &'a Path, spent: &'a Cell<u64>, packed: u64) -> Self {
        Self {
            dir,
            spent,
            packed,
            scanned: 0,
            files: Vec::new(),
            nested: Vec::new(),
            special: 0,
            unsafe_paths: 0,
            duplicates: 0,
            failed: 0,
            beyond: 0,
            stopped: None,
        }
    }

    /// 条目数到了上限就停下，返回是否还能接着看。
    fn next_entry(&mut self) -> bool {
        if self.scanned == MAX_ENTRIES {
            self.stopped = Some(format!("条目超过 {MAX_ENTRIES} 个，后面的没有看"));
            return false;
        }
        self.scanned += 1;
        true
    }

    fn zip(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|err| format!("压缩包无法打开：{err}"))?;
        for i in 0..archive.len() {
            if !self.next_entry() {
                break;
            }
            let Ok(file) = archive.by_index(i) else {
                self.failed += 1;
                continue;
            };
            if file.is_dir() {
                continue;
            }
            match file.unix_mode().map_or(0, |mode| mode & 0o170000) {
                0 | 0o100000 => {}
                0o040000 => continue,
                _ => {
                    self.special += 1;
                    continue;
                }
            }
            let name = entry_name(file.name_raw());
            let declared = file.size();
            let reader = Meter::new(file, self.spent, self.packed);
            if let Err(reason) = self.file(&name, declared, reader) {
                self.stopped = Some(reason);
                break;
            }
        }
        Ok(())
    }

    fn tar(&mut self, stream: impl Read) -> Result<(), String> {
        let mut archive = tar::Archive::new(stream);
        let entries = archive
            .entries()
            .map_err(|err| format!("压缩包无法读取：{err}"))?;
        for entry in entries {
            if !self.next_entry() {
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    self.stopped = Some(
                        over(self.spent.get(), self.packed)
                            .unwrap_or_else(|| format!("压缩包读到一半出错（{err}）")),
                    );
                    break;
                }
            };
            let kind = entry.header().entry_type();
            if kind.is_dir() || kind.is_pax_global_extensions() {
                continue;
            }
            if !kind.is_file() && !kind.is_contiguous() {
                self.special += 1;
                continue;
            }
            let name = entry_name(&entry.path_bytes());
            let declared = entry.size();
            if let Err(reason) = self.file(&name, declared, entry) {
                self.stopped = Some(reason);
                break;
            }
        }
        Ok(())
    }

    /// 解一个普通文件。返回 `Err` 表示整个压缩包都要停下（解压量超了上限）。
    fn file(&mut self, name: &str, declared: u64, mut reader: impl Read) -> Result<(), String> {
        // macOS 打包时附带的资源文件：占名额，没有内容
        if name.starts_with("__MACOSX/") || name.rsplit('/').next() == Some(".DS_Store") {
            return Ok(());
        }
        if self.files.len() == MAX_FILES {
            self.beyond += 1;
            return Ok(());
        }
        let Some(path) = safe_path(name) else {
            self.unsafe_paths += 1;
            return Ok(());
        };
        // 先读开头判断类型
        let mut head = Vec::with_capacity(512);
        if let Err(err) = (&mut reader).take(512).read_to_end(&mut head) {
            self.failed += 1;
            return self.check(&err);
        }
        if nested(name, &head) {
            self.nested.push((path, declared));
            return Ok(());
        }
        let target = self.dir.join(&path);
        if let Some(parent) = target.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            // 前面有个同名的文件占了目录的位置
            self.failed += 1;
            return Ok(());
        }
        // 只新建不覆盖：清洗后重名的不会互相覆盖，也不会顺着已有的链接写出去
        let mut out = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(out) => out,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                self.duplicates += 1;
                return Ok(());
            }
            Err(_) => {
                self.failed += 1;
                return Ok(());
            }
        };
        let mut entry = Entry {
            path,
            size: 0,
            image: image::is_image(&head),
            partial: false,
        };
        let copied = copy(&head, &mut reader, &mut out, &mut entry);
        entry.partial |= copied.is_err();
        self.files.push(entry);
        match copied {
            Ok(()) => Ok(()),
            Err(err) => self.check(&err),
        }
    }

    /// 读出错时：超了上限就整个停下，否则只是这个条目坏了（校验和不对之类），接着解。
    fn check(&self, err: &io::Error) -> Result<(), String> {
        match over(self.spent.get(), self.packed) {
            Some(reason) => Err(reason),
            None => {
                tracing::debug!(%err, "压缩包里的条目读取失败");
                Ok(())
            }
        }
    }

    fn finish(self, dir_name: &str) -> Result<Output, String> {
        let skipped = self.skipped();
        if self.files.is_empty() && self.nested.is_empty() {
            let mut reasons: Vec<String> = self.stopped.into_iter().collect();
            reasons.extend(skipped);
            if reasons.is_empty() {
                reasons.push("压缩包是空的".to_owned());
            }
            return Err(format!("没有解出文件：{}", reasons.join("；")));
        }
        let mut manifest = format!("解出 {} 个文件：\n", self.files.len());
        for entry in self.files.iter().take(LISTED) {
            let mut marks = String::new();
            if entry.image {
                marks.push_str("，图片");
            }
            if entry.partial {
                marks.push_str("，只解出了前面一部分");
            }
            manifest.push_str(&format!(
                "- {}（{} 字节{marks}）\n",
                entry.path.display(),
                entry.size
            ));
        }
        let room = LISTED.saturating_sub(self.files.len());
        for (path, size) in self.nested.iter().take(room) {
            manifest.push_str(&format!(
                "- {}（{size} 字节，压缩包，没有再展开）\n",
                path.display()
            ));
        }
        let unlisted = (self.files.len() + self.nested.len()).saturating_sub(LISTED);
        if unlisted > 0 {
            manifest.push_str(&format!("- …另有 {unlisted} 个没有列出\n"));
        }
        if let Some(skipped) = skipped {
            manifest.push_str(&format!("{skipped}\n"));
        }
        let (mut out, _) = clip_bytes(&manifest, TEXT_BYTES / 2);
        let condensed = self.texts(&mut out);

        let mut notes = vec![format!(
            "压缩包解到了原件旁边的 {dir_name}/ 目录里，清单里的路径相对这个目录，完整内容在那里"
        )];
        notes.extend(self.stopped);
        if self.beyond > 0 {
            notes.push(format!(
                "只解了前 {MAX_FILES} 个文件，另有 {} 个没有解",
                self.beyond
            ));
        }
        if condensed {
            notes.push("较长的文本只保留了开头、结尾和出错的行".to_owned());
        }
        Ok(Output {
            text: out,
            note: Some(notes.join("；")),
            ..Output::default()
        })
    }

    /// 跳过了哪些条目，一句话。
    fn skipped(&self) -> Option<String> {
        let parts: Vec<String> = [
            (self.special, "链接或设备文件"),
            (self.unsafe_paths, "路径不安全（绝对路径或带 ..）的条目"),
            (self.duplicates, "改名后重名的文件"),
            (self.failed, "读不出来的条目（加密或不支持的压缩方式）"),
        ]
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} 个{what}"))
        .collect();
        (!parts.is_empty()).then(|| format!("跳过了 {}", parts.join("、")))
    }

    /// 文本文件接在清单后面，共用剩下的预算：有报错的在前，排前面的分得多。返回有没有
    /// 压缩过的。
    fn texts(&self, out: &mut String) -> bool {
        // 先挑出是文本的，看有没有报错；内容不留着，免得几十个文件都压在内存里
        let mut candidates: Vec<(usize, bool)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, entry)| !entry.image)
            .filter_map(|(i, entry)| {
                let decoded = self.decode(entry)?;
                Some((i, text::has_errors(&decoded.text)))
            })
            .collect();
        candidates.sort_by_key(|&(_, errors)| !errors);
        let mut condensed = false;
        for (n, &(i, _)) in candidates.iter().enumerate() {
            let entry = &self.files[i];
            let left = TEXT_BYTES.saturating_sub(out.len());
            let share = if n + 1 < candidates.len() {
                left / 2
            } else {
                left
            };
            let overhead = entry.path.as_os_str().len() + HEADING_BYTES;
            if share < overhead + MIN_SLICE {
                break;
            }
            let Some(decoded) = self.decode(entry) else {
                continue;
            };
            let (body, cut) = text::condense(&decoded.text, share - overhead);
            condensed |= cut;
            let mut notes = text::describe(&decoded);
            if cut {
                notes.push("只保留了开头、结尾和出错的行".to_owned());
            }
            if entry.partial {
                notes.push("只解出了前面一部分".to_owned());
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!("（{}）", notes.join("；"))
            };
            out.push_str(&format!("\n### {}{notes}\n", entry.path.display()));
            out.push_str(body.trim_end());
            out.push('\n');
        }
        condensed
    }

    fn decode(&self, entry: &Entry) -> Option<text::Decoded> {
        let bytes = std::fs::read(self.dir.join(&entry.path)).ok()?;
        text::decode(&bytes)
    }
}

/// 写进文件，单个文件超过上限的只留前面。
fn copy(head: &[u8], reader: &mut impl Read, out: &mut File, entry: &mut Entry) -> io::Result<()> {
    if !write_capped(out, head, entry)? {
        return Ok(());
    }
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 || !write_capped(out, &buf[..n], entry)? {
            return Ok(());
        }
    }
}

/// 写到单个文件的上限为止，返回还要不要接着写。
fn write_capped(out: &mut File, data: &[u8], entry: &mut Entry) -> io::Result<bool> {
    let room = usize::try_from(ENTRY_LIMIT - entry.size).unwrap_or(usize::MAX);
    let take = data.len().min(room);
    out.write_all(&data[..take])?;
    entry.size += take as u64;
    entry.partial |= take < data.len();
    Ok(take == data.len())
}

/// 条目名：UTF-8 解不开的按 GBK 解（中文 Windows 自带的压缩就是这样）。
fn entry_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(name) => name.to_owned(),
        Err(_) => GB18030.decode_without_bom_handling(raw).0.into_owned(),
    }
}

/// 条目名换成解压目录里的相对路径，每一段按文件名清洗；绝对路径、带 `..` 的返回 `None`。
fn safe_path(name: &str) -> Option<PathBuf> {
    if name.starts_with(['/', '\\']) || name.get(1..2) == Some(":") {
        return None;
    }
    let mut path = PathBuf::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return None,
            part => path.push(sanitize(part)),
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

/// 里面再套的压缩包：只列出来，不再展开。zip 的魔数不算（docx、xlsx 也是 zip），
/// 按扩展名认。
fn nested(name: &str, head: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    let by_name = [
        ".zip", ".gz", ".tgz", ".tar", ".7z", ".rar", ".bz2", ".xz", ".jar", ".war",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext));
    let by_content = head.starts_with(&[0x1F, 0x8B])
        || head.starts_with(b"7z\xBC\xAF\x27\x1C")
        || head.starts_with(b"Rar!\x1A\x07")
        || head.starts_with(&[0xFD, b'7', b'z', b'X', b'Z', 0x00])
        || is_tar(head);
    by_name || by_content
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use zip::write::SimpleFileOptions;

    use super::super::office::tests::zip_bytes;
    use super::*;

    fn png() -> Vec<u8> {
        let image = ::image::DynamicImage::ImageRgb8(::image::RgbImage::new(8, 8));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), ::image::ImageFormat::Png)
            .expect("编码");
        out
    }

    fn log(lines: usize, error_at: Option<usize>) -> String {
        (0..lines)
            .map(|i| match error_at {
                Some(at) if at == i => {
                    "2026-09-28 10:00:00 ERROR 调用支付网关失败，连接超时\n".to_owned()
                }
                _ => format!("2026-09-28 10:00:00 INFO 第 {i} 行 正常\n"),
            })
            .collect()
    }

    async fn unpack_file(dir: &Path, name: &str, bytes: Vec<u8>) -> Result<Output, String> {
        let saved = dir.join(format!("03-{name}"));
        std::fs::write(&saved, &bytes).expect("写原件");
        let format = match super::super::detect(name, &bytes) {
            super::super::Kind::Archive(format) => format,
            other => panic!("{name} 认成了 {other:?}"),
        };
        extract(format, &saved, name, bytes, dir).await
    }

    #[tokio::test]
    async fn a_zip_is_unpacked_with_a_manifest_and_the_error_log_first() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (app, error, shot) = (log(50, None), log(50, Some(20)), png());
        let bytes = zip_bytes(&[
            ("logs/app.log", app.as_bytes()),
            ("logs/error.log", error.as_bytes()),
            ("shots/报错.png", &shot),
            ("inner.tar.gz", b"\x1f\x8b\x08\x00"),
            ("../evil.txt", b"escape"),
            ("/abs.txt", b"escape"),
            ("__MACOSX/logs/._app.log", b"junk"),
        ]);
        let output = unpack_file(dir.path(), "logs.zip", bytes)
            .await
            .expect("解开");
        let unpacked = dir.path().join("03-logs.d");
        assert_eq!(
            std::fs::read_to_string(unpacked.join("logs/app.log")).expect("解出的文件"),
            app
        );
        assert!(unpacked.join("shots/报错.png").exists(), "图片也解到磁盘上");
        assert!(!unpacked.join("inner.tar.gz").exists(), "套着的压缩包不解");
        assert!(!dir.path().join("evil.txt").exists() && !unpacked.join("evil.txt").exists());
        assert!(!unpacked.join("__MACOSX").exists());

        let text = &output.text;
        assert!(text.starts_with("解出 3 个文件"), "{text}");
        assert!(text.contains("- logs/app.log（"), "{text}");
        assert!(
            text.contains("- shots/报错.png（") && text.contains("，图片）"),
            "{text}"
        );
        assert!(
            text.contains("inner.tar.gz（4 字节，压缩包，没有再展开）"),
            "{text}"
        );
        assert!(text.contains("2 个路径不安全"), "{text}");
        let error_log = text.find("### logs/error.log").expect("有报错的日志放进来");
        let app_log = text.find("### logs/app.log").expect("其他日志也放进来");
        assert!(error_log < app_log, "有报错的在前：{text}");
        assert!(text.contains("调用支付网关失败"));
        assert!(output.images.is_empty(), "压缩包里的图片只列出来");
        let note = output.note.expect("说明");
        assert!(note.contains("03-logs.d/"), "{note}");
        assert!(output.text.len() <= TEXT_BYTES);
    }

    fn tar_of(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        build(&mut builder);
        builder.into_inner().expect("完成")
    }

    fn append(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_path(name).expect("路径");
        header.set_cksum();
        builder.append(&header, data).expect("写条目");
    }

    /// 直接写头里的名字：tar 库自己不让写绝对路径和 `..`。
    fn append_raw(builder: &mut tar::Builder<Vec<u8>>, name: &str, kind: tar::EntryType) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_size(0);
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        if kind != tar::EntryType::Regular {
            header.set_link_name("/etc/passwd").expect("链接");
        }
        header.set_cksum();
        builder.append(&header, io::empty()).expect("写条目");
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).expect("压缩");
        encoder.finish().expect("完成")
    }

    #[tokio::test]
    async fn tar_and_tar_gz_skip_links_devices_and_escaping_paths() {
        let tarball = tar_of(|builder| {
            append(builder, "app/app.log", log(30, Some(3)).as_bytes());
            append_raw(builder, "app/passwd", tar::EntryType::Symlink);
            append_raw(builder, "app/hard", tar::EntryType::Link);
            append_raw(builder, "app/tty", tar::EntryType::Char);
            append_raw(builder, "../../escape.log", tar::EntryType::Regular);
            append_raw(builder, "/etc/cron.d/x", tar::EntryType::Regular);
        });
        for (name, bytes) in [
            ("bundle.tar", tarball.clone()),
            ("bundle.tar.gz", gzip(&tarball)),
            ("bundle.tgz", gzip(&tarball)),
        ] {
            let dir = tempfile::tempdir().expect("临时目录");
            let output = unpack_file(dir.path(), name, bytes).await.expect("解开");
            let unpacked = dir.path().join("03-bundle.d");
            assert!(unpacked.join("app/app.log").exists(), "{name}");
            for skipped in ["app/passwd", "app/hard", "app/tty"] {
                assert!(!unpacked.join(skipped).exists(), "{name}：{skipped}");
            }
            assert!(!dir.path().join("escape.log").exists());
            assert!(
                output.text.starts_with("解出 1 个文件"),
                "{name}：{}",
                output.text
            );
            assert!(
                output.text.contains("3 个链接或设备文件"),
                "{}",
                output.text
            );
            assert!(output.text.contains("2 个路径不安全"), "{}", output.text);
            assert!(output.text.contains("调用支付网关失败"), "{}", output.text);
        }
    }

    #[tokio::test]
    async fn a_single_gz_file_is_decompressed_next_to_the_original() {
        let dir = tempfile::tempdir().expect("临时目录");
        let content = log(5000, Some(4000));
        let output = unpack_file(dir.path(), "app.log.gz", gzip(content.as_bytes()))
            .await
            .expect("解开");
        let unpacked = dir.path().join("03-app.log.d/app.log");
        assert_eq!(std::fs::read_to_string(unpacked).expect("解出"), content);
        assert!(
            output
                .text
                .contains("### app.log（只保留了开头、结尾和出错的行）"),
            "{}",
            output.text
        );
        assert!(output.text.contains("调用支付网关失败"));
        assert!(output.text.contains("第 4999 行"), "结尾");
        assert!(output.text.len() <= TEXT_BYTES);
    }

    #[tokio::test]
    async fn file_and_entry_limits_hold_and_names_do_not_collide() {
        let dir = tempfile::tempdir().expect("临时目录");
        let names: Vec<String> = (0..60).map(|i| format!("logs/{i:02}.log")).collect();
        let mut entries: Vec<(&str, &[u8])> = names
            .iter()
            .map(|n| (n.as_str(), b"line\n".as_slice()))
            .collect();
        entries.insert(0, ("a b.log", b"first"));
        entries.insert(1, ("a_b.log", b"second"));
        let output = unpack_file(dir.path(), "many.zip", zip_bytes(&entries))
            .await
            .expect("解开");
        let unpacked = dir.path().join("03-many.d");
        assert_eq!(
            std::fs::read_to_string(unpacked.join("a_b.log")).expect("先到的"),
            "first"
        );
        assert!(
            output.text.contains("1 个改名后重名的文件"),
            "{}",
            output.text
        );
        assert!(output.text.starts_with("解出 50 个文件"));
        assert!(output.note.is_some_and(|n| n.contains("另有 11 个没有解")));

        let dir = tempfile::tempdir().expect("临时目录");
        let names: Vec<String> = (0..520).map(|i| format!("d{i}/")).collect();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("first.log", SimpleFileOptions::default())
            .expect("条目");
        writer.write_all(b"x").expect("写");
        for name in &names {
            writer
                .add_directory(name.as_str(), SimpleFileOptions::default())
                .expect("目录");
        }
        let bytes = writer.finish().expect("完成").into_inner();
        let output = unpack_file(dir.path(), "dirs.zip", bytes)
            .await
            .expect("解开");
        assert!(output.note.is_some_and(|n| n.contains("条目超过 500 个")));
    }

    #[tokio::test]
    async fn big_entries_are_cut_and_bombs_are_stopped_while_streaming() {
        // 不压缩的 21 MiB：只解出前 20 MiB
        let dir = tempfile::tempdir().expect("临时目录");
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("big.bin", stored).expect("条目");
        writer.write_all(&vec![7_u8; 21 << 20]).expect("写");
        let bytes = writer.finish().expect("完成").into_inner();
        let output = unpack_file(dir.path(), "big.zip", bytes)
            .await
            .expect("解开");
        let size = std::fs::metadata(dir.path().join("03-big.d/big.bin"))
            .expect("解出")
            .len();
        assert_eq!(size, ENTRY_LIMIT);
        assert!(
            output.text.contains("只解出了前面一部分"),
            "{}",
            output.text
        );

        // 16 MiB 的 0 压成十几 KB：压缩比离谱，边解边发现，停在 8 MiB
        let dir = tempfile::tempdir().expect("临时目录");
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&vec![0_u8; 16 << 20]).expect("压缩");
        let bomb = encoder.finish().expect("完成");
        let output = unpack_file(dir.path(), "zeros.gz", bomb)
            .await
            .expect("停下也有结果");
        let size = std::fs::metadata(dir.path().join("03-zeros.d/zeros"))
            .expect("解出")
            .len();
        assert!(size <= BOMB_FLOOR, "{size}");
        assert!(output.note.is_some_and(|n| n.contains("疑似压缩炸弹")));
    }

    #[test]
    fn the_meter_counts_what_is_skipped_too() {
        let spent = Cell::new(0);
        let mut meter = Meter::new(io::repeat(1).take(TOTAL_LIMIT + 1), &spent, u64::MAX);
        assert!(io::copy(&mut meter, &mut io::sink()).is_err());
        assert!(over(spent.get(), u64::MAX).is_some_and(|r| r.contains("100 MB")));
        assert_eq!(over(10 << 20, 1 << 20), None, "十倍的压缩比是正常的");
    }

    #[test]
    fn entry_paths_are_cleaned_and_confined() {
        assert_eq!(
            safe_path("logs/a b.log"),
            Some(PathBuf::from("logs/a_b.log"))
        );
        assert_eq!(
            safe_path("win\\dir\\x.log"),
            Some(PathBuf::from("win/dir/x.log"))
        );
        assert_eq!(safe_path("./a/./b"), Some(PathBuf::from("a/b")));
        for bad in [
            "../x",
            "a/../../x",
            "/etc/passwd",
            "\\\\host\\share",
            "C:\\x",
            "",
        ] {
            assert_eq!(safe_path(bad), None, "{bad}");
        }
        let (gbk, _, _) = GB18030.encode("日志/错误.log");
        assert_eq!(entry_name(&gbk), "日志/错误.log");
        assert_eq!(stem("03-logs.tar.gz"), "03-logs");
        assert_eq!(stem("03-app.log.GZ"), "03-app.log");
        assert_eq!(stem(".zip"), ".zip");
    }
}
