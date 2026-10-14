//! 使用本机 Codex 刷新目录，只提取允许展示的模型，不读取登录凭据。

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use super::CodexBackend;
use crate::{AgentError, ModelOption};

#[derive(Deserialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    slug: String,
    display_name: String,
    visibility: String,
}

impl CodexBackend {
    pub(super) async fn query_models(&self) -> Result<Vec<ModelOption>, AgentError> {
        // debug models 不接受 -p；复用 CODEX_HOME 中的当前账号和模型目录。
        // 不传 --bundled，让 CLI 刷新远端目录。
        let mut command = tokio::process::Command::new(&self.config.program);
        command
            .args(["debug", "models"])
            .env_clear()
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(20), command.output())
            .await
            .map_err(|_| AgentError::Preflight("实时查询 Codex 模型超时".into()))?
            .map_err(|source| AgentError::Spawn {
                program: self.config.program.display().to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(AgentError::Preflight("Codex 模型目录查询失败".into()));
        }
        let catalog: Catalog = serde_json::from_slice(&output.stdout)
            .map_err(|err| AgentError::Preflight(format!("无法解析 Codex 模型目录：{err}")))?;
        // 保持服务端的目录顺序，过滤隐藏项后取前五个，不按名称猜测发布时间。
        let models: Vec<_> = catalog
            .models
            .into_iter()
            .filter(|model| model.visibility == "list" && !model.slug.is_empty())
            .take(5)
            .map(|model| ModelOption {
                id: model.slug,
                name: model.display_name,
            })
            .collect();
        if models.is_empty() {
            return Err(AgentError::Preflight("Codex 没有返回可选模型".into()));
        }
        Ok(models)
    }
}
