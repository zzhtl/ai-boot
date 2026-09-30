//! 闭环方案的文档：由结构化答案确定性地拼成 markdown，再转成 Jira wiki 标记
//! （评论）和 Confluence storage XHTML（页面）。
//!
//! 转换走 pulldown-cmark 的事件流，不拼接模型原文：原文里的 HTML、宏语法一律
//! 当文字转义，写出去的东西只有我们认识的结构。

use std::fmt::Write as _;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sha2::{Digest as _, Sha256};

use crate::answer::Answer;

/// 闭环方案的 markdown 正文。`footer` 是出处说明（会话、日期）。
pub fn document(answer: &Answer, footer: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "**结论**：{}\n", answer.summary.trim());
    if !answer.conflicts.is_empty() {
        let _ = writeln!(out, "## 信息冲突\n");
        for c in &answer.conflicts {
            let _ = write!(out, "- {}：{} → 采信：{}", c.topic, c.claim, c.adopted);
            if !c.sources.is_empty() {
                let _ = write!(out, "（依据：{}）", c.sources.join("、"));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    for section in &answer.sections {
        let _ = writeln!(
            out,
            "## {}\n\n{}\n",
            section.title.trim(),
            section.body.trim()
        );
    }
    if !answer.open_questions.is_empty() {
        let _ = writeln!(out, "## 待确认\n");
        for q in &answer.open_questions {
            let _ = writeln!(out, "- {}", q.trim());
        }
        out.push('\n');
    }
    if !answer.references.is_empty() {
        let _ = writeln!(out, "## 参考来源\n");
        for r in &answer.references {
            if r.url.starts_with("https://") || r.url.starts_with("http://") {
                let _ = writeln!(out, "- [{}]({})", r.title.trim(), r.url.trim());
            } else {
                let _ = writeln!(out, "- {}", r.title.trim());
            }
        }
        out.push('\n');
    }
    let _ = writeln!(out, "---\n\n{footer}");
    out
}

/// 内容摘要：卡片按钮带着它，执行时重算一遍，对不上就拒绝（内容已经变了）。
pub fn content_hash(markdown: &str) -> String {
    let digest = Sha256::digest(markdown.as_bytes());
    digest.iter().take(8).fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn parser(markdown: &str) -> Parser<'_> {
    Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
}

/// markdown → Jira wiki 标记（`jira_comment add` 只收这种格式）。
pub fn jira_wiki(markdown: &str) -> String {
    let mut out = String::new();
    // 列表嵌套：每层记住是有序（#）还是无序（*）
    let mut lists: Vec<char> = Vec::new();
    let mut link: Option<String> = None;
    let mut link_text = String::new();
    let mut in_code_block = false;
    let mut table_header = false;
    for event in parser(markdown) {
        let text_sink =
            |out: &mut String, link_text: &mut String, link: &Option<String>, s: &str| {
                if link.is_some() {
                    link_text.push_str(s);
                } else {
                    out.push_str(s);
                }
            };
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let _ = write!(out, "h{}. ", heading_number(level));
            }
            Event::End(TagEnd::Heading(_)) => out.push_str("\n\n"),
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if lists.is_empty() {
                    out.push_str("\n\n");
                }
            }
            Event::Start(Tag::Strong) => text_sink(&mut out, &mut link_text, &link, "*"),
            Event::End(TagEnd::Strong) => text_sink(&mut out, &mut link_text, &link, "*"),
            Event::Start(Tag::Emphasis) => text_sink(&mut out, &mut link_text, &link, "_"),
            Event::End(TagEnd::Emphasis) => text_sink(&mut out, &mut link_text, &link, "_"),
            Event::Start(Tag::Strikethrough) => text_sink(&mut out, &mut link_text, &link, "-"),
            Event::End(TagEnd::Strikethrough) => text_sink(&mut out, &mut link_text, &link, "-"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some(dest_url.to_string());
                link_text.clear();
            }
            Event::End(TagEnd::Link) => {
                if let Some(url) = link.take() {
                    let url = url.replace(['|', ']'], "");
                    if link_text.is_empty() || link_text == url {
                        let _ = write!(out, "[{url}]");
                    } else {
                        let _ = write!(out, "[{}|{url}]", link_text.replace(['|', ']'], ""));
                    }
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                // 图片不内嵌，给链接
                link = Some(dest_url.to_string());
                link_text.clear();
            }
            Event::End(TagEnd::Image) => {
                if let Some(url) = link.take() {
                    let _ = write!(
                        out,
                        "[{}|{}]",
                        if link_text.is_empty() {
                            "图片"
                        } else {
                            &link_text
                        },
                        url.replace(['|', ']'], "")
                    );
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                        .to_ascii_lowercase(),
                    CodeBlockKind::Indented => String::new(),
                };
                if lang.is_empty() {
                    out.push_str("{code}\n");
                } else {
                    let _ = writeln!(out, "{{code:{lang}}}");
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("{code}\n\n");
            }
            Event::Start(Tag::List(start)) => {
                if lists.is_empty() && !out.is_empty() && !out.ends_with("\n\n") {
                    out.push('\n');
                }
                lists.push(if start.is_some() { '#' } else { '*' });
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                if lists.is_empty() {
                    out.push('\n');
                }
            }
            Event::Start(Tag::Item) => {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                let marks: String = lists.iter().collect();
                let _ = write!(out, "{marks} ");
            }
            Event::End(TagEnd::Item) => {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            Event::Start(Tag::BlockQuote(_)) => out.push_str("{quote}\n"),
            Event::End(TagEnd::BlockQuote(_)) => {
                let trimmed = out.trim_end_matches('\n').len();
                out.truncate(trimmed);
                out.push_str("\n{quote}\n\n");
            }
            Event::Start(Tag::TableHead) => table_header = true,
            Event::End(TagEnd::TableHead) => {
                out.push_str("||\n");
                table_header = false;
            }
            Event::Start(Tag::TableRow) => {}
            Event::End(TagEnd::TableRow) => out.push_str("|\n"),
            Event::Start(Tag::TableCell) => out.push_str(if table_header { "||" } else { "|" }),
            Event::End(TagEnd::Table) => out.push('\n'),
            Event::Text(text) => {
                if in_code_block {
                    // 代码块里原样，只防住提前结束代码块
                    out.push_str(&text.replace("{code}", "{ code}"));
                } else {
                    let escaped = escape_wiki(&text);
                    text_sink(&mut out, &mut link_text, &link, &escaped);
                }
            }
            Event::Code(code) => {
                let inner = code.replace('}', "\\}").replace('{', "\\{");
                text_sink(&mut out, &mut link_text, &link, &format!("{{{{{inner}}}}}"));
            }
            // 原文里的 HTML 当文字
            Event::Html(html) | Event::InlineHtml(html) => {
                let escaped = escape_wiki(&html);
                text_sink(&mut out, &mut link_text, &link, &escaped);
            }
            Event::SoftBreak => text_sink(&mut out, &mut link_text, &link, " "),
            Event::HardBreak => out.push_str("\\\\\n"),
            Event::Rule => out.push_str("----\n\n"),
            _ => {}
        }
    }
    out.trim_end().to_owned()
}

/// Jira wiki 的格式字符前加反斜杠，显示为字面值。
fn escape_wiki(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '{' | '}' | '[' | ']' | '|' | '*' | '_' | '^' | '~' | '+' | '!' | '#' | '-'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn heading_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// markdown → Confluence storage（XHTML）。代码块用 code 宏，CDATA 里的 `]]>` 拆开。
pub fn storage(markdown: &str) -> String {
    let mut out = String::new();
    let mut in_code_block = false;
    let mut table_header = false;
    for event in parser(markdown) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                let _ = write!(out, "<h{}>", heading_number(level));
            }
            Event::End(TagEnd::Heading(level)) => {
                let _ = write!(out, "</h{}>", heading_number(level));
            }
            Event::Start(Tag::Paragraph) => out.push_str("<p>"),
            Event::End(TagEnd::Paragraph) => out.push_str("</p>"),
            Event::Start(Tag::Strong) => out.push_str("<strong>"),
            Event::End(TagEnd::Strong) => out.push_str("</strong>"),
            Event::Start(Tag::Emphasis) => out.push_str("<em>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</em>"),
            Event::Start(Tag::Strikethrough) => out.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => out.push_str("</s>"),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let _ = write!(out, "<a href=\"{}\">", escape_xml(&dest_url));
            }
            Event::End(TagEnd::Link) => out.push_str("</a>"),
            Event::Start(Tag::Image { dest_url, .. }) => {
                let _ = write!(out, "<a href=\"{}\">", escape_xml(&dest_url));
            }
            Event::End(TagEnd::Image) => out.push_str("</a>"),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                out.push_str("<ac:structured-macro ac:name=\"code\">");
                if let CodeBlockKind::Fenced(lang) = kind {
                    let lang: String = lang
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect::<String>()
                        .to_ascii_lowercase();
                    if !lang.is_empty() {
                        let _ = write!(
                            out,
                            "<ac:parameter ac:name=\"language\">{lang}</ac:parameter>"
                        );
                    }
                }
                out.push_str("<ac:plain-text-body><![CDATA[");
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                out.push_str("]]></ac:plain-text-body></ac:structured-macro>");
            }
            Event::Start(Tag::List(Some(_))) => out.push_str("<ol>"),
            Event::Start(Tag::List(None)) => out.push_str("<ul>"),
            Event::End(TagEnd::List(true)) => out.push_str("</ol>"),
            Event::End(TagEnd::List(false)) => out.push_str("</ul>"),
            Event::Start(Tag::Item) => out.push_str("<li>"),
            Event::End(TagEnd::Item) => out.push_str("</li>"),
            Event::Start(Tag::BlockQuote(_)) => out.push_str("<blockquote>"),
            Event::End(TagEnd::BlockQuote(_)) => out.push_str("</blockquote>"),
            Event::Start(Tag::Table(_)) => out.push_str("<table><tbody>"),
            Event::End(TagEnd::Table) => out.push_str("</tbody></table>"),
            Event::Start(Tag::TableHead) => {
                table_header = true;
                out.push_str("<tr>");
            }
            Event::End(TagEnd::TableHead) => {
                table_header = false;
                out.push_str("</tr>");
            }
            Event::Start(Tag::TableRow) => out.push_str("<tr>"),
            Event::End(TagEnd::TableRow) => out.push_str("</tr>"),
            Event::Start(Tag::TableCell) => {
                out.push_str(if table_header { "<th>" } else { "<td>" })
            }
            Event::End(TagEnd::TableCell) => {
                out.push_str(if table_header { "</th>" } else { "</td>" })
            }
            Event::Text(text) => {
                if in_code_block {
                    out.push_str(&text.replace("]]>", "]]]]><![CDATA[>"));
                } else {
                    out.push_str(&escape_xml(&text));
                }
            }
            Event::Code(code) => {
                let _ = write!(out, "<code>{}</code>", escape_xml(&code));
            }
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&escape_xml(&html)),
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push_str("<br/>"),
            Event::Rule => out.push_str("<hr/>"),
            _ => {}
        }
    }
    out
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::{Confidence, Conflict, Kind, Reference, Section, Status};

    fn answer() -> Answer {
        Answer {
            kind: Kind::Diagnosis,
            title: "登录偶发 500".into(),
            status: Status::Answered,
            confidence: Confidence::High,
            summary: "连接池耗尽，3.2.2 已修复".into(),
            corrections: vec![],
            conflicts: vec![Conflict {
                topic: "引入版本".into(),
                claim: "群聊说 3.2.0".into(),
                adopted: "以 Jira 为准：3.1.9".into(),
                sources: vec!["ABC-12".into()],
            }],
            sections: vec![Section {
                title: "根因".into(),
                body: "`maxPoolSize=10` 过小，见：\n\n```java\npool.setMaxPoolSize(10);\n```\n\n| 版本 | 结果 |\n|---|---|\n| 3.2.1 | 复现 |".into(),
                images: vec![],
                charts: vec![],
                diagrams: vec![],
            }],
            open_questions: vec!["生产环境的 JDK 版本".into()],
            references: vec![Reference {
                kind: "jira".into(),
                title: "ABC-12".into(),
                url: "https://jira.example.com/browse/ABC-12".into(),
            }],
            jira_keys: vec!["ABC-12".into()],
            image_keys: Default::default(),
        }
    }

    const FOOTER: &str = "由 ai-boot 生成";

    #[test]
    fn the_document_follows_the_answer_structure() {
        let doc = document(&answer(), FOOTER);
        let order = [
            "**结论**",
            "## 信息冲突",
            "## 根因",
            "## 待确认",
            "## 参考来源",
            FOOTER,
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|part| doc.find(part).unwrap_or_else(|| panic!("缺 {part}：{doc}")))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{doc}");
        assert_eq!(
            content_hash(&doc),
            content_hash(&document(&answer(), FOOTER))
        );
        assert_ne!(
            content_hash(&doc),
            content_hash(&document(&answer(), "别的尾注"))
        );
        assert_eq!(content_hash(&doc).len(), 16);
    }

    #[test]
    fn jira_wiki_golden() {
        let markdown = "## 根因\n\n**加粗** 与 *斜体*，`a{b}`，见 [ABC-12](https://jira.example.com/browse/ABC-12)。\n\n- 一\n  - 二\n1. 甲\n\n```java\nint a = 1; // {code}\n```\n\n| 版本 | 结果 |\n|---|---|\n| 3.2.1 | 复现 |\n\n> 引用\n\n<at id=all></at> [x]";
        assert_eq!(
            jira_wiki(markdown),
            "h2. 根因\n\n\
             *加粗* 与 _斜体_，{{a\\{b\\}}}，见 [ABC\\-12|https://jira.example.com/browse/ABC-12]。\n\n\
             * 一\n** 二\n\n# 甲\n\n\
             {code:java}\nint a = 1; // { code}\n{code}\n\n\
             ||版本||结果||\n|3.2.1|复现|\n\n\
             {quote}\n引用\n{quote}\n\n\
             <at id=all></at> \\[x\\]"
        );
    }

    #[test]
    fn storage_is_well_formed_xhtml_and_keeps_code_in_a_macro() {
        let markdown = format!(
            "{}\n\n```\nif a < b && c > d {{ }} ]]> end\n```\n\n<script>alert(1)</script>\n\n换行  \n之后",
            document(&answer(), FOOTER)
        );
        let xhtml = storage(&markdown);
        assert!(xhtml.contains("<ac:structured-macro ac:name=\"code\">"));
        assert!(xhtml.contains("<ac:parameter ac:name=\"language\">java</ac:parameter>"));
        assert!(xhtml.contains("]]]]><![CDATA[>"), "CDATA 里的 ]]> 要拆开");
        assert!(!xhtml.contains("<script>"), "原文 HTML 转义成文字");
        assert!(xhtml.contains("<table><tbody><tr><th>版本</th>"));
        // 用 XML 解析器验证结构完整
        let wrapped = format!(
            "<root xmlns:ac=\"http://atlassian.com/content\" xmlns:ri=\"http://atlassian.com/resource\">{xhtml}</root>"
        );
        let mut reader = quick_xml::Reader::from_str(&wrapped);
        let mut depth = 0_i32;
        loop {
            match reader.read_event().expect("XHTML 必须能被解析") {
                quick_xml::events::Event::Start(_) => depth += 1,
                quick_xml::events::Event::End(_) => depth -= 1,
                quick_xml::events::Event::Eof => break,
                _ => {}
            }
        }
        assert_eq!(depth, 0, "标签必须成对");
    }
}
