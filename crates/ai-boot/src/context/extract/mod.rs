//! 聊天里的文件在本地转成文字：两个后端拿到同样的内容（Codex 不能自己读文件），
//! 原件留在工作目录里给 Claude 用 Grep 查。
//!
//! 文件内容都是不可信输入：优先用纯 Rust 解析；外部工具（poppler、soffice）一律
//! 清空环境、限时、限输出，超时即杀。解压类格式先按声明的解压后大小把关；表格在
//! 限了内存的子进程里解析。

mod archive;
mod image;
mod office;
mod pdf;
mod sheet;
mod text;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

pub use image::{WORKERS as IMAGE_WORKERS, is_image, normalize, normalize_tiled};
pub use sheet::child as sheet_child;

/// 单个文件解析出的文字上限（字节）。更长的先压缩（日志保留头尾和出错的行）；prompt 里
/// 各层的预算也按字节算，按字符截的话中文要超出三倍，压缩好的结尾又被截掉。
pub const TEXT_BYTES: usize = 24 * 1024;
/// zip 类文档里单个 XML 解压后的上限。
const XML_LIMIT: u64 = 32 * 1024 * 1024;
/// 外部命令的 PATH：不继承服务自己的环境。
pub(crate) const TOOL_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// 解析用到的环境。
#[derive(Debug, Clone)]
pub struct Tools {
    /// 旧版 Office（doc、ppt、odt、odp）要不要用 soffice 转换。
    pub office_legacy: bool,
    /// 外部命令的临时目录（soffice 的配置目录也放这里）。
    pub scratch: PathBuf,
    /// ai-boot 自己的可执行文件：表格在它的 `sheet` 子命令里解析。
    pub program: PathBuf,
}

/// 解析结果。
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// 给模型看的文字，已压缩到 `TEXT_BYTES` 以内。
    pub text: String,
    /// 解析时生成的图片（扫描版 PDF 的页面、文档里嵌的图），放在 `out_dir` 里。
    pub images: Vec<PathBuf>,
    /// 处理说明：压缩了、只列了清单、转成了图片……
    pub note: Option<String>,
    /// 另存的全文（在 `out_dir` 里）：原件模型读不了（docx 这类 zip）时代替原件。
    pub saved: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Pdf,
    Docx,
    Pptx,
    Sheet,
    /// 旧版或 OpenDocument 文档，先转成括号里的格式。
    Legacy(&'static str),
    /// 能解开的压缩包。
    Archive(archive::Format),
    /// 解不开的压缩包，括号里是格式名。
    Unsupported(&'static str),
    Image,
    Binary,
}

/// 解析一个文件。`path` 是已经存进工作目录的原件，`out_dir` 放派生出的文件
/// （页面图片、全文、解开的压缩包），也必须在工作目录里。
pub async fn extract(
    path: &Path,
    name: &str,
    bytes: Vec<u8>,
    out_dir: &Path,
    tools: &Tools,
) -> Result<Output, String> {
    match detect(name, &bytes) {
        Kind::Text => blocking(move || text::extract(&bytes)).await,
        Kind::Pdf => pdf::extract(path, out_dir, tools).await,
        Kind::Docx => office::read(path, "docx", bytes, out_dir).await,
        Kind::Pptx => office::read(path, "pptx", bytes, out_dir).await,
        Kind::Sheet => sheet::isolated(path, tools).await,
        Kind::Legacy(target) if tools.office_legacy => {
            office::legacy(path, target, out_dir, tools).await
        }
        Kind::Legacy(_) => Err("旧版 Office 文档的转换没有开启".to_owned()),
        Kind::Archive(format) => archive::extract(format, path, name, bytes, out_dir).await,
        Kind::Unsupported(format) => Err(format!(
            "{format} 格式的压缩包解不开，请改成 zip 或 tar.gz 再发（其他附件照常读取）"
        )),
        Kind::Image => Err("这是图片，不按文件解析".to_owned()),
        Kind::Binary => Err("二进制文件，无法解析".to_owned()),
    }
}

/// 按内容（魔数）判断，扩展名只用来区分 OLE2 里的 doc/xls/ppt。是不是文本要完整
/// 解码才知道，留给解析时判断，这里只挡掉明显的二进制。
fn detect(name: &str, bytes: &[u8]) -> Kind {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if bytes.starts_with(b"%PDF") {
        return Kind::Pdf;
    }
    if image::is_image(bytes) {
        return Kind::Image;
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return office::zip_kind(bytes);
    }
    const OLE2: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(&OLE2) {
        return match ext.as_str() {
            "xls" => Kind::Sheet,
            "doc" => Kind::Legacy("docx"),
            "ppt" => Kind::Legacy("pptx"),
            _ => Kind::Binary,
        };
    }
    if bytes.starts_with(&[0x1F, 0x8B]) {
        return Kind::Archive(archive::Format::Gzip);
    }
    if archive::is_tar(bytes) || ext == "tar" {
        return Kind::Archive(archive::Format::Tar);
    }
    const UNSUPPORTED: [(&[u8], &str); 4] = [
        (b"7z\xBC\xAF\x27\x1C", "7z"),
        (b"Rar!\x1A\x07", "rar"),
        (b"BZh", "bzip2"),
        (&[0xFD, b'7', b'z', b'X', b'Z', 0x00], "xz"),
    ];
    if let Some(format) = UNSUPPORTED
        .iter()
        .find_map(|&(magic, format)| bytes.starts_with(magic).then_some(format))
    {
        return Kind::Unsupported(format);
    }
    if text::maybe_text(bytes) {
        return Kind::Text;
    }
    Kind::Binary
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|err| format!("解析任务异常：{err}"))?
}

/// 跑一个外部命令，收集最多 `cap` 字节的标准输出。超时、非零退出都算失败；
/// 输出超过上限时截断并杀掉进程。
async fn run(
    mut command: Command,
    scratch: &Path,
    timeout: Duration,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let program = command
        .as_std()
        .get_program()
        .to_string_lossy()
        .into_owned();
    command
        .env_clear()
        .env("PATH", TOOL_PATH)
        .env("HOME", scratch)
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // 自成进程组：soffice 是个 shell 包装，真正干活的 soffice.bin 是它的子进程，只杀
        // 直接子进程会留下孤儿，一直占着配置目录的锁，之后的转换全卡住
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("启动 {program} 失败：{err}"))?;
    // 不管怎么结束（正常退出、超时、调用方不等了）都对整组补一刀；在 child 之后声明，
    // 先于它析构
    let group = Group(
        child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw),
    );
    let Some(mut stdout) = child.stdout.take() else {
        return Err(format!("拿不到 {program} 的输出"));
    };
    let work = async {
        let mut output = Vec::new();
        (&mut stdout)
            .take(cap as u64 + 1)
            .read_to_end(&mut output)
            .await
            .map_err(|err| format!("读取 {program} 的输出失败：{err}"))?;
        if output.len() > cap {
            // 输出太长：够用了，不等它写完
            output.truncate(cap);
            group.kill();
            let _ = child.wait().await;
            return Ok(output);
        }
        let status = child
            .wait()
            .await
            .map_err(|err| format!("等待 {program} 失败：{err}"))?;
        if status.success() {
            Ok(output)
        } else {
            Err(format!("{program} 失败（{status}）"))
        }
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        // 超时：整组由 group 杀掉，直接子进程由 kill_on_drop 回收
        Err(_) => Err(format!("{program} 超过 {} 秒没有完成", timeout.as_secs())),
    }
}

/// 外部命令所在的进程组，析构时整组 SIGKILL。组里还有成员时组 ID 不会被系统复用，
/// 这一刀只会落在它自己拉起的进程上；没有成员时返回 ESRCH。
struct Group(Option<Pid>);

impl Group {
    fn kill(&self) {
        if let Some(pid) = self.0
            && let Err(err) = kill_process_group(pid, Signal::KILL)
            && err != rustix::io::Errno::SRCH
        {
            tracing::debug!(%err, "对进程组发送 SIGKILL 失败");
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// 截到 `limit` 个字节以内，落在字符边界上。
pub(crate) fn clip_bytes(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// 文件名来自聊天或压缩包，是不可信输入：只留字母数字（含中文）和 `.-_`，不能出现
/// 路径分隔。
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    let mut kept: String = cleaned
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if kept.is_empty() {
        kept = "file".to_owned();
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_goes_by_content_not_by_name() {
        assert_eq!(detect("a.txt", b"%PDF-1.4 ..."), Kind::Pdf);
        assert_eq!(detect("a.log", b"\x89PNG\r\n\x1a\n...."), Kind::Image);
        assert_eq!(
            detect("a.bin", "2026-09-28 ERROR 连接池耗尽\n".as_bytes()),
            Kind::Text
        );
        assert_eq!(
            detect(
                "a.doc",
                &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0]
            ),
            Kind::Legacy("docx")
        );
        assert_eq!(
            detect("a.txt", &[0x1F, 0x8B, 8, 0]),
            Kind::Archive(archive::Format::Gzip)
        );
        assert_eq!(
            detect("a.log", b"7z\xBC\xAF\x27\x1C\x00\x04"),
            Kind::Unsupported("7z")
        );
        assert_eq!(detect("a.dat", &[0, 1, 2, 3, 0, 0, 0xFF]), Kind::Binary);
    }

    #[tokio::test]
    async fn unsupported_archives_name_the_format() {
        let dir = tempfile::tempdir().expect("临时目录");
        let tools = Tools {
            office_legacy: false,
            scratch: dir.path().to_path_buf(),
            program: PathBuf::from("ai-boot"),
        };
        let path = dir.path().join("logs.rar");
        let err = extract(
            &path,
            "logs.rar",
            b"Rar!\x1A\x07\x01\x00".to_vec(),
            dir.path(),
            &tools,
        )
        .await
        .expect_err("不支持");
        assert!(
            err.contains("rar") && err.contains("其他附件照常读取"),
            "{err}"
        );
    }

    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(clip_bytes("中文日志", 7), ("中文".to_owned(), true));
        assert_eq!(clip_bytes("ab", 5), ("ab".to_owned(), false));
    }

    #[test]
    fn file_names_cannot_escape_the_directory() {
        assert_eq!(sanitize("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize("错误 日志(1).log"), "错误_日志_1_.log");
        assert_eq!(sanitize("..."), "file");
        assert_eq!(
            sanitize(&format!("{}.log", "长".repeat(200)))
                .chars()
                .count(),
            80
        );
        assert!(sanitize(&format!("{}.log", "长".repeat(200))).ends_with(".log"));
    }

    #[tokio::test]
    async fn a_hanging_tool_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = std::time::Instant::now();
        let err = run(command, dir.path(), Duration::from_millis(200), 1024)
            .await
            .expect_err("应当超时");
        assert!(err.contains("没有完成"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// 像 soffice 那样的包装脚本：拉起真正干活的子进程后等它，子进程的 pid 写进文件。
    fn wrapper(dir: &Path) -> (Command, PathBuf) {
        let pid_file = dir.join("worker.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 30 & echo $! > {}; wait", pid_file.display()));
        (command, pid_file)
    }

    /// 进程不在了（或者只剩还没被回收的僵尸）。
    async fn gone(pid_file: &Path) -> bool {
        let pid = std::fs::read_to_string(pid_file).expect("pid 文件");
        let stat = format!("/proc/{}/stat", pid.trim());
        for _ in 0..100 {
            match std::fs::read_to_string(&stat) {
                Err(_) => return true,
                Ok(stat)
                    if stat
                        .rsplit(')')
                        .next()
                        .is_some_and(|s| s.trim_start().starts_with('Z')) =>
                {
                    return true;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        false
    }

    #[tokio::test]
    async fn the_whole_process_group_dies_on_timeout_and_when_abandoned() {
        let dir = tempfile::tempdir().expect("临时目录");
        let (command, pid_file) = wrapper(dir.path());
        let err = run(command, dir.path(), Duration::from_millis(500), 1024)
            .await
            .expect_err("应当超时");
        assert!(err.contains("没有完成"), "{err}");
        assert!(gone(&pid_file).await, "超时后包装脚本拉起的子进程也要杀掉");

        // 调用方不等了（整轮被取消）：future 被丢弃时同样整组杀掉
        std::fs::remove_file(&pid_file).expect("删掉旧 pid");
        let (command, pid_file) = wrapper(dir.path());
        let abandoned = run(command, dir.path(), Duration::from_secs(30), 1024);
        let _ = tokio::time::timeout(Duration::from_millis(500), abandoned).await;
        assert!(gone(&pid_file).await, "future 被丢弃后子进程也要杀掉");
    }

    #[tokio::test]
    async fn tool_output_is_capped() {
        let dir = tempfile::tempdir().expect("临时目录");
        let mut command = Command::new("yes");
        command.arg("x");
        let output = run(command, dir.path(), Duration::from_secs(10), 4096)
            .await
            .expect("截断后返回");
        assert_eq!(output.len(), 4096);
    }

    #[tokio::test]
    async fn tools_do_not_see_the_service_environment() {
        let dir = tempfile::tempdir().expect("临时目录");
        let output = run(
            Command::new("env"),
            dir.path(),
            Duration::from_secs(10),
            4096,
        )
        .await
        .expect("运行");
        let env = String::from_utf8_lossy(&output);
        let names: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ["HOME", "LANG", "PATH"], "{env}");
    }
}
