//! 中立的执行事件。编排层只认这些类型，看不到任何后端特有的形状。

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Started(Started),
    Step(Step),
    /// 会话累计用量。Claude 和 Codex 在 resume 之后报的都是整个会话的累计值，
    /// 每轮用量由调用方按前后两次做差。
    Usage(Usage),
    /// 终态，每轮恰好一条且在最后。
    Finished(Outcome),
    /// 看不懂的输出。不会让执行失败，但升级 CLI 后出现它说明防腐层要跟着改。
    Warning(String),
    /// 模型正在输出。进度卡靠它显示「思考中」「正在整理结论」，也靠它判断是不是卡住了。
    Activity(Activity),
    /// 结构化答案还在写，其中已经写完的字段。写整份答案要二三十秒，进度卡先露出结论。
    Draft(Draft),
}

/// 写到一半的结构化答案里已经完整的字段。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub title: Option<String>,
    pub summary: Option<String>,
}

/// 模型此刻在做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Thinking,
    /// 在工具调用之间说话。
    Writing,
    /// 在写最终的结构化答案。
    Concluding,
    /// 输出还在流，阶段没变。
    Streaming,
}

impl AgentEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished(_))
    }
}

/// 会话起点的元信息。后端给不出的字段为 `None`，自检只检查给得出的部分。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Started {
    pub session_id: String,
    pub model: String,
    pub cli_version: Option<String>,
    pub tools: Option<Vec<String>>,
    pub mcp_servers: Option<Vec<McpStatus>>,
    /// 插件的来源标识（如 `telemetry@builtin`）。
    pub plugins: Option<Vec<String>>,
    pub skills: Option<Vec<String>>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpStatus {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    ToolCall {
        id: String,
        tool: String,
        input: Value,
    },
    ToolDone {
        id: String,
        ok: bool,
        /// 结果摘要，有长度上限。
        preview: String,
    },
    /// 工具调用被拦下（权限层、hook 或 `--restricted`）。
    Denied {
        id: String,
        tool: String,
        reason: String,
    },
    /// 模型在工具调用之间说的话。
    Text(String),
    /// API 出错后 CLI 正在重试。
    Retry { attempt: u32, error: String },
    /// 订阅额度或 API 限流。
    RateLimited {
        status: String,
        /// Unix 秒。
        resets_at: Option<i64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// CLI 给出的估算成本（微美元）。订阅账号下只是参考值；Codex 不提供。
    pub cost_usd_micros: Option<i64>,
    /// 本轮最后一次请求带上的上下文（输入加缓存读写）。每次请求都要带上整段会话，
    /// 调用方据此决定下一轮是否把会话收拢成摘要、换新会话。给不出时为 0。
    pub context_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Success {
        /// 按输出契约给出的结构化结果；后端没给或给的不是 JSON 时为 `None`。
        structured: Option<Value>,
        text: String,
        turns: u32,
    },
    Failed {
        kind: FailKind,
        reason: String,
    },
    /// 被取消，或者 CLI 被外部中断。
    Interrupted,
    TimedOut,
    BudgetExceeded,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success { .. } => "success",
            Self::Failed { .. } => "failed",
            Self::Interrupted => "interrupted",
            Self::TimedOut => "timeout",
            Self::BudgetExceeded => "budget_exceeded",
        }
    }
}

/// 失败的大类，决定卡片上怎么提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailKind {
    /// 登录失效、token 过期。
    Auth,
    /// 额度耗尽或持续限流。
    RateLimit,
    /// 上游 API 错误。
    Api,
    /// CLI 自身异常退出、输出不完整。
    Cli,
    /// 运行时自检不通过（工具面、MCP 状态等与预期不符）。
    SelfCheck,
    /// 要续接的会话不存在（会话文件被清理、换了配置目录或工作目录）。
    /// 调用方据此改为新开会话。
    SessionNotFound,
    Other,
}
