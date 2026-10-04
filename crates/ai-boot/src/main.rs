//! ai-boot：飞书问题排查机器人。

mod alert;
mod answer;
mod callback;
mod config;
mod context;
mod conversation;
mod doctor;
mod hook;
mod ingest;
mod lock;
mod maintenance;
mod oauth;
mod prompt;
mod render;
mod runner;
mod store;
mod whitelist;
mod writeback;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ai_boot_agent::claude::{ClaudeBackend, ClaudeConfig};
use ai_boot_agent::{AgentBackend, McpServer};
use ai_boot_feishu::api::ApiClient;
use ai_boot_feishu::ws::{WsClient, WsConfig};
use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// 连发合并的窗口：一轮的第一条消息之后这么久内的消息并进同一轮。进度卡在收到
/// 第一条时就回了，这段只推迟开始分析，越短越快；只发了图片、文件的会多等一会儿
/// 后面的提问（见 conversation::actor）。
const DEBOUNCE: Duration = Duration::from_millis(1500);

#[derive(Parser)]
#[command(version, about = "飞书问题排查机器人")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 连上飞书长连接并开始处理消息
    Run {
        /// 配置文件路径
        #[arg(
            long,
            env = "AI_BOOT_CONFIG",
            default_value = "/etc/ai-boot/config.toml"
        )]
        config: PathBuf,
    },
    /// 部署后自检：逐项检查配置、权限、飞书、Agent、MCP、外部工具
    ///
    /// 要用服务的运行用户和环境执行才有意义：部署后用 `deploy/doctor.sh`
    Doctor {
        #[arg(
            long,
            env = "AI_BOOT_CONFIG",
            default_value = "/etc/ai-boot/config.toml"
        )]
        config: PathBuf,
    },
    /// Agent CLI 调用的工具判决 hook（由 CLI 按 hook 配置调起，不要手动运行）
    #[command(hide = true)]
    Hook(hook::HookArgs),
    /// 在限了内存的子进程里解析表格（由服务自己调起，不要手动运行）
    #[command(hide = true)]
    Sheet {
        /// 表格文件
        path: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        // hook 是每次工具调用都要起一次的短命进程，stdout 专用于判决：
        // 不初始化日志，也不构建运行时
        Command::Hook(args) => hook::run(&args),
        // 同样是短命进程，stdout 专用于解析结果
        Command::Sheet { path } => context::extract::sheet_child(&path),
        Command::Run { config } => serve(&config),
        Command::Doctor { config } => doctor::run(&config),
    }
}

fn serve(config: &Path) -> ExitCode {
    init_tracing();
    // Agent 能执行命令，又和服务是同一个 Unix 用户：进程可转储的话，命令读得到
    // /proc/<pid>/environ 里的 FEISHU_APP_SECRET，也能 ptrace 进来
    if let Err(err) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!(%err, "没能把进程设成不可转储，同一用户的进程可能读到它的环境变量");
    }
    // 运行时手动构建而不是用 #[tokio::main]，这样 hook 子命令用不着它
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(%err, "创建运行时失败");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("AI_BOOT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // 在 journald 里看日志时不要颜色控制符
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = config::Config::load(config_path)?;
    let app_secret = config::app_secret_from_env()?;
    let data_dir = &config.storage.data_dir;
    prepare_data_dir(data_dir)?;
    let _lock = lock::acquire(data_dir)?;
    let store = store::Store::open(data_dir).await?;

    let api = Arc::new(
        ApiClient::new(
            config.feishu.base_url.clone(),
            config.feishu.app_id.clone(),
            app_secret.clone(),
        )
        .context("创建 OpenAPI 客户端失败")?,
    );
    let cancel = CancellationToken::new();
    spawn_signal_handler(cancel.clone());

    let whitelist = Arc::new(whitelist::Whitelist::new(
        config.access.allowed_open_ids.clone(),
    ));
    tokio::spawn(whitelist::resolve_emails(
        Arc::clone(&api),
        Arc::clone(&whitelist),
        config.access.allowed_emails.clone(),
        cancel.clone(),
    ));
    let bot_open_id = Arc::new(OnceLock::new());
    tokio::spawn(fetch_bot_open_id(
        Arc::clone(&api),
        Arc::clone(&bot_open_id),
        cancel.clone(),
    ));

    let backend = claude_backend(&config.agent);
    let claude_config_dir = backend.config_dir();
    let info = backend
        .preflight()
        .await
        .context("claude 不可用，检查 agent.claude.program")?;
    tracing::info!(backend = ?info.kind, version = %info.version, "Agent 后端就绪");
    let backends: HashMap<String, Arc<dyn AgentBackend>> =
        HashMap::from([(info.kind.as_str().to_owned(), Arc::new(backend) as _)]);
    let scratch = data_dir.join("tmp");
    prepare_data_dir(&scratch)?;
    let window = (config.context.window_messages > 0).then(|| {
        (
            config.context.window_messages,
            Duration::from_secs(config.context.window_minutes * 60),
        )
    });
    let settings = runner::Settings {
        data_dir: data_dir.clone(),
        tools: context::extract::Tools {
            office_legacy: config.context.office_legacy,
            scratch,
            // 不用 current_exe()：二进制升级替换后它指向「(deleted)」
            program: config.agent.hook.program.clone(),
        },
        window,
        model: config.agent.model.clone(),
        effort: config.agent.effort_level(),
        timeout: Duration::from_secs(config.agent.timeout_secs),
        budget_usd_micros: config.agent.budget_micros(),
        mcp_servers: config
            .agent
            .mcp
            .iter()
            .map(|server| McpServer {
                name: server.name.clone(),
                command: server.command.clone(),
                args: server.args.clone(),
                env: server.env.clone(),
            })
            .collect(),
        link_hosts: config.render.link_hosts.clone(),
    };
    let mut runner = runner::Runner::new(
        Arc::clone(&api),
        store.clone(),
        backends,
        settings,
        config.agent.max_concurrent,
        Arc::clone(&bot_open_id),
    );
    if let Some(oauth_config) = &config.oauth {
        let oauth = Arc::new(oauth::Oauth::new(
            Arc::clone(&api),
            store.clone(),
            Arc::clone(&whitelist),
            oauth::OauthSettings {
                app_id: config.feishu.app_id.clone(),
                redirect_uri: oauth_config.redirect_uri.clone(),
                accounts_url: oauth_config.accounts_url.clone(),
                scopes: oauth_config.scopes.clone(),
            },
        ));
        let listen = oauth_config.listen;
        let server = Arc::clone(&oauth);
        let stop = cancel.clone();
        tokio::spawn(async move {
            if let Err(err) = oauth::serve(server, listen, stop).await {
                tracing::error!("{err:#}");
            }
        });
        tokio::spawn(oauth::keep_fresh(Arc::clone(&oauth), cancel.clone()));
        runner = runner.with_oauth(oauth);
    } else {
        tracing::info!("没有配置 [oauth]，不读取群里贴的云文档");
    }
    let writer = config.writeback.as_ref().map(|wb| {
        Arc::new(writeback::Writer::new(
            Arc::clone(&api),
            store.clone(),
            writeback::Settings {
                server: writeback::mcp::Server {
                    command: wb.command.clone(),
                    args: wb.args.clone(),
                    env: wb.env.clone(),
                },
                confluence: wb
                    .confluence_space
                    .clone()
                    .zip(wb.confluence_parent_id.clone()),
            },
            config.render.link_hosts.clone(),
        ))
    });
    if let Some(writer) = &writer {
        runner = runner.with_writer(Arc::clone(writer));
    }
    runner = runner.with_alerts(Arc::new(alert::Alerts::new(
        Arc::clone(&api),
        Arc::clone(&whitelist),
    )));
    let runner = Arc::new(runner);
    // 定时清理和「清空上下文」共用：删会话数据的办法只有这一处
    let maintenance = Arc::new(maintenance::Maintenance {
        store: store.clone(),
        data_dir: data_dir.clone(),
        // 和传给 claude 的环境一致：复用本机登录时没有 CLAUDE_CONFIG_DIR，会话记录在 ~/.claude
        agent_records: claude_config_dir.map(|dir| maintenance::AgentRecords {
            root: dir.join("projects"),
            dir_name: ai_boot_agent::claude::project_slug,
            paths: Box::new(move |workdir: &Path| {
                ai_boot_agent::claude::session_records(&dir, workdir)
            }),
        }),
    });
    tokio::spawn(maintenance::run(Arc::clone(&maintenance), cancel.clone()));

    // 上次退出时没结束的轮次记为中断，卡片上给重试按钮
    let interrupted = store.interrupt_open_turns(store::now_ms()).await?;
    if !interrupted.is_empty() {
        tracing::warn!(
            count = interrupted.len(),
            "上次退出时有没结束的轮次，已记为中断"
        );
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.mark_interrupted(interrupted).await });
    }
    // 上次退出时还在写的写回：结果不明，卡片上给重试（重试会先查重）
    let unclear = store.interrupt_writebacks(store::now_ms()).await?;
    if let Some(writer) = writer.filter(|_| !unclear.is_empty()) {
        tracing::warn!(
            count = unclear.len(),
            "上次退出时有没写完的写回，已记为结果不明"
        );
        tokio::spawn(async move {
            for turn_id in unclear {
                writer.refresh_card(&turn_id).await;
            }
        });
    }
    let (jobs_tx, jobs_rx) = mpsc::channel(256);
    let registry = conversation::Registry::new(
        store.clone(),
        runner,
        Arc::clone(&bot_open_id),
        config.agent.backend.clone(),
        DEBOUNCE,
    )
    .with_eraser(maintenance);
    tokio::spawn(registry.run(jobs_rx));
    // 上次退出时还没分派的消息
    for message_id in store.unassigned_inputs().await? {
        jobs_tx
            .send(conversation::Job::Input { message_id })
            .await
            .context("处理队列已关闭")?;
    }

    let ingest = Arc::new(
        ingest::Ingest::new(store, whitelist, bot_open_id, jobs_tx)
            .with_card_lookup(Arc::clone(&api), config.feishu.app_id.clone()),
    );
    let mut ws_config = WsConfig::new(config.feishu.app_id.clone(), app_secret);
    ws_config.base_url = config.feishu.base_url.clone();
    let client = WsClient::new(ws_config).context("创建长连接客户端失败")?;
    tracing::info!("ai-boot 已启动");
    client.run(ingest, cancel).await;
    tracing::info!("ai-boot 已退出");
    Ok(())
}

/// Claude 后端：环境变量按白名单从 ai-boot 自己的环境里挑，其余一律不传。
pub(crate) fn claude_backend(agent: &config::AgentConfig) -> ClaudeBackend {
    ClaudeBackend::new(ClaudeConfig {
        program: agent.claude.program.clone(),
        hook_program: agent.hook.program.clone(),
        hook_timeout_secs: agent.hook.timeout_secs,
        env: claude_env(agent),
        stop_grace: Duration::from_secs(10),
    })
}

/// 传给 claude 的环境变量：按白名单从 ai-boot 自己的环境里挑，其余一律不传。
pub(crate) fn claude_env(agent: &config::AgentConfig) -> Vec<(OsString, OsString)> {
    agent
        .claude
        .env_passthrough
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect()
}

/// 数据目录里有聊天内容，只允许属主访问。已存在的目录不改权限（可能是
/// systemd 的 StateDirectory 管着），但权限过宽时要告警。
fn prepare_data_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("创建数据目录 {} 失败", dir.display()))?;
    let mode = std::fs::metadata(dir)
        .with_context(|| format!("读取数据目录 {} 失败", dir.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        tracing::warn!(
            dir = %dir.display(),
            mode = format!("{:o}", mode & 0o777),
            "数据目录对其他用户可访问，里面有聊天内容，建议改成 0700"
        );
    }
    Ok(())
}

/// 取机器人自己的 open_id。失败就退避重试；取到之前群消息不会被当成 @ 了机器人。
async fn fetch_bot_open_id(
    api: Arc<ApiClient>,
    cell: Arc<OnceLock<String>>,
    cancel: CancellationToken,
) {
    let mut delay = Duration::from_secs(5);
    loop {
        match api.bot_info().await {
            Ok(bot) => {
                tracing::info!(app_name = %bot.app_name, "已取得机器人信息");
                let _ = cell.set(bot.open_id);
                return;
            }
            Err(err) => {
                tracing::error!(%err, retry_in_secs = delay.as_secs(), "获取机器人信息失败")
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(300));
    }
}

fn spawn_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(err) => {
                    tracing::warn!(%err, "无法监听 SIGTERM，只响应 Ctrl-C");
                    std::future::pending::<()>().await;
                }
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
        tracing::info!("收到退出信号");
        cancel.cancel();
    });
}
