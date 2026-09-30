//! Agent CLI 的中立驱动层。
//!
//! 编排层只依赖 [`AgentBackend`] 和 [`event`] 里的类型；Claude Code 与 Codex
//! 的差异（会话 ID 由谁分配、结构化输出在哪、工具面怎么收窄）都收在各自的
//! 后端模块里。两家共用的是：子进程管理（[`process`]）、工具放行策略
//! （[`policy`]）和同一份输出契约。

pub mod claude;
pub mod event;
pub mod policy;
mod process;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use event::{Activity, AgentEvent, Draft, FailKind, McpStatus, Outcome, Started, Step, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Claude,
    Codex,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// 新开会话，还是续接已有会话（值是后端的会话 ID）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRef {
    New,
    Resume(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// 一个 stdio MCP server。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    /// 只放路径类配置，不放凭据：凭据由 server 自己从它的配置文件读。
    pub env: BTreeMap<String, String>,
}

/// 一轮执行的输入。
#[derive(Debug, Clone)]
pub struct TurnRequest {
    pub session: SessionRef,
    /// 走 stdin 传给 CLI，不进 argv（argv 在 `/proc/*/cmdline` 里对本机所有用户可见）。
    pub prompt: String,
    /// CLI 的工作目录，也是文件工具唯一能读的范围。必须已存在。
    pub workdir: PathBuf,
    /// 本轮生成的配置文件（MCP、hook）放这里。必须在 `workdir` 之外，
    /// 免得模型能读到或被当成项目配置加载。
    pub run_dir: PathBuf,
    /// 随本轮提问附带的图片。
    pub images: Vec<PathBuf>,
    /// 追加的规则文本。每轮原样传同一份：压缩后 CLI 会按本次参数重建系统提示。
    pub rules: String,
    /// 输出契约（OpenAI strict 兼容的 JSON Schema）。
    pub schema: serde_json::Value,
    pub mcp_servers: Vec<McpServer>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// 墙钟超时，超时按中断处理。
    pub timeout: Duration,
    /// 单轮预算（微美元）。实测 `--max-budget-usd` 按单次调用计，不含会话历史。
    pub budget_usd_micros: Option<u64>,
}

/// 一轮执行的句柄。
#[derive(Debug)]
pub struct TurnHandle {
    pub events: mpsc::Receiver<AgentEvent>,
    /// 触发后先发 SIGINT 让 CLI 体面地结束本轮，宽限期后杀掉整个进程组。
    pub cancel: CancellationToken,
    /// 启动时就能确定的会话 ID（Claude 由我们预先分配）；Codex 要等
    /// `Started` 事件。
    pub session_id: Option<String>,
}

#[derive(Debug)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub version: String,
    /// 解码层的回归样本是在哪个 CLI 版本上录的；和 `version` 不一致时解码可能要跟着改。
    pub recorded_version: Option<&'static str>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    #[error("找不到可执行文件 {program}：{source}")]
    NotFound {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("启动 {program} 失败：{source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("工作目录 {0} 不存在")]
    MissingWorkdir(PathBuf),
    #[error("写入 {path} 失败：{source}")]
    Prepare {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    Preflight(String),
}

/// Agent CLI 后端。
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    fn kind(&self) -> BackendKind;

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnHandle, AgentError>;

    /// 版本与可用性检查，给启动自检和 doctor 用。
    async fn preflight(&self) -> Result<BackendInfo, AgentError>;
}
