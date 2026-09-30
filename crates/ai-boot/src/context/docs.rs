//! 读云文档（用户身份）：新版文档导出 Markdown 并取几张图；知识库节点按实际类型
//! 分发；电子表格按工作表读值；多维表格读前几十条记录；云空间文件下载后本地
//! 解析；思维导图、幻灯片、旧版文档只取标题。

use std::path::{Path, PathBuf};

use ai_boot_feishu::api::{ApiClient, ApiError};
use secrecy::SecretString;
use serde_json::Value;

use super::attach::{Attachment, Layer, MAX_DOWNLOAD, Workspace};
use super::extract::{self, TEXT_CHARS};
use super::links::{DocKind, DocLink};

/// 每轮最多读几篇文档。
pub const MAX_DOCS: usize = 8;
/// 每篇新版文档最多取几张图。
const DOC_IMAGES: usize = 3;
/// 电子表格最多读几个工作表、每个读多大范围（30 列 × 200 行）。
const SHEET_TABS: usize = 3;
const SHEET_RANGE: &str = "A1:AD200";
const BITABLE_RECORDS: usize = 50;

/// 要读的一篇文档。
#[derive(Debug, Clone)]
pub struct WantedDoc {
    pub link: DocLink,
    pub layer: Layer,
    /// 谁、在哪贴的。
    pub origin: String,
}

#[derive(Debug, Default)]
pub struct Docs {
    pub attachments: Vec<Attachment>,
    pub missing: Vec<String>,
    /// 接口说 token 无效：调用方要刷新或请用户重新授权。
    pub rejected: bool,
}

enum Failure {
    Rejected,
    Message(String),
}

impl From<ApiError> for Failure {
    fn from(err: ApiError) -> Self {
        match (err.code(), &err) {
            (Some(99_991_668 | 99_991_677), _) | (_, ApiError::Http { status: 401 }) => {
                Self::Rejected
            }
            (Some(2_889_902), _) | (_, ApiError::Http { status: 403 }) => {
                Self::Message("没有权限读取（授权人也打不开这篇文档）".to_owned())
            }
            (Some(2_889_906), _) => Self::Message("文档已删除".to_owned()),
            (Some(2_889_914), _) | (_, ApiError::Http { status: 404 }) => {
                Self::Message("文档不存在".to_owned())
            }
            (_, ApiError::TooLarge { .. }) => Self::Message(format!(
                "文件超过 {} MB，没有下载",
                MAX_DOWNLOAD / 1024 / 1024
            )),
            _ => Self::Message(format!("读取失败（{err}）")),
        }
    }
}

struct Read {
    title: String,
    text: Option<String>,
    images: Vec<PathBuf>,
    saved: PathBuf,
    note: Option<String>,
}

/// 按顺序读，最多 `MAX_DOCS` 篇；`images_left` 是本轮还能交给模型的图片数。
pub async fn read(
    api: &ApiClient,
    token: &SecretString,
    wanted: &[WantedDoc],
    workspace: &Workspace<'_>,
    mut images_left: usize,
) -> Docs {
    let mut docs = Docs::default();
    if wanted.is_empty() {
        return docs;
    }
    if let Err(err) = tokio::fs::create_dir_all(workspace.dir).await {
        docs.missing.push(format!("文档目录创建失败：{err}"));
        return docs;
    }
    let titles = titles(api, token, wanted).await;
    for (index, want) in wanted.iter().enumerate() {
        let label = titles
            .iter()
            .find(|(t, _)| *t == want.link.token)
            .map(|(_, title)| title.clone())
            .unwrap_or_else(|| want.link.url.clone());
        if index >= MAX_DOCS {
            docs.missing
                .push(format!("{label}：超过每轮 {MAX_DOCS} 篇的上限，没有读取"));
            continue;
        }
        let context = Reading {
            api,
            token,
            workspace,
            index,
        };
        match context.one(&want.link, &label, &mut images_left).await {
            Ok(read) => {
                if read.text.is_none() && read.images.is_empty() {
                    docs.missing.push(format!(
                        "{}：{}",
                        read.title,
                        read.note.as_deref().unwrap_or("没有内容")
                    ));
                }
                docs.attachments.push(Attachment {
                    layer: want.layer,
                    title: read.title,
                    origin: want.origin.clone(),
                    text: read.text,
                    images: read.images,
                    saved: read.saved,
                    note: read.note,
                });
            }
            Err(Failure::Rejected) => {
                docs.rejected = true;
                return docs;
            }
            Err(Failure::Message(reason)) => docs.missing.push(format!("{label}：{reason}")),
        }
    }
    docs
}

/// 一次取回所有文档的标题；失败不影响读正文。
async fn titles(
    api: &ApiClient,
    token: &SecretString,
    wanted: &[WantedDoc],
) -> Vec<(String, String)> {
    let request: Vec<(String, String)> = wanted
        .iter()
        .take(MAX_DOCS)
        .filter_map(|w| {
            w.link
                .kind
                .doc_type()
                .map(|t| (w.link.token.clone(), t.to_owned()))
        })
        .collect();
    if request.is_empty() {
        return Vec::new();
    }
    match api.doc_metas(token, &request).await {
        Ok(metas) => metas
            .into_iter()
            .filter(|m| !m.title.is_empty())
            .map(|m| (m.doc_token, m.title))
            .collect(),
        Err(err) => {
            tracing::debug!(%err, "取文档标题失败");
            Vec::new()
        }
    }
}

struct Reading<'a> {
    api: &'a ApiClient,
    token: &'a SecretString,
    workspace: &'a Workspace<'a>,
    index: usize,
}

impl Reading<'_> {
    async fn one(
        &self,
        link: &DocLink,
        title: &str,
        images_left: &mut usize,
    ) -> Result<Read, Failure> {
        match &link.kind {
            DocKind::Docx => self.docx(&link.token, title, images_left).await,
            DocKind::Wiki => {
                let node = self.api.wiki_node(self.token, &link.token).await?;
                let kind = match node.obj_type.as_str() {
                    "docx" => DocKind::Docx,
                    "doc" => DocKind::Doc,
                    "sheet" => DocKind::Sheet { sheet: None },
                    "bitable" => DocKind::Bitable { table: None },
                    "file" => DocKind::File,
                    "mindnote" => DocKind::Mindnote,
                    "slides" => DocKind::Slides,
                    other => {
                        return Err(Failure::Message(format!("不支持的知识库节点类型 {other}")));
                    }
                };
                let title = if node.title.is_empty() {
                    title
                } else {
                    &node.title
                };
                let inner = DocLink {
                    kind,
                    token: node.obj_token.clone(),
                    url: link.url.clone(),
                };
                // 知识库节点指向的是实际文档，不会再是知识库
                Box::pin(self.one(&inner, title, images_left)).await
            }
            DocKind::Sheet { sheet } => self.sheet(&link.token, sheet.as_deref(), title).await,
            DocKind::Bitable { table } => self.bitable(&link.token, table.as_deref(), title).await,
            DocKind::File => self.file(&link.token, title, images_left).await,
            DocKind::Doc | DocKind::Mindnote | DocKind::Slides => Ok(Read {
                title: title.to_owned(),
                text: None,
                images: Vec::new(),
                saved: PathBuf::new(),
                note: Some("这类文档暂不支持读取正文，只取了标题".to_owned()),
            }),
            DocKind::Minutes => Err(Failure::Message("妙记暂不支持读取".to_owned())),
        }
    }

    fn path(&self, suffix: &str) -> PathBuf {
        self.workspace
            .dir
            .join(format!("doc{:02}-{suffix}", self.index))
    }

    fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(self.workspace.root)
            .unwrap_or(path)
            .to_path_buf()
    }

    async fn save(&self, path: &Path, bytes: &[u8]) -> Result<(), Failure> {
        tokio::fs::write(path, bytes)
            .await
            .map_err(|err| Failure::Message(format!("保存失败：{err}")))
    }

    async fn docx(
        &self,
        document: &str,
        title: &str,
        images_left: &mut usize,
    ) -> Result<Read, Failure> {
        let markdown = self.api.docx_markdown(self.token, document).await?;
        let path = self.path("doc.md");
        self.save(&path, markdown.as_bytes()).await?;
        let (text, cut) = clip(&markdown);
        let mut notes = Vec::new();
        if cut {
            notes.push("内容较长，只放了前面一部分，完整内容见原件".to_owned());
        }
        let mut images = Vec::new();
        let wanted_images = DOC_IMAGES.min(*images_left);
        if wanted_images > 0 {
            match self
                .api
                .docx_image_tokens(self.token, document, wanted_images)
                .await
            {
                Ok(tokens) => {
                    for (i, media) in tokens.iter().enumerate() {
                        match self.media(media, i).await {
                            Ok(path) => images.push(path),
                            Err(reason) => notes.push(format!("第 {} 张图：{reason}", i + 1)),
                        }
                    }
                }
                Err(err) => notes.push(format!("文档里的图片没有取到（{err}）")),
            }
        }
        *images_left = images_left.saturating_sub(images.len());
        Ok(Read {
            title: title.to_owned(),
            text: (!text.trim().is_empty()).then_some(text),
            images,
            saved: self.relative(&path),
            note: (!notes.is_empty()).then(|| notes.join("；")),
        })
    }

    async fn media(&self, media: &str, i: usize) -> Result<PathBuf, String> {
        let download = self
            .api
            .download_media(self.token, media, MAX_DOWNLOAD)
            .await
            .map_err(|err| format!("下载失败（{err}）"))?;
        let raw = download.bytes;
        let (bytes, ext) = tokio::task::spawn_blocking(move || extract::normalize(raw))
            .await
            .map_err(|err| format!("处理图片失败：{err}"))??;
        let path = self.path(&format!("image{i}.{ext}"));
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|err| format!("保存失败：{err}"))?;
        Ok(self.relative(&path))
    }

    async fn sheet(
        &self,
        spreadsheet: &str,
        only: Option<&str>,
        title: &str,
    ) -> Result<Read, Failure> {
        let sheets = self.api.sheets(self.token, spreadsheet).await?;
        let visible: Vec<_> = sheets
            .iter()
            .filter(|s| !s.hidden && (s.resource_type.is_empty() || s.resource_type == "sheet"))
            .filter(|s| only.is_none_or(|id| s.sheet_id == id))
            .collect();
        let chosen: Vec<_> = visible.iter().take(SHEET_TABS).collect();
        if chosen.is_empty() {
            return Err(Failure::Message("没有可读的工作表".to_owned()));
        }
        let ranges: Vec<String> = chosen
            .iter()
            .map(|s| format!("{}!{SHEET_RANGE}", s.sheet_id))
            .collect();
        let values = self
            .api
            .sheet_values(self.token, spreadsheet, &ranges)
            .await?;
        let mut out = String::new();
        for (sheet, rows) in chosen.iter().zip(values.iter()) {
            let size = sheet
                .grid_properties
                .as_ref()
                .map(|g| format!("（共 {} 行 × {} 列）", g.row_count, g.column_count))
                .unwrap_or_default();
            out.push_str(&format!("## 工作表：{}{size}\n", sheet.title));
            for row in rows {
                let mut cells: Vec<String> = row.iter().map(cell_text).collect();
                while cells.last().is_some_and(String::is_empty) {
                    cells.pop();
                }
                if !cells.is_empty() {
                    out.push_str(&cells.join("\t"));
                    out.push('\n');
                }
            }
        }
        let path = self.path("sheet.tsv");
        self.save(&path, out.as_bytes()).await?;
        let (text, cut) = clip(&out);
        let mut notes = vec![format!("每个工作表最多读 {SHEET_RANGE}")];
        if visible.len() > SHEET_TABS {
            notes.push(format!(
                "共 {} 个工作表，只读了前 {SHEET_TABS} 个",
                visible.len()
            ));
        }
        if cut {
            notes.push("内容较长，只放了前面一部分".to_owned());
        }
        Ok(Read {
            title: title.to_owned(),
            text: Some(text),
            images: Vec::new(),
            saved: self.relative(&path),
            note: Some(notes.join("；")),
        })
    }

    async fn bitable(&self, app: &str, only: Option<&str>, title: &str) -> Result<Read, Failure> {
        let tables = self.api.bitable_tables(self.token, app).await?;
        let table = match only {
            Some(id) => tables.iter().find(|t| t.table_id == id),
            None => tables.first(),
        }
        .ok_or_else(|| Failure::Message("没有可读的数据表".to_owned()))?;
        let records = self
            .api
            .bitable_records(self.token, app, &table.table_id, BITABLE_RECORDS)
            .await?;
        let mut out = format!("## 数据表：{}（前 {} 条记录）\n", table.name, records.len());
        for record in &records {
            let fields: Vec<String> = record
                .iter()
                .map(|(name, value)| format!("{name}={}", cell_text(value)))
                .filter(|pair| !pair.ends_with('='))
                .collect();
            out.push_str(&format!("- {}\n", fields.join("；")));
        }
        let path = self.path("bitable.txt");
        self.save(&path, out.as_bytes()).await?;
        let (text, _) = clip(&out);
        let note = (tables.len() > 1 && only.is_none())
            .then(|| format!("共 {} 个数据表，只读了第一个", tables.len()));
        Ok(Read {
            title: title.to_owned(),
            text: Some(text),
            images: Vec::new(),
            saved: self.relative(&path),
            note,
        })
    }

    async fn file(
        &self,
        file: &str,
        title: &str,
        images_left: &mut usize,
    ) -> Result<Read, Failure> {
        let download = self
            .api
            .download_drive_file(self.token, file, MAX_DOWNLOAD)
            .await?;
        let bytes = download.bytes;
        if extract::is_image(&bytes) {
            let raw = bytes;
            let (image, ext) = tokio::task::spawn_blocking(move || extract::normalize(raw))
                .await
                .map_err(|err| Failure::Message(format!("处理图片失败：{err}")))?
                .map_err(Failure::Message)?;
            let path = self.path(&format!("file.{ext}"));
            self.save(&path, &image).await?;
            let images = if *images_left > 0 {
                *images_left -= 1;
                vec![self.relative(&path)]
            } else {
                Vec::new()
            };
            return Ok(Read {
                title: title.to_owned(),
                text: None,
                images,
                saved: self.relative(&path),
                note: None,
            });
        }
        let path = self.path("file");
        self.save(&path, &bytes).await?;
        let output = extract::extract(
            &path,
            title,
            bytes,
            self.workspace.dir,
            self.workspace.tools,
        )
        .await
        .map_err(Failure::Message)?;
        let images: Vec<PathBuf> = output
            .images
            .iter()
            .take(*images_left)
            .map(|p| self.relative(p))
            .collect();
        *images_left -= images.len();
        Ok(Read {
            title: title.to_owned(),
            text: (!output.text.trim().is_empty()).then_some(output.text),
            images,
            saved: self.relative(&path),
            note: output.note,
        })
    }
}

fn clip(text: &str) -> (String, bool) {
    match text.char_indices().nth(TEXT_CHARS) {
        Some((end, _)) => (text[..end].to_owned(), true),
        None => (text.to_owned(), false),
    }
}

/// 表格、多维表格的单元格：文字、数字、链接、人员、选项……都拍成一行字。
fn cell_text(value: &Value) -> String {
    let text = match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => if *b { "是" } else { "否" }.to_owned(),
        Value::Array(items) => items
            .iter()
            .map(cell_text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("、"),
        Value::Object(map) => ["text", "name", "link", "full_address"]
            .iter()
            .find_map(|key| map.get(*key).map(cell_text).filter(|t| !t.is_empty()))
            .unwrap_or_default(),
    };
    text.replace(['\t', '\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::context::extract::Tools;
    use crate::context::links;

    fn ok(data: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({"code": 0, "msg": "ok", "data": data}))
    }

    fn png() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            4,
            4,
            image::Rgb([1, 2, 3]),
        ));
        let mut out = Vec::new();
        image
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("编码");
        out
    }

    struct Env {
        server: MockServer,
        api: ApiClient,
        dir: tempfile::TempDir,
        tools: Tools,
    }

    async fn env() -> Env {
        let server = MockServer::start().await;
        let base = url::Url::parse(&format!("{}/", server.uri())).expect("地址");
        let api = ApiClient::new(base, "cli_x", SecretString::from("s")).expect("客户端");
        let dir = tempfile::tempdir().expect("临时目录");
        let tools = Tools {
            office_legacy: false,
            scratch: dir.path().join("tmp"),
        };
        Mock::given(method("POST"))
            .and(path("/open-apis/drive/v1/metas/batch_query"))
            .respond_with(ok(json!({"metas": [
                {"doc_token": "doxcnAbcdefghijklmnop", "doc_type": "docx", "title": "故障复盘"}
            ]})))
            .mount(&server)
            .await;
        Env {
            server,
            api,
            dir,
            tools,
        }
    }

    async fn read_links(env: &Env, text: &str) -> Docs {
        let wanted: Vec<WantedDoc> = links::find(text)
            .into_iter()
            .map(|link| WantedDoc {
                link,
                layer: Layer::Question,
                origin: "提问里的链接".into(),
            })
            .collect();
        let dir = env.dir.path().join("attachments/1");
        let workspace = Workspace {
            root: env.dir.path(),
            dir: &dir,
            tools: &env.tools,
        };
        read(
            &env.api,
            &SecretString::from("u-token"),
            &wanted,
            &workspace,
            10,
        )
        .await
    }

    #[tokio::test]
    async fn a_docx_link_is_read_as_markdown_with_its_images() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path("/open-apis/docs/v1/content"))
            .and(query_param("doc_token", "doxcnAbcdefghijklmnop"))
            .respond_with(ok(
                json!({"content": "# 复盘\n根因：连接池耗尽，见 ABC-12"}),
            ))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/docx/v1/documents/doxcnAbcdefghijklmnop/blocks",
            ))
            .respond_with(ok(json!({"has_more": false, "items": [
                {"block_type": 27, "image": {"token": "boxcnImage1234567890"}}
            ]})))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/drive/v1/medias/boxcnImage1234567890/download",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(png()),
            )
            .mount(&env.server)
            .await;
        let docs = read_links(&env, "看下 https://x.feishu.cn/docx/doxcnAbcdefghijklmnop").await;
        assert!(!docs.rejected);
        assert!(docs.missing.is_empty(), "{:?}", docs.missing);
        let doc = &docs.attachments[0];
        assert_eq!(doc.title, "故障复盘");
        assert!(
            doc.text
                .as_deref()
                .is_some_and(|t| t.contains("连接池耗尽"))
        );
        assert_eq!(doc.images.len(), 1);
        assert!(env.dir.path().join(&doc.images[0]).exists());
        assert!(env.dir.path().join(&doc.saved).exists(), "完整内容落盘");
    }

    #[tokio::test]
    async fn a_wiki_node_is_followed_to_the_sheet_it_points_to() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path("/open-apis/wiki/v2/spaces/get_node"))
            .and(query_param("token", "wikcnNode12345678901"))
            .respond_with(ok(json!({"node": {
                "obj_token": "shtcnSheet1234567890", "obj_type": "sheet", "title": "环境清单"
            }})))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/sheets/v3/spreadsheets/shtcnSheet1234567890/sheets/query",
            ))
            .respond_with(ok(json!({"sheets": [
                {"sheet_id": "s1", "title": "环境", "hidden": false, "resource_type": "sheet",
                 "grid_properties": {"row_count": 2, "column_count": 2}},
                {"sheet_id": "s2", "title": "隐藏页", "hidden": true, "resource_type": "sheet"}
            ]})))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/sheets/v2/spreadsheets/shtcnSheet1234567890/values_batch_get",
            ))
            .and(query_param("ranges", "s1!A1:AD200"))
            .respond_with(ok(json!({"valueRanges": [
                {"values": [["主机", "JDK", null], ["app-01", 17, null]]}
            ]})))
            .mount(&env.server)
            .await;
        let docs = read_links(&env, "https://x.feishu.cn/wiki/wikcnNode12345678901").await;
        let doc = &docs.attachments[0];
        assert_eq!(doc.title, "环境清单");
        assert_eq!(
            doc.text.as_deref(),
            Some("## 工作表：环境（共 2 行 × 2 列）\n主机\tJDK\napp-01\t17\n")
        );
    }

    #[tokio::test]
    async fn a_bitable_is_read_as_records() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/bitable/v1/apps/bascnApp123456789012/tables",
            ))
            .respond_with(ok(json!({"has_more": false, "items": [
                {"table_id": "tblA", "name": "缺陷"}, {"table_id": "tblB", "name": "需求"}
            ]})))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/open-apis/bitable/v1/apps/bascnApp123456789012/tables/tblB/records"))
            .respond_with(ok(json!({"has_more": false, "items": [
                {"record_id": "r1", "fields": {"标题": "登录 500", "负责人": [{"name": "张三"}], "备注": null}}
            ]})))
            .mount(&env.server)
            .await;
        let docs = read_links(
            &env,
            "https://x.feishu.cn/base/bascnApp123456789012?table=tblB",
        )
        .await;
        let doc = &docs.attachments[0];
        assert_eq!(
            doc.text.as_deref(),
            Some("## 数据表：需求（前 1 条记录）\n- 标题=登录 500；负责人=张三\n")
        );
        assert_eq!(doc.note, None, "指定了数据表就不提示只读了第一个");
    }

    #[tokio::test]
    async fn drive_files_are_downloaded_and_parsed_locally() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(
                "/open-apis/drive/v1/files/boxcnFile12345678901/download",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("2026-09-28 ERROR 连接池耗尽\n"),
            )
            .mount(&env.server)
            .await;
        let docs = read_links(&env, "https://x.feishu.cn/file/boxcnFile12345678901").await;
        assert!(
            docs.attachments[0]
                .text
                .as_deref()
                .is_some_and(|t| t.contains("连接池耗尽"))
        );
    }

    #[tokio::test]
    async fn permission_errors_are_explained_and_invalid_tokens_are_flagged() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path("/open-apis/docs/v1/content"))
            .and(query_param("doc_token", "doxcnAbcdefghijklmnop"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({"code": 2889902, "msg": "no permission"})),
            )
            .mount(&env.server)
            .await;
        let docs = read_links(&env, "https://x.feishu.cn/docx/doxcnAbcdefghijklmnop").await;
        assert!(!docs.rejected);
        assert!(docs.missing[0].contains("没有权限"), "{:?}", docs.missing);

        Mock::given(method("GET"))
            .and(path("/open-apis/docs/v1/content"))
            .and(query_param("doc_token", "doxcnExpired12345678"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"code": 99991677, "msg": "token expire"})),
            )
            .mount(&env.server)
            .await;
        let docs = read_links(&env, "https://x.feishu.cn/docx/doxcnExpired12345678").await;
        assert!(docs.rejected);
    }

    #[tokio::test]
    async fn only_titles_are_read_for_unsupported_types() {
        let env = env().await;
        let docs = read_links(&env, "https://x.feishu.cn/mindnotes/bmncnMind12345678901").await;
        assert!(docs.missing[0].contains("只取了标题"), "{:?}", docs.missing);
    }

    #[test]
    fn cells_of_every_shape_become_one_line() {
        assert_eq!(cell_text(&json!("a\tb\nc")), "a b c");
        assert_eq!(cell_text(&json!(17)), "17");
        assert_eq!(cell_text(&json!(true)), "是");
        assert_eq!(
            cell_text(&json!([{"name": "张三"}, {"name": "李四"}])),
            "张三、李四"
        );
        assert_eq!(
            cell_text(&json!({"link": "https://x", "text": "官网"})),
            "官网"
        );
        assert_eq!(cell_text(&json!(["选项1", "选项2"])), "选项1、选项2");
        assert_eq!(cell_text(&Value::Null), "");
    }

    #[test]
    fn api_errors_are_explained_or_flag_the_token() {
        let rejected = Failure::from(ApiError::Api {
            code: 99_991_677,
            msg: "token expire".into(),
        });
        assert!(matches!(rejected, Failure::Rejected));
        let Failure::Message(text) = Failure::from(ApiError::Api {
            code: 2_889_902,
            msg: String::new(),
        }) else {
            panic!("应当是说明");
        };
        assert!(text.contains("没有权限"));
    }
}
