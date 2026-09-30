//! 组装 `claude` 的命令行。每个参数的取舍都有实测依据（2.1.283 / 2.1.284）：
//!
//! - `--restricted`：忽略 user/project/local 设置文件（个人的 bypassPermissions、
//!   OTEL、插件和 CLAUDE.md 都不会带进来），并把文件工具限定在工作目录内。
//! - `--strict-mcp-config` + `--mcp-config`：只挂我们给的 MCP server。
//! - `--permission-mode auto`：没人值守，不能停下来等确认。`--permission-prompts none`
//!   兜底：万一还有要问人的调用，直接拒绝而不是挂住。
//! - `--allowedTools mcp__<server>`：qtmcp 的读写都要能用；不放行的话 auto 模式的
//!   分类器可能拦下写操作（实测放行后写操作不再经过它）。
//! - `--include-partial-messages`：增量事件里能看出模型在思考还是在写结论，进度卡据此
//!   显示阶段；长时间一条输出都没有，才说明真的卡住了。
//! - `--system-prompt-snapshot off`：默认 CLI 在会话第一次请求时把系统提示（含追加的
//!   规则）记下来，之后续接一直用这份，规则改了老会话用不上（2.1.285 实测）。关掉后
//!   每次按本次传入的规则渲染。
//! - `--autocompact`：一轮里上下文涨过这个数就先压缩再继续。每次请求都要带上整段会话，
//!   窗口越大每一步越慢；轮与轮之间由编排层按上下文大小换新会话。
//! - `--add-dir`：超大的工具结果 CLI 存成文件、只回一个路径，文件在工作目录外；
//!   不放开，模型就读不到这份结果（线上出现过）。
//! - prompt 不在这里：它走 stdin。

use std::path::Path;

use crate::TurnRequest;

/// 模型能用的内置工具：只有读，且被 `--restricted` 限定在工作目录内。
const TOOLS: &str = "Read,Glob,Grep";
/// 自动压缩的窗口（token）。
const AUTOCOMPACT_TOKENS: &str = "200000";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invocation {
    args: Vec<String>,
}

pub(crate) enum SessionArg<'a> {
    New(&'a str),
    Resume(&'a str),
}

impl Invocation {
    /// `results_dir` 是 CLI 存放超大工具结果的目录，给了就放开给文件工具读。
    pub(crate) fn build(
        request: &TurnRequest,
        session: SessionArg<'_>,
        mcp_config: &Path,
        settings: &Path,
        results_dir: Option<&Path>,
    ) -> Self {
        let mut args: Vec<String> = Vec::with_capacity(32);
        let mut push = |flag: &str, value: Option<String>| {
            args.push(flag.to_owned());
            if let Some(value) = value {
                args.push(value);
            }
        };

        push("-p", None);
        // stream-json 必须配 --verbose，否则 CLI 拒绝启动
        push("--output-format", Some("stream-json".into()));
        push("--verbose", None);
        push("--include-partial-messages", None);
        match session {
            SessionArg::New(id) => push("--session-id", Some(id.to_owned())),
            SessionArg::Resume(id) => push("--resume", Some(id.to_owned())),
        }
        push("--restricted", None);
        push("--strict-mcp-config", None);
        push("--disable-slash-commands", None);
        push("--permission-mode", Some("auto".into()));
        push("--permission-prompts", Some("none".into()));
        push("--system-prompt-snapshot", Some("off".into()));
        push("--autocompact", Some(AUTOCOMPACT_TOKENS.into()));
        if let Some(dir) = results_dir {
            push("--add-dir", Some(dir.display().to_string()));
        }
        if !request.mcp_servers.is_empty() {
            let servers: Vec<String> = request
                .mcp_servers
                .iter()
                .map(|server| format!("mcp__{}", server.name))
                .collect();
            push("--allowedTools", Some(servers.join(",")));
        }
        push("--mcp-config", Some(mcp_config.display().to_string()));
        push("--settings", Some(settings.display().to_string()));
        push("--tools", Some(TOOLS.into()));
        push("--json-schema", Some(request.schema.to_string()));
        if !request.rules.trim().is_empty() {
            push("--append-system-prompt", Some(request.rules.clone()));
        }
        if let Some(model) = &request.model {
            push("--model", Some(model.clone()));
        }
        if let Some(effort) = request.effort {
            push("--effort", Some(effort.as_str().into()));
        }
        if let Some(micros) = request.budget_usd_micros {
            push("--max-budget-usd", Some(usd_decimal(micros)));
        }
        Self { args }
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    /// 可复制去复现的命令行（参数按 shell 规则引好），只进 debug 日志。
    pub(crate) fn command_line(&self, program: &str) -> String {
        std::iter::once(program)
            .chain(self.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// 微美元转成 CLI 接受的十进制字符串（不用浮点，避免 0.1 变 0.100000000001）。
fn usd_decimal(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

/// POSIX 单引号引用：单引号内没有转义，只需处理单引号本身。
pub(crate) fn shell_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ',' | '=' | ':')
        });
    if plain {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Effort, SessionRef};
    use std::path::PathBuf;
    use std::time::Duration;

    fn request() -> TurnRequest {
        TurnRequest {
            session: SessionRef::New,
            prompt: "机密的群聊内容".into(),
            workdir: PathBuf::from("/w"),
            run_dir: PathBuf::from("/r"),
            images: Vec::new(),
            rules: "按规则回答".into(),
            schema: serde_json::json!({"type": "object"}),
            mcp_servers: Vec::new(),
            model: Some("opus".into()),
            effort: Some(Effort::High),
            timeout: Duration::from_secs(60),
            budget_usd_micros: Some(2_500_000),
        }
    }

    fn value_of<'a>(inv: &'a Invocation, flag: &str) -> Option<&'a str> {
        let i = inv.args().iter().position(|a| a == flag)?;
        inv.args().get(i + 1).map(String::as_str)
    }

    #[test]
    fn isolation_flags_are_always_present() {
        let inv = Invocation::build(
            &request(),
            SessionArg::New("s-1"),
            Path::new("/r/mcp.json"),
            Path::new("/r/settings.json"),
            Some(Path::new("/c/projects/-w/s-1/tool-results")),
        );
        for flag in [
            "-p",
            "--verbose",
            "--include-partial-messages",
            "--restricted",
            "--strict-mcp-config",
            "--disable-slash-commands",
        ] {
            assert!(inv.args().iter().any(|a| a == flag), "缺少 {flag}");
        }
        assert_eq!(value_of(&inv, "--permission-prompts"), Some("none"));
        assert_eq!(value_of(&inv, "--permission-mode"), Some("auto"));
        assert_eq!(value_of(&inv, "--tools"), Some("Read,Glob,Grep"));
        assert_eq!(value_of(&inv, "--session-id"), Some("s-1"));
        assert_eq!(value_of(&inv, "--max-budget-usd"), Some("2.500000"));
        assert_eq!(value_of(&inv, "--effort"), Some("high"));
        // 规则改了老会话也要按新的来；超大工具结果所在的目录要能读
        assert_eq!(value_of(&inv, "--system-prompt-snapshot"), Some("off"));
        assert_eq!(value_of(&inv, "--autocompact"), Some(AUTOCOMPACT_TOKENS));
        assert_eq!(
            value_of(&inv, "--add-dir"),
            Some("/c/projects/-w/s-1/tool-results")
        );
    }

    #[test]
    fn only_our_mcp_servers_are_pre_approved_and_the_prompt_stays_out_of_argv() {
        let mut req = request();
        let inv = Invocation::build(
            &req,
            SessionArg::Resume("s-2"),
            Path::new("/r/mcp.json"),
            Path::new("/r/settings.json"),
            None,
        );
        assert!(!inv.args().iter().any(|a| a == "--allowedTools"));
        assert!(
            !inv.args()
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );

        req.mcp_servers.push(crate::McpServer {
            name: "qtmcp".into(),
            command: PathBuf::from("/bin/qtmcp"),
            args: Vec::new(),
            env: Default::default(),
        });
        let inv = Invocation::build(
            &req,
            SessionArg::Resume("s-2"),
            Path::new("/r/mcp.json"),
            Path::new("/r/settings.json"),
            None,
        );
        assert_eq!(value_of(&inv, "--allowedTools"), Some("mcp__qtmcp"));
        assert!(!inv.args().iter().any(|a| a.contains("机密的群聊内容")));
        assert_eq!(value_of(&inv, "--resume"), Some("s-2"));
        assert!(!inv.args().iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn shell_quoting_survives_quotes_and_newlines() {
        assert_eq!(shell_quote("--verbose"), "--verbose");
        assert_eq!(shell_quote("it's\nfine"), "'it'\\''s\nfine'");
        assert_eq!(shell_quote(""), "''");
    }
}
