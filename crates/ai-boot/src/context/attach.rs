//! 附件：下载聊天里的图片和文件，存进 Agent 的工作目录，文件解析成文字。
//!
//! 每轮有数量和大小上限，提问附带的优先；读不到的逐条记下原因，写进 prompt
//! 和进度卡片。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use ai_boot_feishu::api::{ApiClient, ApiError, ResourceKind};
use futures_util::StreamExt as _;
use tokio::sync::watch;

use super::Gathering;
use super::extract::{self, Tools, sanitize};
use super::flatten::AttachmentRef;

/// 每轮最多交给模型的图片（含扫描件转出的页面、长截图切出的段）。群聊记录里的截图
/// 常常是关键证据，500 条消息里的图一般不超过这个数。
pub const MAX_IMAGES: usize = 20;
/// 每轮最多解析的文件。
pub const MAX_FILES: usize = 8;
/// 单个文件的下载上限。
pub const MAX_DOWNLOAD: usize = 20 * 1024 * 1024;
/// 每轮下载总量上限。并发下载时是软上限：已经在下载的几个不会被打断。
const TOTAL_DOWNLOAD: usize = 60 * 1024 * 1024;
/// 同时下载、解析的附件数。
const CONCURRENCY: usize = 6;

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
    let mut results: Vec<(usize, Wanted, Result<Attachment, String>)> =
        futures_util::stream::iter(selected.into_iter().enumerate())
            .map(|(index, want)| {
                let downloaded = &downloaded;
                async move {
                    let result = one(api, index, &want, workspace, downloaded).await;
                    progress.send_modify(|g| match want.reference.kind {
                        ResourceKind::Image => g.images_done += 1,
                        ResourceKind::File => g.files_done += 1,
                    });
                    (index, want, result)
                }
            })
            // 不用 buffered：它按顺序交出结果，排在前面的一个慢（soffice、20 MB 的文件），
            // 后面做完的也占着名额，新的下载开不了
            .buffer_unordered(CONCURRENCY)
            .collect()
            .await;
    // 图片按顺序占名额（提问附带的在前），排回原来的顺序
    results.sort_by_key(|(index, _, _)| *index);

    let mut given = 0_usize;
    for (_, want, result) in results {
        let label = label(&want.reference);
        match result {
            Ok(mut attachment) => {
                // 扫描件的页面、长截图的段也算图片，总数不超过上限
                let room = MAX_IMAGES.saturating_sub(given);
                if attachment.images.len() > room {
                    let dropped = attachment.images.len() - room;
                    attachment.images.truncate(room);
                    attachment.note = Some(append(
                        attachment.note.take(),
                        &format!("图片数量已达上限，后面的 {dropped} 张没有交给模型"),
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
        ResourceKind::Image => save_image(index, bytes, want, workspace).await,
        ResourceKind::File => save_file(index, bytes, want, workspace).await,
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
        // 前面的附件已经用掉了大半的总量，这次的上限比单个文件的上限小
        (ApiError::TooLarge { limit }, _) if *limit < MAX_DOWNLOAD => format!(
            "本轮附件下载总量接近 {} MB 的上限，剩下的额度放不下这个{what}，没有下载",
            TOTAL_DOWNLOAD / 1024 / 1024
        ),
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
) -> Result<Attachment, String> {
    let tiles = {
        let _permit = extract::IMAGE_WORKERS
            .acquire()
            .await
            .map_err(|err| format!("处理图片失败：{err}"))?;
        tokio::task::spawn_blocking(move || extract::normalize_tiled(bytes))
            .await
            .map_err(|err| format!("处理图片失败：{err}"))??
    };
    let mut images = Vec::new();
    for (n, (data, ext)) in tiles.iter().enumerate() {
        // 整张的沿用原来的名字（答案里引用截图用的就是这个路径），切段的按段编号
        let name = match tiles.len() {
            1 => format!("{index:02}-image.{ext}"),
            _ => format!("{index:02}-image-{}.{ext}", n + 1),
        };
        let path = workspace.dir.join(name);
        write(&path, data).await?;
        images.push(relative(workspace.root, &path));
    }
    let mut title = label(&want.reference);
    if tiles.len() > 1 {
        // 图片清单里会带上标题：让模型知道这几张是同一张图，别当成几张截图
        title = format!(
            "{title}（长截图，切成 {} 段，相邻两段略有重叠）",
            tiles.len()
        );
    }
    Ok(Attachment {
        layer: want.layer,
        title,
        origin: want.origin.clone(),
        text: None,
        saved: images.first().cloned().unwrap_or_default(),
        images,
        note: None,
    })
}

async fn save_file(
    index: usize,
    bytes: Vec<u8>,
    want: &Wanted,
    workspace: &Workspace<'_>,
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
        let image = save_image(index, bytes, want, workspace).await?;
        attachment.images = image.images;
        attachment.title = image.title;
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
            // 原件模型读不了（docx 这类 zip）时，prompt 里的「原件」指向另存的全文
            if let Some(saved) = &output.saved {
                attachment.saved = relative(workspace.root, saved);
            }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use secrecy::SecretString;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

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
        let over = ApiError::TooLarge {
            limit: MAX_DOWNLOAD,
        };
        assert!(download_error(&over, false, ResourceKind::File).contains("超过 20 MB"));
        // 本轮总量只剩 3 MB 时，说的是总量，不是「超过 20 MB」
        let rest = ApiError::TooLarge {
            limit: 3 * 1024 * 1024,
        };
        let message = download_error(&rest, false, ResourceKind::File);
        assert!(
            message.contains("60 MB") && !message.contains("超过 20 MB"),
            "{message}"
        );
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(width, height, |x, y| {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 7])
            }));
        let mut out = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("编码");
        out
    }

    fn image_wanted(key: &str, layer: Layer) -> Wanted {
        Wanted {
            message_id: format!("om_{key}"),
            reference: AttachmentRef {
                kind: ResourceKind::Image,
                key: key.to_owned(),
                name: None,
            },
            layer,
            origin: key.to_owned(),
            forwarded: false,
        }
    }

    async fn serve(server: &MockServer, key: &str, body: Vec<u8>, delay: Duration) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/open-apis/im/v1/messages/om_{key}/resources/{key}"
            )))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(body)
                    .set_delay(delay),
            )
            .mount(server)
            .await;
    }

    /// 一个慢的附件不挡住后面的下载；结果仍按原来的顺序，长截图切出的段也占图片名额，
    /// 提问附带的排在前面先占。
    #[tokio::test]
    async fn a_slow_attachment_does_not_block_the_rest_and_tiles_count_as_images() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/open-apis/auth/v3/tenant_access_token/internal"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"code": 0, "tenant_access_token": "t-x", "expire": 7200}),
                ),
            )
            .mount(&server)
            .await;
        let base = url::Url::parse(&format!("{}/", server.uri())).expect("地址");
        let api = ApiClient::new(base, "cli_x", SecretString::from("s")).expect("客户端");
        let slow = Duration::from_millis(2500);
        // 群里分享的 18 张小图，第一张和第 7 张都慢；提问附带一张长截图（切成 5 段）
        let mut wanted = Vec::new();
        for i in 0..18 {
            let key = format!("s{i:02}");
            let delay = if matches!(i, 0 | 6) {
                slow
            } else {
                Duration::ZERO
            };
            serve(&server, &key, png(40, 30), delay).await;
            wanted.push(image_wanted(&key, Layer::Shared));
        }
        // 窄一点的长图，测试里编码快：整张缩放后只有 41 像素宽
        serve(&server, "tall", png(200, 9600), Duration::ZERO).await;
        wanted.push(image_wanted("tall", Layer::Question));

        let dir = tempfile::tempdir().expect("临时目录");
        let attachments = dir.path().join("attachments/1");
        let tools = Tools {
            office_legacy: false,
            scratch: dir.path().join("tmp"),
            program: PathBuf::from("ai-boot"),
        };
        let workspace = Workspace {
            root: dir.path(),
            dir: &attachments,
            tools: &tools,
        };
        let (progress, _) = watch::channel(Gathering::default());
        let started = Instant::now();
        let fetched = fetch(&api, wanted, &workspace, &progress).await;
        // 按顺序交出结果的话，第 7 张要等第一张的名额空出来，前后至少 5 秒；
        // 不按顺序时两张慢的同时下，余下的是处理图片的时间
        assert!(
            started.elapsed() < Duration::from_millis(4800),
            "{:?}",
            started.elapsed()
        );

        let origins: Vec<&str> = fetched
            .attachments
            .iter()
            .map(|a| a.origin.as_str())
            .collect();
        let mut expected = vec!["tall".to_owned()];
        expected.extend((0..18).map(|i| format!("s{i:02}")));
        assert_eq!(origins, expected);
        let tall = &fetched.attachments[0];
        assert_eq!(tall.images.len(), 5);
        assert!(tall.title.contains("切成 5 段"), "{}", tall.title);
        assert!(
            tall.images[0].ends_with("00-image-1.png")
                || tall.images[0].ends_with("00-image-1.webp")
        );
        let given: usize = fetched.attachments.iter().map(|a| a.images.len()).sum();
        assert_eq!(given, MAX_IMAGES);
        // 5 段加 15 张占满 20 张，最后三张没交给模型
        assert!(
            fetched.attachments[16..]
                .iter()
                .all(|a| a.images.is_empty())
        );
        assert_eq!(fetched.missing.len(), 3, "{:?}", fetched.missing);
        assert!(
            fetched.missing[0].contains("图片数量已达上限"),
            "{:?}",
            fetched.missing
        );
    }
}
