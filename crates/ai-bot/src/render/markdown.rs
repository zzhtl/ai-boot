//! 把模型输出的 GFM 转成飞书卡片 markdown 支持的子集。
//!
//! 用 pulldown-cmark 解析后重新输出，而不是字符串替换：
//! - 文本里的特殊字符一律转成 HTML 实体（飞书文档给出的转义方式）。卡片
//!   markdown 会识别 `<at id=all>`、`<font>`、`<link>` 等标签，转义之后模型输出里
//!   就不可能冒出一个 @所有人。
//! - 链接只放行配置里的 host，其余降为纯文本：模型给的链接不可信（可能是被
//!   提示注入塞进来的钓鱼或外发地址）。
//! - 图片需要飞书的 image_key 才能显示，外链图片降为链接。
//! - HTML 块、行内 HTML 按文本显示。
//! - 标题降为加粗行：卡片里折叠面板的标题已经是一级结构，正文里再出现大号
//!   标题会很突兀。
//! - 单个 markdown 组件最多放 4 个表格（飞书限制），超过就切成多段。

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// 单个 markdown 组件里允许的表格数。
const TABLES_PER_BLOCK: usize = 4;

/// 渲染结果：通常只有一段；表格超过上限时切成多段，每段对应一个 markdown 组件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub blocks: Vec<String>,
}

/// 链接放行判断。
pub trait LinkPolicy {
    fn allows(&self, url: &str) -> bool;
}

/// 按 host 白名单放行 http(s) 链接。host 相同或是其子域都算。
pub struct HostAllowlist<'a>(pub &'a [String]);

impl LinkPolicy for HostAllowlist<'_> {
    fn allows(&self, url: &str) -> bool {
        let Ok(parsed) = url::Url::parse(url) else {
            return false;
        };
        if !matches!(parsed.scheme(), "http" | "https") {
            return false;
        }
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        self.0.iter().any(|allowed| {
            let allowed = allowed.to_ascii_lowercase();
            host == allowed
                || host
                    .strip_suffix(allowed.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    }
}

/// 转义纯文本，使其在卡片 markdown 里按字面显示。
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&#60;"),
            '>' => out.push_str("&#62;"),
            '*' => out.push_str("&#42;"),
            '_' => out.push_str("&#95;"),
            '~' => out.push_str("&sim;"),
            '`' => out.push_str("&#96;"),
            '[' => out.push_str("&#91;"),
            ']' => out.push_str("&#93;"),
            '#' => out.push_str("&#35;"),
            '|' => out.push_str("&#124;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn render(markdown: &str, links: &dyn LinkPolicy) -> Rendered {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut writer = Writer::new(links);
    for event in Parser::new_ext(markdown, options) {
        writer.event(event);
    }
    writer.finish()
}

struct Table {
    rows: Vec<Vec<String>>,
    cell: Option<String>,
    head_rows: usize,
}

/// 正在读的代码块：内容读完才知道能不能原样放进围栏。
struct Code {
    lang: String,
    text: String,
}

struct Writer<'a> {
    links: &'a dyn LinkPolicy,
    blocks: Vec<String>,
    out: String,
    tables_in_block: usize,
    /// 每层列表的下一个序号（无序列表为 None）。
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    code: Option<Code>,
    /// 每层链接是否被放行（放行时记下地址，收尾时补上）。
    link_stack: Vec<Option<String>>,
    table: Option<Table>,
    at_line_start: bool,
}

impl<'a> Writer<'a> {
    fn new(links: &'a dyn LinkPolicy) -> Self {
        Self {
            links,
            blocks: Vec::new(),
            out: String::new(),
            tables_in_block: 0,
            lists: Vec::new(),
            quote_depth: 0,
            code: None,
            link_stack: Vec::new(),
            table: None,
            at_line_start: true,
        }
    }

    fn finish(mut self) -> Rendered {
        self.flush_block();
        if self.blocks.is_empty() {
            self.blocks.push(String::new());
        }
        Rendered {
            blocks: self.blocks,
        }
    }

    fn flush_block(&mut self) {
        let block = self.out.trim_end().to_owned();
        if !block.trim().is_empty() {
            self.blocks.push(block);
        }
        self.out.clear();
        self.tables_in_block = 0;
        self.at_line_start = true;
    }

    /// 写入内容；在行首时先补上引用前缀。
    fn write(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(table) = self.table.as_mut()
            && let Some(cell) = table.cell.as_mut()
        {
            cell.push_str(text);
            return;
        }
        if self.at_line_start && self.quote_depth > 0 {
            self.out.push_str(&"> ".repeat(self.quote_depth));
        }
        self.out.push_str(text);
        self.at_line_start = text.ends_with('\n');
    }

    fn newline(&mut self) {
        if self.table.as_ref().is_some_and(|t| t.cell.is_some()) {
            self.write(" ");
            return;
        }
        self.out.push('\n');
        self.at_line_start = true;
    }

    fn ensure_line_start(&mut self) {
        if !self.at_line_start {
            self.newline();
        }
    }

    /// 段落之间留一个空行；列表项内部不留，免得把列表打断。
    fn block_gap(&mut self) {
        self.ensure_line_start();
        if self.lists.is_empty() && !self.out.is_empty() && !self.out.ends_with("\n\n") {
            if self.quote_depth > 0 {
                self.write(">");
                self.newline();
            } else {
                self.newline();
            }
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = self.code.as_mut() {
                    code.text.push_str(&text);
                } else {
                    let escaped = escape(&text);
                    self.write(&escaped);
                }
            }
            Event::Code(code) => {
                // 内容里有连续的反引号，外面用几个反引号包都可能被提前闭合，后面的内容
                // 就按 markdown 生效了（比如 <at id=all>）：退回转义后的文字
                if code.contains("``") {
                    let escaped = escape(&code);
                    self.write(&escaped);
                    return;
                }
                let fence = if code.contains('`') { "``" } else { "`" };
                let pad = if code.starts_with('`') || code.ends_with('`') {
                    " "
                } else {
                    ""
                };
                let inline = format!("{fence}{pad}{code}{pad}{fence}");
                self.write(&inline);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let escaped = escape(&html);
                self.write(&escaped);
            }
            Event::SoftBreak | Event::HardBreak => self.newline(),
            Event::Rule => {
                self.block_gap();
                self.write("---");
                self.newline();
            }
            Event::TaskListMarker(checked) => self.write(if checked { "☑ " } else { "☐ " }),
            Event::FootnoteReference(name) => {
                let escaped = escape(&name);
                self.write(&escaped);
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                let escaped = escape(&math);
                self.write(&escaped);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.lists.is_empty() {
                    self.block_gap();
                }
            }
            Tag::Heading { .. } => {
                self.block_gap();
                self.write("**");
            }
            Tag::BlockQuote(_) => {
                self.block_gap();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_gap();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => sanitize_lang(&info),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some(Code {
                    lang,
                    text: String::new(),
                });
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.block_gap();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.ensure_line_start();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "- ".to_owned(),
                };
                // 飞书约定：4 个空格一层缩进
                self.write(&format!("{}{marker}", "    ".repeat(depth)));
            }
            Tag::Emphasis => self.write("*"),
            Tag::Strong => self.write("**"),
            Tag::Strikethrough => self.write("~~"),
            Tag::Link { dest_url, .. } => {
                let allowed = self.links.allows(&dest_url).then(|| dest_url.to_string());
                if allowed.is_some() {
                    self.write("[");
                }
                self.link_stack.push(allowed);
            }
            Tag::Image { dest_url, .. } => {
                let allowed = self.links.allows(&dest_url).then(|| dest_url.to_string());
                self.write(if allowed.is_some() {
                    "[图片："
                } else {
                    "（图片："
                });
                self.link_stack.push(allowed);
            }
            Tag::Table(_) => {
                if self.tables_in_block == TABLES_PER_BLOCK {
                    self.flush_block();
                }
                self.block_gap();
                self.table = Some(Table {
                    rows: Vec::new(),
                    cell: None,
                    head_rows: 0,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.cell = Some(String::new());
                }
            }
            // 其余结构（脚注定义、定义列表、元数据块等）只保留文字
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.ensure_line_start(),
            TagEnd::Heading(_) => {
                self.write("**");
                self.newline();
            }
            TagEnd::BlockQuote(_) => {
                self.ensure_line_start();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    self.emit_code(code);
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.ensure_line_start();
            }
            TagEnd::Item => self.ensure_line_start(),
            TagEnd::Emphasis => self.write("*"),
            TagEnd::Strong => self.write("**"),
            TagEnd::Strikethrough => self.write("~~"),
            TagEnd::Link => {
                if let Some(Some(url)) = self.link_stack.pop() {
                    let tail = format!("]({})", encode_url(&url));
                    self.write(&tail);
                }
            }
            TagEnd::Image => match self.link_stack.pop() {
                Some(Some(url)) => {
                    let tail = format!("]({})", encode_url(&url));
                    self.write(&tail);
                }
                _ => self.write("）"),
            },
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut()
                    && let Some(cell) = table.cell.take()
                    && let Some(row) = table.rows.last_mut()
                {
                    row.push(cell.trim().to_owned());
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.emit_table(table);
                }
            }
            _ => {}
        }
    }

    /// 代码块内容原样放进 ``` 围栏（飞书按代码显示），逐行补引用前缀。内容里有以 ```
    /// 开头的行（原文用 ~~~、四个反引号或缩进写的代码块里可以有），飞书会在那一行
    /// 提前结束代码块，后面的内容就按 markdown 生效了：这种整块退回转义后的文字。
    fn emit_code(&mut self, code: Code) {
        if code
            .text
            .lines()
            .any(|line| line.trim_start().starts_with("```"))
        {
            for line in code.text.lines() {
                let escaped = escape(line);
                self.write(&escaped);
                self.newline();
            }
            return;
        }
        self.write(&format!("```{}\n", code.lang));
        for piece in code.text.split_inclusive('\n') {
            self.write(piece);
        }
        self.ensure_line_start();
        self.write("```");
        self.newline();
    }

    fn emit_table(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let line = |cells: &[String]| {
            let mut padded: Vec<String> = cells.to_vec();
            padded.resize(columns, String::new());
            format!("| {} |", padded.join(" | "))
        };
        let mut text = String::new();
        for (i, row) in table.rows.iter().enumerate() {
            text.push_str(&line(row));
            text.push('\n');
            if i + 1 == table.head_rows.max(1) {
                text.push_str(&format!("|{}\n", " --- |".repeat(columns)));
            }
        }
        self.write(&text);
        self.tables_in_block += 1;
    }
}

/// 代码块语言标记只保留安全字符，挡住借语言标记夹带内容。
fn sanitize_lang(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'))
        .take(20)
        .collect()
}

/// 链接地址里会截断 markdown 语法的字符做百分号编码。
fn encode_url(url: &str) -> String {
    url.replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('<', "%3C")
        .replace('>', "%3E")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &[&str] = &["jira.example.com", "example.org"];

    fn md(input: &str) -> String {
        let hosts: Vec<String> = HOSTS.iter().map(|h| (*h).to_owned()).collect();
        render(input, &HostAllowlist(&hosts)).blocks.join("\n\n")
    }

    #[test]
    fn feishu_tags_in_model_output_never_survive() {
        for attack in [
            "<at id=all></at> 请大家看一下",
            "<at user_id=\"all\">所有人</at>",
            "<font color='red'>紧急</font>",
            "<link url=\"https://evil.example\">点我</link>",
            "<text_tag color='red'>x</text_tag>",
        ] {
            let out = md(attack);
            assert!(!out.contains('<'), "{attack} → {out}");
            assert!(out.contains("&#60;"), "{out}");
        }
    }

    #[test]
    fn markdown_special_characters_in_plain_text_are_literal() {
        let out = md(r"a \* b \_ c \~ d \` e \[f\] \# g \| h & i");
        assert_eq!(
            out,
            "a &#42; b &#95; c &sim; d &#96; e &#91;f&#93; &#35; g &#124; h &amp; i"
        );
    }

    #[test]
    fn links_are_kept_only_for_allowed_hosts() {
        assert_eq!(
            md("[XX-1](https://jira.example.com/browse/XX-1)"),
            "[XX-1](https://jira.example.com/browse/XX-1)"
        );
        assert_eq!(
            md("[wiki](https://wiki.example.org/x)"),
            "[wiki](https://wiki.example.org/x)"
        );
        // 不在白名单、伪装成子域、非 http(s) 的都降为纯文本
        for url in [
            "https://evil.example.net/x",
            "https://jira.example.com.evil.net/x",
            "javascript:alert(1)",
            "ftp://jira.example.com/x",
        ] {
            assert_eq!(md(&format!("[点我]({url})")), "点我", "{url}");
        }
    }

    #[test]
    fn images_become_links_or_captions() {
        assert_eq!(
            md("![截图](https://jira.example.com/a.png)"),
            "[图片：截图](https://jira.example.com/a.png)"
        );
        assert_eq!(md("![截图](https://evil.net/a.png)"), "（图片：截图）");
    }

    #[test]
    fn headings_become_bold_lines() {
        assert_eq!(md("## 根因\n\n正文"), "**根因**\n\n正文");
    }

    #[test]
    fn nested_lists_use_four_space_indentation() {
        let out = md("- a\n  - b\n    1. c\n    2. d\n- e");
        assert_eq!(out, "- a\n    - b\n        1. c\n        2. d\n- e");
    }

    #[test]
    fn code_blocks_keep_their_content_and_a_sanitised_language() {
        let out = md("```rust\nfn main() { let v: Vec<String> = vec![]; }\n```");
        assert_eq!(
            out,
            "```rust\nfn main() { let v: Vec<String> = vec![]; }\n```"
        );
        assert!(md("```rust<at>\nx\n```").starts_with("```rustat\n"));
    }

    /// 原文里的代码块可以夹着一行 ```：原样放进围栏的话，飞书会在那一行提前结束代码块，
    /// 后面的标签就生效了。
    #[test]
    fn a_fence_inside_a_code_block_cannot_end_it_early() {
        for attack in [
            "~~~\n```\n<at id=all></at>\n~~~",
            "````\n```\n<at id=all></at>\n````",
            "    ```\n    <at id=all></at>",
        ] {
            let out = md(attack);
            assert!(!out.contains('<'), "{attack:?} → {out}");
            assert!(out.contains("&#60;at id=all&#62;"), "{out}");
            assert!(!out.contains("```"), "退回文字后不再有围栏：{out}");
        }
        // 正常的代码块不受影响
        assert_eq!(md("~~~sh\nls -l\n~~~"), "```sh\nls -l\n```");
    }

    #[test]
    fn inline_code_is_kept() {
        assert_eq!(md("调用 `a && b` 即可"), "调用 `a && b` 即可");
        assert_eq!(md("``a`b``"), "``a`b``");
    }

    /// 行内代码里有连续的反引号，再用 `` 包就会被提前闭合。
    #[test]
    fn inline_code_with_a_backtick_run_falls_back_to_text() {
        let out = md("看 ```a``<at id=all></at>``b``` 这里");
        assert!(!out.contains('<'), "{out}");
        assert!(out.contains("&#96;&#96;&#60;at id=all&#62;"), "{out}");
    }

    #[test]
    fn raw_html_is_shown_as_text() {
        let out = md("<div onclick=x>hi</div>\n\nok");
        assert!(!out.contains("<div"), "{out}");
        assert!(out.contains("&#60;div"));
    }

    #[test]
    fn tables_are_rebuilt_and_split_after_four_per_block() {
        let table = "| a | b |\n|---|---|\n| 1 | x&#124;y |\n";
        assert_eq!(md(table), "| a | b |\n| --- | --- |\n| 1 | x&#124;y |");

        let hosts: Vec<String> = Vec::new();
        let five = (0..5).map(|_| table).collect::<Vec<_>>().join("\n");
        let rendered = render(&five, &HostAllowlist(&hosts));
        assert_eq!(rendered.blocks.len(), 2);
        assert_eq!(rendered.blocks[0].matches("| --- |").count(), 4);
        assert_eq!(rendered.blocks[1].matches("| --- |").count(), 1);
    }

    #[test]
    fn quotes_and_rules() {
        assert_eq!(md("> 引用\n> 第二行"), "> 引用\n> 第二行");
        assert_eq!(md("上\n\n---\n\n下"), "上\n\n---\n\n下");
    }

    #[test]
    fn rendering_is_idempotent_on_its_own_output_for_plain_text() {
        let once = md("普通的一段话，没有特殊字符。");
        assert_eq!(md(&once), once);
    }
}
