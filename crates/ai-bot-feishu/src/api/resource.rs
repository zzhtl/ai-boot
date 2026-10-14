//! 消息里的图片与文件。

use super::client::{ApiClient, ApiError, Auth, Call, Download, path_segment};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// 图片消息、富文本里的图片。
    Image,
    /// 文件、音频、视频（不含表情包）。
    File,
}

impl ApiClient {
    /// 下载消息里的图片或文件（单个上限 100 MB，`max_bytes` 是调用方自己的上限）。
    /// `key` 必须属于 `message_id` 这条消息；官方文档说合并转发的子消息、卡片、
    /// 表情包都不支持。
    pub async fn download_resource(
        &self,
        message_id: &str,
        key: &str,
        kind: ResourceKind,
        max_bytes: usize,
    ) -> Result<Download, ApiError> {
        let path = format!(
            "open-apis/im/v1/messages/{}/resources/{}",
            path_segment(message_id)?,
            path_segment(key)?
        );
        let kind = match kind {
            ResourceKind::Image => "image",
            ResourceKind::File => "file",
        };
        self.download(Call::get(path).query("type", kind), Auth::Tenant, max_bytes)
            .await
    }
}
