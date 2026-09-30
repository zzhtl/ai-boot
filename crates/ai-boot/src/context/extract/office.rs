//! Office 文档：docx、pptx 是 zip 里的 XML，逐事件读出文字；doc、ppt、odt、odp
//! 先用 soffice 转成新格式再读。
//!
//! 原件是 zip，模型读不了：全文另存一份纯文本给它 Grep，嵌在里面的截图按图片交给它。

use std::io::{Cursor, Read as _};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};
use tokio::process::Command;
use zip::ZipArchive;

use super::archive::Format;
use super::image::{WORKERS, normalize_tiled};
use super::{Kind, Output, TEXT_BYTES, Tools, XML_LIMIT, blocking, clip_bytes, run, sanitize};

const MAX_SLIDES: usize = 100;
/// 每份文档最多交给模型几张图（长截图切出的段也算）：每轮一共才 20 张。
const DOC_IMAGES: usize = 10;
/// 另存的全文上限，更长的只存前面。
const FULL_TEXT: usize = 8 * 1024 * 1024;
const SOFFICE_TIMEOUT: Duration = Duration::from_secs(90);

/// soffice 同一个配置目录不能并发使用，转换一个一个来。
static SOFFICE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// zip 容器里装的是什么。
pub(super) fn zip_kind(bytes: &[u8]) -> Kind {
    let Ok(mut archive) = ZipArchive::new(Cursor::new(bytes)) else {
        return Kind::Binary;
    };
    if archive.index_for_name("word/document.xml").is_some() {
        return Kind::Docx;
    }
    if archive.index_for_name("ppt/presentation.xml").is_some() {
        return Kind::Pptx;
    }
    if archive.index_for_name("xl/workbook.xml").is_some() {
        return Kind::Sheet;
    }
    // OpenDocument 的第一个条目 mimetype 写着类型
    let mut mimetype = String::new();
    if let Ok(file) = archive.by_name("mimetype") {
        let _ = file.take(128).read_to_string(&mut mimetype);
    }
    match mimetype.trim() {
        "application/vnd.oasis.opendocument.spreadsheet" => Kind::Sheet,
        "application/vnd.oasis.opendocument.text" => Kind::Legacy("docx"),
        "application/vnd.oasis.opendocument.presentation" => Kind::Legacy("pptx"),
        _ => Kind::Archive(Format::Zip),
    }
}

fn read_entry(archive: &mut ZipArchive<Cursor<&[u8]>>, name: &str) -> Result<Vec<u8>, String> {
    let file = archive
        .by_name(name)
        .map_err(|err| format!("文档里缺少 {name}：{err}"))?;
    if file.size() > XML_LIMIT {
        return Err("文档解压后过大".to_owned());
    }
    let mut data = Vec::new();
    file.take(XML_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|err| format!("文档解压失败：{err}"))?;
    if data.len() as u64 > XML_LIMIT {
        return Err("文档解压后过大".to_owned());
    }
    Ok(data)
}

fn open(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, String> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|err| format!("文档无法打开：{err}"))
}

/// XML 里的实体引用（quick-xml 把它们单独报出来）。
fn entity(reference: &BytesRef<'_>) -> String {
    if let Ok(Some(c)) = reference.resolve_char_ref() {
        return c.to_string();
    }
    match reference.decode().as_deref() {
        Ok("amp") => "&",
        Ok("lt") => "<",
        Ok("gt") => ">",
        Ok("quot") => "\"",
        Ok("apos") => "'",
        _ => "",
    }
    .to_owned()
}

/// `<w:pStyle w:val="Heading2"/>` 这类标题样式的级别。
fn heading_level(element: &BytesStart<'_>) -> Option<usize> {
    let value = element
        .attributes()
        .filter_map(Result::ok)
        .find(|a| a.key.as_ref() == b"w:val")?
        .value;
    let value = std::str::from_utf8(&value).ok()?;
    if value.eq_ignore_ascii_case("Title") {
        return Some(1);
    }
    let level = value.strip_prefix("Heading")?.parse::<usize>().ok()?;
    (1..=6).contains(&level).then_some(level)
}

/// 解析出的文档：全文，和嵌在里面的图片在 zip 里的条目名。
pub(super) struct Document {
    pub text: String,
    pub media: Vec<String>,
    pub note: Option<String>,
}

pub(super) fn docx(bytes: &[u8]) -> Result<Document, String> {
    let mut archive = open(bytes)?;
    let xml = read_entry(&mut archive, "word/document.xml")?;
    let mut reader = Reader::from_reader(xml.as_slice());
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut paragraph = String::new();
    let mut heading = None;
    let mut in_text = false;
    let mut table_depth = 0_usize;
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|err| format!("文档结构有误：{err}"))?;
        match event {
            Event::Start(e) => match e.name().as_ref() {
                b"w:t" => in_text = true,
                b"w:p" => {
                    paragraph.clear();
                    heading = None;
                }
                b"w:tbl" => table_depth += 1,
                b"w:tr" if table_depth == 1 => row.clear(),
                b"w:tc" if table_depth == 1 => cell.clear(),
                _ => {}
            },
            Event::Empty(e) => match e.name().as_ref() {
                b"w:tab" => paragraph.push('\t'),
                b"w:br" | b"w:cr" => paragraph.push('\n'),
                b"w:pStyle" => heading = heading_level(&e),
                _ => {}
            },
            Event::Text(e) if in_text => {
                paragraph.push_str(&e.decode().map_err(|err| format!("文字编码有误：{err}"))?);
            }
            Event::GeneralRef(e) if in_text => paragraph.push_str(&entity(&e)),
            Event::End(e) => match e.name().as_ref() {
                b"w:t" => in_text = false,
                b"w:p" => {
                    let text = paragraph.trim_end();
                    if table_depth > 0 {
                        if !cell.is_empty() && !text.is_empty() {
                            cell.push(' ');
                        }
                        cell.push_str(text);
                    } else if !text.is_empty() {
                        if let Some(level) = heading {
                            out.push_str(&"#".repeat(level));
                            out.push(' ');
                        }
                        out.push_str(text);
                        out.push('\n');
                    }
                    paragraph.clear();
                }
                b"w:tc" if table_depth == 1 => {
                    row.push(cell.split_whitespace().collect::<Vec<_>>().join(" "));
                }
                b"w:tr" if table_depth == 1 => {
                    out.push_str(&format!("| {} |\n", row.join(" | ")));
                }
                b"w:tbl" => table_depth = table_depth.saturating_sub(1),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
        if out.len() > FULL_TEXT {
            break;
        }
    }
    Ok(Document {
        note: truncated(&out),
        text: out,
        media: media(&archive, "word/media/"),
    })
}

pub(super) fn pptx(bytes: &[u8]) -> Result<Document, String> {
    let mut archive = open(bytes)?;
    let mut slides: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(|name| {
            let index = name
                .strip_prefix("ppt/slides/slide")?
                .strip_suffix(".xml")?
                .parse()
                .ok()?;
            Some((index, name.to_owned()))
        })
        .collect();
    slides.sort();
    let mut out = String::new();
    for (index, name) in slides.iter().take(MAX_SLIDES) {
        let xml = read_entry(&mut archive, name)?;
        let text = slide_text(&xml)?;
        if !text.is_empty() {
            out.push_str(&format!("## 第 {index} 页\n{text}\n"));
        }
        if out.len() > FULL_TEXT {
            break;
        }
    }
    let note = if slides.len() > MAX_SLIDES {
        Some(format!("只读了前 {MAX_SLIDES} 页"))
    } else {
        truncated(&out)
    };
    Ok(Document {
        note,
        text: out,
        media: media(&archive, "ppt/media/"),
    })
}

fn truncated(text: &str) -> Option<String> {
    (text.len() > FULL_TEXT).then(|| format!("文档很长，只读了前 {} MB 的文字", FULL_TEXT >> 20))
}

/// 文档里嵌的图片：只要模型能看的格式，emf、wmf 这类矢量图跳过。按编号排（image2 在
/// image10 前面），大致就是在文档里出现的顺序。
fn media(archive: &ZipArchive<Cursor<&[u8]>>, prefix: &str) -> Vec<String> {
    let mut found: Vec<(u32, String)> = archive
        .file_names()
        .filter_map(|name| {
            let (stem, ext) = name.strip_prefix(prefix)?.rsplit_once('.')?;
            let wanted = matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp"
            );
            let number = stem
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .unwrap_or(u32::MAX);
            wanted.then(|| (number, name.to_owned()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, name)| name).collect()
}

/// docx、pptx：解析，全文存到原件旁边，嵌入的图片按截图处理。
pub(super) async fn read(
    path: &Path,
    format: &'static str,
    bytes: Vec<u8>,
    out_dir: &Path,
) -> Result<Output, String> {
    let bytes = Arc::new(bytes);
    let parsed = Arc::clone(&bytes);
    let document = match format {
        "pptx" => blocking(move || pptx(&parsed)).await?,
        _ => blocking(move || docx(&parsed)).await?,
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("document");
    let mut notes: Vec<String> = document.note.into_iter().collect();
    let saved = if document.text.trim().is_empty() {
        None
    } else {
        let saved = path.with_file_name(format!("{name}.txt"));
        tokio::fs::write(&saved, document.text.as_bytes())
            .await
            .map_err(|err| format!("保存失败：{err}"))?;
        Some(saved)
    };
    let (text, cut) = clip_bytes(document.text.trim(), TEXT_BYTES);
    if cut {
        notes.push("内容较长，只放了前面一部分，完整内容见原件".to_owned());
    }
    let mut images = Vec::new();
    let mut failed = 0;
    for entry in &document.media {
        if images.len() >= DOC_IMAGES {
            notes.push(format!(
                "文档里有 {} 张图片，只取了前面的 {DOC_IMAGES} 张",
                document.media.len()
            ));
            break;
        }
        // 文件名跟着文档里的条目名走（image2.png → 原件名-image2.webp），对得上号
        let stem = sanitize(
            Path::new(entry.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("image"),
        );
        let (bytes, entry) = (Arc::clone(&bytes), entry.clone());
        // 和聊天里的图共用解码的并发：一张大图解码后要几十 MB 内存
        let tiles = match WORKERS.acquire().await {
            Ok(_permit) => {
                blocking(move || normalize_tiled(read_entry(&mut open(&bytes)?, &entry)?)).await
            }
            Err(err) => Err(format!("处理图片失败：{err}")),
        };
        let Ok(tiles) = tiles else {
            failed += 1;
            continue;
        };
        let single = tiles.len() == 1;
        for (t, (data, ext)) in tiles.into_iter().enumerate() {
            if images.len() >= DOC_IMAGES {
                break;
            }
            let file = if single {
                format!("{name}-{stem}.{ext}")
            } else {
                format!("{name}-{stem}-{}.{ext}", t + 1)
            };
            let image = out_dir.join(file);
            tokio::fs::write(&image, &data)
                .await
                .map_err(|err| format!("保存失败：{err}"))?;
            images.push(image);
        }
    }
    if failed > 0 {
        notes.push(format!("{failed} 张图片无法处理"));
    }
    Ok(Output {
        text,
        images,
        note: (!notes.is_empty()).then(|| notes.join("；")),
        saved,
    })
}

fn slide_text(xml: &[u8]) -> Result<String, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut in_text = false;
    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|err| format!("幻灯片结构有误：{err}"))?;
        match event {
            Event::Start(e) if e.name().as_ref() == b"a:t" => in_text = true,
            Event::Text(e) if in_text => {
                line.push_str(&e.decode().map_err(|err| format!("文字编码有误：{err}"))?);
            }
            Event::GeneralRef(e) if in_text => line.push_str(&entity(&e)),
            Event::End(e) => match e.name().as_ref() {
                b"a:t" => in_text = false,
                b"a:p" => {
                    if !line.trim().is_empty() {
                        lines.push(line.trim().to_owned());
                    }
                    line.clear();
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(lines.join("\n"))
}

/// 旧版 Office 与 OpenDocument：soffice 转成 docx/pptx 再读。
pub(super) async fn legacy(
    path: &Path,
    target: &'static str,
    out_dir: &Path,
    tools: &Tools,
) -> Result<Output, String> {
    // 锁只管转换：之后处理嵌入的图片还要排队等解码的名额，不该占着 soffice
    let result = {
        let _serial = SOFFICE.lock().await;
        let soffice_dir = tools
            .scratch
            .join(format!("soffice-{}", uuid::Uuid::now_v7()));
        let result = convert(path, target, &soffice_dir, tools).await;
        if let Err(err) = tokio::fs::remove_dir_all(&soffice_dir).await
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(%err, dir = %soffice_dir.display(), "清理转换目录失败");
        }
        result
    };
    let mut output = read(path, target, result?, out_dir).await?;
    let converted = "由 soffice 转换后读取".to_owned();
    output.note = Some(match output.note {
        Some(note) => format!("{converted}；{note}"),
        None => converted,
    });
    Ok(output)
}

async fn convert(
    path: &Path,
    target: &str,
    out_dir: &Path,
    tools: &Tools,
) -> Result<Vec<u8>, String> {
    tokio::fs::create_dir_all(out_dir)
        .await
        .map_err(|err| format!("创建转换目录失败：{err}"))?;
    let profile = tools.scratch.join("soffice-profile");
    let profile = url::Url::from_directory_path(&profile)
        .map_err(|()| format!("soffice 配置目录必须是绝对路径：{}", profile.display()))?;
    let mut command = Command::new("soffice");
    command
        .arg(format!("-env:UserInstallation={profile}"))
        .args([
            "--headless",
            "--norestore",
            "--nologo",
            "--nodefault",
            "--nolockcheck",
            "--convert-to",
            target,
            "--outdir",
        ])
        .arg(out_dir)
        .arg(path);
    run(command, &tools.scratch, SOFFICE_TIMEOUT, 64 * 1024).await?;
    let stem = path.file_stem().ok_or_else(|| "文件名为空".to_owned())?;
    let converted = out_dir.join(stem).with_extension(target);
    tokio::fs::read(&converted)
        .await
        .map_err(|err| format!("soffice 没有生成 {target}：{err}"))
}

#[cfg(test)]
pub(super) mod tests {
    use std::io::Write as _;

    use super::*;

    /// 把若干个文件打成 zip。
    pub fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        let entries: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(name, content)| (*name, content.as_bytes()))
            .collect();
        zip_bytes(&entries)
    }

    pub fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            writer.start_file(*name, options).expect("写条目");
            writer.write_all(content).expect("写内容");
        }
        writer.finish().expect("完成").into_inner()
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 9])
            }));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("编码");
        out
    }

    pub const DOCUMENT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>
<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>故障复盘</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">根因：连接池 </w:t></w:r><w:r><w:t>耗尽 &amp; 未告警</w:t></w:r></w:p>
<w:tbl>
<w:tr><w:tc><w:p><w:r><w:t>版本</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>现象</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:p><w:r><w:t>3.2.1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>登录 500</w:t></w:r></w:p><w:p><w:r><w:t>偶发</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl>
<w:p><w:r><w:t>结论</w:t></w:r><w:r><w:tab/><w:t>扩容</w:t></w:r></w:p>
</w:body></w:document>"#;

    #[test]
    fn docx_keeps_headings_paragraphs_and_tables() {
        let bytes = zip_of(&[("word/document.xml", DOCUMENT)]);
        assert_eq!(zip_kind(&bytes), Kind::Docx);
        let output = docx(&bytes).expect("解析");
        assert_eq!(
            output.text.trim(),
            "# 故障复盘\n根因：连接池 耗尽 & 未告警\n| 版本 | 现象 |\n| 3.2.1 | 登录 500 偶发 |\n结论\t扩容"
        );
    }

    /// 全文另存成原件旁边的 .txt（`saved` 指向它），嵌入的截图交给模型，矢量图跳过。
    #[tokio::test]
    async fn docx_full_text_is_saved_and_embedded_images_are_kept() {
        let dir = tempfile::tempdir().expect("临时目录");
        let paragraphs: String = (0..3000)
            .map(|i| format!("<w:p><w:r><w:t>第 {i} 段：排查记录，连接池耗尽</w:t></w:r></w:p>"))
            .collect();
        let document = DOCUMENT.replace("<w:body>", &format!("<w:body>{paragraphs}"));
        let (shot, logo) = (png(400, 300), png(64, 64));
        let bytes = zip_bytes(&[
            ("word/document.xml", document.as_bytes()),
            ("word/media/image10.png", &logo),
            ("word/media/image2.png", &shot),
            ("word/media/image3.emf", b"vector"),
            ("word/media/image4.png", b"broken"),
        ]);
        let path = dir.path().join("03-复盘.docx");
        std::fs::write(&path, &bytes).expect("写原件");
        let output = read(&path, "docx", bytes, dir.path()).await.expect("解析");

        let saved = output.saved.as_ref().expect("另存了全文");
        assert_eq!(saved, &dir.path().join("03-复盘.docx.txt"));
        let full = std::fs::read_to_string(saved).expect("全文");
        assert!(full.contains("第 2999 段") && full.contains("结论\t扩容"));
        assert!(output.text.len() <= TEXT_BYTES);
        assert!(
            !output.text.contains("第 2999 段"),
            "prompt 里只放前面一部分"
        );
        let note = output.note.as_deref().unwrap_or_default();
        assert!(note.contains("只放了前面一部分"), "{note}");
        assert!(note.contains("1 张图片无法处理"), "{note}");

        let names: Vec<String> = output
            .images
            .iter()
            .map(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names[0].starts_with("03-复盘.docx-image2."),
            "按编号排：{names:?}"
        );
        assert!(names[1].starts_with("03-复盘.docx-image10."), "{names:?}");
        assert!(
            output
                .images
                .iter()
                .all(|p| p.starts_with(dir.path()) && p.exists())
        );
    }

    #[tokio::test]
    async fn a_document_contributes_at_most_ten_images() {
        let dir = tempfile::tempdir().expect("临时目录");
        let shot = png(32, 32);
        let names: Vec<String> = (1..=12)
            .map(|i| format!("ppt/media/image{i}.png"))
            .collect();
        let mut entries: Vec<(&str, &[u8])> = vec![("ppt/presentation.xml", b"<p/>")];
        entries.extend(names.iter().map(|n| (n.as_str(), shot.as_slice())));
        let bytes = zip_bytes(&entries);
        let path = dir.path().join("01-deck.pptx");
        let output = read(&path, "pptx", bytes, dir.path()).await.expect("解析");
        assert_eq!(output.images.len(), DOC_IMAGES);
        assert!(
            output
                .note
                .is_some_and(|n| n.contains("只取了前面的 10 张"))
        );
        assert_eq!(output.saved, None, "没有文字就不另存");
    }

    #[test]
    fn pptx_reads_slides_in_order() {
        let slide = |text: &str| {
            format!(
                r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p><a:p><a:r><a:t>要点</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        };
        let (s1, s2, s10) = (slide("背景"), slide("方案"), slide("结论"));
        let bytes = zip_of(&[
            ("ppt/presentation.xml", "<p/>"),
            ("ppt/slides/slide10.xml", &s10),
            ("ppt/slides/slide2.xml", &s2),
            ("ppt/slides/slide1.xml", &s1),
        ]);
        assert_eq!(zip_kind(&bytes), Kind::Pptx);
        let output = pptx(&bytes).expect("解析");
        assert_eq!(
            output.text.trim(),
            "## 第 1 页\n背景\n要点\n## 第 2 页\n方案\n要点\n## 第 10 页\n结论\n要点"
        );
    }

    #[test]
    fn a_plain_zip_is_an_archive() {
        let bytes = zip_of(&[("logs/app.log", "x"), ("logs/gc.log", "yy")]);
        assert_eq!(zip_kind(&bytes), Kind::Archive(Format::Zip));
    }

    #[test]
    fn a_broken_docx_is_an_error() {
        let bytes = zip_of(&[("word/document.xml", "<w:document><w:body><w:p>")]);
        assert!(docx(&bytes).is_ok_and(|o| o.text.is_empty()));
        assert!(docx(b"PK\x03\x04 not really a zip").is_err());
    }

    #[tokio::test]
    async fn legacy_documents_are_converted_with_soffice() {
        if std::process::Command::new("soffice")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("跳过：没有安装 LibreOffice");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let tools = Tools {
            office_legacy: true,
            scratch: dir.path().to_path_buf(),
            program: std::path::PathBuf::from("ai-boot"),
        };
        // 先用 soffice 从纯文本造一个旧版 .doc
        let source = dir.path().join("note.txt");
        std::fs::write(&source, "旧版文档里的排查记录\n").expect("写文件");
        let made = dir.path().join("made");
        let mut command = Command::new("soffice");
        command
            .arg(format!(
                "-env:UserInstallation={}",
                url::Url::from_directory_path(dir.path().join("p0")).expect("url")
            ))
            .args(["--headless", "--convert-to", "doc", "--outdir"])
            .arg(&made)
            .arg(&source);
        run(command, dir.path(), SOFFICE_TIMEOUT, 64 * 1024)
            .await
            .expect("造 doc");
        let doc = made.join("note.doc");
        let output = legacy(&doc, "docx", &made, &tools).await.expect("转换");
        assert!(output.text.contains("旧版文档里的排查记录"), "{output:?}");
        assert_eq!(
            output.saved,
            Some(made.join("note.doc.txt")),
            "全文存在原件旁边"
        );
        assert!(output.note.is_some_and(|n| n.contains("soffice")));
    }
}
