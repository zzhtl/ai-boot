//! 直接执行编出来的二进制测表格子进程：服务调起的就是 `ai-boot sheet <文件>`，只看它的
//! stdout 和退出状态。父进程这边就是测试进程自己：子进程炸了，测试照常往下走。

use std::io::{Cursor, Read as _, Write as _};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// 像服务那样调：清空环境、stdin 为空，限时等它结束。
fn sheet(path: &Path) -> (ExitStatus, String, Duration) {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ai-boot"))
        .arg("sheet")
        .arg(path)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动子进程");
    let mut stdout = child.stdout.take().expect("stdout");
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let status = loop {
        if let Some(status) = child.try_wait().expect("等待子进程") {
            break status;
        }
        if started.elapsed() > Duration::from_secs(90) {
            let _ = child.kill();
            panic!("子进程 90 秒没有结束");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status, reader.join().expect("读输出"), started.elapsed())
}

/// 手工拼一个只有一个工作表的 xlsx，`cells` 是（位置，文字）。
fn xlsx(cells: &[(&str, &str)]) -> Vec<u8> {
    let rows: String = cells
        .iter()
        .map(|(at, value)| {
            let row: String = at.chars().filter(char::is_ascii_digit).collect();
            format!(r#"<row r="{row}"><c r="{at}" t="inlineStr"><is><t>{value}</t></is></c></row>"#)
        })
        .collect();
    let sheet = format!(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
    );
    let entries = [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="数据" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet.as_str()),
    ];
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, content) in entries {
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("写条目");
        writer.write_all(content.as_bytes()).expect("写内容");
    }
    writer.finish().expect("完成").into_inner()
}

/// 只在 A1 和 XFD1048576 各有一个值：calamine 要按 1048576 行 × 16384 列建数组，一次
/// 分配几百 GB。子进程限了地址空间，分配失败只死它自己，也很快就死。
#[test]
fn a_sparse_sheet_bomb_only_kills_the_child() {
    let dir = tempfile::tempdir().expect("临时目录");
    let bytes = xlsx(&[("A1", "a"), ("XFD1048576", "b")]);
    assert!(bytes.len() < 4096, "炸弹文件只有 {} 字节", bytes.len());
    let path = dir.path().join("bomb.xlsx");
    std::fs::write(&path, bytes).expect("写文件");
    let (status, stdout, elapsed) = sheet(&path);
    assert!(!status.success(), "应当失败：{status:?} {stdout}");
    assert!(!stdout.contains("\"Ok\""), "{stdout}");
    assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
}

#[test]
fn a_normal_sheet_comes_back_as_json() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("hosts.xlsx");
    std::fs::write(
        &path,
        xlsx(&[
            ("A1", "主机"),
            ("B1", "JDK"),
            ("A2", "app-01"),
            ("B2", "17"),
        ]),
    )
    .expect("写文件");
    let (status, stdout, _) = sheet(&path);
    assert!(status.success(), "{status:?}");
    let result: Value = serde_json::from_str(&stdout).expect("stdout 是一条 JSON");
    let text = result["Ok"]["text"].as_str().expect("解析结果");
    assert_eq!(
        text,
        "## 工作表：数据（2 行 × 2 列）\n主机\tJDK\napp-01\t17"
    );
}

#[test]
fn a_broken_sheet_comes_back_as_an_error_message() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("broken.xlsx");
    std::fs::write(&path, b"PK\x03\x04garbage").expect("写文件");
    let (status, stdout, _) = sheet(&path);
    assert!(status.success(), "{status:?}");
    let result: Value = serde_json::from_str(&stdout).expect("stdout 是一条 JSON");
    let reason = result["Err"].as_str().expect("错误说明");
    assert!(reason.contains("表格无法打开"), "{reason}");
}
