//! PDF：pdftotext 取文字；几乎没有文字的当扫描件，前几页转成图片给模型看。

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

use super::{Output, TEXT_BYTES, Tools, clip_bytes, run};

const TEXT_TIMEOUT: Duration = Duration::from_secs(30);
const RENDER_TIMEOUT: Duration = Duration::from_secs(60);
/// 只取前这么多页的文字，更长的看原件。
const TEXT_PAGES: &str = "50";
/// 扫描件转图片的页数与分辨率：110 dpi 的 A4 约 900×1300，看得清字。
const SCANNED_PAGES: &str = "5";
const SCANNED_DPI: &str = "110";
/// 少于这么多个非空白字符就当作扫描件。
const MIN_TEXT_CHARS: usize = 100;
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

pub async fn extract(path: &Path, out_dir: &Path, tools: &Tools) -> Result<Output, String> {
    let mut command = Command::new("pdftotext");
    command
        .args(["-layout", "-enc", "UTF-8", "-l", TEXT_PAGES])
        .arg(path)
        .arg("-");
    let raw = run(command, &tools.scratch, TEXT_TIMEOUT, OUTPUT_CAP).await?;
    let text = String::from_utf8_lossy(&raw).replace('\u{c}', "\n");
    let meaningful = text.chars().filter(|c| !c.is_whitespace()).count();
    if meaningful >= MIN_TEXT_CHARS {
        let (text, cut) = clip_bytes(text.trim(), TEXT_BYTES);
        return Ok(Output {
            text,
            note: cut.then(|| "内容较长，只放了前面一部分，完整内容见原件".to_owned()),
            ..Output::default()
        });
    }

    // 扫描件：转成图片
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pdf")
        .to_owned();
    let prefix = out_dir.join(format!("{stem}-page"));
    let mut command = Command::new("pdftoppm");
    command
        .args(["-r", SCANNED_DPI, "-png", "-l", SCANNED_PAGES])
        .arg(path)
        .arg(&prefix);
    run(command, &tools.scratch, RENDER_TIMEOUT, 64 * 1024).await?;
    let images = rendered_pages(out_dir, &format!("{stem}-page-"))?;
    if images.is_empty() {
        return Err("扫描版 PDF 转图片失败".to_owned());
    }
    Ok(Output {
        text: text.trim().to_owned(),
        note: Some(format!(
            "几乎没有文字（可能是扫描件），前 {} 页转成了图片",
            images.len()
        )),
        images,
        saved: None,
    })
}

/// pdftoppm 按页数位数补零（page-1.png 或 page-01.png），按文件名排序即页序。
fn rendered_pages(dir: &Path, prefix: &str) -> Result<Vec<PathBuf>, String> {
    let mut pages: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("读取页面图片失败：{err}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".png"))
        })
        .collect();
    pages.sort();
    Ok(pages)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// 手写一个最小的合法 PDF：一页，`text` 为空时没有任何文字（相当于扫描件）。
    /// 按词换行，每行不超出页面宽度（pdftotext 会丢掉页面外的文字）。
    pub fn pdf(text: &str) -> Vec<u8> {
        let mut lines: Vec<String> = Vec::new();
        for word in text.split_whitespace() {
            match lines.last_mut() {
                Some(line) if line.len() + word.len() < 60 => {
                    line.push(' ');
                    line.push_str(word);
                }
                _ => lines.push(word.to_owned()),
            }
        }
        let stream = if lines.is_empty() {
            String::new()
        } else {
            let shown: Vec<String> = lines.iter().map(|l| format!("({l}) Tj")).collect();
            format!("BT /F1 12 Tf 14 TL 72 712 Td {} ET", shown.join(" T* "))
        };
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(
            format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
        );
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn tools(dir: &Path) -> Tools {
        Tools {
            office_legacy: true,
            scratch: dir.to_path_buf(),
            program: PathBuf::from("ai-boot"),
        }
    }

    fn poppler() -> bool {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn text_pdfs_come_out_as_text() {
        if !poppler() {
            eprintln!("跳过：没有安装 poppler-utils");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("report.pdf");
        // 要超过扫描件的判定阈值（100 个非空白字符）
        let body = "Connection pool exhausted at 10:00 on app-01, see ticket ABC-12 for the root cause analysis and the retest plan of release 3.2.1";
        std::fs::write(&path, pdf(body)).expect("写文件");
        let output = extract(&path, dir.path(), &tools(dir.path()))
            .await
            .expect("解析");
        assert!(
            output.text.contains("Connection pool exhausted"),
            "{output:?}"
        );
        assert!(output.images.is_empty());
    }

    #[tokio::test]
    async fn scanned_pdfs_become_page_images() {
        if !poppler() {
            eprintln!("跳过：没有安装 poppler-utils");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("scan.pdf");
        std::fs::write(&path, pdf("")).expect("写文件");
        let output = extract(&path, dir.path(), &tools(dir.path()))
            .await
            .expect("解析");
        assert_eq!(output.images.len(), 1, "{output:?}");
        assert!(output.images[0].starts_with(dir.path()));
        assert!(output.note.as_deref().is_some_and(|n| n.contains("扫描件")));
    }

    #[tokio::test]
    async fn a_broken_pdf_is_an_error_not_a_hang() {
        if !poppler() {
            eprintln!("跳过：没有安装 poppler-utils");
            return;
        }
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("broken.pdf");
        std::fs::write(&path, b"%PDF-1.4\nthis is not a pdf").expect("写文件");
        let result = extract(&path, dir.path(), &tools(dir.path())).await;
        assert!(result.is_err(), "{result:?}");
    }
}
