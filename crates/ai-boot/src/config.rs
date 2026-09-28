//! 配置：TOML 文件，加上环境变量里的密钥。
//!
//! 密钥只从环境变量读、不进配置文件：配置文件要能放心地拷给别人看、贴进工单。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use secrecy::SecretString;
use serde::Deserialize;
use url::Url;

pub const APP_SECRET_ENV: &str = "FEISHU_APP_SECRET";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub feishu: FeishuConfig,
    pub access: AccessConfig,
    pub storage: StorageConfig,
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
        Ok(())
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
        "#;
        assert!(Config::parse(raw).is_err());
    }
}
