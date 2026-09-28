//! ai-boot：飞书问题排查机器人。

mod config;
mod echo;
mod ingest;
mod lock;
mod store;
mod whitelist;

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use ai_boot_feishu::api::ApiClient;
use ai_boot_feishu::ws::{WsClient, WsConfig};
use anyhow::Context as _;
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

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
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    // 运行时手动构建而不是用 #[tokio::main]：之后的 hook 子命令要在
    // 构建运行时之前就分发出去（hook 是短命进程，起运行时既慢又多一个失败点）
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
    let result = match cli.command {
        Command::Run { config } => runtime.block_on(run(&config)),
    };
    match result {
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

    let (jobs_tx, jobs_rx) = mpsc::channel(256);
    tokio::spawn(echo::run(Arc::clone(&api), store.clone(), jobs_rx));
    // 上次退出时还没处理完的消息
    for message_id in store.pending_inputs().await? {
        jobs_tx
            .send(echo::Job::Echo { message_id })
            .await
            .context("处理队列已关闭")?;
    }

    let ingest = Arc::new(ingest::Ingest::new(store, whitelist, bot_open_id, jobs_tx));
    let mut ws_config = WsConfig::new(config.feishu.app_id.clone(), app_secret);
    ws_config.base_url = config.feishu.base_url.clone();
    let client = WsClient::new(ws_config).context("创建长连接客户端失败")?;
    tracing::info!("ai-boot 已启动");
    client.run(ingest, cancel).await;
    tracing::info!("ai-boot 已退出");
    Ok(())
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
