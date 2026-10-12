//! Codex 后端：`codex exec -p <profile> --json --output-schema ...`。
//! 复用本机 CODEX_HOME、配置、skills、插件和规则；提问和回复约束只走 stdin。

pub mod decode;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, ProcessSpec};
use crate::{
    AgentBackend, AgentError, BackendInfo, BackendKind, Effort, SessionRef, TurnHandle, TurnRequest,
};

pub const RECORDED_VERSION: &str = "0.161.0";

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub program: PathBuf,
    pub profile: Option<String>,
    pub env: Vec<(OsString, OsString)>,
    pub stop_grace: Duration,
}

pub struct CodexBackend {
    config: CodexConfig,
}

impl CodexBackend {
    pub fn new(config: CodexConfig) -> Self {
        Self { config }
    }

    pub fn config_dir(&self) -> Option<PathBuf> {
        let var = |name: &str| {
            self.config
                .env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| PathBuf::from(value))
        };
        var("CODEX_HOME").or_else(|| var("HOME").map(|home| home.join(".codex")))
    }

    pub fn records_root(&self) -> Option<PathBuf> {
        // 只用于清理旧版隔离目录；共享 CODEX_HOME 下的历史由 Codex 自己管理。
        self.config_dir().map(|home| home.join("ai-boot-sessions"))
    }

    /// 未显式指定模型时，按本机配置和 profile 解析模型，供启动参数和日志使用。
    fn configured_model(&self) -> Option<String> {
        let home = self.config_dir()?;
        let profiles = self
            .config
            .profile
            .iter()
            .map(|name| home.join(format!("{name}.config.toml")));
        for path in profiles.chain([home.join("config.toml")]) {
            let Ok(raw) = std::fs::read_to_string(path) else {
                continue;
            };
            let Ok(config) = toml::from_str::<toml::Value>(&raw) else {
                continue;
            };
            if let Some(model) = config
                .get("model")
                .and_then(toml::Value::as_str)
                .filter(|model| !model.trim().is_empty())
            {
                return Some(model.to_owned());
            }
        }
        None
    }

    fn args(&self, request: &TurnRequest, schema: &Path) -> Result<Vec<String>, AgentError> {
        if request.budget_usd_micros.is_some() {
            return Err(AgentError::Preflight(
                "Codex 不提供美元预算参数，请去掉 agent.budget_usd".into(),
            ));
        }
        let mut args = vec!["-a".into(), "never".into(), "exec".into()];
        if let Some(profile) = &self.config.profile {
            args.extend(["-p".into(), profile.clone()]);
        }
        // 与终端一样加载本机配置；飞书只额外提供结构化输出和本轮覆盖项。
        args.extend([
            "--skip-git-repo-check".into(),
            "--json".into(),
            "--output-schema".into(),
            schema.display().to_string(),
        ]);
        if let Some(model) = &request.model {
            args.extend(["-m".into(), model.clone()]);
        }
        args.extend([
            "-c".into(),
            format!(
                "model_reasoning_effort={}",
                toml::Value::String(request.effort.unwrap_or(Effort::High).as_str().into())
            ),
        ]);
        for server in &request.mcp_servers {
            let mut config = toml::Table::new();
            config.insert(
                "command".into(),
                toml::Value::String(server.command.display().to_string()),
            );
            config.insert(
                "args".into(),
                toml::Value::Array(
                    server
                        .args
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect(),
                ),
            );
            config.insert(
                "env".into(),
                toml::Value::Table(
                    server
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
                        .collect(),
                ),
            );
            config.insert("required".into(), toml::Value::Boolean(true));
            // qtmcp 的一个工具同时支持读写 action，没有 readOnlyHint；排错时的
            // get/list/log 也会被当成需要确认。飞书无人值守，显式预授权这个 server。
            if server.name == "qtmcp" {
                config.insert(
                    "default_tools_approval_mode".into(),
                    toml::Value::String("approve".into()),
                );
            }
            config.insert("startup_timeout_sec".into(), toml::Value::Integer(30));
            config.insert("tool_timeout_sec".into(), toml::Value::Integer(120));
            // 按字段叠加显式配置，保留本机其余 MCP server 及未覆盖的选项。
            for (key, value) in config {
                args.extend([
                    "-c".into(),
                    format!("mcp_servers.{}.{key}={value}", server.name),
                ]);
            }
        }
        // resume 的子命令不接受 -p 等全局参数，所有选项放在 resume 之前。
        if let SessionRef::Resume(id) = &request.session {
            args.extend(["resume".into(), id.clone()]);
        }
        for image in &request.images {
            args.extend(["--image".into(), image.display().to_string()]);
        }
        args.push("-".into());
        Ok(args)
    }
}

#[async_trait::async_trait]
impl AgentBackend for CodexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Codex
    }

    async fn start_turn(&self, mut request: TurnRequest) -> Result<TurnHandle, AgentError> {
        use std::os::unix::fs::DirBuilderExt as _;
        if request.model.is_none() {
            request.model = self.configured_model();
        }
        if !request.workdir.is_dir() {
            return Err(AgentError::MissingWorkdir(request.workdir));
        }
        let tmp = request.workdir.join(".tmp");
        for dir in [&request.run_dir, &tmp] {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|source| AgentError::Prepare {
                    path: dir.clone(),
                    source,
                })?;
        }
        let schema = request.run_dir.join("schema.json");
        std::fs::write(&schema, strict_schema(request.schema.clone()).to_string()).map_err(
            |source| AgentError::Prepare {
                path: schema.clone(),
                source,
            },
        )?;
        let args = self.args(&request, &schema)?;
        let model = request.model.clone().unwrap_or_default();
        let mut env = self.config.env.clone();
        env.retain(|(key, _)| key != "TMPDIR");
        env.push(("TMPDIR".into(), tmp.into_os_string()));
        let stdin = format!(
            "<ai-boot-rules>\n{}\n</ai-boot-rules>\n\n{}",
            request.rules, request.prompt
        )
        .into_bytes();
        process::spawn(
            ProcessSpec {
                program: self.config.program.clone(),
                args,
                cwd: request.workdir,
                env,
                stdin,
                timeout: request.timeout,
                stop_grace: self.config.stop_grace,
            },
            decode::Decoder::new(model),
            None,
        )
    }

    async fn preflight(&self) -> Result<BackendInfo, AgentError> {
        let output = tokio::process::Command::new(&self.config.program)
            .arg("--version")
            .env_clear()
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|source| AgentError::NotFound {
                program: self.config.program.display().to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(AgentError::Preflight("`codex --version` 失败".into()));
        }
        Ok(BackendInfo {
            kind: BackendKind::Codex,
            version: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            recorded_version: Some(RECORDED_VERSION),
        })
    }
}

/// Codex strict 输出要求每个对象的全部字段都在 required 中；Claude 允许省略的
/// 图片、图表等数组，在 Codex 下由模型输出空数组，不改动共用的原始契约。
fn strict_schema(mut schema: serde_json::Value) -> serde_json::Value {
    match &mut schema {
        serde_json::Value::Object(object) => {
            for value in object.values_mut() {
                *value = strict_schema(std::mem::take(value));
            }
            if let Some(properties) = object
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                let required = properties
                    .keys()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect();
                object.insert("required".into(), serde_json::Value::Array(required));
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                *value = strict_schema(std::mem::take(value));
            }
        }
        _ => {}
    }
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_follows_the_local_default_and_profile_override() {
        let home = tempfile::tempdir().expect("Codex 配置目录");
        let backend = CodexBackend::new(CodexConfig {
            program: "/bin/codex".into(),
            profile: Some("ai-boot".into()),
            env: vec![("CODEX_HOME".into(), home.path().as_os_str().into())],
            stop_grace: Duration::from_secs(1),
        });
        assert_eq!(backend.configured_model(), None);
        std::fs::write(home.path().join("config.toml"), "model = 'local-default'\n")
            .expect("本机模型");
        assert_eq!(backend.configured_model().as_deref(), Some("local-default"));
        std::fs::write(
            home.path().join("ai-boot.config.toml"),
            "model_reasoning_effort = 'xhigh'\n",
        )
        .expect("profile");
        assert_eq!(backend.configured_model().as_deref(), Some("local-default"));
        std::fs::write(home.path().join("config.toml"), "model = 'new-default'\n")
            .expect("切换默认模型");
        assert_eq!(backend.configured_model().as_deref(), Some("new-default"));
        std::fs::write(
            home.path().join("ai-boot.config.toml"),
            "model = 'profile-model'\n",
        )
        .expect("指定模型");
        assert_eq!(backend.configured_model().as_deref(), Some("profile-model"));
    }

    #[test]
    fn qtmcp_is_preapproved_for_both_new_and_resumed_turns() {
        let backend = CodexBackend::new(CodexConfig {
            program: "/bin/codex".into(),
            profile: None,
            env: Vec::new(),
            stop_grace: Duration::from_secs(1),
        });
        for session in [SessionRef::New, SessionRef::Resume("existing".into())] {
            let request = TurnRequest {
                session,
                prompt: String::new(),
                workdir: "/work".into(),
                run_dir: "/run".into(),
                images: Vec::new(),
                rules: String::new(),
                schema: serde_json::json!({}),
                mcp_servers: ["qtmcp", "other"]
                    .into_iter()
                    .map(|name| crate::McpServer {
                        name: name.into(),
                        command: "/bin/mcp".into(),
                        args: Vec::new(),
                        env: Default::default(),
                    })
                    .collect(),
                model: None,
                effort: None,
                timeout: Duration::from_secs(1),
                budget_usd_micros: None,
            };
            let args = backend
                .args(&request, Path::new("/run/schema.json"))
                .expect("参数");
            let value = args
                .iter()
                .filter(|arg| arg.starts_with("mcp_servers."))
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("\n");
            let config: toml::Value = toml::from_str(&value).expect("TOML");
            assert_eq!(
                config["mcp_servers"]["qtmcp"]["default_tools_approval_mode"].as_str(),
                Some("approve")
            );
            assert!(
                config["mcp_servers"]["other"]
                    .get("default_tools_approval_mode")
                    .is_none()
            );
            assert_eq!(&args[..2], ["-a", "never"]);
        }
    }

    #[test]
    fn strict_output_requires_optional_fields_in_nested_objects_too() {
        let schema = serde_json::json!({"type":"object", "properties": {
            "sections": {"type":"array", "items": {"type":"object", "properties": {
                "body":{"type":"string"}, "images":{"type":"array", "items":{"type":"string"}}
            }, "required":["body"]}}
        }});
        let strict = strict_schema(schema.clone());
        assert_eq!(strict["required"], serde_json::json!(["sections"]));
        assert_eq!(
            strict["properties"]["sections"]["items"]["required"],
            serde_json::json!(["body", "images"])
        );
        assert_eq!(
            schema["properties"]["sections"]["items"]["required"],
            serde_json::json!(["body"])
        );
    }
}
