//! 工具判决：hook 的判决逻辑，与后端无关的纯函数。
//!
//! MCP（qtmcp）的读写、执行命令和上网全部放行：命令行上用 `--allowedTools` 预先批准，
//! 这里也不拦。命令和网页按部署者的决定直接开放、不加沙箱：服务用户读得到的凭据只靠
//! 提示词约束，不是硬隔离。hook 只剩兜底的两件事（实测 Claude Code 2.1.283，Codex 格式一致）：
//! - **PreToolUse** 对所有工具触发，只负责拒绝：文件工具越出工作目录（和
//!   `--restricted` 双保险；CLI 存放超大工具结果的目录另外放开）；万一配置漂移冒出
//!   改文件、派子 Agent 的工具，也在这里拦下。
//! - **PermissionRequest** 只在调用需要批准时触发：MCP 工具、执行命令、上网和结构化输出
//!   给 allow，其余 deny。hook 自身崩溃或超时时，`--permission-prompts none` 下调用会被
//!   自动拒绝。

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// hook 的判决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    /// 不表态，交给 CLI 的正常流程。
    Abstain,
}

/// `--strict-mcp-config` 下只有我们配置的 MCP server，前缀即可认定。
const MCP_PREFIX: &str = "mcp__";
/// 直接开放的内置工具：执行命令、打开网页、搜索网页。
const OPEN_TOOLS: [&str; 3] = ["Bash", "WebFetch", "WebSearch"];

/// 一次 hook 调用。
#[derive(Debug, Clone, Copy)]
pub struct HookCall<'a> {
    pub event: &'a str,
    pub tool_name: &'a str,
    pub tool_input: &'a Value,
}

/// 文件工具能读的范围：工作目录（相对路径按它解析），外加几个只读目录。
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    pub workdir: &'a Path,
    /// CLI 存放超大工具结果的目录：结果只以文件的形式交给模型。
    pub readable: &'a [PathBuf],
}

impl Scope<'_> {
    fn roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.workdir).chain(self.readable.iter().map(PathBuf::as_path))
    }
}

pub fn decide(call: HookCall<'_>, scope: Scope<'_>) -> Decision {
    match call.event {
        "PermissionRequest" => permission_request(call.tool_name),
        "PreToolUse" => pre_tool_use(call.tool_name, call.tool_input, scope),
        _ => Decision::Abstain,
    }
}

fn permission_request(tool: &str) -> Decision {
    // 结构化输出是模型交还结果的通道；实测它不经过权限层，放行只是保险
    if tool == "StructuredOutput" || tool.starts_with(MCP_PREFIX) || OPEN_TOOLS.contains(&tool) {
        return Decision::Allow;
    }
    Decision::Deny(format!("不允许使用 {tool}"))
}

fn pre_tool_use(tool: &str, input: &Value, scope: Scope<'_>) -> Decision {
    if tool.starts_with(MCP_PREFIX) || OPEN_TOOLS.contains(&tool) {
        return Decision::Abstain;
    }
    match tool {
        "Read" => file_path_check(input, "file_path", scope, true),
        // Glob 的 pattern 是路径模式；Grep 的 pattern 是正则，路径模式在 glob 参数里
        "Glob" | "Grep" => {
            if let Decision::Deny(reason) = file_path_check(input, "path", scope, false) {
                return Decision::Deny(reason);
            }
            let key = if tool == "Glob" { "pattern" } else { "glob" };
            pattern_check(input, key, scope)
        }
        // 这些工具都没有开放；万一配置漂移让它们出现了，也在这里拦下
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "Task" | "Agent" | "apply_patch" => {
            Decision::Deny(format!("不允许使用 {tool}"))
        }
        _ => Decision::Abstain,
    }
}

/// 路径参数必须落在可读范围内（按真实路径比较，挡住 `..` 和符号链接逃逸）。
fn file_path_check(input: &Value, key: &str, scope: Scope<'_>, required: bool) -> Decision {
    let Some(raw) = input.get(key).and_then(Value::as_str) else {
        return if required {
            Decision::Deny(format!("缺少参数 {key}"))
        } else {
            Decision::Abstain
        };
    };
    if inside(scope, Path::new(raw)) {
        Decision::Abstain
    } else {
        Decision::Deny(format!("只能读取工作目录内的文件：{raw}"))
    }
}

fn inside(scope: Scope<'_>, path: &Path) -> bool {
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        scope.workdir.join(path)
    };
    // 不存在的路径无从判断，按越界处理：读不存在的文件本来也读不到
    let Ok(full) = full.canonicalize() else {
        return false;
    };
    scope
        .roots()
        .filter_map(|root| root.canonicalize().ok())
        .any(|root| full.starts_with(root))
}

/// 路径模式不能跳出可读范围：不许 `..`，绝对路径必须在范围内。
fn pattern_check(input: &Value, key: &str, scope: Scope<'_>) -> Decision {
    let Some(raw) = input.get(key).and_then(Value::as_str) else {
        return Decision::Abstain;
    };
    let pattern = Path::new(raw);
    if pattern.components().any(|c| c == Component::ParentDir) {
        return Decision::Deny(format!("模式里不允许出现 ..：{raw}"));
    }
    if pattern.is_absolute() {
        let allowed = scope.roots().any(|root| {
            let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
            pattern.starts_with(root)
        });
        if !allowed {
            return Decision::Deny(format!("只能在工作目录内检索：{raw}"));
        }
    }
    Decision::Abstain
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 只有工作目录，没有额外的可读目录。
    fn only(workdir: &Path) -> Scope<'_> {
        Scope {
            workdir,
            readable: &[],
        }
    }

    fn call<'a>(event: &'a str, tool: &'a str, input: &'a Value) -> HookCall<'a> {
        HookCall {
            event,
            tool_name: tool,
            tool_input: input,
        }
    }

    #[test]
    fn mcp_reads_and_writes_are_both_allowed() {
        let dir = tempfile::tempdir().expect("临时目录");
        for input in [
            json!({"action": "get", "key": "XX-1"}),
            json!({"action": "delete", "key": "XX-1"}),
            json!({"jql": "text ~ \"boom\""}),
        ] {
            assert_eq!(
                decide(
                    call("PermissionRequest", "mcp__qtmcp__jira_issue", &input),
                    only(dir.path())
                ),
                Decision::Allow
            );
            assert_eq!(
                decide(
                    call("PreToolUse", "mcp__qtmcp__jira_issue", &input),
                    only(dir.path())
                ),
                Decision::Abstain
            );
        }
    }

    #[test]
    fn commands_and_the_web_are_open_but_file_writes_are_refused() {
        let dir = tempfile::tempdir().expect("临时目录");
        for (tool, input) in [
            (
                "Bash",
                json!({"command": "git clone --depth 1 https://github.com/x/y"}),
            ),
            (
                "WebFetch",
                json!({"url": "https://github.com/x/y", "prompt": "看 README"}),
            ),
            ("WebSearch", json!({"query": "harness formal"})),
        ] {
            assert_eq!(
                decide(call("PermissionRequest", tool, &input), only(dir.path())),
                Decision::Allow,
                "{tool}"
            );
            assert_eq!(
                decide(call("PreToolUse", tool, &input), only(dir.path())),
                Decision::Abstain,
                "{tool}"
            );
        }
        for tool in ["Write", "Edit", "Agent"] {
            let input = json!({"file_path": "a"});
            for event in ["PermissionRequest", "PreToolUse"] {
                assert!(
                    matches!(
                        decide(call(event, tool, &input), only(dir.path())),
                        Decision::Deny(_)
                    ),
                    "{event} {tool}"
                );
            }
        }
    }

    #[test]
    fn structured_output_is_never_blocked() {
        // 拦下它结构化输出就永远出不来，还白白烧钱（ai-task ADR 0010 的事故）
        let dir = tempfile::tempdir().expect("临时目录");
        let answer = json!({"summary": "x"});
        assert_eq!(
            decide(
                call("PermissionRequest", "StructuredOutput", &answer),
                only(dir.path())
            ),
            Decision::Allow
        );
        assert_eq!(
            decide(
                call("PreToolUse", "StructuredOutput", &answer),
                only(dir.path())
            ),
            Decision::Abstain
        );
    }

    #[test]
    fn file_tools_stay_inside_the_workdir() {
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::write(dir.path().join("a.txt"), "x").expect("写文件");
        let outside = tempfile::tempdir().expect("另一个目录");
        std::fs::write(outside.path().join("secret"), "x").expect("写文件");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link"))
            .expect("符号链接");

        let read = |path: &str| {
            let input = json!({"file_path": path});
            decide(call("PreToolUse", "Read", &input), only(dir.path()))
        };
        assert_eq!(read("a.txt"), Decision::Abstain);
        assert_eq!(
            read(&dir.path().join("a.txt").display().to_string()),
            Decision::Abstain
        );
        for escape in ["../secret", "/etc/hostname", "link", "missing.txt"] {
            assert!(matches!(read(escape), Decision::Deny(_)), "{escape}");
        }
    }

    #[test]
    fn glob_and_grep_cannot_search_outside_the_workdir() {
        let dir = tempfile::tempdir().expect("临时目录");
        let check =
            |tool: &str, input: Value| decide(call("PreToolUse", tool, &input), only(dir.path()));
        assert_eq!(
            check("Glob", json!({"pattern": "**/*.log"})),
            Decision::Abstain
        );
        assert_eq!(
            check("Grep", json!({"pattern": "ERROR"})),
            Decision::Abstain
        );
        // Grep 的 pattern 是正则，长得像路径也不是路径
        assert_eq!(
            check("Grep", json!({"pattern": "/api/v1/../x"})),
            Decision::Abstain
        );
        assert!(matches!(
            check("Glob", json!({"pattern": "../**"})),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check("Glob", json!({"pattern": "/etc/*"})),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check("Grep", json!({"pattern": "x", "path": "/etc"})),
            Decision::Deny(_)
        ));
        assert!(matches!(
            check("Grep", json!({"pattern": "x", "glob": "../*"})),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn oversized_tool_results_can_be_read_but_nothing_else_outside() {
        let dir = tempfile::tempdir().expect("临时目录");
        let results = tempfile::tempdir().expect("工具结果目录");
        std::fs::write(results.path().join("mcp-qtmcp-log.txt"), "x").expect("写文件");
        let sibling = tempfile::tempdir().expect("别的目录");
        std::fs::write(sibling.path().join("secret"), "x").expect("写文件");
        let readable = [results.path().to_path_buf()];
        let scope = Scope {
            workdir: dir.path(),
            readable: &readable,
        };
        let check = |tool: &str, input: Value| decide(call("PreToolUse", tool, &input), scope);
        let saved = results
            .path()
            .join("mcp-qtmcp-log.txt")
            .display()
            .to_string();
        assert_eq!(
            check("Read", json!({"file_path": saved})),
            Decision::Abstain
        );
        assert_eq!(
            check(
                "Grep",
                json!({"pattern": "ERROR", "path": results.path().display().to_string()})
            ),
            Decision::Abstain
        );
        let secret = sibling.path().join("secret").display().to_string();
        assert!(matches!(
            check("Read", json!({"file_path": secret})),
            Decision::Deny(_)
        ));
        // 从工具结果目录往上跳出去也不行
        let escape = format!("{}/../secret", results.path().display());
        assert!(matches!(
            check("Read", json!({"file_path": escape})),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn other_hook_events_are_left_alone() {
        let dir = tempfile::tempdir().expect("临时目录");
        assert_eq!(
            decide(call("PostToolUse", "Read", &json!({})), only(dir.path())),
            Decision::Abstain
        );
    }
}
