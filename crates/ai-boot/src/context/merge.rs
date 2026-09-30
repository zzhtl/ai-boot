//! 合并转发：一次取回全部子消息（平铺，`upper_message_id` 指向上一层），按层级
//! 还原成聊天记录。嵌套的转发继续展开，有深度和条数上限，并防环。

use std::collections::{HashMap, HashSet};

use ai_boot_feishu::api::{ApiClient, MessageItem};

use super::Line;
use super::flatten::{AttachmentRef, Mention, flatten};

const MAX_DEPTH: usize = 3;
const MAX_LINES: usize = 300;

/// 展开后的一段转发记录。
#[derive(Debug, Clone, Default)]
pub struct Forward {
    pub lines: Vec<Line>,
    /// 子消息里的图片和文件，以及发送人。
    pub attachments: Vec<(AttachmentRef, String)>,
    /// 超过深度或条数上限，没有全部展开。
    pub truncated: bool,
}

/// 展开 `message_id` 这条合并转发。子消息的发送人多半不在当前群里，拿不到
/// 名字时用 `name_of` 的兜底（open_id 末几位）。
pub async fn expand(
    api: &ApiClient,
    message_id: &str,
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
) -> Result<Forward, String> {
    let items = api
        .get_message(message_id)
        .await
        .map_err(|err| format!("取合并转发的内容失败：{err}"))?;
    Ok(build(message_id, &items, bot_open_id, name_of))
}

fn build(
    root: &str,
    items: &[MessageItem],
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
) -> Forward {
    let mut children: HashMap<&str, Vec<&MessageItem>> = HashMap::new();
    for item in items {
        if let Some(upper) = item.upper_message_id.as_deref().filter(|u| !u.is_empty())
            && item.message_id != root
        {
            children.entry(upper).or_default().push(item);
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|item| item.create_time.parse::<i64>().unwrap_or_default());
    }
    let mut forward = Forward::default();
    let mut visited = HashSet::from([root]);
    walk(
        root,
        1,
        &children,
        &mut visited,
        bot_open_id,
        name_of,
        &mut forward,
    );
    forward
}

fn walk<'a>(
    parent: &str,
    depth: usize,
    children: &HashMap<&str, Vec<&'a MessageItem>>,
    visited: &mut HashSet<&'a str>,
    bot_open_id: Option<&str>,
    name_of: &impl Fn(&str) -> String,
    forward: &mut Forward,
) {
    for item in children.get(parent).into_iter().flatten() {
        if forward.lines.len() >= MAX_LINES {
            forward.truncated = true;
            return;
        }
        if !visited.insert(item.message_id.as_str()) {
            continue;
        }
        let mentions: Vec<Mention> = item
            .mentions
            .iter()
            .map(|m| Mention {
                key: m.key.clone(),
                open_id: m.id.clone(),
                name: m.name.clone(),
            })
            .collect();
        let content = item.body.as_ref().map(|b| b.content.as_str()).unwrap_or("");
        let flat = flatten(&item.msg_type, content, &mentions, bot_open_id);
        let sender = name_of(&item.sender.id);
        let indent = "  ".repeat(depth - 1);
        forward.lines.push(Line {
            at_ms: item.create_time.parse().unwrap_or_default(),
            sender: format!("{indent}{sender}"),
            text: flat.text,
        });
        forward
            .attachments
            .extend(flat.attachments.into_iter().map(|a| (a, sender.clone())));
        if flat.forwarded {
            if depth < MAX_DEPTH {
                walk(
                    &item.message_id,
                    depth + 1,
                    children,
                    visited,
                    bot_open_id,
                    name_of,
                    forward,
                );
            } else {
                forward.truncated = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_boot_feishu::api::{ItemBody, ItemSender};

    fn item(id: &str, upper: Option<&str>, at: i64, msg_type: &str, content: &str) -> MessageItem {
        MessageItem {
            message_id: id.into(),
            root_id: None,
            parent_id: None,
            thread_id: None,
            msg_type: msg_type.into(),
            create_time: at.to_string(),
            deleted: false,
            chat_id: String::new(),
            sender: ItemSender {
                id: format!("ou_{id}"),
                id_type: "open_id".into(),
                sender_type: "user".into(),
            },
            body: Some(ItemBody {
                content: content.into(),
            }),
            mentions: vec![],
            upper_message_id: upper.map(str::to_owned),
        }
    }

    fn text(t: &str) -> String {
        serde_json::json!({ "text": t }).to_string()
    }

    fn name(open_id: &str) -> String {
        open_id.trim_start_matches("ou_").to_owned()
    }

    #[test]
    fn a_forward_is_rebuilt_in_order_with_nested_levels_and_attachments() {
        let items = vec![
            item(
                "om_root",
                None,
                0,
                "merge_forward",
                r#"{"content":"Merged and Forwarded Message"}"#,
            ),
            item(
                "c2",
                Some("om_root"),
                20,
                "image",
                r#"{"image_key":"img_1"}"#,
            ),
            item("c1", Some("om_root"), 10, "text", &text("3.2 登录报 500")),
            item("c3", Some("om_root"), 30, "merge_forward", "{}"),
            item(
                "n1",
                Some("c3"),
                25,
                "file",
                r#"{"file_key":"file_1","file_name":"gc.log"}"#,
            ),
        ];
        let forward = build("om_root", &items, None, &name);
        let rendered: Vec<String> = forward
            .lines
            .iter()
            .map(|l| format!("{}：{}", l.sender, l.text))
            .collect();
        assert_eq!(
            rendered,
            [
                "c1：3.2 登录报 500",
                "c2：[图片]",
                "c3：[合并转发的聊天记录]",
                "  n1：[文件：gc.log]"
            ]
        );
        let keys: Vec<(&str, &str)> = forward
            .attachments
            .iter()
            .map(|(a, sender)| (a.key.as_str(), sender.as_str()))
            .collect();
        assert_eq!(keys, [("img_1", "c2"), ("file_1", "n1")]);
        assert!(!forward.truncated);
    }

    #[test]
    fn deep_nesting_and_cycles_are_cut_off() {
        let items = vec![
            item("a", Some("om_root"), 1, "merge_forward", "{}"),
            item("b", Some("a"), 2, "merge_forward", "{}"),
            item("c", Some("b"), 3, "merge_forward", "{}"),
            item("d", Some("c"), 4, "text", &text("太深了")),
            // 指回自己的环
            item("om_root", Some("c"), 5, "merge_forward", "{}"),
        ];
        let forward = build("om_root", &items, None, &name);
        assert_eq!(forward.lines.len(), 3, "{:?}", forward.lines);
        assert!(forward.truncated);
    }

    #[test]
    fn a_huge_forward_is_capped() {
        let mut items = Vec::new();
        for i in 0..500 {
            items.push(item(
                &format!("c{i}"),
                Some("om_root"),
                i,
                "text",
                &text("x"),
            ));
        }
        let forward = build("om_root", &items, None, &name);
        assert_eq!(forward.lines.len(), MAX_LINES);
        assert!(forward.truncated);
    }
}
