//! 聊天里的飞书云文档链接。按路径认类型，域名只要求是飞书系的（企业自定义
//! 域名也是 xxx.feishu.cn）；文字里的链接可能被百分号编码过，先解码再找。

use std::sync::LazyLock;

use regex::Regex;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocKind {
    Docx,
    /// 旧版文档。
    Doc,
    Wiki,
    Sheet {
        sheet: Option<String>,
    },
    Bitable {
        table: Option<String>,
    },
    File,
    Mindnote,
    Slides,
    Minutes,
}

impl DocKind {
    /// 元数据接口里的类型名。
    pub fn doc_type(&self) -> Option<&'static str> {
        Some(match self {
            Self::Docx => "docx",
            Self::Doc => "doc",
            Self::Wiki => "wiki",
            Self::Sheet { .. } => "sheet",
            Self::Bitable { .. } => "bitable",
            Self::File => "file",
            Self::Mindnote => "mindnote",
            Self::Slides => "slides",
            Self::Minutes => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocLink {
    pub kind: DocKind,
    pub token: String,
    pub url: String,
}

/// 只认 URL 合法的 ASCII 字符：群聊里链接后面经常直接跟着中文；括号不算，
/// 免得把 markdown 的 `](url)` 和中文括号吞进来。
static URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"https?://[A-Za-z0-9._~:/?#@!$&*+,;=%\-]+").ok());

const HOSTS: [&str; 3] = ["feishu.cn", "larksuite.com", "larkoffice.com"];
/// 文档 token 的长度范围（实际多为 22~27 位），太短的是 settings 之类的页面路径。
const TOKEN_LEN: std::ops::RangeInclusive<usize> = 16..=64;

/// 找出文字里的云文档链接，按出现顺序去重。
pub fn find(text: &str) -> Vec<DocLink> {
    let Some(pattern) = URL.as_ref() else {
        return Vec::new();
    };
    let decoded = percent_decode(text);
    let mut links: Vec<DocLink> = Vec::new();
    for found in pattern.find_iter(&decoded) {
        if let Some(link) = parse(found.as_str())
            && !links.iter().any(|l| l.token == link.token)
        {
            links.push(link);
        }
    }
    links
}

fn parse(raw: &str) -> Option<DocLink> {
    let raw = raw.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    if !HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .filter(|value| value.bytes().all(|b| b.is_ascii_alphanumeric()))
    };
    for (i, segment) in segments.iter().enumerate() {
        let kind = match *segment {
            "docx" => DocKind::Docx,
            "docs" | "doc" => DocKind::Doc,
            "wiki" => DocKind::Wiki,
            "sheets" | "sheet" => DocKind::Sheet {
                sheet: param("sheet"),
            },
            "base" | "bitable" => DocKind::Bitable {
                table: param("table"),
            },
            "file" => DocKind::File,
            "mindnote" | "mindnotes" => DocKind::Mindnote,
            "slides" => DocKind::Slides,
            "minutes" => DocKind::Minutes,
            _ => continue,
        };
        let token = segments.get(i + 1)?;
        let valid =
            TOKEN_LEN.contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric());
        return valid.then(|| DocLink {
            kind,
            token: (*token).to_owned(),
            url: raw.to_owned(),
        });
    }
    None
}

/// 把 `%XX` 还原成字节，解不出的原样保留。
fn percent_decode(text: &str) -> String {
    if !text.contains('%') {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(high * 16 + low);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(DocKind, String)> {
        find(text).into_iter().map(|l| (l.kind, l.token)).collect()
    }

    #[test]
    fn every_document_type_is_recognised_by_its_path() {
        let text = "方案 https://example.feishu.cn/docx/FlYadoUfloTbYcxoJcccEoabcef ，\
            知识库 https://example.feishu.cn/wiki/wikcnKQ1k3p1234568Vabcef\
            表格：https://example.feishu.cn/sheets/shtcngNygNfuqhxTBf588jabcef?sheet=Q7PlXT\
            多维表格（https://example.larkoffice.com/base/bascnCMII2ORej2RItqpZZUNMIe?table=tblxI2tWaxP5dG7p&view=vew）\
            文件 https://example.feishu.cn/file/boxcnrHpsg1QDqXAAAyachabcef\
            旧文档 https://example.feishu.cn/docs/doccnfYZzTlvXqZIGTdAHKabcef。";
        assert_eq!(
            kinds(text),
            [
                (DocKind::Docx, "FlYadoUfloTbYcxoJcccEoabcef".to_owned()),
                (DocKind::Wiki, "wikcnKQ1k3p1234568Vabcef".to_owned()),
                (
                    DocKind::Sheet {
                        sheet: Some("Q7PlXT".to_owned())
                    },
                    "shtcngNygNfuqhxTBf588jabcef".to_owned()
                ),
                (
                    DocKind::Bitable {
                        table: Some("tblxI2tWaxP5dG7p".to_owned())
                    },
                    "bascnCMII2ORej2RItqpZZUNMIe".to_owned()
                ),
                (DocKind::File, "boxcnrHpsg1QDqXAAAyachabcef".to_owned()),
                (DocKind::Doc, "doccnfYZzTlvXqZIGTdAHKabcef".to_owned()),
            ]
        );
    }

    #[test]
    fn encoded_links_and_duplicates_are_handled() {
        let text = "见 https%3A%2F%2Fexample.feishu.cn%2Fdocx%2FFlYadoUfloTbYcxoJcccEoabcef 和 \
            [同一篇](https://example.feishu.cn/docx/FlYadoUfloTbYcxoJcccEoabcef)";
        assert_eq!(
            kinds(text),
            [(DocKind::Docx, "FlYadoUfloTbYcxoJcccEoabcef".to_owned())]
        );
    }

    #[test]
    fn other_links_are_ignored() {
        assert!(find("https://jira.example.com/browse/ABC-1").is_empty());
        assert!(find("https://evil.com/docx/FlYadoUfloTbYcxoJcccEoabcef").is_empty());
        assert!(find("https://feishu.cn.evil.com/docx/FlYadoUfloTbYcxoJcccEoabcef").is_empty());
        assert!(find("https://example.feishu.cn/wiki/settings").is_empty());
        assert!(find("https://example.feishu.cn/docx/../../etc").is_empty());
        assert!(find("100% 复现，%E4 不是完整编码").is_empty());
    }
}
