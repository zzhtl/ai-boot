//! 表格：calamine 读 xlsx/xls/ods，每个工作表转成 TSV，行列有上限。
//!
//! calamine 按行列建稠密数组：只在 A1 和 XFD1048576 各有一个值的 1.5 KB 文件就要一次
//! 分配几百 GB，能把整个服务拖死。所以放在子进程（`ai-boot sheet`）里解析，子进程先
//! 给自己限了地址空间和 CPU 时间，炸了也只死它自己。

use std::io::{Cursor, Write as _};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use calamine::{Data, Reader as _, open_workbook_auto_from_rs};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use tokio::process::Command;
use zip::ZipArchive;

use super::{Output, TEXT_BYTES, Tools, clip_bytes, run};

const MAX_SHEETS: usize = 10;
const MAX_ROWS: usize = 200;
const MAX_COLS: usize = 30;
/// xlsx/ods 声明的解压后总大小上限：calamine 会整表读进内存，先挡住 zip 炸弹。
const UNPACKED_LIMIT: u64 = 200 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;
/// 子进程的地址空间和 CPU 时间上限；父进程等它多久、收多少输出。
const CHILD_MEMORY: u64 = 1024 * 1024 * 1024;
const CHILD_CPU_SECS: u64 = 30;
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);
const CHILD_OUTPUT: usize = 1024 * 1024;

/// 在子进程里解析 `path`（见 `child`）。
pub(super) async fn isolated(path: &Path, tools: &Tools) -> Result<Output, String> {
    let mut command = Command::new(&tools.program);
    command.arg("sheet").arg(path);
    let raw = run(command, &tools.scratch, CHILD_TIMEOUT, CHILD_OUTPUT)
        .await
        .map_err(|err| {
            tracing::warn!(%err, path = %path.display(), "表格子进程失败");
            "表格解析失败：超出了内存或时间上限（可能是内容异常的文件）".to_owned()
        })?;
    let output = serde_json::from_slice::<Result<Output, String>>(&raw)
        .map_err(|err| format!("表格解析结果读不出来：{err}"))??;
    // 子进程处理的是不可信文件，结果里只认文字：表格不会生成文件，路径一概不收
    Ok(Output {
        text: output.text,
        note: output.note,
        ..Output::default()
    })
}

/// `ai-boot sheet <文件>`：先给自己限内存、CPU 时间，再解析，结果以 JSON 写到 stdout。
/// 超限时分配失败直接 abort，不留 core。
pub fn child(path: &Path) -> ExitCode {
    for (resource, limit) in [
        (Resource::As, CHILD_MEMORY),
        (Resource::Cpu, CHILD_CPU_SECS),
        (Resource::Core, 0),
    ] {
        // 只能往下调：原来的硬上限更低就用原来的
        let limit = getrlimit(resource)
            .maximum
            .map_or(limit, |max| max.min(limit));
        let rlimit = Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        };
        // 限不住就不解析：宁可这个文件读不到，也不冒拖垮服务的险
        if let Err(err) = setrlimit(resource, rlimit) {
            eprintln!("ai-boot sheet：设置资源上限失败：{err}");
            return ExitCode::FAILURE;
        }
    }
    let result = std::fs::read(path)
        .map_err(|err| format!("读取表格失败：{err}"))
        .and_then(extract);
    let Ok(json) = serde_json::to_string(&result) else {
        return ExitCode::FAILURE;
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(json.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// 在当前进程里解析：只给子进程和测试用。
pub(super) fn extract(bytes: Vec<u8>) -> Result<Output, String> {
    if bytes.starts_with(b"PK") {
        check_unpacked(&bytes)?;
    }
    let mut workbook = open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|err| format!("表格无法打开：{err}"))?;
    let names = workbook.sheet_names();
    let mut out = String::new();
    let mut notes = Vec::new();
    for name in names.iter().take(MAX_SHEETS) {
        let range = match workbook.worksheet_range(name) {
            Ok(range) => range,
            Err(err) => {
                out.push_str(&format!("## 工作表：{name}（读取失败：{err}）\n"));
                continue;
            }
        };
        let (rows, cols) = range.get_size();
        out.push_str(&format!("## 工作表：{name}（{rows} 行 × {cols} 列）\n"));
        for row in range.rows().take(MAX_ROWS) {
            let mut cells: Vec<String> = row.iter().take(MAX_COLS).map(cell_text).collect();
            while cells.last().is_some_and(String::is_empty) {
                cells.pop();
            }
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        if rows > MAX_ROWS {
            out.push_str(&format!("…（省略后面 {} 行）\n", rows - MAX_ROWS));
        }
        if cols > MAX_COLS {
            notes.push(format!("{name} 只取了前 {MAX_COLS} 列"));
        }
        if out.len() > TEXT_BYTES {
            break;
        }
    }
    if names.len() > MAX_SHEETS {
        notes.push(format!(
            "共 {} 个工作表，只读了前 {MAX_SHEETS} 个",
            names.len()
        ));
    }
    let (text, cut) = clip_bytes(out.trim_end(), TEXT_BYTES);
    if cut {
        notes.push("内容较长，只放了前面一部分，完整内容见原件".to_owned());
    }
    Ok(Output {
        text,
        note: (!notes.is_empty()).then(|| notes.join("；")),
        ..Output::default()
    })
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        other => other.to_string().replace(['\t', '\n', '\r'], " "),
    }
}

fn check_unpacked(bytes: &[u8]) -> Result<(), String> {
    let mut archive =
        ZipArchive::new(Cursor::new(bytes)).map_err(|err| format!("表格无法打开：{err}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err("表格里的条目过多".to_owned());
    }
    let mut total = 0_u64;
    for i in 0..archive.len() {
        let file = archive
            .by_index_raw(i)
            .map_err(|err| format!("表格无法读取：{err}"))?;
        total = total.saturating_add(file.size());
    }
    if total > UNPACKED_LIMIT {
        return Err("表格解压后过大".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::office::tests::zip_of;
    use super::*;

    fn sheet_xml(rows: &[&[&str]]) -> String {
        let mut xml = String::from(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
        );
        for (r, row) in rows.iter().enumerate() {
            xml.push_str(&format!(r#"<row r="{}">"#, r + 1));
            for (c, value) in row.iter().enumerate() {
                let col = char::from(b'A' + u8::try_from(c).expect("列数很少"));
                if let Ok(number) = value.parse::<f64>() {
                    xml.push_str(&format!(r#"<c r="{col}{}"><v>{number}</v></c>"#, r + 1));
                } else {
                    xml.push_str(&format!(
                        r#"<c r="{col}{}" t="inlineStr"><is><t>{value}</t></is></c>"#,
                        r + 1
                    ));
                }
            }
            xml.push_str("</row>");
        }
        xml.push_str("</sheetData></worksheet>");
        xml
    }

    /// 手工拼一个最小的两页 xlsx。
    fn workbook() -> Vec<u8> {
        let s1 = sheet_xml(&[&["主机", "JDK"], &["app-01", "17"]]);
        let s2 = sheet_xml(&[&["版本", "日期"], &["3.2.1", "20260901"]]);
        workbook_of(&s1, &s2)
    }

    fn workbook_of(s1: &str, s2: &str) -> Vec<u8> {
        let content_types = r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/worksheets/sheet2.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let book = r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="环境" sheetId="1" r:id="rId1"/><sheet name="版本" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let book_rels = r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        zip_of(&[
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", rels),
            ("xl/workbook.xml", book),
            ("xl/_rels/workbook.xml.rels", book_rels),
            ("xl/worksheets/sheet1.xml", s1),
            ("xl/worksheets/sheet2.xml", s2),
        ])
    }

    #[test]
    fn every_sheet_becomes_a_tsv_section() {
        let output = extract(workbook()).expect("解析");
        assert_eq!(
            output.text,
            "## 工作表：环境（2 行 × 2 列）\n主机\tJDK\napp-01\t17\n## 工作表：版本（2 行 × 2 列）\n版本\t日期\n3.2.1\t20260901"
        );
        assert_eq!(output.note, None);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(extract(b"PK\x03\x04garbage".to_vec()).is_err());
    }

    #[test]
    fn long_sheets_are_clipped_by_bytes() {
        let cells: Vec<[String; 1]> = (0..200)
            .map(|i| [format!("第{i}行的中文说明{}", "很长".repeat(40))])
            .collect();
        let cells: Vec<[&str; 1]> = cells.iter().map(|[c]| [c.as_str()]).collect();
        let rows: Vec<&[&str]> = cells.iter().map(|r| r.as_slice()).collect();
        let output = extract(workbook_of(&sheet_xml(&rows), &sheet_xml(&[&["x"]]))).expect("解析");
        assert!(output.text.len() <= TEXT_BYTES, "{}", output.text.len());
        assert!(output.note.is_some_and(|n| n.contains("只放了前面一部分")));
    }

    /// 子进程没有正常给出结果（被信号杀掉、退出码非零）时，报一句能看懂的原因。
    #[tokio::test]
    async fn a_failing_child_is_an_error_not_a_crash() {
        let dir = tempfile::tempdir().expect("临时目录");
        let tools = Tools {
            office_legacy: false,
            scratch: dir.path().to_path_buf(),
            program: std::path::PathBuf::from("/bin/false"),
        };
        let err = isolated(&dir.path().join("bomb.xlsx"), &tools)
            .await
            .expect_err("应当失败");
        assert!(err.contains("超出了内存或时间上限"), "{err}");
    }
}
