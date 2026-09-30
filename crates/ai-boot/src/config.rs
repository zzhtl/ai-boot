//! 配置：TOML 文件，加上环境变量里的密钥。
//!
//! 密钥只从环境变量读、不进配置文件：配置文件要能放心地拷给别人看、贴进工单。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use secrecy::SecretString;
use serde::Deserialize;
use url::Url;

pub const APP_SECRET_ENV: &str = "FEISHU_APP_SECRET";

/// 群聊上下文最多拉多少条。
pub const MAX_WINDOW_MESSAGES: usize = 500;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub feishu: FeishuConfig,
    pub access: AccessConfig,
    pub storage: StorageConfig,
    pub agent: AgentConfig,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub context: ContextConfig,
    /// 以用户身份读云文档。不配就不读文档，只在答案里注明。
    pub oauth: Option<OauthConfig>,
    /// 闭环方案写回 Jira、Confluence。不配就只生成方案，不给写回按钮。
    pub writeback: Option<WritebackConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WritebackConfig {
    /// 写回用的 qtmcp（绝对路径）。和 Agent 用的是同一个程序，但不带只读开关。
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// 只放路径类配置，凭据由 qtmcp 自己从它的配置文件读。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 发布到 Confluence 的空间和父页面；都配了才提供这个目标。
    pub confluence_space: Option<String>,
    pub confluence_parent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OauthConfig {
    /// 回调服务监听的地址（本机内网 IP 和端口）。
    pub listen: std::net::SocketAddr,
    /// 在开发者后台「安全设置」里登记的重定向 URL，必须与这里完全一致。
    pub redirect_uri: Url,
    /// 授权页所在的域名。
    #[serde(default = "default_accounts_url")]
    pub accounts_url: Url,
    /// 申请的用户权限，与开发者后台开通的一致；必须包含 offline_access 才能续期。
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
}

fn default_accounts_url() -> Url {
    Url::parse("https://accounts.feishu.cn/").expect("常量地址必然合法")
}

fn default_scopes() -> Vec<String> {
    [
        "offline_access",
        "docs:document.content:read",
        "docx:document:readonly",
        "wiki:wiki:readonly",
        "sheets:spreadsheet:readonly",
        "bitable:app:readonly",
        "drive:file:download",
        "docs:document.media:download",
        "drive:drive.metadata:readonly",
    ]
    .map(str::to_owned)
    .to_vec()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
    pub app_id: String,
    /// 开放平台地址，必须以 `/` 结尾。
    #[serde(default = "default_base_url")]
    pub base_url: Url,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    /// 用户资料里的邮箱（不是企业邮箱），启动时解析成 open_id。
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// 直接给 open_id，不依赖通讯录权限。
    #[serde(default)]
    pub allowed_open_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// 目前只有 claude；codex 在 M7 接入。
    #[serde(default = "default_backend")]
    pub backend: String,
    pub model: Option<String>,
    /// low / medium / high / xhigh / max
    pub effort: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// 单轮预算（美元）。订阅账号下是估算值，只起熔断作用。
    pub budget_usd: Option<f64>,
    /// 同时跑几轮分析。SSO 共享会话（qtmcp Q-auth）上线前保持 1。
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    pub claude: ClaudeSection,
    pub hook: HookSection,
    #[serde(default)]
    pub mcp: Vec<McpSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeSection {
    /// 固定版本的 claude 可执行文件（绝对路径）。
    pub program: PathBuf,
    /// 传给 claude 的环境变量，按名字从 ai-boot 自己的环境里挑；其余一律不传。
    #[serde(default = "default_env_passthrough")]
    pub env_passthrough: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookSection {
    /// ai-boot 自己的可执行文件（绝对路径），CLI 通过它的 `hook` 子命令判决。
    pub program: PathBuf,
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpSection {
    pub name: String,
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// 只放路径类配置，凭据由 server 自己从它的配置文件读。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    /// 卡片里允许出现链接的 host（含子域），比如 Jira、Confluence、GitLab 的域名。
    #[serde(default)]
    pub link_hosts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    /// 群聊附带的最近消息：最多几条（0 表示不取）……
    #[serde(default = "default_window_messages")]
    pub window_messages: usize,
    /// ……以及往前看多少分钟（0 表示不限时间，只按条数）。
    #[serde(default = "default_window_minutes")]
    pub window_minutes: u64,
    /// 旧版 Office（doc、ppt）和 OpenDocument 用 soffice 转换后读取。
    #[serde(default = "default_office_legacy")]
    pub office_legacy: bool,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            window_messages: default_window_messages(),
            window_minutes: default_window_minutes(),
            office_legacy: default_office_legacy(),
        }
    }
}

fn default_window_messages() -> usize {
    500
}

fn default_window_minutes() -> u64 {
    0
}

fn default_office_legacy() -> bool {
    true
}

fn default_backend() -> String {
    "claude".to_owned()
}

fn default_timeout_secs() -> u64 {
    1200
}

fn default_max_concurrent() -> usize {
    1
}

fn default_hook_timeout_secs() -> u32 {
    10
}

fn default_env_passthrough() -> Vec<String> {
    [
        "HOME",
        "PATH",
        "LANG",
        "XDG_RUNTIME_DIR",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn default_base_url() -> Url {
    Url::parse("https://open.feishu.cn/").expect("常量地址必然合法")
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读取配置文件 {} 失败", path.display()))?;
        Self::parse(&raw).with_context(|| format!("配置文件 {} 有误", path.display()))
    }

    fn parse(raw: &str) -> anyhow::Result<Self> {
        let config: Self = toml::from_str(raw)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.feishu.app_id.trim().is_empty() {
            bail!("feishu.app_id 不能为空");
        }
        // 缺了结尾的 /，Url::join 会把最后一段路径吃掉
        if !self.feishu.base_url.path().ends_with('/') {
            bail!("feishu.base_url 必须以 / 结尾");
        }
        if self.access.allowed_emails.is_empty() && self.access.allowed_open_ids.is_empty() {
            bail!("白名单为空：access.allowed_emails 和 access.allowed_open_ids 至少配一个");
        }
        // 分页拉取，每页 50 条：再多既拖慢每一轮，也远超 prompt 能放下的量
        if self.context.window_messages > MAX_WINDOW_MESSAGES {
            bail!("context.window_messages 最多 {MAX_WINDOW_MESSAGES}");
        }
        if let Some(writeback) = &self.writeback {
            if !writeback.command.is_absolute() {
                bail!("writeback.command 必须是绝对路径");
            }
            if writeback.confluence_space.is_some() != writeback.confluence_parent_id.is_some() {
                bail!(
                    "writeback.confluence_space 和 writeback.confluence_parent_id 要么都配，要么都不配"
                );
            }
        }
        if let Some(oauth) = &self.oauth {
            if !oauth
                .redirect_uri
                .path()
                .ends_with(crate::oauth::CALLBACK_PATH)
            {
                bail!(
                    "oauth.redirect_uri 的路径必须是 {}",
                    crate::oauth::CALLBACK_PATH
                );
            }
            if !oauth.scopes.iter().any(|s| s == "offline_access") {
                bail!("oauth.scopes 必须包含 offline_access，否则 token 两小时后就无法续期");
            }
            if !oauth.accounts_url.path().ends_with('/') {
                bail!("oauth.accounts_url 必须以 / 结尾");
            }
        }
        self.agent.validate()
    }
}

impl AgentConfig {
    fn validate(&self) -> anyhow::Result<()> {
        if self.backend != "claude" {
            bail!("agent.backend 目前只支持 claude，收到 {:?}", self.backend);
        }
        if let Some(effort) = &self.effort
            && self.effort_level().is_none()
        {
            bail!("agent.effort 只能是 low/medium/high/xhigh/max，收到 {effort:?}");
        }
        if self.timeout_secs < 60 {
            bail!("agent.timeout_secs 至少 60 秒");
        }
        if !(1..=4).contains(&self.max_concurrent) {
            bail!("agent.max_concurrent 取 1~4");
        }
        if let Some(budget) = self.budget_usd
            && !(budget > 0.0 && budget.is_finite())
        {
            bail!("agent.budget_usd 必须是正数");
        }
        for (key, path) in [
            ("agent.claude.program", &self.claude.program),
            ("agent.hook.program", &self.hook.program),
        ] {
            if !path.is_absolute() {
                bail!("{key} 必须是绝对路径：{}", path.display());
            }
        }
        for server in &self.mcp {
            // 名字会拼进工具名 mcp__<name>__<tool>，也是放行表判断的依据
            let valid = !server.name.is_empty()
                && server
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !valid {
                bail!(
                    "MCP server 名只能用小写字母、数字和下划线：{:?}",
                    server.name
                );
            }
            if !server.command.is_absolute() {
                bail!("MCP server {} 的 command 必须是绝对路径", server.name);
            }
        }
        Ok(())
    }

    pub fn effort_level(&self) -> Option<ai_boot_agent::Effort> {
        use ai_boot_agent::Effort;
        match self.effort.as_deref()? {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }

    /// 单轮预算（微美元）。
    pub fn budget_micros(&self) -> Option<u64> {
        self.budget_usd
            .map(|usd| (usd * 1_000_000.0).round() as u64)
    }
}

pub fn app_secret_from_env() -> anyhow::Result<SecretString> {
    let value =
        std::env::var(APP_SECRET_ENV).with_context(|| format!("缺少环境变量 {APP_SECRET_ENV}"))?;
    if value.trim().is_empty() {
        bail!("环境变量 {APP_SECRET_ENV} 为空");
    }
    Ok(SecretString::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../config.example.toml");

    #[test]
    fn the_shipped_example_is_valid() {
        let config = Config::parse(EXAMPLE).expect("示例配置");
        assert_eq!(config.feishu.base_url.as_str(), "https://open.feishu.cn/");
        assert!(!config.access.allowed_emails.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let raw = format!("{EXAMPLE}\n[extra]\nkey = 1\n");
        assert!(Config::parse(&raw).is_err());
    }

    #[test]
    fn an_empty_whitelist_is_rejected() {
        let raw = r#"
            [feishu]
            app_id = "cli_x"
            [access]
            [storage]
            data_dir = "/tmp/x"
            [agent.claude]
            program = "/opt/ai-boot/bin/claude"
            [agent.hook]
            program = "/opt/ai-boot/bin/ai-boot"
        "#;
        let err = Config::parse(raw).expect_err("空白名单必须报错");
        assert!(err.to_string().contains("白名单"), "{err:#}");
    }

    #[test]
    fn a_base_url_without_trailing_slash_is_rejected() {
        let raw = r#"
            [feishu]
            app_id = "cli_x"
            base_url = "https://open.feishu.cn/x"
            [access]
            allowed_open_ids = ["ou_1"]
            [storage]
            data_dir = "/tmp/x"
            [agent.claude]
            program = "/opt/ai-boot/bin/claude"
            [agent.hook]
            program = "/opt/ai-boot/bin/ai-boot"
        "#;
        assert!(Config::parse(raw).is_err());
    }

    #[test]
    fn agent_settings_are_validated() {
        for (patch, fragment) in [
            ("backend = \"codex\"", "claude"),
            ("effort = \"extreme\"", "effort"),
            ("timeout_secs = 5", "timeout"),
            ("max_concurrent = 9", "max_concurrent"),
            ("budget_usd = -1.0", "budget"),
        ] {
            let raw = EXAMPLE.replacen("[agent]\n", &format!("[agent]\n{patch}\n"), 1);
            let err = Config::parse(&raw).expect_err(patch);
            assert!(format!("{err:#}").contains(fragment), "{patch}: {err:#}");
        }
    }

    #[test]
    fn mcp_names_and_paths_are_checked() {
        let raw = EXAMPLE.replace("name = \"qtmcp\"", "name = \"Qt-MCP\"");
        assert!(Config::parse(&raw).is_err());
        let raw = EXAMPLE.replace(
            "command = \"/opt/ai-boot/bin/qtmcp\"",
            "command = \"qtmcp\"",
        );
        assert!(Config::parse(&raw).is_err());
    }
}
