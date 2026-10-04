//! 进度卡上的步骤日志：把工具调用翻译成一眼能看懂的一行。
//!
//! 参数来自模型（间接来自聊天内容），一律转义并截断后才放进卡片。

use ai_boot_agent::Step;
use serde_json::Value;

use super::markdown::escape;
use super::redact::redact;

const ARG_CHARS: usize = 60;

/// 这一步要不要在进度卡上显示，显示成什么。
pub fn label(step: &Step) -> Option<String> {
    match step {
        Step::ToolCall { tool, input, .. } => Some(tool_label(tool, input)),
        Step::Denied { tool, reason, .. } => Some(format!(
            "⛔ 已拦截 {}：{}",
            escape(&short_tool(tool)),
            short(reason)
        )),
        Step::Retry { error, .. } => Some(format!("⚠️ API 出错，正在重试（{}）", short(error))),
        Step::RateLimited { status, .. } => Some(format!("⚠️ 额度受限（{}）", short(status))),
        Step::ToolDone { .. } | Step::Text(_) => None,
    }
}

fn tool_label(tool: &str, input: &Value) -> String {
    let arg = |key: &str| input.get(key).and_then(Value::as_str).map(short);
    let action = input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = tool.strip_prefix("mcp__qtmcp__").unwrap_or(tool);
    match name {
        "jira_issue" => format!("🎫 Jira {}", arg("key").unwrap_or_default()),
        "jira_search" => format!("🔎 搜索 Jira：{}", arg("jql").unwrap_or_default()),
        "jira_comment" => format!("🎫 Jira 评论 {}", arg("key").unwrap_or_default()),
        "jira_meta" | "jira_agile" | "jira_attachment" => format!("🎫 Jira {}", escape(action)),
        "confluence_search" => format!("📚 搜索 Confluence：{}", arg("cql").unwrap_or_default()),
        "confluence_page" => format!(
            "📚 Confluence 页面 {}",
            arg("title").or_else(|| arg("page_id")).unwrap_or_default()
        ),
        "gitlab_repo" => {
            let project = arg("project").unwrap_or_default();
            match (action, arg("path")) {
                ("search", _) => format!(
                    "🔍 搜索代码 {project}：{}",
                    arg("query").unwrap_or_default()
                ),
                ("commit", _) => format!(
                    "💻 GitLab {project}：提交 {}",
                    arg("git_ref").unwrap_or_default()
                ),
                ("blame", Some(path)) => format!("💻 GitLab {project}：追溯 {path}"),
                (_, Some(path)) => format!("💻 GitLab {project}：{path}"),
                ("compare", None) => format!(
                    "💻 GitLab {project}：对比 {}…{}",
                    arg("from").unwrap_or_default(),
                    arg("to").unwrap_or_default()
                ),
                _ => format!("💻 GitLab {project}：{}", escape(action)),
            }
        }
        "gitlab_project" => format!(
            "💻 GitLab 项目 {}",
            arg("project").or_else(|| arg("search")).unwrap_or_default()
        ),
        "gitlab_mr" => format!("🔀 MR {}", arg("project").unwrap_or_default()),
        "gitlab_pipeline" => format!("🚦 流水线 {}", arg("project").unwrap_or_default()),
        "jenkins_job" => format!(
            "🏗 Jenkins 任务 {}",
            arg("job").or_else(|| arg("search")).unwrap_or_default()
        ),
        "jenkins_build" => match arg("build") {
            Some(build) => format!("🏗 Jenkins 构建 {} #{build}", arg("job").unwrap_or_default()),
            None => format!(
                "🏗 Jenkins 构建 {}：{}",
                arg("job").unwrap_or_default(),
                escape(action)
            ),
        },
        "Read" => format!(
            "📄 阅读 {}",
            input
                .get("file_path")
                .and_then(Value::as_str)
                .map(|p| short(p.rsplit('/').next().unwrap_or(p)))
                .unwrap_or_default()
        ),
        "Grep" => format!("🔍 检索 {}", arg("pattern").unwrap_or_default()),
        // 命令里可能带着 token，先脱敏再上卡片
        "Bash" => format!(
            "⌨️ 执行 {}",
            input
                .get("command")
                .and_then(Value::as_str)
                .map(|c| short(&redact(c)))
                .unwrap_or_default()
        ),
        // 网址的查询串里也可能带 token
        "WebFetch" => format!(
            "🌐 打开 {}",
            input
                .get("url")
                .and_then(Value::as_str)
                .map(|u| short(&redact(u)))
                .unwrap_or_default()
        ),
        "WebSearch" => format!("🔎 搜索网页：{}", arg("query").unwrap_or_default()),
        "Glob" => "📁 列出文件".to_owned(),
        "StructuredOutput" => "📝 整理结论".to_owned(),
        other => format!("🔧 {}", escape(other)),
    }
}

fn short_tool(tool: &str) -> String {
    tool.strip_prefix("mcp__qtmcp__").unwrap_or(tool).to_owned()
}

/// 截断并转义。
fn short(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = flat.chars().take(ARG_CHARS).collect();
    if cut.chars().count() < flat.chars().count() {
        format!("{}…", escape(&cut))
    } else {
        escape(&cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, input: Value) -> Option<String> {
        label(&Step::ToolCall {
            id: "t".into(),
            tool: tool.into(),
            input,
        })
    }

    #[test]
    fn common_tools_read_naturally() {
        assert_eq!(
            call(
                "mcp__qtmcp__jira_issue",
                json!({"action": "get", "key": "ABC-1"})
            )
            .as_deref(),
            Some("🎫 Jira ABC-1")
        );
        assert_eq!(
            call(
                "mcp__qtmcp__gitlab_repo",
                json!({"action": "read_file", "project": "g/p", "path": "src/a.rs"})
            )
            .as_deref(),
            Some("💻 GitLab g/p：src/a.rs")
        );
        assert_eq!(
            call(
                "mcp__qtmcp__gitlab_repo",
                json!({"action": "search", "project": "g/p", "query": "Lock wait timeout"})
            )
            .as_deref(),
            Some("🔍 搜索代码 g/p：Lock wait timeout")
        );
        assert_eq!(
            call(
                "mcp__qtmcp__gitlab_repo",
                json!({"action": "blame", "project": "g/p", "path": "src/a.rs", "start_line": 10})
            )
            .as_deref(),
            Some("💻 GitLab g/p：追溯 src/a.rs")
        );
        assert_eq!(
            call(
                "mcp__qtmcp__gitlab_repo",
                json!({"action": "commit", "project": "g/p", "git_ref": "abc1234"})
            )
            .as_deref(),
            Some("💻 GitLab g/p：提交 abc1234")
        );
        assert_eq!(
            call(
                "Bash",
                json!({"command": "git clone --depth 1 https://github.com/x/y repos/y"})
            )
            .as_deref(),
            Some("⌨️ 执行 git clone --depth 1 https://github.com/x/y repos/y")
        );
        assert_eq!(
            call(
                "WebFetch",
                json!({"url": "https://github.com/x/y", "prompt": "README"})
            )
            .as_deref(),
            Some("🌐 打开 https://github.com/x/y")
        );
        // 命令里的 token 不能上卡片
        let leaked = call(
            "Bash",
            json!({"command": "curl -H 'PRIVATE-TOKEN: glpat-abcdefghijklmnopqrstuv' https://x"}),
        )
        .expect("要显示");
        assert!(!leaked.contains("abcdefghijklmnopqrstuv"), "{leaked}");
        let leaked = call(
            "WebFetch",
            json!({"url": "https://x.io/a?token=abcdefghijklmnop", "prompt": "看看"}),
        )
        .expect("要显示");
        assert!(!leaked.contains("abcdefghijklmnop"), "{leaked}");
        assert_eq!(
            call("Read", json!({"file_path": "/w/context/transcript.md"})).as_deref(),
            Some("📄 阅读 transcript.md")
        );
        assert_eq!(
            call(
                "mcp__qtmcp__jenkins_build",
                json!({"action": "log", "job": "team/app-build", "build": "lastFailedBuild"})
            )
            .as_deref(),
            Some("🏗 Jenkins 构建 team/app-build #lastFailedBuild")
        );
    }

    #[test]
    fn denials_are_visible_so_injection_attempts_show_up() {
        let text = label(&Step::Denied {
            id: "t".into(),
            tool: "mcp__qtmcp__jira_issue".into(),
            reason: "只读模式：jira_issue.delete 不在放行范围内".into(),
        })
        .expect("要显示");
        assert!(text.starts_with("⛔ 已拦截 jira&#95;issue"), "{text}");
    }

    #[test]
    fn arguments_are_escaped_and_truncated() {
        let text = call(
            "mcp__qtmcp__jira_search",
            json!({"jql": format!("text ~ \"<at id=all>\" {}", "x".repeat(200))}),
        )
        .expect("要显示");
        assert!(!text.contains('<'));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn chatter_and_results_are_not_shown() {
        assert_eq!(label(&Step::Text("我来看看".into())), None);
        assert_eq!(
            label(&Step::ToolDone {
                id: "t".into(),
                ok: true,
                preview: String::new()
            }),
            None
        );
    }
}
