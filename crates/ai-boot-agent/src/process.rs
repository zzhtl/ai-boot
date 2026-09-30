//! 驱动 Agent CLI 子进程：两家 CLI 共用。
//!
//! - 环境变量先清空再按白名单传：飞书 AppSecret、个人 token 不该出现在
//!   模型能间接触达的进程里。
//! - prompt 写进 stdin 后立刻关闭：不关 CLI 会一直等输入；放 argv 又会被本机
//!   任何用户从 `/proc` 读到。
//! - 子进程自成进程组。停止时先对 CLI 发 SIGINT，让它体面地结束本轮、会话
//!   还能续接（实测 Claude 不到 1 秒就退出）；宽限期过了才对整组发 SIGKILL。
//!   CLI 退出后也会对整组补一刀，清掉它拉起却没退出的子进程——否则残留进程
//!   占着管道，输出永远等不到 EOF。
//! - 终态只有一条：我们主动停下时，不管 CLI 自己报了什么结局，都以「中断 /
//!   超时」为准；CLI 退出却没给终态，就补一条失败。

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::mpsc;
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};
use tokio_util::sync::CancellationToken;

use crate::event::{AgentEvent, FailKind, Outcome};
use crate::{AgentError, TurnHandle};

/// 事件通道容量。满了就背压到读循环，也就背压到 CLI 的 stdout。
const EVENT_CHANNEL_CAPACITY: usize = 256;
/// 单行上限。工具结果（比如一整页 Confluence 正文）会整个出现在一行里。
const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
/// stderr 只留末尾几行，拼进「异常退出」的原因里。
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_CHARS: usize = 500;
/// CLI 退出后继续读剩余输出的时长。
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(1);
/// CLI 关闭 stdout 之后等它退出的时长。
const EXIT_WAIT: Duration = Duration::from_secs(5);
/// 收尾时等 stdin 写入任务、stderr 收集任务的时长。
const TASK_WAIT: Duration = Duration::from_secs(1);

/// 逐行解码 CLI 的 JSONL 输出。
pub(crate) trait Decode: Send + 'static {
    fn line(&mut self, line: &str) -> Vec<AgentEvent>;
}

pub(crate) struct ProcessSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
    /// SIGINT 之后等多久才对整个进程组发 SIGKILL。
    pub stop_grace: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancelled,
    TimedOut,
}

impl Stop {
    fn outcome(self) -> Outcome {
        match self {
            Self::Cancelled => Outcome::Interrupted,
            Self::TimedOut => Outcome::TimedOut,
        }
    }
}

pub(crate) fn spawn(
    spec: ProcessSpec,
    decoder: impl Decode,
    session_id: Option<String>,
) -> Result<TurnHandle, AgentError> {
    if !spec.cwd.is_dir() {
        return Err(AgentError::MissingWorkdir(spec.cwd));
    }
    let program = spec.program.display().to_string();
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            AgentError::NotFound {
                program: program.clone(),
                source,
            }
        } else {
            AgentError::Spawn {
                program: program.clone(),
                source,
            }
        }
    })?;

    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let cancel = CancellationToken::new();
    tokio::spawn(drive(child, spec, decoder, tx, cancel.clone()));
    Ok(TurnHandle {
        events: rx,
        cancel,
        session_id,
    })
}

async fn drive(
    mut child: Child,
    spec: ProcessSpec,
    mut decoder: impl Decode,
    tx: mpsc::Sender<AgentEvent>,
    cancel: CancellationToken,
) {
    let pid = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(Pid::from_raw);

    // stdin 单独一个任务写：prompt 可能比管道缓冲大，CLI 开始读之前写会阻塞
    let stdin = child.stdin.take();
    let input = spec.stdin;
    // 写完后 stdin 随任务结束被 drop，即关闭；CLI 读到 EOF 才会开始执行
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin
            && let Err(err) = stdin.write_all(&input).await
        {
            tracing::warn!(%err, "写入 CLI 的 stdin 失败");
        }
    });
    let stderr_tail = Arc::new(Mutex::new(VecDeque::with_capacity(STDERR_TAIL_LINES)));
    let stderr_task = tokio::spawn(collect_stderr(
        child.stderr.take(),
        Arc::clone(&stderr_tail),
    ));

    let mut stop: Option<Stop> = None;
    let mut terminal_sent = false;
    // CLI 已退出时的退出状态（`None` 表示状态取不到）
    let mut exited: Option<Option<ExitStatus>> = None;
    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline);
    // 停下之后继续读输出，直到 EOF、CLI 退出或宽限期结束：既拿得到最后的
    // 用量，也不会因为管道写满把 CLI 卡住
    let grace_end = tokio::time::sleep(Duration::MAX);
    tokio::pin!(grace_end);
    let drain_end = tokio::time::sleep(Duration::MAX);
    tokio::pin!(drain_end);

    if let Some(stdout) = child.stdout.take() {
        let mut lines = FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        let running = |stop: &Option<Stop>, exited: &Option<Option<ExitStatus>>| {
            stop.is_none() && exited.is_none()
        };
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if running(&stop, &exited) => {
                    stop = Some(Stop::Cancelled);
                    interrupt(pid);
                    grace_end.as_mut().reset(tokio::time::Instant::now() + spec.stop_grace);
                }
                () = &mut deadline, if running(&stop, &exited) => {
                    tracing::warn!(timeout_secs = spec.timeout.as_secs(), "执行超时，中断 CLI");
                    stop = Some(Stop::TimedOut);
                    interrupt(pid);
                    grace_end.as_mut().reset(tokio::time::Instant::now() + spec.stop_grace);
                }
                // 没人收事件了：不等下一行输出才发现，立刻让 CLI 停下
                () = tx.closed(), if running(&stop, &exited) => {
                    stop = Some(Stop::Cancelled);
                    interrupt(pid);
                    grace_end.as_mut().reset(tokio::time::Instant::now() + spec.stop_grace);
                }
                () = &mut grace_end, if stop.is_some() && exited.is_none() => break,
                () = &mut drain_end, if exited.is_some() => break,
                status = child.wait(), if exited.is_none() => {
                    exited = Some(status.ok());
                    drain_end.as_mut().reset(tokio::time::Instant::now() + DRAIN_AFTER_EXIT);
                }
                next = lines.next() => match next {
                    Some(Ok(line)) => {
                        for event in decoder.line(&line) {
                            if event.is_terminal() {
                                // 主动停下时 CLI 会补一条「执行出错」的终态，以我们的为准
                                if stop.is_some() || terminal_sent {
                                    continue;
                                }
                                terminal_sent = true;
                            }
                            let _ = tx.send(event).await;
                        }
                    }
                    Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                        let _ = tx
                            .send(AgentEvent::Warning(format!(
                                "CLI 输出了超过 {MAX_LINE_BYTES} 字节的一行，已丢弃"
                            )))
                            .await;
                    }
                    Some(Err(LinesCodecError::Io(err))) => {
                        tracing::warn!(%err, "读取 CLI 输出失败");
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    let status = match exited {
        Some(status) => status,
        None => {
            let limit = if stop.is_some() {
                grace_end
                    .deadline()
                    .saturating_duration_since(tokio::time::Instant::now())
            } else {
                EXIT_WAIT
            };
            wait_exit(&mut child, pid, limit).await
        }
    };
    // CLI 已退出。组里还有成员（残留的子进程）时组 ID 不会被系统复用，
    // 所以这一刀只会落在它自己拉起的进程上；没有成员时调用返回 ESRCH
    kill_group(pid);

    let _ = tokio::time::timeout(TASK_WAIT, writer).await;
    let _ = tokio::time::timeout(TASK_WAIT, stderr_task).await;
    let tail: Vec<String> = stderr_tail
        .lock()
        .map(|tail| tail.iter().cloned().collect())
        .unwrap_or_default();

    if let Some(stop) = stop {
        if !terminal_sent {
            let _ = tx.send(AgentEvent::Finished(stop.outcome())).await;
        }
        return;
    }
    if !terminal_sent {
        let reason = abnormal_exit_reason(status, &tail);
        let _ = tx
            .send(AgentEvent::Finished(Outcome::Failed {
                kind: FailKind::Cli,
                reason,
            }))
            .await;
    }
}

/// 请 CLI 结束本轮。
fn interrupt(pid: Option<Pid>) {
    if let Some(pid) = pid
        && let Err(err) = kill_process(pid, Signal::INT)
    {
        tracing::debug!(%err, "发送 SIGINT 失败（进程可能已退出）");
    }
}

fn kill_group(pid: Option<Pid>) {
    if let Some(pid) = pid
        && let Err(err) = kill_process_group(pid, Signal::KILL)
        && err != rustix::io::Errno::SRCH
    {
        tracing::debug!(%err, "对进程组发送 SIGKILL 失败");
    }
}

/// 等 CLI 退出，超过 `limit` 就杀掉整个进程组（此时 CLI 还没被回收）。
async fn wait_exit(child: &mut Child, pid: Option<Pid>, limit: Duration) -> Option<ExitStatus> {
    match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => return Some(status),
        Ok(Err(err)) => {
            tracing::warn!(%err, "等待 CLI 退出失败");
            return None;
        }
        Err(_) => {}
    }
    tracing::warn!("CLI 未在时限内退出，杀掉整个进程组");
    kill_group(pid);
    child.wait().await.ok()
}

async fn collect_stderr(stderr: Option<ChildStderr>, tail: Arc<Mutex<VecDeque<String>>>) {
    let Some(stderr) = stderr else {
        return;
    };
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "agent_cli::stderr", "{line}");
        if let Ok(mut tail) = tail.lock() {
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.chars().take(STDERR_LINE_CHARS).collect());
        }
    }
}

fn abnormal_exit_reason(status: Option<ExitStatus>, tail: &[String]) -> String {
    let head = match status.and_then(|s| s.code()) {
        Some(code) => format!("CLI 退出但没有给出结果（退出码 {code}）"),
        None => "CLI 退出但没有给出结果（被信号终止）".to_owned(),
    };
    if tail.is_empty() {
        head
    } else {
        format!("{head}。stderr 末尾：{}", tail.join(" / "))
    }
}
