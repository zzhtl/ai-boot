//! 流程图：模型给 Graphviz DOT，用本机的 `dot` 渲染成 PNG，作为图片放进卡片。
//!
//! DOT 来自模型，是不可信输入（可能被聊天记录里的提示注入左右）：会读本地文件的
//! 属性（image、imagepath、shapefile、fontpath）和 HTML 标签（里面能写
//! `<IMG SRC=...>`）一律拒绝，渲染出来的图会发到群里。`dot` 在清空的环境里跑，
//! 开 Graphviz 的服务器模式兜底，限时、限输出。没装 graphviz 就不渲染，卡片照常只有文字。

use std::path::Path;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;

use crate::context::extract::TOOL_PATH;

const MAX_DOT_BYTES: usize = 16 * 1024;
/// 卡片图片的上限是 10 MB，留出余量。
const MAX_PNG_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 2048;
const TIMEOUT: Duration = Duration::from_secs(10);

static FORBIDDEN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(image|imagepath|shapefile|fontpath)\s*=|=\s*<|<\s*img\b").ok()
});
static GRAPH: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(strict\s+)?(di)?graph\b").ok());

/// 渲染失败的原因。没装 graphviz 单独区分：部署时的可选依赖，不算错误。
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    NotInstalled,
    Rejected(String),
    Failed(String),
}

/// 把 DOT 渲染成 PNG。`scratch` 是可以随便写的临时目录。
pub async fn render(dot: &str, scratch: &Path) -> Result<Vec<u8>, Failure> {
    check(dot).map_err(Failure::Rejected)?;
    // 服务器模式下只许从 GV_FILE_PATH 读图片：给一个空目录
    let empty = scratch.join("graphviz");
    tokio::fs::create_dir_all(&empty)
        .await
        .map_err(|err| Failure::Failed(format!("准备临时目录失败：{err}")))?;
    let mut child = Command::new("dot")
        .args([
            "-Tpng",
            // 卡片按宽度缩放，144 dpi 在高分屏上字也不糊
            "-Gdpi=144",
            "-Gbgcolor=white",
            "-Gpad=0.2",
            // 交给 fontconfig 挑字体，中文会落到系统里的 CJK 字体上
            "-Gfontname=sans-serif",
            "-Nfontname=sans-serif",
            "-Efontname=sans-serif",
        ])
        .env_clear()
        .env("PATH", TOOL_PATH)
        .env("HOME", scratch)
        .env("LANG", "C.UTF-8")
        .env("SERVER_NAME", "ai-boot")
        .env("GV_FILE_PATH", &empty)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Failure::NotInstalled,
            _ => Failure::Failed(format!("启动 dot 失败：{err}")),
        })?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(Failure::Failed("拿不到 dot 的输入输出".to_owned()));
    };
    let input = dot.as_bytes().to_vec();
    let work = async {
        let write = async move {
            // 写完就关掉 stdin，dot 才知道输入结束；它提前退出时写入会失败，以退出码为准
            let _ = stdin.write_all(&input).await;
        };
        let mut png = Vec::new();
        let mut errors = Vec::new();
        let mut stdout = stdout.take(MAX_PNG_BYTES as u64 + 1);
        let mut stderr = stderr.take(MAX_STDERR_BYTES as u64);
        let ((), read, _) = tokio::join!(
            write,
            stdout.read_to_end(&mut png),
            stderr.read_to_end(&mut errors)
        );
        read.map_err(|err| Failure::Failed(format!("读取 dot 的输出失败：{err}")))?;
        if png.len() > MAX_PNG_BYTES {
            return Err(Failure::Failed("渲染出的图片太大".to_owned()));
        }
        let status = child
            .wait()
            .await
            .map_err(|err| Failure::Failed(format!("等待 dot 失败：{err}")))?;
        if status.success() && png.starts_with(b"\x89PNG") {
            Ok(png)
        } else {
            let message = String::from_utf8_lossy(&errors);
            Err(Failure::Failed(format!("dot 渲染失败：{}", message.trim())))
        }
    };
    match tokio::time::timeout(TIMEOUT, work).await {
        Ok(result) => result,
        // 超时：future 被丢弃，kill_on_drop 负责杀进程
        Err(_) => Err(Failure::Failed(format!(
            "dot 超过 {} 秒没有完成",
            TIMEOUT.as_secs()
        ))),
    }
}

/// 只收一张 graph / digraph，拒绝会读本地文件的写法。
fn check(dot: &str) -> Result<(), String> {
    if dot.len() > MAX_DOT_BYTES {
        return Err(format!("流程图超过 {} KB", MAX_DOT_BYTES / 1024));
    }
    if !GRAPH.as_ref().is_some_and(|re| re.is_match(dot)) {
        return Err("不是 Graphviz 的 graph / digraph".to_owned());
    }
    if FORBIDDEN.as_ref().is_none_or(|re| re.is_match(dot)) {
        return Err("用了会读本地文件的属性或 HTML 标签".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_graphs_are_accepted() {
        assert!(check("digraph { 网关 -> 订单服务 -> 数据库 }").is_ok());
        assert!(check("strict graph g { a -- b [label=\"URL 解析\"] }").is_ok());
        for bad in [
            "digraph { a [image=\"/etc/ssl/private/key.png\"] }",
            "digraph { a [shape=custom, shapefile=\"/etc/passwd\"] }",
            "digraph { imagepath=\"/\"; a }",
            "digraph { a [label=<<TABLE><TR><TD><IMG SRC=\"/x.png\"/></TD></TR></TABLE>>] }",
            "digraph { a [fontpath=\"/root\"] }",
            "rm -rf /",
        ] {
            assert!(check(bad).is_err(), "{bad}");
        }
        assert!(check(&format!("digraph {{ {} }}", "a -> b; ".repeat(3000))).is_err());
    }

    /// 装了 graphviz 的机器上真渲染一次；没装就确认报的是「没装」。
    #[tokio::test]
    async fn a_chain_renders_to_png_or_reports_graphviz_missing() {
        let dir = tempfile::tempdir().expect("临时目录");
        match render("digraph { 网关 -> 订单服务 -> 数据库 }", dir.path()).await {
            Ok(png) => assert!(png.starts_with(b"\x89PNG")),
            Err(failure) => assert_eq!(failure, Failure::NotInstalled),
        }
    }
}
