//! 附件：下载聊天里的图片和文件，存进 Agent 的工作目录，文件解析成文字。
//!
//! 每轮有数量和大小上限，提问附带的优先；读不到的逐条记下原因，写进 prompt
//! 和进度卡片。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_boot_feishu::api::{ApiClient, ApiError, ResourceKind};
use futures_util::StreamExt as _;
use tokio::sync::{Semaphore, watch};

use super::Gathering;
use super::extract::{self, Tools};
use super::flatten::AttachmentRef;

/// 每轮最多交给模型的图片（含扫描件转出的页面）。群聊记录里的截图常常是关键证据，
/// 500 条消息里的图一般不超过这个数。
pub const MAX_IMAGES: usize = 20;
/// 每轮最多解析的文件。
pub const MAX_FILES: usize = 8;
/// 单个文件的下载上限。
pub const MAX_DOWNLOAD: usize = 20 * 1024 * 1024;
/// 每轮下载总量上限。并发下载时是软上限：已经在下载的几个不会被打断。
const TOTAL_DOWNLOAD: usize = 60 * 1024 * 1024;
/// 同时下载、解析的附件数。
const CONCURRENCY: usize = 6;
/// 同时解码、压缩的图片数：一张手机照片解码后要几十 MB 内存，不能跟着下载的并发走。
const IMAGE_WORKERS: usize = 2;

/// 附件属于哪一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// T1：提问本身和它引用的消息带的。
    Question,
    /// T2：话题和群里分享的。
    Shared,
}

/// 要下载的一个附件。
#[derive(Debug, Clone)]
pub struct Wanted {
    /// 下载用的消息 ID。合并转发里的附件用外层那条转发消息的 ID。
    pub message_id: String,
    pub reference: AttachmentRef,
    pub layer: Layer,
    /// 谁、在哪发的，写进 prompt 帮模型对上号。
    pub origin: String,
    /// 在合并转发里：官方接口不支持，只能尽力而为。
    pub forwarded: bool,
}

/// 读到的一个附件。路径都相对 Agent 的工作目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub layer: Layer,
    pub title: String,
    pub origin: String,
    /// 文件解析出的文字；图片没有。
    pub text: Option<String>,
    pub images: Vec<PathBuf>,
    /// 原件。
    pub saved: PathBuf,
    pub note: Option<String>,
}

/// 附件存放的位置。
pub struct Workspace<'a> {
    /// Agent 的工作目录。
    pub root: &'a Path,
    /// 本轮附件的目录，在 `root` 里面。
    pub dir: &'a Path,
    pub tools: &'a Tools,
}

#[derive(Debug, Default)]
pub struct Fetched {
    pub attachments: Vec<Attachment>,
    /// 读不到的，每条一句话说明原因。
    pub missing: Vec<String>,
}

/// 下载并解析，几个同时进行；结果按调用方给的顺序排，提问附带的整体在前。
/// 每处理完一个就在 `progress` 里累加。
pub async fn fetch(
    api: &ApiClient,
    wanted: Vec<Wanted>,
    workspace: &Workspace<'_>,
    progress: &watch::Sender<Gathering>,
) -> Fetched {
    let mut fetched = Fetched::default();
    if wanted.is_empty() {
        return fetched;
    }
    if let Err(err) = tokio::fs::create_dir_all(workspace.dir).await {
        fetched.missing.push(format!("附件目录创建失败：{err}"));
        return fetched;
    }
    let mut ordered = wanted;
    // 同一层内保持调用方给的顺序，提问附带的整体排前
    ordered.sort_by_key(|w| w.layer != Layer::Question);
    let mut seen = HashSet::new();
    let (mut images, mut files) = (0_usize, 0_usize);
    let (mut skipped_images, mut skipped_files) = (0_usize, 0_usize);
    let mut selected = Vec::new();
    for want in ordered {
        if !seen.insert(want.reference.key.clone()) {
            continue;
        }
        match want.reference.kind {
            ResourceKind::Image if images >= MAX_IMAGES => skipped_images += 1,
            ResourceKind::File if files >= MAX_FILES => skipped_files += 1,
            ResourceKind::Image => {
                images += 1;
                selected.push(want);
            }
            ResourceKind::File => {
                files += 1;
                selected.push(want);
            }
        }
    }
    // 群里几百条消息里的图可能很多，超出的合成一句，不逐条列
    if skipped_images > 0 {
        fetched.missing.push(format!(
            "另有 {skipped_images} 张更早的图片超过每轮 {MAX_IMAGES} 张的上限，没有读取"
        ));
    }
    if skipped_files > 0 {
        fetched.missing.push(format!(
            "另有 {skipped_files} 个更早的文件超过每轮 {MAX_FILES} 个的上限，没有读取"
        ));
    }
    progress.send_modify(|g| {
        g.images_total += images;
        g.files_total += files;
    });

    let downloaded = AtomicUsize::new(0);
    let image_workers = Semaphore::new(IMAGE_WORKERS);
    let results: Vec<(Wanted, Result<Attachment, String>)> =
        futures_util::stream::iter(selected.into_iter().enumerate())
            .map(|(index, want)| {
                let (downloaded, image_workers) = (&downloaded, &image_workers);
                async move {
                    let result = one(api, index, &want, workspace, downloaded, image_workers).await;
                    progress.send_modify(|g| match want.reference.kind {
                        ResourceKind::Image => g.images_done += 1,
                        ResourceKind::File => g.files_done += 1,
                    });
                    (want, result)
                }
            })
            .buffered(CONCURRENCY)
            .collect()
            .await;

    let mut given = 0_usize;
    for (want, result) in results {
        let label = label(&want.reference);
        match result {
            Ok(mut attachment) => {
                // 扫描件的页面也算图片，总数不超过上限
                let room = MAX_IMAGES.saturating_sub(given);
                if attachment.images.len() > room {
                    attachment.images.truncate(room);
                    attachment.note = Some(append(
                        attachment.note.take(),
                        "图片数量已达上限，后面的页面没有交给模型",
                    ));
                }
                given += attachment.images.len();
                if attachment.text.is_none() && attachment.images.is_empty() {
                    fetched.missing.push(format!(
                        "{label}：{}",
                        attachment.note.as_deref().unwrap_or("无法解析")
                    ));
                }
                fetched.attachments.push(attachment);
            }
            Err(reason) => fetched.missing.push(format!("{label}：{reason}")),
        }
    }
    fetched
}

/// 下载一个附件并存进工作目录。
async fn one(
    api: &ApiClient,
    index: usize,
    want: &Wanted,
    workspace: &Workspace<'_>,
    downloaded: &AtomicUsize,
    image_workers: &Semaphore,
) -> Result<Attachment, String> {
    let budget =
        MAX_DOWNLOAD.min(TOTAL_DOWNLOAD.saturating_sub(downloaded.load(Ordering::Relaxed)));
    if budget == 0 {
        return Err("本轮下载总量已达上限，没有读取".to_owned());
    }
    let bytes = api
        .download_resource(
            &want.message_id,
            &want.reference.key,
            want.reference.kind,
            budget,
        )
        .await
        .map_err(|err| {
            tracing::info!(%err, key = %want.reference.key, "附件下载失败");
            download_error(&err, want.forwarded, want.reference.kind)
        })?
        .bytes;
    downloaded.fetch_add(bytes.len(), Ordering::Relaxed);
    match want.reference.kind {
        ResourceKind::Image => save_image(index, bytes, want, workspace, image_workers).await,
        ResourceKind::File => save_file(index, bytes, want, workspace, image_workers).await,
    }
}

fn label(reference: &AttachmentRef) -> String {
    match (&reference.name, reference.kind) {
        (Some(name), _) => name.clone(),
        (None, ResourceKind::Image) => "图片".to_owned(),
        (None, ResourceKind::File) => "文件".to_owned(),
    }
}

fn download_error(err: &ApiError, forwarded: bool, kind: ResourceKind) -> String {
    let what = match kind {
        ResourceKind::Image => "图片",
        ResourceKind::File => "文件",
    };
    match (err, err.code()) {
        (ApiError::TooLarge { .. }, _) | (_, Some(234_037)) => {
            format!("超过 {} MB，没有下载", MAX_DOWNLOAD / 1024 / 1024)
        }
        (_, Some(234_043 | 234_003 | 234_009)) if forwarded => {
            format!("合并转发里的{what}取不到，请单独转发原{what}")
        }
        (_, Some(234_038)) => "保密消息里的资源不能下载".to_owned(),
        (_, Some(230_110)) => "消息已撤回".to_owned(),
        (_, Some(234_043)) => format!("这类消息里的{what}不支持下载"),
        _ => format!("下载失败（{err}）"),
    }
}

async fn save_image(
    index: usize,
    bytes: Vec<u8>,
    want: &Wanted,
    workspace: &Workspace<'_>,
    image_workers: &Semaphore,
) -> Result<Attachment, String> {
    let _permit = image_workers
        .acquire()
        .await
        .map_err(|err| format!("处理图片失败：{err}"))?;
    let (normalized, ext) = tokio::task::spawn_blocking(move || extract::normalize(bytes))
        .await
        .map_err(|err| format!("处理图片失败：{err}"))??;
    let path = workspace.dir.join(format!("{index:02}-image.{ext}"));
    write(&path, &normalized).await?;
    let relative = relative(workspace.root, &path);
    Ok(Attachment {
        layer: want.layer,
        title: label(&want.reference),
        origin: want.origin.clone(),
        text: None,
        images: vec![relative.clone()],
        saved: relative,
        note: None,
    })
}

async fn save_file(
    index: usize,
    bytes: Vec<u8>,
    want: &Wanted,
    workspace: &Workspace<'_>,
    image_workers: &Semaphore,
) -> Result<Attachment, String> {
    let name = want
        .reference
        .name
        .clone()
        .unwrap_or_else(|| "file".to_owned());
    let path = workspace
        .dir
        .join(format!("{index:02}-{}", sanitize(&name)));
    write(&path, &bytes).await?;
    let saved = relative(workspace.root, &path);
    let mut attachment = Attachment {
        layer: want.layer,
        title: name.clone(),
        origin: want.origin.clone(),
        text: None,
        images: Vec::new(),
        saved,
        note: None,
    };
    // 以文件形式发的截图，按图片处理
    if extract::is_image(&bytes) {
        let image = save_image(index, bytes, want, workspace, image_workers).await?;
        attachment.images = image.images;
        return Ok(attachment);
    }
    match extract::extract(&path, &name, bytes, workspace.dir, workspace.tools).await {
        Ok(output) => {
            attachment.text = (!output.text.trim().is_empty()).then_some(output.text);
            attachment.images = output
                .images
                .iter()
                .map(|p| relative(workspace.root, p))
                .collect();
            attachment.note = output.note;
        }
        Err(reason) => attachment.note = Some(reason),
    }
    Ok(attachment)
}

async fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    tokio::fs::write(path, bytes)
        .await
        .map_err(|err| format!("保存失败：{err}"))
}

fn relative(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn append(note: Option<String>, more: &str) -> String {
    match note {
        Some(note) => format!("{note}；{more}"),
        None => more.to_owned(),
    }
}

/// 文件名来自聊天，是不可信输入：只留字母数字（含中文）和 `.-_`，不能出现路径分隔。
fn sanitize(name: &str) -> String {
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

    #[test]
    fn download_errors_explain_what_to_do() {
        let unsupported = ApiError::Api {
            code: 234_043,
            msg: String::new(),
        };
        assert!(
            download_error(&unsupported, true, ResourceKind::Image).contains("请单独转发原图片")
        );
        assert!(download_error(&unsupported, false, ResourceKind::File).contains("不支持"));
        assert!(
            download_error(&ApiError::TooLarge { limit: 1 }, false, ResourceKind::File)
                .contains("20 MB")
        );
    }
}
