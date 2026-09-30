//! 聊天里的文件在本地转成文字：两个后端拿到同样的内容（Codex 不能自己读文件），
//! 原件留在工作目录里给 Claude 用 Grep 查。
//!
//! 文件内容都是不可信输入：优先用纯 Rust 解析；外部工具（poppler、soffice）一律
//! 清空环境、限时、限输出，超时即杀。解压类格式先按声明的解压后大小把关。

mod image;
mod office;
mod pdf;
mod sheet;
mod text;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

pub use image::{is_image, normalize};

/// 单个文件解析出的文字上限（字符）。更长的先压缩（日志保留头尾和出错的行），
/// 放进 prompt 时各层还有各自的预算。
pub const TEXT_CHARS: usize = 24 * 1024;
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
}

/// 解析结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// 给模型看的文字，已压缩到 `TEXT_CHARS` 以内。
    pub text: String,
    /// 解析时生成的图片（扫描版 PDF 的页面），放在 `out_dir` 里。
    pub images: Vec<PathBuf>,
    /// 处理说明：压缩了、只列了清单、转成了图片……
    pub note: Option<String>,
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
    /// 能列清单的 zip。
    Zip,
    /// 其他压缩包。
    Archive,
    Image,
    Binary,
}

/// 解析一个文件。`path` 是已经存进工作目录的原件，`out_dir` 放派生出的文件
/// （页面图片），也必须在工作目录里。
pub async fn extract(
    path: &Path,
    name: &str,
    bytes: Vec<u8>,
    out_dir: &Path,
    tools: &Tools,
) -> Result<Output, String> {
    match detect(name, &bytes) {
        Kind::Text => Ok(text::extract(&bytes)),
        Kind::Pdf => pdf::extract(path, out_dir, tools).await,
        Kind::Docx => blocking(move || office::docx(&bytes)).await,
        Kind::Pptx => blocking(move || office::pptx(&bytes)).await,
        Kind::Sheet => blocking(move || sheet::extract(bytes)).await,
        Kind::Legacy(target) if tools.office_legacy => office::legacy(path, target, tools).await,
        Kind::Legacy(_) => Err("旧版 Office 文档的转换没有开启".to_owned()),
        Kind::Zip => blocking(move || office::zip_listing(&bytes)).await,
        Kind::Archive => Err("压缩包没有解压，请把里面的文件单独发出来".to_owned()),
        Kind::Image => Err("这是图片，不按文件解析".to_owned()),
        Kind::Binary => Err("二进制文件，无法解析".to_owned()),
    }
}

/// 按内容（魔数）判断，扩展名只用来区分 OLE2 里的 doc/xls/ppt。
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
    const ARCHIVES: [&[u8]; 5] = [
        &[0x1F, 0x8B],                         // gzip
        b"7z\xBC\xAF\x27\x1C",                 // 7z
        b"Rar!\x1A\x07",                       // rar
        b"BZh",                                // bzip2
        &[0xFD, b'7', b'z', b'X', b'Z', 0x00], // xz
    ];
    if ARCHIVES.iter().any(|magic| bytes.starts_with(magic)) || ext == "tar" {
        return Kind::Archive;
    }
    if text::decode(bytes).is_some() {
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
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|err| format!("启动 {program} 失败：{err}"))?;
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
            let _ = child.start_kill();
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
        // 超时：future 被丢弃，kill_on_drop 负责杀进程
        Err(_) => Err(format!("{program} 超过 {} 秒没有完成", timeout.as_secs())),
    }
}

/// 截到 `limit` 个字符。
fn clip_chars(text: &str, limit: usize) -> (String, bool) {
    match text.char_indices().nth(limit) {
        Some((end, _)) => (text[..end].to_owned(), true),
        None => (text.to_owned(), false),
    }
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
        assert_eq!(detect("a.gz", &[0x1F, 0x8B, 8, 0]), Kind::Archive);
        assert_eq!(detect("a.dat", &[0, 1, 2, 3, 0, 0, 0xFF]), Kind::Binary);
    }

    #[test]
    fn clipping_respects_char_boundaries() {
        assert_eq!(clip_chars("中文日志", 2), ("中文".to_owned(), true));
        assert_eq!(clip_chars("ab", 5), ("ab".to_owned(), false));
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
