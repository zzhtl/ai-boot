//! 卡片按钮：按钮上带的回传值，以及回调的应答。
//!
//! 回调只回 toast、不改卡片：卡片统一由这一轮的执行任务（或写回任务）更新，
//! 回调也去改卡片的话，两边的更新会互相覆盖。

use ai_boot_feishu::event::{ToastKind, toast_response};
use serde_json::{Value, json};

/// 写回的目标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Jira,
    Confluence,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jira => "jira",
            Self::Confluence => "confluence",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "jira" => Some(Self::Jira),
            "confluence" => Some(Self::Confluence),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 停止正在跑（或排队）的一轮。
    Stop,
    /// 重跑没拿到答案的最新一轮。
    Retry,
    /// 问题已解决：基于会话生成闭环方案。
    Resolve,
    /// 把闭环方案写回。`reference` 是 Jira 单号或 Confluence 的「空间:父页面」，
    /// `hash` 是卡片上显示的内容摘要。
    Writeback {
        target: Target,
        reference: String,
        hash: String,
    },
    /// 答案卡输入框里的补充、纠正。内容在回调的 `input_value` 里，不在回传值里。
    FollowUp,
}

impl Action {
    /// 按钮 `behaviors` 里 `callback` 的回传值。
    pub fn value(&self, turn_id: &str) -> Value {
        match self {
            Self::Stop => json!({ "action": "stop", "turn": turn_id }),
            Self::Retry => json!({ "action": "retry", "turn": turn_id }),
            Self::Resolve => json!({ "action": "resolve", "turn": turn_id }),
            Self::FollowUp => json!({ "action": "follow_up", "turn": turn_id }),
            Self::Writeback {
                target,
                reference,
                hash,
            } => json!({
                "action": "writeback",
                "turn": turn_id,
                "target": target.as_str(),
                "ref": reference,
                "hash": hash,
            }),
        }
    }

    /// 从回传值里认出是哪个按钮、哪一轮。
    pub fn parse(value: &Value) -> Option<(Self, String)> {
        let field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        let action = match field("action")? {
            "stop" => Self::Stop,
            "retry" => Self::Retry,
            "resolve" => Self::Resolve,
            "follow_up" => Self::FollowUp,
            "writeback" => Self::Writeback {
                target: Target::parse(field("target")?)?,
                reference: field("ref")?.to_owned(),
                hash: field("hash")?.to_owned(),
            },
            _ => return None,
        };
        Some((action, field("turn")?.to_owned()))
    }
}

/// 回调的应答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub kind: ToastKind,
    pub text: String,
}

impl Toast {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Info,
            text: text.into(),
        }
    }

    pub fn success(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Success,
            text: text.into(),
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Warning,
            text: text.into(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            text: text.into(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        toast_response(self.kind, &self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_values_round_trip() {
        let writeback = Action::Writeback {
            target: Target::Jira,
            reference: "ABC-12".into(),
            hash: "0123456789abcdef".into(),
        };
        for action in [Action::Stop, Action::Retry, Action::Resolve, writeback] {
            assert_eq!(
                Action::parse(&action.value("t-1")),
                Some((action, "t-1".to_owned()))
            );
        }
    }

    #[test]
    fn foreign_or_broken_values_are_not_recognised() {
        for value in [
            json!({}),
            json!({"action": "delete", "turn": "t"}),
            json!({"action": "stop"}),
            json!({"action": "stop", "turn": ""}),
            json!({"action": 1, "turn": "t"}),
            json!({"action": "writeback", "turn": "t", "target": "wiki", "ref": "x", "hash": "h"}),
            json!({"action": "writeback", "turn": "t", "target": "jira", "ref": "ABC-1"}),
            json!("stop"),
        ] {
            assert_eq!(Action::parse(&value), None, "{value}");
        }
    }
}
