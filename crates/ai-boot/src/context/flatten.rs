//! 把一条消息的 content 拍平成文字，并找出里面带的图片和文件。
//!
//! 输出是给 Agent 看的纯文字，不是给卡片的 markdown，不做飞书转义。附件只记下
//! key，下载与解析在 `attach` 里做；音视频、表情包、文件夹只留占位。

use ai_boot_feishu::api::ResourceKind;
use serde_json::Value;

/// 消息里的一个 @：占位符、被 @ 的人、显示名。
#[derive(Debug, Clone)]
pub struct Mention {
    pub key: String,
    pub open_id: String,
    pub name: String,
}

/// 消息里带的一个图片或文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    pub kind: ResourceKind,
    pub key: String,
    /// 文件名；图片没有。
    pub name: Option<String>,
}

/// 拍平的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flat {
    pub text: String,
    pub attachments: Vec<AttachmentRef>,
    /// 合并转发：子消息要另外取。
    pub forwarded: bool,
}

/// 卡片拍平后的长度上限：卡片通常是通知，太长的部分没有信息量。
const CARD_TEXT_CHARS: usize = 4000;

/// `skip_open_id` 是机器人自己：@ 它只是触发方式，不是内容。
pub fn flatten(
    msg_type: &str,
    content: &str,
    mentions: &[Mention],
    skip_open_id: Option<&str>,
) -> Flat {
    let parsed: Value = serde_json::from_str(content).unwrap_or(Value::Null);
    let field = |key: &str| parsed.get(key).and_then(Value::as_str).unwrap_or_default();
    let named = |key: &str| {
        let name = field(key);
        if name.is_empty() { "未命名" } else { name }.to_owned()
    };
    let mut flat = Flat::default();
    let text = match msg_type {
        "text" => replace_mentions(field("text"), mentions, skip_open_id),
        "post" => post(&parsed, mentions, skip_open_id, &mut flat.attachments),
        "image" => {
            push(
                &mut flat.attachments,
                ResourceKind::Image,
                field("image_key"),
                None,
            );
            "[图片]".to_owned()
        }
        "file" => {
            push(
                &mut flat.attachments,
                ResourceKind::File,
                field("file_key"),
                Some(named("file_name")),
            );
            format!("[文件：{}]", named("file_name"))
        }
        "audio" => format!("[语音 {}，未解析]", duration(&parsed)),
        "media" => format!(
            "[视频：{}，{}，未解析]",
            named("file_name"),
            duration(&parsed)
        ),
        "folder" => format!("[文件夹：{}，未读取]", named("file_name")),
        "sticker" => "[表情]".to_owned(),
        "interactive" => format!("[卡片] {}", card_text(&parsed)),
        "merge_forward" => {
            flat.forwarded = true;
            "[合并转发的聊天记录]".to_owned()
        }
        "share_chat" => "[群名片]".to_owned(),
        "share_user" => "[个人名片]".to_owned(),
        "location" => format!("[位置：{}]", field("name")),
        "vote" => {
            let options: Vec<&str> = parsed
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            format!("[投票：{}（{}）]", field("topic"), options.join(" / "))
        }
        "todo" => {
            let summary = parsed
                .get("summary")
                .map(|s| post(s, mentions, skip_open_id, &mut flat.attachments))
                .unwrap_or_default();
            format!("[任务] {summary}")
        }
        "share_calendar_event" | "calendar" | "general_calendar" => {
            format!("[日程：{}]", field("summary"))
        }
        "video_chat" => format!("[视频会议：{}]", field("topic")),
        "hongbao" => "[红包]".to_owned(),
        "system" => "[系统消息]".to_owned(),
        other => format!("[{other} 消息]"),
    };
    flat.text = text.trim().to_owned();
    flat
}

fn push(list: &mut Vec<AttachmentRef>, kind: ResourceKind, key: &str, name: Option<String>) {
    if !key.is_empty() {
        list.push(AttachmentRef {
            kind,
            key: key.to_owned(),
            name,
        });
    }
}

fn duration(value: &Value) -> String {
    let ms = value.get("duration").and_then(Value::as_i64).unwrap_or(0);
    format!("{} 秒", (ms + 999) / 1000)
}

fn replace_mentions(text: &str, mentions: &[Mention], skip_open_id: Option<&str>) -> String {
    let mut out = text.to_owned();
    // 先替换长的占位符：@_user_10 不能被 @_user_1 截走一半
    let mut ordered: Vec<&Mention> = mentions.iter().collect();
    ordered.sort_by_key(|m| std::cmp::Reverse(m.key.len()));
    for mention in ordered {
        let skip = skip_open_id.is_some_and(|id| id == mention.open_id);
        let replacement = if skip {
            String::new()
        } else {
            format!("@{}", mention.name)
        };
        out = out.replace(&mention.key, &replacement);
    }
    out
}

/// 富文本。收到的格式有带 locale 包裹（`{"zh_cn":{...}}`）和不带两种。
fn post(
    value: &Value,
    mentions: &[Mention],
    skip_open_id: Option<&str>,
    attachments: &mut Vec<AttachmentRef>,
) -> String {
    let body = if value.get("content").is_some() {
        value
    } else {
        value
            .as_object()
            .and_then(|locales| locales.values().find(|v| v.get("content").is_some()))
            .unwrap_or(&Value::Null)
    };
    let mut lines = Vec::new();
    if let Some(title) = body.get("title").and_then(Value::as_str)
        && !title.trim().is_empty()
    {
        lines.push(title.trim().to_owned());
    }
    for paragraph in body
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut line = String::new();
        for element in paragraph.as_array().into_iter().flatten() {
            let text = |key: &str| element.get(key).and_then(Value::as_str).unwrap_or_default();
            match element
                .get("tag")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text" | "md" => line.push_str(text("text")),
                "a" => {
                    line.push_str(text("text"));
                    if !text("href").is_empty() {
                        line.push_str(&format!("（{}）", text("href")));
                    }
                }
                "at" => {
                    let key = text("user_id");
                    match mentions.iter().find(|m| m.key == key) {
                        Some(m) if skip_open_id.is_some_and(|id| id == m.open_id) => {}
                        Some(m) => line.push_str(&format!("@{}", m.name)),
                        None => line.push_str(&format!("@{}", text("user_name"))),
                    }
                }
                "code_block" => {
                    line.push_str(&format!(
                        "\n```{}\n{}\n```\n",
                        text("language").to_ascii_lowercase(),
                        text("text")
                    ));
                }
                "img" => {
                    push(attachments, ResourceKind::Image, text("image_key"), None);
                    line.push_str("[图片]");
                }
                "media" => line.push_str("[视频，未解析]"),
                "emotion" => line.push_str("[表情]"),
                "hr" => line.push_str("---"),
                _ => {}
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// 卡片（原始 JSON 或默认的简化结构）：收集所有文字和链接，不管具体组件。
fn card_text(value: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_card(value, &mut parts);
    parts.dedup();
    let joined = parts.join(" ");
    if joined.chars().count() <= CARD_TEXT_CHARS {
        return joined;
    }
    let head: String = joined.chars().take(CARD_TEXT_CHARS).collect();
    format!("{head}…")
}

fn collect_card(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                match (key.as_str(), child) {
                    ("content" | "text" | "title", Value::String(text)) => {
                        let text = text.trim();
                        if !text.is_empty() {
                            out.push(text.to_owned());
                        }
                    }
                    ("url" | "href" | "default_url", Value::String(url)) if !url.is_empty() => {
                        out.push(format!("（{url}）"));
                    }
                    // 样式、图标、图片 key 之类没有信息量
                    (
                        "tag" | "img_key" | "icon" | "element_id" | "template" | "color" | "style"
                        | "i18n_content" | "i18n_elements",
                        _,
                    ) => {}
                    _ => collect_card(child, out),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_card(item, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mentions() -> Vec<Mention> {
        vec![
            Mention {
                key: "@_user_1".into(),
                open_id: "ou_bot".into(),
                name: "排查助手".into(),
            },
            Mention {
                key: "@_user_2".into(),
                open_id: "ou_zhang".into(),
                name: "张三".into(),
            },
        ]
    }

    fn text_of(msg_type: &str, content: &str) -> String {
        flatten(msg_type, content, &[], None).text
    }

    #[test]
    fn text_mentions_become_names_and_the_bot_mention_disappears() {
        let content = r#"{"text":"@_user_1 @_user_2 这个 500 怎么回事"}"#;
        assert_eq!(
            flatten("text", content, &mentions(), Some("ou_bot")).text,
            "@张三 这个 500 怎么回事"
        );
    }

    #[test]
    fn rich_text_keeps_links_mentions_code_and_its_images() {
        let content = r#"{"title":"现场反馈","content":[
            [{"tag":"text","text":"日志见 "},{"tag":"a","text":"这里","href":"https://example.com/log"}],
            [{"tag":"at","user_id":"@_user_2","user_name":""},{"tag":"text","text":" 帮忙看下"}],
            [{"tag":"code_block","language":"JAVA","text":"NullPointerException"}],
            [{"tag":"img","image_key":"img_1"}],
            [{"tag":"media","file_key":"file_v","image_key":"img_v"}]
        ]}"#;
        let flat = flatten("post", content, &mentions(), Some("ou_bot"));
        assert_eq!(
            flat.text,
            "现场反馈\n日志见 这里（https://example.com/log）\n@张三 帮忙看下\n\n```java\nNullPointerException\n```\n\n[图片]\n[视频，未解析]"
        );
        assert_eq!(
            flat.attachments,
            [AttachmentRef {
                kind: ResourceKind::Image,
                key: "img_1".into(),
                name: None
            }]
        );
    }

    #[test]
    fn a_locale_wrapped_post_is_unwrapped() {
        let content = r#"{"zh_cn":{"title":"","content":[[{"tag":"text","text":"你好"}]]}}"#;
        assert_eq!(text_of("post", content), "你好");
    }

    #[test]
    fn images_and_files_are_referenced_for_download() {
        let image = flatten("image", r#"{"image_key":"img_4adb"}"#, &[], None);
        assert_eq!(image.text, "[图片]");
        assert_eq!(image.attachments[0].kind, ResourceKind::Image);
        assert_eq!(image.attachments[0].key, "img_4adb");

        let file = flatten(
            "file",
            r#"{"file_key":"file_75","file_name":"error.log"}"#,
            &[],
            None,
        );
        assert_eq!(file.text, "[文件：error.log]");
        assert_eq!(
            file.attachments,
            [AttachmentRef {
                kind: ResourceKind::File,
                key: "file_75".into(),
                name: Some("error.log".into())
            }]
        );
    }

    #[test]
    fn media_and_stickers_are_placeholders_only() {
        for (msg_type, content, expected) in [
            (
                "audio",
                r#"{"file_key":"f","duration":2000}"#,
                "[语音 2 秒，未解析]",
            ),
            (
                "media",
                r#"{"file_key":"f","image_key":"i","file_name":"复现.mp4","duration":2500}"#,
                "[视频：复现.mp4，3 秒，未解析]",
            ),
            ("sticker", r#"{"file_key":"f"}"#, "[表情]"),
            (
                "folder",
                r#"{"file_key":"f","file_name":"资料"}"#,
                "[文件夹：资料，未读取]",
            ),
        ] {
            let flat = flatten(msg_type, content, &[], None);
            assert_eq!(flat.text, expected);
            assert!(flat.attachments.is_empty(), "{msg_type} 不下载");
        }
    }

    #[test]
    fn a_forward_is_marked_for_expansion() {
        let flat = flatten(
            "merge_forward",
            r#"{"content":"Merged and Forwarded Message"}"#,
            &[],
            None,
        );
        assert!(flat.forwarded);
        assert_eq!(flat.text, "[合并转发的聊天记录]");
    }

    #[test]
    fn cards_keep_their_words_and_links_but_not_their_styling() {
        let raw = r#"{"schema":"2.0","header":{"title":{"tag":"plain_text","content":"告警：登录 500"},"template":"red"},
            "body":{"elements":[
              {"tag":"markdown","content":"错误率 **12%**"},
              {"tag":"button","text":{"tag":"plain_text","content":"查看详情"},
               "behaviors":[{"type":"open_url","default_url":"https://grafana.example.com/d/1"}]},
              {"tag":"img","img_key":"img_x","alt":{"tag":"plain_text","content":""}}
            ]}}"#;
        let text = text_of("interactive", raw);
        assert_eq!(
            text,
            "[卡片] 告警：登录 500 错误率 **12%** 查看详情 （https://grafana.example.com/d/1）"
        );
        // 默认的简化结构同样能拍平
        let simple = r#"{"title":"部署通知","elements":[[{"tag":"text","text":"3.2.1 已发布"}]]}"#;
        assert_eq!(
            text_of("interactive", simple),
            "[卡片] 部署通知 3.2.1 已发布"
        );
    }

    #[test]
    fn small_structured_messages_read_naturally() {
        assert_eq!(
            text_of("vote", r#"{"topic":"周五发版？","options":["是","否"]}"#),
            "[投票：周五发版？（是 / 否）]"
        );
        assert_eq!(
            text_of(
                "location",
                r#"{"name":"上海市","longitude":"1","latitude":"2"}"#
            ),
            "[位置：上海市]"
        );
        assert_eq!(
            text_of(
                "todo",
                r#"{"task_id":"t","summary":{"title":"","content":[[{"tag":"text","text":"补日志"}]]}}"#
            ),
            "[任务] 补日志"
        );
        assert_eq!(text_of("hongbao", r#"{"text":"[红包]"}"#), "[红包]");
        assert_eq!(text_of("brand_new", "{}"), "[brand_new 消息]");
    }

    #[test]
    fn longer_placeholders_are_replaced_first() {
        let many: Vec<Mention> = (1..=10)
            .map(|i| Mention {
                key: format!("@_user_{i}"),
                open_id: format!("ou_{i}"),
                name: format!("人{i}"),
            })
            .collect();
        assert_eq!(
            flatten("text", r#"{"text":"@_user_10 和 @_user_1"}"#, &many, None).text,
            "@人10 和 @人1"
        );
    }
}
