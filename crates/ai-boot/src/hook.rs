//! `ai-boot hook`：Agent CLI 的 PermissionRequest / PreToolUse hook。
//!
//! 判决规则在 `ai_boot_agent::policy`，这里只管协议：从 stdin 读一条 JSON，
//! 往 stdout 写判决。输出格式是对 Claude Code 2.1.283 实测过的，Codex 与之一致：
//! - PermissionRequest：`{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"|"deny",...}}}`
//! - PreToolUse 拒绝：`{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",...}}`
//! - 不表态：什么都不输出，exit 0。
//!
//! 输入看不懂时 exit 2：PreToolUse 会因此拦下调用；PermissionRequest 不认
//! exit 2，但没有判决时调用会被自动拒绝。两种情况结果都是拒绝。
//! 这个子命令在构建 tokio 运行时之前就分发出去：hook 是短命进程，每次工具调用
//! 都要起一个。

use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use ai_boot_agent::policy::{self, Decision, HookCall};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, clap::Args)]
pub struct HookArgs {
    /// 调用方是哪个 CLI。目前两家的格式一致，参数留着区分将来的差异
    #[arg(long, value_enum)]
    backend: Backend,
    /// 本轮的工作目录：文件工具只能读这里面的东西
    #[arg(long)]
    workdir: PathBuf,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Backend {
    Claude,
    Codex,
}

#[derive(Deserialize)]
struct HookInput {
    hook_event_name: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    tool_input: Value,
}

/// 输入看不懂时的退出码。
const EXIT_BLOCK: u8 = 2;

pub fn run(args: &HookArgs) -> ExitCode {
    let mut raw = String::new();
    if let Err(err) = std::io::stdin().read_to_string(&mut raw) {
        eprintln!("ai-boot hook：读取输入失败：{err}");
        return ExitCode::from(EXIT_BLOCK);
    }
    // 判决逻辑是纯函数，不该 panic；真 panic 了也要按拒绝收场
    let verdict = std::panic::catch_unwind(|| respond(args, &raw));
    match verdict {
        Ok(Ok(Some(output))) => {
            println!("{output}");
            ExitCode::SUCCESS
        }
        Ok(Ok(None)) => ExitCode::SUCCESS,
        Ok(Err(reason)) => {
            eprintln!("ai-boot hook：{reason}");
            ExitCode::from(EXIT_BLOCK)
        }
        Err(_) => {
            eprintln!("ai-boot hook：内部错误，按拒绝处理");
            ExitCode::from(EXIT_BLOCK)
        }
    }
}

/// `Ok(None)` 表示不表态。
fn respond(args: &HookArgs, raw: &str) -> Result<Option<String>, String> {
    let input: HookInput =
        serde_json::from_str(raw).map_err(|err| format!("输入不是预期的 JSON：{err}"))?;
    let _ = args.backend;
    let decision = policy::decide(
        HookCall {
            event: &input.hook_event_name,
            tool_name: &input.tool_name,
            tool_input: &input.tool_input,
        },
        &args.workdir,
    );
    let output = match (input.hook_event_name.as_str(), decision) {
        (_, Decision::Abstain) => return Ok(None),
        ("PermissionRequest", Decision::Allow) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "allow" },
            }
        }),
        ("PermissionRequest", Decision::Deny(reason)) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": { "behavior": "deny", "message": reason },
            }
        }),
        ("PreToolUse", Decision::Deny(reason)) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        }),
        // 策略只会在 PermissionRequest 里放行；别的组合不表态
        _ => return Ok(None),
    };
    Ok(Some(output.to_string()))
}
