//! 云文档：都以用户身份调用（能读到的范围就是授权人能打开的范围）。

use secrecy::SecretString;
use serde::Deserialize;
use serde_json::Value;

use super::client::{ApiClient, ApiError, Auth, Call, Download, path_segment};
use super::message::Page;

/// 新版文档里图片块的类型。
const IMAGE_BLOCK: i64 = 27;

#[derive(Debug, Clone, Deserialize)]
pub struct WikiNode {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub obj_token: String,
    /// doc / docx / sheet / mindnote / bitable / file / slides。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub obj_type: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SheetInfo {
    pub sheet_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub hidden: bool,
    #[serde(default)]
    pub grid_properties: Option<GridProperties>,
    /// sheet / bitable / 其他。
    #[serde(default, deserialize_with = "crate::nullable")]
    pub resource_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GridProperties {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub row_count: u64,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub column_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BitableTable {
    pub table_id: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DocMeta {
    #[serde(default, deserialize_with = "crate::nullable")]
    pub doc_token: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub doc_type: String,
    #[serde(default, deserialize_with = "crate::nullable")]
    pub title: String,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
struct NodeWrapper {
    node: WikiNode,
}

#[derive(Deserialize)]
struct Sheets {
    #[serde(default)]
    sheets: Vec<SheetInfo>,
}

#[derive(Deserialize)]
struct ValueRanges {
    #[serde(default, rename = "valueRanges", deserialize_with = "crate::nullable")]
    value_ranges: Vec<ValueRange>,
}

#[derive(Deserialize)]
struct ValueRange {
    #[serde(default)]
    values: Vec<Vec<Value>>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(default)]
    fields: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
struct Metas {
    #[serde(default)]
    metas: Vec<DocMeta>,
}

impl ApiClient {
    /// 新版文档导出成 Markdown（上限 10 MB，5 次/秒）。
    pub async fn docx_markdown(
        &self,
        token: &SecretString,
        document_id: &str,
    ) -> Result<String, ApiError> {
        let call = Call::get("open-apis/docs/v1/content")
            .query("doc_token", path_segment(document_id)?)
            .query("doc_type", "docx")
            .query("content_type", "markdown");
        let content: Content = self.call_as(call, Auth::User(token)).await?;
        Ok(content.content)
    }

    /// 新版文档里图片块的素材 token，按出现顺序，最多 `limit` 个。
    pub async fn docx_image_tokens(
        &self,
        token: &SecretString,
        document_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, ApiError> {
        let path = format!(
            "open-apis/docx/v1/documents/{}/blocks",
            path_segment(document_id)?
        );
        let mut tokens = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut call = Call::get(path.clone()).query("page_size", "500");
            if let Some(next) = &page_token {
                call = call.query("page_token", next.clone());
            }
            let page: Page<Value> = self.call_as(call, Auth::User(token)).await?;
            for block in &page.items {
                let image = block.get("block_type").and_then(Value::as_i64) == Some(IMAGE_BLOCK);
                if let Some(media) = block
                    .pointer("/image/token")
                    .and_then(Value::as_str)
                    .filter(|t| image && !t.is_empty())
                {
                    tokens.push(media.to_owned());
                    if tokens.len() >= limit {
                        return Ok(tokens);
                    }
                }
            }
            match page.page_token.filter(|t| page.has_more && !t.is_empty()) {
                Some(next) => page_token = Some(next),
                None => return Ok(tokens),
            }
        }
    }

    /// 下载文档里的素材（图片等）。
    pub async fn download_media(
        &self,
        token: &SecretString,
        file_token: &str,
        max_bytes: usize,
    ) -> Result<Download, ApiError> {
        let path = format!(
            "open-apis/drive/v1/medias/{}/download",
            path_segment(file_token)?
        );
        self.download(Call::get(path), Auth::User(token), max_bytes)
            .await
    }

    /// 下载云空间里的文件。
    pub async fn download_drive_file(
        &self,
        token: &SecretString,
        file_token: &str,
        max_bytes: usize,
    ) -> Result<Download, ApiError> {
        let path = format!(
            "open-apis/drive/v1/files/{}/download",
            path_segment(file_token)?
        );
        self.download(Call::get(path), Auth::User(token), max_bytes)
            .await
    }

    /// 知识库节点：实际的文档类型和 token。
    pub async fn wiki_node(
        &self,
        token: &SecretString,
        node_token: &str,
    ) -> Result<WikiNode, ApiError> {
        let call = Call::get("open-apis/wiki/v2/spaces/get_node")
            .query("token", path_segment(node_token)?);
        let wrapper: NodeWrapper = self.call_as(call, Auth::User(token)).await?;
        Ok(wrapper.node)
    }

    /// 电子表格里的工作表。
    pub async fn sheets(
        &self,
        token: &SecretString,
        spreadsheet: &str,
    ) -> Result<Vec<SheetInfo>, ApiError> {
        let path = format!(
            "open-apis/sheets/v3/spreadsheets/{}/sheets/query",
            path_segment(spreadsheet)?
        );
        let sheets: Sheets = self.call_as(Call::get(path), Auth::User(token)).await?;
        Ok(sheets.sheets)
    }

    /// 读若干个范围（`<sheetId>!A1:D100`），单元格按纯文本返回。
    pub async fn sheet_values(
        &self,
        token: &SecretString,
        spreadsheet: &str,
        ranges: &[String],
    ) -> Result<Vec<Vec<Vec<Value>>>, ApiError> {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/values_batch_get",
            path_segment(spreadsheet)?
        );
        let call = Call::get(path)
            .query("ranges", ranges.join(","))
            .query("valueRenderOption", "ToString")
            .query("dateTimeRenderOption", "FormattedString");
        let ranges: ValueRanges = self.call_as(call, Auth::User(token)).await?;
        Ok(ranges.value_ranges.into_iter().map(|r| r.values).collect())
    }

    /// 多维表格里的数据表（第一页）。
    pub async fn bitable_tables(
        &self,
        token: &SecretString,
        app: &str,
    ) -> Result<Vec<BitableTable>, ApiError> {
        let path = format!("open-apis/bitable/v1/apps/{}/tables", path_segment(app)?);
        let page: Page<BitableTable> = self
            .call_as(Call::get(path).query("page_size", "100"), Auth::User(token))
            .await?;
        Ok(page.items)
    }

    /// 数据表的前 `limit` 条记录（每条是字段名到值的映射）。
    pub async fn bitable_records(
        &self,
        token: &SecretString,
        app: &str,
        table: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Map<String, Value>>, ApiError> {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/records",
            path_segment(app)?,
            path_segment(table)?
        );
        let page: Page<Record> = self
            .call_as(
                Call::get(path).query("page_size", limit.min(500).to_string()),
                Auth::User(token),
            )
            .await?;
        Ok(page.items.into_iter().map(|r| r.fields).collect())
    }

    /// 批量取文档的标题等元数据（每次最多 200 个）。
    pub async fn doc_metas(
        &self,
        token: &SecretString,
        docs: &[(String, String)],
    ) -> Result<Vec<DocMeta>, ApiError> {
        let request_docs: Vec<Value> = docs
            .iter()
            .take(200)
            .map(|(doc_token, doc_type)| {
                serde_json::json!({ "doc_token": doc_token, "doc_type": doc_type })
            })
            .collect();
        let body = serde_json::json!({ "request_docs": request_docs, "with_url": false });
        let metas: Metas = self
            .call_as(
                Call::post("open-apis/drive/v1/metas/batch_query", body),
                Auth::User(token),
            )
            .await?;
        Ok(metas.metas)
    }
}
