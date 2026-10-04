//! Claude Code 后端：`claude -p --output-format stream-json`。
//!
//! 会话 ID 由我们预先分配（`--session-id`），所以启动那一刻就能落库，崩溃后
//! 也能续接。MCP 与 hook 的配置文件写在 `run_dir`（工作目录之外），模型读不到，
//! 也不会被当成项目配置加载。
//!
//! 超大的工具结果（整页 Confluence、长日志）CLI 不放进上下文，而是存到
//! `<配置目录>/projects/<工作目录>/<会话>/tool-results/` 下、只回一个路径。这个目录
//! 在工作目录外，得同时交给 `--add-dir` 和 hook，模型才读得到。

pub mod decode;
mod invocation;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

use crate::process::{self, ProcessSpec};
use crate::{
    AgentBackend, AgentError, BackendInfo, BackendKind, SessionRef, TurnHandle, TurnRequest,
};
use invocation::{Invocation, SessionArg, shell_quote};

/// `tests/fixtures/claude` 录制时的 CLI 版本。线上版本和它不一致时，防腐层可能
/// 看不懂新的输出：先重录 fixtures、跑一遍解码测试再升级。
pub const RECORDED_VERSION: &str = "2.1.283";

const MCP_CONFIG_FILE: &str = "mcp.json";
const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    /// 固定版本的 claude 可执行文件。
    pub program: PathBuf,
    /// ai-boot 自己的可执行文件，hook 通过它的 `hook` 子命令判决。用配置里的
    /// 绝对路径而不是 `current_exe()`：二进制被替换后后者会带上 `(deleted)`。
    pub hook_program: PathBuf,
    /// hook 超时（秒）。超时等同 hook 失效，需要批准的调用会被自动拒绝。
    pub hook_timeout_secs: u32,
    /// 传给 CLI 的全部环境变量（调用方已按白名单挑好，比如 HOME、PATH、
    /// CLAUDE_CONFIG_DIR 和登录 token）。
    pub env: Vec<(OsString, OsString)>,
    /// SIGINT 之后等多久才杀掉整个进程组。
    pub stop_grace: Duration,
}

pub struct ClaudeBackend {
    config: ClaudeConfig,
}

impl ClaudeBackend {
    pub fn new(config: ClaudeConfig) -> Self {
        Self { config }
    }

    /// CLI 的配置目录：会话记录、超大工具结果都在它下面。按实际传给 CLI 的环境算，
    /// 和 CLI 自己的取法一致：有 `CLAUDE_CONFIG_DIR` 用它，否则是 `~/.claude`。
    pub fn config_dir(&self) -> Option<PathBuf> {
        let var = |name: &str| {
            self.config
                .env
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| PathBuf::from(value))
        };
        var("CLAUDE_CONFIG_DIR").or_else(|| var("HOME").map(|home| home.join(".claude")))
    }

    fn write_run_files(
        &self,
        request: &TurnRequest,
        results_dir: Option<&Path>,
    ) -> Result<(PathBuf, PathBuf), AgentError> {
        create_private_dir(&request.run_dir)?;

        let servers: serde_json::Map<String, serde_json::Value> = request
            .mcp_servers
            .iter()
            .map(|server| {
                (
                    server.name.clone(),
                    json!({
                        "command": server.command,
                        "args": server.args,
                        "env": server.env,
                    }),
                )
            })
            .collect();
        let mcp_path = request.run_dir.join(MCP_CONFIG_FILE);
        write_private(&mcp_path, &json!({ "mcpServers": servers }))?;

        let mut command = format!(
            "{} hook --backend claude --workdir {}",
            shell_quote(&self.config.hook_program.display().to_string()),
            shell_quote(&request.workdir.display().to_string()),
        );
        if let Some(dir) = results_dir {
            command.push_str(" --read-dir ");
            command.push_str(&shell_quote(&dir.display().to_string()));
        }
        let hook = json!([{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": command,
                "timeout": self.config.hook_timeout_secs,
            }],
        }]);
        let settings_path = request.run_dir.join(SETTINGS_FILE);
        write_private(
            &settings_path,
            &json!({ "hooks": { "PermissionRequest": hook, "PreToolUse": hook } }),
        )?;
        Ok((mcp_path, settings_path))
    }
}

#[async_trait::async_trait]
impl AgentBackend for ClaudeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Claude
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnHandle, AgentError> {
        if !request.workdir.is_dir() {
            return Err(AgentError::MissingWorkdir(request.workdir));
        }
        let session_id = match &request.session {
            SessionRef::New => uuid::Uuid::now_v7().to_string(),
            SessionRef::Resume(id) => id.clone(),
        };
        // 先建好：hook 按真实路径判断，不存在的目录没法比
        let results_dir = self
            .config_dir()
            .map(|dir| results_dir(&dir, &request.workdir, &session_id));
        if let Some(dir) = &results_dir {
            create_private_dir(dir)?;
        }
        let (mcp_path, settings_path) = self.write_run_files(&request, results_dir.as_deref())?;
        let session = match &request.session {
            SessionRef::New => SessionArg::New(&session_id),
            SessionRef::Resume(_) => SessionArg::Resume(&session_id),
        };
        let invocation = Invocation::build(
            &request,
            session,
            &mcp_path,
            &settings_path,
            results_dir.as_deref(),
        );
        tracing::debug!(
            target: "claude::invocation",
            command = %invocation.command_line(&self.config.program.display().to_string()),
            "启动 claude"
        );
        // 临时文件放进本轮的运行目录，这一轮结束随它一起删：CLI 自己的命令输出、模型
        // 跑命令时 mktemp 出来的文件都不会留在服务的 /tmp 里
        let tmp = request.run_dir.join("tmp");
        create_private_dir(&tmp)?;
        let mut env = self.config.env.clone();
        env.retain(|(key, _)| key != "TMPDIR");
        env.push(("TMPDIR".into(), tmp.into_os_string()));
        let spec = ProcessSpec {
            program: self.config.program.clone(),
            args: invocation.args().to_vec(),
            cwd: request.workdir,
            env,
            stdin: request.prompt.into_bytes(),
            timeout: request.timeout,
            stop_grace: self.config.stop_grace,
        };
        process::spawn(spec, decode::Decoder::new(), Some(session_id))
    }

    async fn preflight(&self) -> Result<BackendInfo, AgentError> {
        let output = tokio::process::Command::new(&self.config.program)
            .arg("--version")
            .env_clear()
            .envs(self.config.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|source| AgentError::NotFound {
                program: self.config.program.display().to_string(),
                source,
            })?;
        if !output.status.success() {
            return Err(AgentError::Preflight(format!(
                "`claude --version` 失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        // 形如「2.1.283 (Claude Code)」
        let version = text
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned();
        Ok(BackendInfo {
            kind: BackendKind::Claude,
            version,
            recorded_version: Some(RECORDED_VERSION),
        })
    }
}

/// CLI 按工作目录存会话：目录名是路径里所有非字母数字字符换成 `-`。
pub fn project_slug(workdir: &Path) -> String {
    workdir
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// CLI 为某个工作目录留下的全部记录：`projects/` 下的会话记录（含超大工具结果），以及
/// 其中每个会话在 `session-env/` 下的目录。删会话时一起删，不留残留。
pub fn session_records(config_dir: &Path, workdir: &Path) -> Vec<PathBuf> {
    let project = config_dir.join("projects").join(project_slug(workdir));
    let sessions: Vec<PathBuf> = std::fs::read_dir(&project)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let id = Path::new(&name)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
                .then(|| Path::new(&name).file_stem().map(ToOwned::to_owned))??;
            Some(config_dir.join("session-env").join(id))
        })
        .collect();
    std::iter::once(project).chain(sessions).collect()
}

/// 某个会话的超大工具结果存放的目录。
fn results_dir(config_dir: &Path, workdir: &Path, session_id: &str) -> PathBuf {
    config_dir
        .join("projects")
        .join(project_slug(workdir))
        .join(session_id)
        .join("tool-results")
}

fn create_private_dir(dir: &Path) -> Result<(), AgentError> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| AgentError::Prepare {
            path: dir.to_path_buf(),
            source,
        })
}

fn write_private(path: &Path, value: &serde_json::Value) -> Result<(), AgentError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let prepare = |source| AgentError::Prepare {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(prepare)?;
    file.write_all(value.to_string().as_bytes())
        .map_err(prepare)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpServer;
    use std::collections::BTreeMap;

    fn backend() -> ClaudeBackend {
        ClaudeBackend::new(ClaudeConfig {
            program: PathBuf::from("/opt/ai-boot/bin/claude"),
            hook_program: PathBuf::from("/opt/ai boot/bin/ai-boot"),
            hook_timeout_secs: 10,
            env: Vec::new(),
            stop_grace: Duration::from_secs(10),
        })
    }

    fn request(run_dir: PathBuf, workdir: PathBuf) -> TurnRequest {
        TurnRequest {
            session: SessionRef::New,
            prompt: String::new(),
            workdir,
            run_dir,
            images: Vec::new(),
            rules: String::new(),
            schema: json!({}),
            mcp_servers: vec![McpServer {
                name: "qtmcp".into(),
                command: PathBuf::from("/opt/ai-boot/bin/qtmcp"),
                args: vec!["--read-only".into()],
                env: BTreeMap::from([("QTMCP_STATE_DIR".into(), "/var/lib/qtmcp".into())]),
            }],
            model: None,
            effort: None,
            timeout: Duration::from_secs(1),
            budget_usd_micros: None,
        }
    }

    #[test]
    fn run_files_wire_both_hooks_and_only_the_given_servers() {
        let root = tempfile::tempdir().expect("临时目录");
        let run = root.path().join("run");
        let work = root.path().join("work dir");
        std::fs::create_dir(&work).expect("工作目录");
        let results = root.path().join("claude/projects/x/s/tool-results");
        let (mcp, settings) = backend()
            .write_run_files(&request(run.clone(), work), Some(&results))
            .expect("写配置");

        let mcp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mcp).expect("读")).expect("JSON");
        assert_eq!(mcp["mcpServers"]["qtmcp"]["args"], json!(["--read-only"]));
        assert_eq!(
            mcp["mcpServers"].as_object().map(serde_json::Map::len),
            Some(1)
        );

        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(settings).expect("读")).expect("JSON");
        for event in ["PermissionRequest", "PreToolUse"] {
            let hook = &settings["hooks"][event][0];
            assert_eq!(hook["matcher"], "", "空 matcher 才能覆盖所有工具");
            let command = hook["hooks"][0]["command"].as_str().expect("命令");
            // 路径里有空格也要引好，否则 hook 根本启动不了
            assert!(
                command.starts_with("'/opt/ai boot/bin/ai-boot' hook --backend claude --workdir '")
            );
            // 超大工具结果的目录也交给 hook，否则模型读它时会被拦下
            assert!(
                command.ends_with(&format!("--read-dir {}", results.display())),
                "{command}"
            );
        }

        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&run)
            .expect("run_dir")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn session_records_cover_the_project_and_each_session_env() {
        let config = tempfile::tempdir().expect("临时目录");
        let workdir = Path::new("/var/lib/ai-boot/sessions/c1");
        let project = config.path().join("projects").join(project_slug(workdir));
        std::fs::create_dir_all(project.join("s-1/tool-results")).expect("目录");
        std::fs::write(project.join("s-1.jsonl"), "{}").expect("会话记录");
        std::fs::write(project.join("s-2.jsonl"), "{}").expect("会话记录");
        let mut records = session_records(config.path(), workdir);
        records.sort();
        let mut expected = vec![
            project,
            config.path().join("session-env/s-1"),
            config.path().join("session-env/s-2"),
        ];
        expected.sort();
        assert_eq!(records, expected);
        // 还没有会话记录时只有项目目录本身
        assert_eq!(
            session_records(config.path(), Path::new("/x")),
            vec![config.path().join("projects/-x")]
        );
    }

    #[test]
    fn project_dirs_follow_the_cli_naming() {
        assert_eq!(
            project_slug(Path::new("/var/lib/ai-boot/sessions/0199a-b")),
            "-var-lib-ai-boot-sessions-0199a-b"
        );
        assert_eq!(
            project_slug(Path::new("/home/u/.claude")),
            "-home-u--claude"
        );
    }

    #[test]
    fn oversized_tool_results_live_under_the_config_dir_the_cli_actually_uses() {
        let with_env = |env: &[(&str, &str)]| {
            ClaudeBackend::new(ClaudeConfig {
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
                ..backend().config
            })
        };
        assert_eq!(
            with_env(&[("HOME", "/home/u")]).config_dir(),
            Some(PathBuf::from("/home/u/.claude"))
        );
        assert_eq!(
            with_env(&[
                ("HOME", "/home/u"),
                ("CLAUDE_CONFIG_DIR", "/var/lib/ai-boot/claude")
            ])
            .config_dir(),
            Some(PathBuf::from("/var/lib/ai-boot/claude"))
        );
        assert_eq!(with_env(&[]).config_dir(), None);
        // 和线上看到的路径一致：/var/lib/ai-boot/sessions/<会话> → -var-lib-ai-boot-sessions-<会话>
        assert_eq!(
            results_dir(
                Path::new("/home/u/.claude"),
                Path::new("/var/lib/ai-boot/sessions/01a0ecb0-f185"),
                "s-1"
            ),
            PathBuf::from(
                "/home/u/.claude/projects/-var-lib-ai-boot-sessions-01a0ecb0-f185/s-1/tool-results"
            )
        );
    }
}
