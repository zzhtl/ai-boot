//! 输出契约：Agent 按 `schemas/answer.json` 给出结构化答案，卡片由它确定性地渲染。
//!
//! schema 写成 OpenAI strict 兼容的子集（所有字段都在 required 里、每层对象
//! `additionalProperties: false`、不用长度类关键字），Claude 和 Codex 共用一份。
//! 长度上限在渲染时封顶。
//!
//! 例外是段落里的 images、charts、diagrams：大多数段落用不上，模型在长任务里常把它们
//! 整个省掉，整份答案就会被 CLI 拒收、重写一遍（实测多花 20～30 秒），所以不放进
//! required，解析时按空数组处理。接 Codex 的 strict 模式时要放回去。

use std::collections::BTreeMap;
use std::sync::LazyLock;

use ai_boot_agent::Outcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const SCHEMA_TEXT: &str = include_str!("schemas/answer.json");

static SCHEMA: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(SCHEMA_TEXT).unwrap_or(Value::Null));

pub fn schema() -> &'static Value {
    &SCHEMA
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Diagnosis,
    Summary,
    Howto,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Answered,
    NeedMoreInfo,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub topic: String,
    pub claim: String,
    pub adopted: String,
    #[serde(default)]
    pub sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    pub body: String,
    /// 旧答案里没有这三项。
    #[serde(default)]
    pub images: Vec<SectionImage>,
    #[serde(default)]
    pub charts: Vec<Chart>,
    #[serde(default)]
    pub diagrams: Vec<Diagram>,
}

/// 段落里展示的截图：工作目录里的附件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SectionImage {
    pub path: String,
    pub caption: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    Line,
    Area,
    Pie,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chart {
    pub title: String,
    #[serde(rename = "type")]
    pub kind: ChartKind,
    pub categories: Vec<String>,
    pub series: Vec<Series>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
}

/// Graphviz DOT 写的流程图，渲染成图片放进卡片。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagram {
    pub title: String,
    pub dot: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reference {
    pub kind: String,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub kind: Kind,
    pub title: String,
    pub status: Status,
    pub confidence: Confidence,
    pub summary: String,
    /// 本轮对上一轮结论的更正；旧答案里没有。
    #[serde(default)]
    pub corrections: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub references: Vec<Reference>,
    #[serde(default)]
    pub jira_keys: Vec<String>,
    /// 截图和流程图上传到飞书后的 image_key（键见 `image_ref`、`diagram_ref`）。
    /// 由我们填，不在输出契约里；随答案落库，旧卡片重画时不用再传一次。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub image_keys: BTreeMap<String, String>,
}

impl Answer {
    /// 截图在 `image_keys` 里的键。
    pub fn image_ref(image: &SectionImage) -> String {
        format!("file:{}", image.path)
    }

    /// 流程图在 `image_keys` 里的键：按内容，同一张图只渲染一次。
    pub fn diagram_ref(diagram: &Diagram) -> String {
        use sha2::{Digest as _, Sha256};
        let digest = Sha256::digest(diagram.dot.as_bytes());
        let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        format!("dot:{hex}")
    }
}

/// 一轮成功执行得到的答复。
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Structured(Answer),
    /// 没给结构化结果，或给的对不上契约：降级成纯文本展示，不丢内容。
    Text(String),
}

/// 从成功的终态里取答复。
pub fn reply_of(outcome: &Outcome) -> Option<Reply> {
    let Outcome::Success {
        structured, text, ..
    } = outcome
    else {
        return None;
    };
    // 结构化结果优先；Codex 的结构化输出在最后一条消息的文本里，同样按 JSON 解析
    let candidate = structured
        .clone()
        .or_else(|| serde_json::from_str::<Value>(text).ok());
    if let Some(value) = candidate
        && let Ok(mut answer) = serde_json::from_value::<Answer>(value)
    {
        // 只认我们自己上传得到的 key：模型输出里带了也不能拿来显示任意图片
        answer.image_keys.clear();
        return Some(Reply::Structured(answer));
    }
    Some(Reply::Text(text.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 有意不放进 required 的字段，原因见模块注释。
    const OPTIONAL: [&str; 3] = [
        "$.sections[].images",
        "$.sections[].charts",
        "$.sections[].diagrams",
    ];

    /// OpenAI strict 模式的约束：每个对象的属性都在 required 里（`OPTIONAL` 除外），
    /// 且不允许额外属性。
    fn assert_strict(node: &Value, path: &str) {
        if node.get("type") == Some(&Value::String("object".into())) {
            let props = node["properties"].as_object().expect("对象要有 properties");
            let required: Vec<&str> = node["required"]
                .as_array()
                .expect("对象要有 required")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for key in props.keys() {
                let field = format!("{path}.{key}");
                assert_eq!(
                    required.contains(&key.as_str()),
                    !OPTIONAL.contains(&field.as_str()),
                    "{field} 是否在 required 里不对"
                );
            }
            assert_eq!(
                node["additionalProperties"], false,
                "{path} 缺 additionalProperties:false"
            );
            for (key, child) in props {
                assert_strict(child, &format!("{path}.{key}"));
            }
        }
        if let Some(items) = node.get("items") {
            assert_strict(items, &format!("{path}[]"));
        }
        for banned in [
            "maxLength",
            "minLength",
            "maxItems",
            "minItems",
            "pattern",
            "format",
        ] {
            assert!(
                node.get(banned).is_none(),
                "{path} 用了 strict 不支持的 {banned}"
            );
        }
    }

    #[test]
    fn the_schema_is_openai_strict_compatible() {
        assert!(schema().is_object(), "schema 必须是合法 JSON");
        assert_strict(schema(), "$");
    }

    fn sample() -> Value {
        serde_json::json!({
            "kind": "diagnosis", "title": "登录偶发 500", "status": "answered", "confidence": "medium",
            "summary": "连接池耗尽", "conflicts": [], "open_questions": [], "jira_keys": ["ABC-1"],
            "sections": [{"title": "根因", "body": "……"}],
            "references": [{"kind": "jira", "title": "ABC-1", "url": "https://jira.example.com/browse/ABC-1"}]
        })
    }

    #[test]
    fn image_keys_in_the_model_output_are_ignored() {
        let mut value = sample();
        value["image_keys"] = serde_json::json!({"file:x": "img_forged"});
        let outcome = Outcome::Success {
            structured: Some(value),
            text: String::new(),
            turns: 1,
        };
        let Some(Reply::Structured(answer)) = reply_of(&outcome) else {
            panic!("应当是结构化答案");
        };
        assert!(answer.image_keys.is_empty());
    }

    #[test]
    fn a_structured_answer_is_preferred() {
        let outcome = Outcome::Success {
            structured: Some(sample()),
            text: "ignored".into(),
            turns: 3,
        };
        let Some(Reply::Structured(answer)) = reply_of(&outcome) else {
            panic!("应当是结构化答案");
        };
        assert_eq!(answer.status, Status::Answered);
        assert_eq!(answer.sections.len(), 1);
    }

    #[test]
    fn json_in_the_text_is_accepted_for_codex() {
        let outcome = Outcome::Success {
            structured: None,
            text: sample().to_string(),
            turns: 1,
        };
        assert!(matches!(reply_of(&outcome), Some(Reply::Structured(_))));
    }

    #[test]
    fn anything_else_degrades_to_text_without_losing_it() {
        let outcome = Outcome::Success {
            structured: Some(serde_json::json!({"unexpected": true})),
            text: "这是模型的原文".into(),
            turns: 1,
        };
        assert_eq!(
            reply_of(&outcome),
            Some(Reply::Text("这是模型的原文".into()))
        );
        assert_eq!(reply_of(&Outcome::Interrupted), None);
    }
}
