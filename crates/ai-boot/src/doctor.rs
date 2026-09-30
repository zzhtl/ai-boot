//! `ai-boot doctor`：部署后逐项自检，每一类故障都给出原因和处理办法。
//!
//! 用服务运行时的身份和环境跑（systemd-run 带上同一个 EnvironmentFile），
//! 检查结果才有意义。只读：不改配置、不写库（打开数据库会跑迁移，迁移是幂等的），
//! 不调任何写接口；输出里不打印任何密钥。

use std::ffi::OsString;
use std::path::Path;
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use ai_boot_agent::AgentBackend as _;
use ai_boot_feishu::api::ApiClient;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::config::{self, Config};
use crate::store::{Store, now_ms};
use crate::writeback::mcp;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Check {
    level: Level,
    name: &'static str,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            level: Level::Ok,
            name,
            detail: detail.into(),
            hint: None,
        }
    }

    fn warn(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            name,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            level: Level::Fail,
            name,
            detail: detail.into(),
            hint: Some(hint.into()),
        }
    }
}

pub fn run(config_path: &Path) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("创建运行时失败：{err}");
            return ExitCode::FAILURE;
        }
    };
    let checks = runtime.block_on(checks(config_path));
    println!("ai-boot doctor");
    for check in &checks {
        let mark = match check.level {
            Level::Ok => "✅",
            Level::Warn => "⚠️",
            Level::Fail => "❌",
        };
        println!("{mark} {}：{}", check.name, check.detail);
        if let Some(hint) = &check.hint {
            println!("   处理：{hint}");
        }
    }
    let count = |level| checks.iter().filter(|c| c.level == level).count();
    let failed = count(Level::Fail);
    println!(
        "共 {} 项：{} 通过，{} 警告，{} 失败",
        checks.len(),
        count(Level::Ok),
        count(Level::Warn),
        failed
    );
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn checks(config_path: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let config = match Config::load(config_path) {
        Ok(config) => {
            checks.push(Check::ok("配置", config_path.display().to_string()));
            config
        }
        Err(err) => {
            checks.push(Check::fail(
                "配置",
                format!("{err:#}"),
                "对照 config.example.toml 修改配置文件",
            ));
            return checks;
        }
    };
    checks.push(data_dir(&config.storage.data_dir));
    checks.push(database(&config.storage.data_dir).await);

    let api = match config::app_secret_from_env() {
        Ok(secret) => {
            match ApiClient::new(
                config.feishu.base_url.clone(),
                config.feishu.app_id.clone(),
                secret,
            ) {
                Ok(api) => Some(Arc::new(api)),
                Err(err) => {
                    checks.push(Check::fail("飞书应用", err.to_string(), "查看服务日志"));
                    None
                }
            }
        }
        Err(err) => {
            checks.push(Check::fail(
                "飞书应用",
                format!("{err:#}"),
                format!(
                    "在 EnvironmentFile 里设置 {}，并用服务的环境运行 doctor",
                    config::APP_SECRET_ENV
                ),
            ));
            None
        }
    };
    if let Some(api) = &api {
        checks.push(feishu(api).await);
        checks.push(whitelist(api, &config).await);
    }
    checks.push(claude(&config).await);
    checks.push(hook(&config).await);
    for server in &config.agent.mcp {
        checks.push(tool_surface(server).await);
        if let Some(state_dir) = server.env.get("QTMCP_STATE_DIR") {
            checks.push(sso_state(Path::new(state_dir)));
        }
    }
    if let Some(writeback) = &config.writeback {
        let server = mcp::Server {
            command: writeback.command.clone(),
            args: writeback.args.clone(),
            env: writeback.env.clone(),
        };
        checks.push(match mcp::Session::start(&server, MCP_TIMEOUT).await {
            Ok(_) => Check::ok("写回", format!("{} 可以连上", server.command.display())),
            Err(err) => Check::fail("写回", err, "检查 [writeback] 的 command、args、env"),
        });
    }
    if config.oauth.is_some() {
        checks.push(user_tokens(&config).await);
    }
    checks.extend(tools(config.context.office_legacy).await);
    checks
}

fn data_dir(dir: &Path) -> Check {
    use std::os::unix::fs::MetadataExt as _;
    let name = "数据目录";
    let Ok(meta) = std::fs::metadata(dir) else {
        return Check::fail(
            name,
            format!("{} 不存在", dir.display()),
            "用 systemd 的 StateDirectory 创建，或手动建成 0700",
        );
    };
    let mode = meta.mode() & 0o777;
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Check::fail(
            name,
            format!("{} 的属主不是当前用户", dir.display()),
            "用服务的运行用户执行 doctor；目录属主改成服务用户",
        );
    }
    if mode & 0o077 != 0 {
        return Check::warn(
            name,
            format!("{}（{mode:o}）对其他用户可访问", dir.display()),
            "里面有聊天内容和授权 token，改成 0700",
        );
    }
    Check::ok(name, format!("{}（{mode:o}）", dir.display()))
}

async fn database(dir: &Path) -> Check {
    match Store::open(dir).await {
        Ok(store) => match store.unassigned_inputs().await {
            Ok(pending) => Check::ok(
                "数据库",
                format!("可以打开，待分派的消息 {} 条", pending.len()),
            ),
            Err(err) => Check::fail("数据库", format!("{err:#}"), "查看服务日志"),
        },
        Err(err) => Check::fail(
            "数据库",
            format!("{err:#}"),
            "检查数据目录权限；数据库损坏时从 backup/ 里的备份恢复",
        ),
    }
}

async fn feishu(api: &ApiClient) -> Check {
    match api.bot_info().await {
        Ok(bot) => Check::ok("飞书应用", format!("凭据有效，机器人：{}", bot.app_name)),
        // 能走到机器人接口说明凭据是对的
        Err(err) if err.code() == Some(11_205) => Check::fail(
            "飞书应用",
            "凭据有效，但应用还没有机器人能力",
            "开发者后台「添加应用能力」里加上机器人，并发布一个新版本",
        ),
        Err(err) => Check::fail(
            "飞书应用",
            err.to_string(),
            "检查 feishu.app_id、FEISHU_APP_SECRET，以及应用是否启用了机器人能力",
        ),
    }
}

async fn whitelist(api: &ApiClient, config: &Config) -> Check {
    let emails = &config.access.allowed_emails;
    let direct = config.access.allowed_open_ids.len();
    if emails.is_empty() {
        return Check::ok("白名单", format!("{direct} 个 open_id"));
    }
    match api.open_ids_by_email(emails).await {
        Ok(found) => {
            let missing: Vec<&String> = emails
                .iter()
                .filter(|email| !found.iter().any(|(e, _)| e.eq_ignore_ascii_case(email)))
                .collect();
            if missing.is_empty() {
                Check::ok(
                    "白名单",
                    format!("{} 个邮箱都能解析，另有 {direct} 个 open_id", emails.len()),
                )
            } else {
                Check::warn(
                    "白名单",
                    format!("{} 个邮箱解析不到", missing.len()),
                    "确认邮箱是飞书个人资料里的邮箱，且应用的可用范围包含这些人",
                )
            }
        }
        Err(err) => Check::fail(
            "白名单",
            format!("邮箱解析失败：{err}"),
            "应用需要「通过手机号或邮箱获取用户 ID」权限；也可以直接配 allowed_open_ids",
        ),
    }
}

async fn claude(config: &Config) -> Check {
    let name = "Claude";
    let backend = crate::claude_backend(&config.agent);
    let info = match backend.preflight().await {
        Ok(info) => info,
        Err(err) => {
            return Check::fail(
                name,
                err.to_string(),
                "检查 agent.claude.program 指向的可执行文件",
            );
        }
    };
    if let Err(detail) = login_status(
        &config.agent.claude.program,
        &crate::claude_env(&config.agent),
    )
    .await
    {
        return Check::fail(
            name,
            format!("{}，{detail}", info.version),
            "用服务的运行用户在本机执行 claude 登录（或在 ai-boot.env 里配 CLAUDE_CODE_OAUTH_TOKEN）",
        );
    }
    // 用的是本机会自动升级的 claude，版本和样本不一致是常态，只标注出来；
    // 真解析不了时每轮的自检和解码告警会在卡片和日志里暴露
    if let Some(recorded) = info.recorded_version
        && !info.version.starts_with(recorded)
    {
        return Check::ok(
            name,
            format!("{}，已登录（解码样本录制于 {recorded}）", info.version),
        );
    }
    Check::ok(name, format!("{}，已登录", info.version))
}

/// claude 的登录态。用和 Agent 一样的环境跑 `auth status`：它只读本地凭据，不发 API 请求。
async fn login_status(program: &Path, env: &[(OsString, OsString)]) -> Result<(), String> {
    let output = tokio::process::Command::new(program)
        .args(["auth", "status", "--json"])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(Duration::from_secs(10), output).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => return Err(format!("auth status 无法执行：{err}")),
        Err(_) => return Err("auth status 10 秒内没有返回".to_owned()),
    };
    let status: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    if status.get("loggedIn") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        Err("没有登录".to_owned())
    }
}

/// hook 必须失败即拒绝：喂一个执行命令的调用，应当拿到拒绝。
async fn hook(config: &Config) -> Check {
    let name = "hook";
    let program = &config.agent.hook.program;
    let workdir = std::env::temp_dir();
    let input = json!({
        "session_id": "doctor", "transcript_path": "/dev/null", "cwd": workdir,
        "permission_mode": "auto", "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": { "command": "id" },
    });
    let mut child = match tokio::process::Command::new(program)
        .args(["hook", "--backend", "claude", "--workdir"])
        .arg(&workdir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return Check::fail(
                name,
                format!("{} 无法执行：{err}", program.display()),
                "agent.hook.program 要指向 ai-boot 自己的可执行文件",
            );
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
    }
    let output = match tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await
    {
        Ok(Ok(output)) => output,
        _ => return Check::fail(name, "10 秒内没有给出判决", "检查 agent.hook.program"),
    };
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    if reply.pointer("/hookSpecificOutput/decision/behavior") == Some(&json!("deny")) {
        Check::ok(name, "执行命令会被拒绝")
    } else {
        Check::fail(
            name,
            "执行命令没有被拒绝",
            "agent.hook.program 指向的程序版本不对，重新安装 ai-boot",
        )
    }
}

/// Agent 用的 MCP server 能起来、能列出工具。
async fn tool_surface(server: &config::McpSection) -> Check {
    let name = "MCP 工具面";
    let spec = mcp::Server {
        command: server.command.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
    };
    let mut session = match mcp::Session::start(&spec, MCP_TIMEOUT).await {
        Ok(session) => session,
        Err(err) => {
            return Check::fail(
                name,
                format!("{}：{err}", server.name),
                "检查 [[agent.mcp]] 的配置",
            );
        }
    };
    let tools = match session.list_tools(MCP_TIMEOUT).await {
        Ok(tools) => tools,
        Err(err) => {
            return Check::fail(
                name,
                format!("{}：{err}", server.name),
                "查看 MCP server 的日志",
            );
        }
    };
    if tools.is_empty() {
        return Check::fail(
            name,
            format!("{}：一个工具都没有", server.name),
            "检查 [[agent.mcp]] 的 args（--toolsets）",
        );
    }
    Check::ok(name, format!("{}：{} 个工具", server.name, tools.len()))
}

fn sso_state(dir: &Path) -> Check {
    use std::os::unix::fs::MetadataExt as _;
    let name = "共享 SSO 状态目录";
    match std::fs::metadata(dir) {
        Ok(meta) if meta.mode() & 0o077 != 0 => Check::warn(
            name,
            format!(
                "{}（{:o}）对其他用户可访问",
                dir.display(),
                meta.mode() & 0o777
            ),
            "里面是登录会话，改成 0700",
        ),
        Ok(_) => Check::ok(name, dir.display().to_string()),
        Err(_) => Check::fail(
            name,
            format!("{} 不存在", dir.display()),
            "按部署文档创建（0700，属主是服务用户），机器人和交互式 qtmcp 共用它",
        ),
    }
}

async fn user_tokens(config: &Config) -> Check {
    let name = "云文档授权";
    let Ok(store) = Store::open(&config.storage.data_dir).await else {
        return Check::fail(name, "数据库打不开", "先解决数据库的问题");
    };
    let mut lines = Vec::new();
    let mut expired = false;
    for open_id in &config.access.allowed_open_ids {
        match store.user_token(open_id).await {
            Ok(Some(token)) if token.needs_reauth => {
                expired = true;
                lines.push(format!("{open_id} 需要重新授权"));
            }
            Ok(Some(token)) => {
                let left = (token.access_expires_at - now_ms()) / 60_000;
                lines.push(format!("{open_id} 已授权（access_token 还剩 {left} 分钟）"));
            }
            Ok(None) => lines.push(format!("{open_id} 还没有授权")),
            Err(err) => lines.push(format!("{open_id}：{err:#}")),
        }
    }
    if lines.is_empty() {
        return Check::ok(name, "白名单里只有邮箱，授权状态见服务日志");
    }
    if expired {
        Check::warn(
            name,
            lines.join("；"),
            "在群里贴一篇文档提问，机器人会私聊发授权链接",
        )
    } else {
        Check::ok(name, lines.join("；"))
    }
}

async fn tools(office_legacy: bool) -> Vec<Check> {
    let mut checks = Vec::new();
    for (program, flag) in [("pdftotext", "-v"), ("pdftoppm", "-v")] {
        checks.push(tool(program, flag, "PDF 附件", "安装 poppler-utils").await);
    }
    checks.push(tool("dot", "-V", "答案里的流程图", "安装 graphviz").await);
    if office_legacy {
        checks.push(
            tool(
                "soffice",
                "--version",
                "旧版 Office 附件",
                "安装 LibreOffice，或在配置里关掉 context.office_legacy",
            )
            .await,
        );
    }
    checks
}

async fn tool(program: &str, flag: &str, what: &'static str, hint: &str) -> Check {
    let output = tokio::process::Command::new(program)
        .arg(flag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(Duration::from_secs(60), output).await {
        Ok(Ok(_)) => Check::ok(what, format!("{program} 可用")),
        _ => Check::warn(what, format!("找不到 {program}，这项用不了"), hint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_world_readable_data_dir_is_flagged() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("临时目录");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("权限");
        assert_eq!(data_dir(dir.path()).level, Level::Warn);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("权限");
        assert_eq!(data_dir(dir.path()).level, Level::Ok);
        assert_eq!(data_dir(&dir.path().join("missing")).level, Level::Fail);
    }

    #[tokio::test]
    async fn login_is_read_from_auth_status_with_only_the_passed_env() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("临时目录");
        // 只有传进来的 HOME 指向「已登录」时才报已登录：证明用的是白名单环境
        let fake = dir.path().join("claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$1 $2 $3\" = 'auth status --json' ] || exit 2\n\
             if [ \"$HOME\" = /logged-in ]; then echo '{\"loggedIn\": true}'; \
             else echo '{\"loggedIn\": false}'; exit 1; fi\n",
        )
        .expect("写脚本");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("权限");

        let env = |home: &str| vec![(OsString::from("HOME"), OsString::from(home))];
        assert_eq!(login_status(&fake, &env("/logged-in")).await, Ok(()));
        assert!(login_status(&fake, &env("/nobody")).await.is_err());
        assert!(
            login_status(&dir.path().join("missing"), &env("/logged-in"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_tool_surface_check_counts_what_the_server_lists() {
        let dir = tempfile::tempdir().expect("临时目录");
        let fake = crate::writeback::mcp::tests::fake_server(
            dir.path(),
            &json!({"__tools__": [
                {"name": "jira_issue", "inputSchema": {"properties": {"action": {"enum": ["get", "delete"]}}}},
                {"name": "jira_search", "inputSchema": {"properties": {"jql": {}}}}
            ]}),
        );
        let server = config::McpSection {
            name: "qtmcp".into(),
            command: fake.command,
            args: fake.args,
            env: Default::default(),
        };
        let check = tool_surface(&server).await;
        assert_eq!(check.level, Level::Ok);
        assert!(check.detail.contains("2 个工具"), "{}", check.detail);
    }
}
