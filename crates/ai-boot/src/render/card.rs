//! 飞书卡片（JSON 2.0）的拼装。
//!
//! 卡片里所有来自模型或用户的文字都先脱敏、再经 markdown 规范化；标题一律用
//! plain_text。尺寸按序列化后的字节数控制在 28 KB 以内（飞书上限 30 KB）。

use std::time::Duration;

use serde_json::{Value, json};

use super::markdown::{LinkPolicy, escape, render};
use super::redact::redact;
use crate::answer::{Answer, Chart, ChartKind, Confidence, Kind, Section, Status};
use crate::callback::Action;

const MAX_CARD_BYTES: usize = 28 * 1024;
const TITLE_CHARS: usize = 40;
const SUMMARY_CHARS: usize = 600;
const SECTION_CHARS: usize = 4000;
const MAX_SECTIONS: usize = 8;
const MAX_LIST_ITEMS: usize = 8;
/// 露在外面的更正条数：多了就不「简洁」了，细节在正文里。
const MAX_CORRECTIONS: usize = 3;
const MAX_REFERENCES: usize = 20;
const MIN_SECTION_CHARS: usize = 200;
const TRUNCATED: &str = "\n\n…（内容过长，已截断）";
/// 进度卡上露出的最近几步：卡片最后会换成答案，不必把过程全摆出来。
const RECENT_STEPS: usize = 6;
/// 一张卡片最多放的图片（截图和流程图合计）和图表；飞书建议图表不超过 5 个。
pub const MAX_IMAGES: usize = 6;
const MAX_CHARTS: usize = 5;
const MAX_CHART_CATEGORIES: usize = 30;
const MAX_CHART_SERIES: usize = 6;
const LABEL_CHARS: usize = 40;

/// 卡片上答案之外的部分：底部的轮次、用时，追问的输入框，以及卡片输入框里提的问题。
/// 模型、token 这些是运维信息，看日志。
#[derive(Debug, Clone, Default)]
pub struct Footer {
    pub turn: u32,
    /// 底部的补充说明（比如写回前会确认），空的不显示。
    pub hint: &'static str,
    pub elapsed: Duration,
    /// 给了就在卡片底部放输入框（这一轮的 ID）：输入补充或纠正，回车发送。
    pub follow_up: Option<String>,
    /// 这一轮是在卡片输入框里问的，群里看不到这句话：显示在卡片顶上。
    pub asked: Option<String>,
}

impl Footer {
    fn line(&self) -> String {
        let mut parts = Vec::new();
        if self.turn > 1 {
            parts.push(format!("第 {} 轮", self.turn));
        }
        parts.push(format!("用时 {}", human_duration(self.elapsed)));
        if !self.hint.is_empty() {
            parts.push(self.hint.to_owned());
        }
        format!("<font color='grey'>{}</font>", parts.join(" · "))
    }

    /// 顶上的「追问：……」。
    fn asked_line(&self) -> Option<Value> {
        let asked = self.asked.as_deref()?.trim();
        (!asked.is_empty()).then(|| {
            markdown(format!(
                "<font color='grey'>追问：</font>{}",
                escape(&truncate(&redact(asked), 300))
            ))
        })
    }

    /// 底部的输入框：点进去写补充或纠正，回车发送，接着这个会话问。
    fn input(&self) -> Option<Value> {
        let turn_id = self.follow_up.as_deref()?;
        Some(json!({
            "tag": "input",
            "element_id": "follow_up",
            "name": "follow_up",
            "placeholder": { "tag": "plain_text", "content": "补充或纠正，回车发送" },
            "width": "fill",
            "max_length": 1000,
            "behaviors": [{ "type": "callback", "value": Action::FollowUp.value(turn_id) }],
        }))
    }
}

/// plain_text 字段（标题、副标题、会话列表预览）：飞书按字面显示，不解析标签，
/// 但这一点没有实测过，所以不赌——尖括号换成全角，内容照样看得懂。
fn plain(text: &str, limit: usize) -> String {
    truncate(&redact(text), limit)
        .replace('<', "＜")
        .replace('>', "＞")
}

fn card(title: &str, subtitle: &str, template: &str, elements: Vec<Value>) -> Value {
    let title = plain(title, TITLE_CHARS);
    let subtitle = plain(subtitle, 60);
    json!({
        "schema": "2.0",
        "config": {
            "update_multi": true,
            // 默认最宽 600px，只占半屏；撑满才看得舒服
            "width_mode": "fill",
            "summary": { "content": &title },
        },
        "header": {
            "title": { "tag": "plain_text", "content": &title },
            "subtitle": { "tag": "plain_text", "content": &subtitle },
            "template": template,
        },
        "body": { "elements": elements },
    })
}

fn markdown(content: String) -> Value {
    json!({ "tag": "markdown", "content": content })
}

fn markdown_blocks(text: &str, links: &dyn LinkPolicy) -> Vec<Value> {
    render(&redact(text), links)
        .blocks
        .into_iter()
        .map(markdown)
        .collect()
}

fn panel(title: &str, expanded: bool, elements: Vec<Value>) -> Value {
    json!({
        "tag": "collapsible_panel",
        "expanded": expanded,
        "header": {
            "title": { "tag": "markdown", "content": format!("**{}**", escape(&redact(title))) },
            "vertical_align": "center",
        },
        "border": { "color": "grey", "corner_radius": "5px" },
        "vertical_spacing": "8px",
        "padding": "8px 8px 8px 8px",
        "elements": elements,
    })
}

/// 回调按钮。`element_id` 在卡片内唯一，只能用字母、数字和下划线。
/// 回调按钮。`element_id` 在卡片内唯一，只能用字母、数字和下划线；`confirm`
/// 给了就先弹窗确认（标题、正文）。
fn button(
    label: &str,
    style: &str,
    element_id: &str,
    action: &Action,
    turn_id: &str,
    confirm: Option<(&str, &str)>,
) -> Value {
    let mut button = json!({
        "tag": "button",
        "element_id": element_id,
        "type": style,
        "size": "small",
        "text": { "tag": "plain_text", "content": plain(label, 40) },
        "behaviors": [{ "type": "callback", "value": action.value(turn_id) }],
    });
    if let Some((title, text)) = confirm {
        button["confirm"] = json!({
            "title": { "tag": "plain_text", "content": plain(title, 40) },
            "text": { "tag": "plain_text", "content": plain(text, 200) },
        });
    }
    button
}

/// 进度卡上显示的内容。
#[derive(Debug, Clone, Copy, Default)]
pub struct Progress<'a> {
    pub question: &'a str,
    /// 读了什么、没读到什么，每行一条。
    pub read: &'a [String],
    pub steps: &'a [String],
    pub elapsed: Duration,
    pub notes: &'a [String],
    /// 这一轮的 ID，给了就带「停止」按钮。
    pub stop: Option<&'a str>,
}

/// 分析中。
pub fn progress(view: &Progress<'_>) -> Value {
    let Progress {
        question,
        read,
        steps,
        elapsed,
        notes,
        stop,
    } = *view;
    // 卡片引用着提问，副标题里也有，正文不再重复
    let mut elements = Vec::new();
    if !read.is_empty() {
        let lines: Vec<String> = read
            .iter()
            .map(|line| escape(&truncate(&redact(line), 200)))
            .collect();
        elements.push(markdown(lines.join("\n")));
    }
    if !steps.is_empty() {
        let recent: Vec<String> = steps
            .iter()
            .rev()
            .take(RECENT_STEPS)
            .rev()
            .map(|s| format!("- {s}"))
            .collect();
        elements.push(markdown(recent.join("\n")));
    }
    let mut status = format!("⏱ 已用时 {}", human_duration(elapsed));
    for note in notes {
        status.push_str(" · ");
        status.push_str(note);
    }
    elements.push(markdown(format!("<font color='grey'>{status}</font>")));
    if let Some(turn_id) = stop {
        elements.push(button(
            "⏹ 停止",
            "default",
            "stop_btn",
            &Action::Stop,
            turn_id,
            Some(("停止这一轮分析？", "停止后可以在卡片上点重试。")),
        ));
    }
    card("⏳ 分析中", question, "wathet", elements)
}

/// 结构化答案：顶上一段概述，详细内容全部折叠。
pub fn answer(answer: &Answer, footer: &Footer, links: &dyn LinkPolicy) -> Value {
    answer_within(answer, footer, links, MAX_CARD_BYTES)
}

/// 结论已被后面某一轮更正的旧答案卡：内容保留，顶上注明以最新回复为准，头部置灰，
/// 免得群里翻到旧卡片的人照着错的结论去做。
pub fn superseded(
    answer: &Answer,
    footer: &Footer,
    links: &dyn LinkPolicy,
    corrected_in: u32,
) -> Value {
    let mut card = answer_within(answer, footer, links, MAX_CARD_BYTES - 1024);
    card["header"]["template"] = json!("grey");
    if let Some(elements) = card["body"]["elements"].as_array_mut() {
        elements.insert(
            0,
            markdown(format!(
                "⚠️ **这条结论已在第 {corrected_in} 轮更正，以最新回复为准**"
            )),
        );
    }
    card
}

/// 写回目标在闭环卡片上的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WritebackState {
    Ready,
    Running,
    Done(Option<String>),
    Failed(String),
    Unknown(String),
}

/// 闭环卡片上的一个写回目标。
#[derive(Debug, Clone)]
pub struct WritebackView {
    pub label: String,
    /// 点下去的动作（写回）。
    pub action: Action,
    pub state: WritebackState,
}

/// 闭环方案：绿色头，正文同答案卡，底下是各个写回目标。
pub fn closure(
    answer: &Answer,
    footer: &Footer,
    links: &dyn LinkPolicy,
    turn_id: &str,
    writebacks: &[WritebackView],
) -> Value {
    // 写回区大约占几 KB，给它留出位置
    let mut card = answer_within(answer, footer, links, MAX_CARD_BYTES - 4 * 1024);
    let title = plain(&format!("✅ 闭环方案：{}", answer.title), TITLE_CHARS);
    card["header"]["title"]["content"] = json!(title);
    card["header"]["template"] = json!("green");
    card["config"]["summary"]["content"] = json!(title);
    let Some(elements) = card["body"]["elements"].as_array_mut() else {
        return card;
    };
    if !writebacks.is_empty() {
        elements.push(markdown("**写回**".to_owned()));
    }
    for (i, view) in writebacks.iter().enumerate() {
        let label = escape(&truncate(&redact(&view.label), 80));
        let line = match &view.state {
            WritebackState::Ready => None,
            WritebackState::Running => Some(format!("⏳ 正在写入 {label}…")),
            WritebackState::Done(Some(url)) if links.allows(url) => Some(format!(
                "✅ 已写入 {label}（[查看]({})）",
                url.replace(')', "%29").replace(' ', "%20")
            )),
            WritebackState::Done(_) => Some(format!("✅ 已写入 {label}")),
            WritebackState::Failed(error) => Some(format!(
                "❌ {label} 写入失败：{}",
                escape(&truncate(&redact(error), 200))
            )),
            WritebackState::Unknown(error) => Some(format!(
                "⚠️ {label} 结果不明：{}",
                escape(&truncate(&redact(error), 200))
            )),
        };
        if let Some(line) = line {
            elements.push(markdown(line));
        }
        let (button_label, show) = match view.state {
            WritebackState::Ready => (format!("写入 {}", view.label), true),
            WritebackState::Failed(_) | WritebackState::Unknown(_) => {
                (format!("重试写入 {}", view.label), true)
            }
            WritebackState::Running | WritebackState::Done(_) => (String::new(), false),
        };
        if show {
            elements.push(button(
                &button_label,
                "default",
                &format!("writeback_{i}"),
                &view.action,
                turn_id,
                Some((
                    "写回闭环方案？",
                    &format!("将把闭环方案写入 {}。", view.label),
                )),
            ));
        }
    }
    card
}

fn answer_within(answer: &Answer, footer: &Footer, links: &dyn LinkPolicy, budget: usize) -> Value {
    let mut bodies: Vec<String> = answer
        .sections
        .iter()
        .take(MAX_SECTIONS)
        .map(|s| truncate(&s.body, SECTION_CHARS))
        .collect();
    let mut keep = Keep {
        charts: true,
        references: true,
    };
    loop {
        let built = build_answer(answer, &bodies, keep, footer, links);
        if serde_json::to_string(&built).map_or(0, |s| s.len()) <= budget {
            return built;
        }
        // 超长：把最长的一段减半；还压不下去就去掉图表（数据点占地方），再去掉参考来源
        let longest = bodies
            .iter_mut()
            .max_by_key(|body| body.chars().count())
            .filter(|body| body.chars().count() > MIN_SECTION_CHARS);
        match longest {
            Some(body) => {
                let half = body.chars().count() / 2;
                *body = format!("{}{TRUNCATED}", body.chars().take(half).collect::<String>());
            }
            None if keep.charts => keep.charts = false,
            None if keep.references => keep.references = false,
            None => return built,
        }
    }
}

/// 卡片放不下时依次舍掉的部分。
#[derive(Debug, Clone, Copy)]
struct Keep {
    charts: bool,
    references: bool,
}

/// 这张卡片上还能放几张图、几个图表。
struct Room {
    images: usize,
    charts: usize,
}

fn build_answer(
    answer: &Answer,
    bodies: &[String],
    keep: Keep,
    footer: &Footer,
    links: &dyn LinkPolicy,
) -> Value {
    // 露在外面的只有概述（和本轮的更正）；其余全部折叠且默认收起，群里也不会刷屏
    let mut elements: Vec<Value> = footer.asked_line().into_iter().collect();
    elements.extend(markdown_blocks(
        &truncate(&answer.summary, SUMMARY_CHARS),
        links,
    ));
    if !answer.corrections.is_empty() {
        let items: Vec<String> = answer
            .corrections
            .iter()
            .take(MAX_CORRECTIONS)
            .map(|c| format!("- {}", inline(c)))
            .collect();
        elements.push(markdown(format!("**🔄 本轮更正**\n{}", items.join("\n"))));
    }

    // 正文（根因、方案……）在前，其次是要对方补充的，再是冲突和来源
    let mut room = Room {
        images: MAX_IMAGES,
        charts: if keep.charts { MAX_CHARTS } else { 0 },
    };
    for (section, body) in answer.sections.iter().zip(bodies) {
        let mut inner = markdown_blocks(body, links);
        inner.extend(visuals(answer, section, &mut room));
        elements.push(panel(&section.title, false, inner));
    }

    if !answer.open_questions.is_empty() {
        let items: Vec<String> = answer
            .open_questions
            .iter()
            .take(MAX_LIST_ITEMS)
            .map(|q| format!("- {}", inline(q)))
            .collect();
        let title = format!("❓ 需要确认或补充（{}）", answer.open_questions.len());
        elements.push(panel(&title, false, vec![markdown(items.join("\n"))]));
    }

    if !answer.conflicts.is_empty() {
        let items: Vec<String> = answer
            .conflicts
            .iter()
            .take(MAX_LIST_ITEMS)
            .map(|c| {
                let mut line = format!(
                    "- {}：{} → 采信：{}",
                    inline(&c.topic),
                    inline(&c.claim),
                    inline(&c.adopted)
                );
                if !c.sources.is_empty() {
                    line.push_str(&format!("（依据：{}）", inline(&c.sources.join("、"))));
                }
                line
            })
            .collect();
        let title = format!("⚠️ 信息冲突（{}）", answer.conflicts.len());
        elements.push(panel(&title, false, vec![markdown(items.join("\n"))]));
    }

    if keep.references && !answer.references.is_empty() {
        let items: Vec<String> = answer
            .references
            .iter()
            .take(MAX_REFERENCES)
            .map(|r| {
                let title = inline(&r.title);
                if links.allows(&r.url) {
                    format!(
                        "- [{title}]({})",
                        r.url.replace(')', "%29").replace(' ', "%20")
                    )
                } else {
                    format!("- {title}")
                }
            })
            .collect();
        elements.push(panel(
            &format!("📎 参考来源（{}）", answer.references.len()),
            false,
            vec![markdown(items.join("\n"))],
        ));
    }

    elements.push(markdown(footer.line()));
    elements.extend(footer.input());
    // 副标题只说结论到了哪一步；把握度只在不高的时候提
    let diagnosis = answer.kind == Kind::Diagnosis;
    let (template, badge) = match answer.status {
        Status::Answered if diagnosis => ("blue", "根因已确认"),
        Status::Answered => ("blue", kind_label(answer.kind)),
        Status::NeedMoreInfo => ("orange", "需要补充信息"),
        Status::Partial if diagnosis => ("orange", "根因待确认"),
        Status::Partial => ("orange", "部分结论"),
    };
    let subtitle = match answer.confidence {
        Confidence::High => badge.to_owned(),
        other => format!("{badge} · 把握：{}", confidence_label(other)),
    };
    card(&answer.title, &subtitle, template, elements)
}

/// 段落里的截图、图表、流程图。截图和流程图要先上传拿到 image_key，没拿到的不画。
fn visuals(answer: &Answer, section: &Section, room: &mut Room) -> Vec<Value> {
    let mut out = Vec::new();
    for image in &section.images {
        if room.images == 0 {
            break;
        }
        if let Some(key) = answer.image_keys.get(&Answer::image_ref(image)) {
            room.images -= 1;
            out.extend(picture(key, &image.caption, "crop_top"));
        }
    }
    for chart in &section.charts {
        if room.charts == 0 {
            break;
        }
        if let Some(element) = chart_element(chart) {
            room.charts -= 1;
            out.push(element);
        }
    }
    for diagram in &section.diagrams {
        if room.images == 0 {
            break;
        }
        if let Some(key) = answer.image_keys.get(&Answer::diagram_ref(diagram)) {
            room.images -= 1;
            // 流程图裁了就看不懂，完整显示
            out.extend(picture(key, &diagram.title, "fit_horizontal"));
        }
    }
    out
}

/// 图片加一行灰色说明。截图按顶部裁到 16:9 以内（长截图才不会占满一屏），点开看原图。
fn picture(key: &str, caption: &str, scale: &str) -> Vec<Value> {
    let mut out = vec![json!({
        "tag": "img",
        "img_key": key,
        "alt": { "tag": "plain_text", "content": plain(caption, 100) },
        "scale_type": scale,
        "size": "stretch",
        "preview": true,
        "corner_radius": "4px",
    })];
    if !caption.trim().is_empty() {
        out.push(markdown(format!(
            "<font color='grey'>{}</font>",
            escape(&truncate(&redact(caption), 100))
        )));
    }
    out
}

/// 飞书卡片的图表组件（VChart）。数据由我们拼，模型只给分类和数值；
/// 数值不是有限数的丢掉，对不上的不画。
fn chart_element(chart: &Chart) -> Option<Value> {
    let label = |text: &str| plain(text, LABEL_CHARS);
    let categories: Vec<String> = chart
        .categories
        .iter()
        .take(MAX_CHART_CATEGORIES)
        .map(|c| label(c))
        .collect();
    let title = json!({ "text": label(&chart.title) });
    let (spec, aspect) = match chart.kind {
        ChartKind::Pie => {
            let series = chart.series.first()?;
            let values: Vec<Value> = categories
                .iter()
                .zip(&series.values)
                .filter(|(_, v)| v.is_finite() && **v >= 0.0)
                .map(|(c, v)| json!({ "category": c, "value": v }))
                .collect();
            if values.is_empty() {
                return None;
            }
            let spec = json!({
                "type": "pie",
                "title": title,
                "data": { "values": values },
                "categoryField": "category",
                "valueField": "value",
                "outerRadius": 0.8,
                "label": { "visible": true },
                "legends": { "visible": true, "orient": "right" },
            });
            (spec, "4:3")
        }
        kind => {
            let series: Vec<_> = chart.series.iter().take(MAX_CHART_SERIES).collect();
            let values: Vec<Value> = series
                .iter()
                .flat_map(|s| {
                    let name = label(&s.name);
                    categories
                        .iter()
                        .zip(&s.values)
                        .filter(|(_, v)| v.is_finite())
                        .map(move |(c, v)| json!({ "x": c, "y": v, "s": name }))
                })
                .collect();
            if values.is_empty() {
                return None;
            }
            let several = series.len() > 1;
            let (kind, x_field) = match kind {
                // 多组柱子并排，分类和系列一起作为横轴
                ChartKind::Bar if several => ("bar", json!(["x", "s"])),
                ChartKind::Bar => ("bar", json!("x")),
                ChartKind::Line => ("line", json!("x")),
                _ => ("area", json!("x")),
            };
            let spec = json!({
                "type": kind,
                "title": title,
                "data": { "values": values },
                "xField": x_field,
                "yField": "y",
                "seriesField": "s",
                "legends": { "visible": several, "orient": "bottom" },
            });
            (spec, "16:9")
        }
    };
    Some(json!({
        "tag": "chart",
        "chart_spec": spec,
        "aspect_ratio": aspect,
        "preview": true,
    }))
}

/// 去掉卡片里的图表和图片（包括折叠面板里的）。卡片被飞书拒收时的退路。
pub fn without_visuals(card: &Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Array(items) => {
                items.retain(|item| !matches!(item["tag"].as_str(), Some("chart" | "img")));
                items.iter_mut().for_each(strip);
            }
            Value::Object(map) => map.values_mut().for_each(strip),
            _ => {}
        }
    }
    let mut card = card.clone();
    strip(&mut card);
    card
}

/// 没有结构化结果时的纯文本答复。
pub fn text_reply(text: &str, footer: &Footer, links: &dyn LinkPolicy) -> Value {
    let mut elements: Vec<Value> = footer.asked_line().into_iter().collect();
    elements.extend(markdown_blocks(&truncate(text, SECTION_CHARS * 2), links));
    elements.push(markdown(footer.line()));
    elements.extend(footer.input());
    card("分析结果", "未按约定格式输出，按原文展示", "blue", elements)
}

/// 失败、超时、中断。`retry` 是这一轮的 ID，给了就带「重试」按钮。
pub fn failure(title: &str, reason: &str, hint: &str, retry: Option<&str>) -> Value {
    let mut elements = vec![markdown(escape(&truncate(&redact(reason), 1000)))];
    if !hint.is_empty() {
        elements.push(markdown(format!(
            "<font color='grey'>{}</font>",
            escape(hint)
        )));
    }
    if let Some(turn_id) = retry {
        elements.push(button(
            "🔁 重试",
            "primary",
            "retry_btn",
            &Action::Retry,
            turn_id,
            None,
        ));
    }
    card(title, "本轮没有完成", "red", elements)
}

/// 私聊给提问人：授权以他的身份读取云文档。按钮指向本服务的发起接口，
/// 点的时候才生成授权参数，卡片放多久都能用。
pub fn authorize(start_link: &str) -> Value {
    let elements = vec![
        markdown(
            "群里贴的飞书云文档需要**以你的身份**读取，授权一次即可，之后自动续期（最长 365 天）。\n\
             授权只包含只读权限；读到的文档内容只用于回答你们的提问。"
                .to_owned(),
        ),
        json!({
            "tag": "button",
            "element_id": "authorize_btn",
            "type": "primary",
            "size": "medium",
            "text": { "tag": "plain_text", "content": "去授权" },
            "behaviors": [{ "type": "open_url", "default_url": start_link }],
        }),
        markdown("<font color='grey'>授权链接需要能访问到机器人所在的内网服务。</font>".to_owned()),
    ];
    card(
        "🔐 授权读取云文档",
        "机器人需要你的授权",
        "indigo",
        elements,
    )
}

fn kind_label(kind: Kind) -> &'static str {
    match kind {
        Kind::Diagnosis => "问题排查",
        Kind::Summary => "讨论总结",
        Kind::Howto => "用法说明",
        Kind::Other => "问答",
    }
}

fn confidence_label(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::High => "高",
        Confidence::Medium => "中",
        Confidence::Low => "低",
    }
}

/// 列表项里的单行文字：脱敏、转义、压成一行。
fn inline(text: &str) -> String {
    let flat: String = redact(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    escape(&truncate(&flat, 300))
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

pub fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::{Conflict, Reference, Section};
    use crate::render::markdown::HostAllowlist;

    fn footer() -> Footer {
        Footer {
            turn: 1,
            elapsed: Duration::from_secs(135),
            follow_up: Some("t-1".into()),
            ..Footer::default()
        }
    }

    fn answer_with(sections: Vec<Section>) -> Answer {
        Answer {
            kind: Kind::Diagnosis,
            title: "登录偶发 500".into(),
            status: Status::Answered,
            confidence: Confidence::Medium,
            summary: "连接池耗尽导致".into(),
            corrections: vec![],
            conflicts: vec![],
            sections,
            open_questions: vec![],
            references: vec![],
            jira_keys: vec![],
            image_keys: Default::default(),
        }
    }

    fn section(title: &str, body: &str) -> Section {
        Section {
            title: title.into(),
            body: body.into(),
            images: vec![],
            charts: vec![],
            diagrams: vec![],
        }
    }

    fn render_answer(answer: &Answer) -> Value {
        let hosts = vec!["jira.example.com".to_owned()];
        super::answer(answer, &footer(), &HostAllowlist(&hosts))
    }

    fn tags(card: &Value) -> Vec<String> {
        card["body"]["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .map(|e| e["tag"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    /// 露在外面的只有概述和底部信息，其余全部折叠且默认收起；卡片撑满宽度。
    #[test]
    fn only_the_summary_shows_and_everything_else_is_folded() {
        let mut a = answer_with(vec![
            section("根本原因", "连接池只有 10"),
            section("解决方案", "调到 50"),
        ]);
        a.conflicts = vec![Conflict {
            topic: "影响版本".into(),
            claim: "群聊说 3.2 引入".into(),
            adopted: "以 Jira 为准：3.1 引入".into(),
            sources: vec!["ABC-1".into()],
        }];
        a.open_questions = vec!["需要确认现场的 JDK 版本".into()];
        a.references = vec![Reference {
            kind: "jira".into(),
            title: "ABC-1".into(),
            url: "https://jira.example.com/browse/ABC-1".into(),
        }];
        let card = render_answer(&a);
        assert_eq!(card["schema"], "2.0");
        assert_eq!(card["config"]["update_multi"], true);
        assert_eq!(card["config"]["width_mode"], "fill");
        let elements = card["body"]["elements"].as_array().expect("elements");
        assert!(
            elements[0].to_string().contains("连接池耗尽导致"),
            "第一段是概述：{}",
            elements[0]
        );
        let panels: Vec<&Value> = elements
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .collect();
        // 信息冲突、待确认、两段正文、参考来源
        assert_eq!(panels.len(), 5, "{card}");
        assert!(panels.iter().all(|p| p["expanded"] == false), "{card}");
        let text = card.to_string();
        assert!(text.contains("信息冲突（1）"));
        assert!(text.contains("需要确认或补充（1）"));
        let visible = elements.iter().filter(|e| e["tag"] == "markdown").count();
        assert_eq!(visible, 2, "露在外面的只有概述和底部信息：{card}");
        assert!(!tags(&card).iter().any(|t| t == "button"), "答案卡不带按钮");
    }

    /// 排查类的状态直接说根因查到哪一步；更正露在概述下面，不折叠。
    #[test]
    fn diagnosis_status_says_whether_the_cause_is_confirmed_and_corrections_show() {
        let mut a = answer_with(vec![section("可能原因与验证方法", "1. DNS 2. 连接池")]);
        a.status = Status::Partial;
        let card = render_answer(&a);
        assert!(
            card["header"]["subtitle"]["content"]
                .as_str()
                .is_some_and(|t| t.contains("根因待确认"))
        );
        a.status = Status::Answered;
        a.corrections = vec!["原来说是连接池耗尽 → 实际是 DNS 超时".into()];
        let card = render_answer(&a);
        assert!(
            card["header"]["subtitle"]["content"]
                .as_str()
                .is_some_and(|t| t.contains("根因已确认"))
        );
        let elements = card["body"]["elements"].as_array().expect("elements");
        assert_eq!(elements[1]["tag"], "markdown", "更正紧跟在概述后面，不折叠");
        assert!(elements[1].to_string().contains("本轮更正"));
        assert!(elements[1].to_string().contains("DNS 超时"));
    }

    /// 截图、图表、流程图放在各自的段落里（默认折叠）；没拿到 image_key 的不画。
    #[test]
    fn visuals_render_inside_their_section() {
        let mut a = answer_with(vec![
            section("根本原因", "见截图"),
            section("解决方案", "调大"),
        ]);
        a.sections[0].images = vec![
            crate::answer::SectionImage {
                path: "attachments/1/00-image.webp".into(),
                caption: "报错截图".into(),
            },
            crate::answer::SectionImage {
                path: "attachments/1/01-image.webp".into(),
                caption: "没上传成功".into(),
            },
        ];
        a.sections[0].diagrams = vec![crate::answer::Diagram {
            title: "调用链".into(),
            dot: "digraph { 网关 -> 订单 }".into(),
        }];
        a.sections[1].charts = vec![Chart {
            title: "各版本失败次数".into(),
            kind: ChartKind::Bar,
            categories: vec!["3.2.0".into(), "3.2.1".into()],
            series: vec![crate::answer::Series {
                name: "失败".into(),
                values: vec![2.0, f64::NAN],
            }],
        }];
        a.image_keys.insert(
            Answer::image_ref(&a.sections[0].images[0]),
            "img_shot".into(),
        );
        a.image_keys.insert(
            Answer::diagram_ref(&a.sections[0].diagrams[0]),
            "img_dot".into(),
        );
        let card = render_answer(&a);
        let panels: Vec<&Value> = card["body"]["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .filter(|e| e["tag"] == "collapsible_panel")
            .collect();
        let first: Vec<&Value> = panels[0]["elements"]
            .as_array()
            .expect("段落")
            .iter()
            .collect();
        let images: Vec<&&Value> = first.iter().filter(|e| e["tag"] == "img").collect();
        assert_eq!(images.len(), 2, "截图和流程图各一张，没上传成功的不画");
        assert_eq!(images[0]["img_key"], "img_shot");
        assert_eq!(images[0]["scale_type"], "crop_top");
        assert_eq!(images[1]["img_key"], "img_dot");
        assert_eq!(images[1]["scale_type"], "fit_horizontal", "流程图完整显示");
        let chart = panels[1]["elements"]
            .as_array()
            .expect("段落")
            .iter()
            .find(|e| e["tag"] == "chart")
            .expect("图表");
        assert_eq!(chart["chart_spec"]["type"], "bar");
        let values = chart["chart_spec"]["data"]["values"]
            .as_array()
            .expect("数据");
        assert_eq!(values.len(), 1, "不是有限数的值丢掉：{values:?}");
        assert_eq!(values[0]["x"], "3.2.0");
    }

    /// 少即是多：副标题只说结论到了哪一步，把握度只在不高时提；底部没有模型和 token。
    #[test]
    fn visuals_can_be_stripped_when_the_card_is_rejected() {
        let mut a = answer_with(vec![section("根本原因", "见截图")]);
        a.sections[0].images = vec![crate::answer::SectionImage {
            path: "attachments/1/00-image.webp".into(),
            caption: "报错截图".into(),
        }];
        a.image_keys.insert(
            Answer::image_ref(&a.sections[0].images[0]),
            "img_shot".into(),
        );
        let card = render_answer(&a);
        assert!(card.to_string().contains("img_shot"));
        let plain = without_visuals(&card);
        assert!(!plain.to_string().contains("img_shot"));
        assert!(plain.to_string().contains("见截图"), "文字留着");
    }

    #[test]
    fn the_chrome_carries_only_what_readers_need() {
        let mut a = answer_with(vec![section("根本原因", "连接池只有 10")]);
        a.confidence = Confidence::High;
        let card = render_answer(&a);
        assert_eq!(card["header"]["subtitle"]["content"], "根因已确认");
        a.confidence = Confidence::Low;
        let card = render_answer(&a);
        assert_eq!(
            card["header"]["subtitle"]["content"],
            "根因已确认 · 把握：低"
        );
        let elements = card["body"]["elements"].as_array().expect("elements");
        assert_eq!(
            elements.last().map(|e| &e["tag"]),
            Some(&json!("input")),
            "最底下是输入框"
        );
        let footer = elements[elements.len() - 2].to_string();
        assert!(footer.contains("用时 2m15s"), "{footer}");
        assert!(
            !footer.contains("tokens") && !footer.contains("opus") && !footer.contains("第 1 轮")
        );
    }

    #[test]
    fn a_superseded_answer_is_greyed_and_points_to_the_latest_reply() {
        let hosts = vec!["jira.example.com".to_owned()];
        let card = superseded(
            &answer_with(vec![section("根本原因", "连接池只有 10")]),
            &footer(),
            &HostAllowlist(&hosts),
            3,
        );
        assert_eq!(card["header"]["template"], "grey");
        assert!(
            card["body"]["elements"][0]
                .to_string()
                .contains("这条结论已在第 3 轮更正")
        );
        assert!(card.to_string().contains("连接池只有 10"), "原来的内容还在");
    }

    #[test]
    fn a_short_answer_is_folded_too() {
        let card = render_answer(&answer_with(vec![section("根本原因", "改配置即可")]));
        let panels: Vec<String> = tags(&card)
            .into_iter()
            .filter(|t| t == "collapsible_panel")
            .collect();
        assert_eq!(panels.len(), 1);
    }

    #[test]
    fn model_text_cannot_inject_tags_or_secrets() {
        let mut a = answer_with(vec![section(
            "<at id=all></at>",
            "password=hunter2 <at id=all></at>",
        )]);
        a.title = "<at id=all>标题</at>".into();
        let text = render_answer(&a).to_string();
        assert!(!text.contains("<at"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn references_link_only_to_allowed_hosts() {
        let mut a = answer_with(vec![]);
        a.references = vec![
            Reference {
                kind: "jira".into(),
                title: "ABC-1".into(),
                url: "https://jira.example.com/browse/ABC-1".into(),
            },
            Reference {
                kind: "other".into(),
                title: "外链".into(),
                url: "https://evil.example.net/x".into(),
            },
        ];
        let text = render_answer(&a).to_string();
        assert!(text.contains("(https://jira.example.com/browse/ABC-1)"));
        assert!(!text.contains("evil.example.net"));
    }

    #[test]
    fn oversized_answers_are_shrunk_under_the_limit() {
        let huge = "很长的一段分析。".repeat(2000);
        let a = answer_with(
            (0..8)
                .map(|i| section(&format!("第 {i} 段"), &huge))
                .collect(),
        );
        let card = render_answer(&a);
        let bytes = serde_json::to_string(&card).expect("序列化").len();
        assert!(bytes <= MAX_CARD_BYTES, "{bytes}");
        assert!(card.to_string().contains("已截断"));
    }

    #[test]
    fn progress_and_failure_cards_are_well_formed() {
        let steps: Vec<String> = (0..20).map(|i| format!("步骤 {i}")).collect();
        let card = progress(&Progress {
            question: "<at id=all> 帮我看看",
            steps: &steps,
            elapsed: Duration::from_secs(75),
            ..Progress::default()
        });
        let text = card.to_string();
        assert!(!text.contains("<at"));
        assert!(
            text.contains("步骤 19") && !text.contains("步骤 13"),
            "只露最近几步：{text}"
        );
        assert!(text.contains("1m15s"));
        assert!(!text.contains("**问题**"), "卡片引用着提问，正文不再重复");
        assert!(!tags(&card).iter().any(|t| t == "button"));

        let card = failure(
            "❌ 分析失败",
            "Invalid API key",
            "请在服务器上重新登录 Claude",
            None,
        );
        assert_eq!(card["header"]["template"], "red");
        assert!(!tags(&card).iter().any(|t| t == "button"));
    }

    fn only_button(card: &Value) -> &Value {
        let buttons: Vec<&Value> = card["body"]["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .filter(|e| e["tag"] == "button")
            .collect();
        assert_eq!(buttons.len(), 1, "{card}");
        buttons[0]
    }

    #[test]
    fn stop_asks_for_confirmation_and_carries_the_turn() {
        let read = vec![
            "📥 已读取：3 条消息".to_owned(),
            "⚠️ a.zip：<at id=all>".to_owned(),
        ];
        let card = progress(&Progress {
            question: "q",
            read: &read,
            stop: Some("t-1"),
            ..Progress::default()
        });
        assert!(card.to_string().contains("已读取"));
        assert!(!card.to_string().contains("<at"));
        let stop = only_button(&card);
        assert_eq!(stop["behaviors"][0]["type"], "callback");
        assert_eq!(
            Action::parse(&stop["behaviors"][0]["value"]),
            Some((Action::Stop, "t-1".to_owned()))
        );
        assert_eq!(stop["confirm"]["title"]["tag"], "plain_text");
    }

    #[test]
    fn the_authorization_card_opens_the_start_link() {
        let card = authorize("http://10.0.0.8:18080/oauth/feishu/start?user=ou_boss");
        let button = card["body"]["elements"]
            .as_array()
            .expect("elements")
            .iter()
            .find(|e| e["tag"] == "button")
            .expect("按钮");
        assert_eq!(button["behaviors"][0]["type"], "open_url");
        assert_eq!(
            button["behaviors"][0]["default_url"],
            "http://10.0.0.8:18080/oauth/feishu/start?user=ou_boss"
        );
    }

    #[test]
    fn retry_is_offered_without_confirmation() {
        let card = failure("⏹ 已停止", "本轮分析被中断。", "", Some("t-2"));
        let retry = only_button(&card);
        assert_eq!(
            Action::parse(&retry["behaviors"][0]["value"]),
            Some((Action::Retry, "t-2".to_owned()))
        );
        assert!(retry.get("confirm").is_none());
    }
}
