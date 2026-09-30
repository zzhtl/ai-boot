//! 白名单：按 open_id 判定。
//!
//! 配置里的邮箱在后台解析成 open_id；解析完成之前，这些邮箱对应的人一律
//! 不在白名单里（宁可暂时不响应，也不能放错人进来）。

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ai_boot_feishu::api::ApiClient;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Default)]
pub struct Whitelist {
    open_ids: RwLock<HashSet<String>>,
}

impl Whitelist {
    pub fn new(open_ids: impl IntoIterator<Item = String>) -> Self {
        Self {
            open_ids: RwLock::new(open_ids.into_iter().filter(|id| !id.is_empty()).collect()),
        }
    }

    pub fn allows(&self, open_id: &str) -> bool {
        if open_id.is_empty() {
            return false;
        }
        // 锁中毒只可能来自持锁时 panic，而这里的临界区不会 panic；
        // 真发生了就按「不在白名单」处理
        self.open_ids
            .read()
            .map(|ids| ids.contains(open_id))
            .unwrap_or(false)
    }

    /// 当前白名单里的所有人（告警发给他们）。
    pub fn open_ids(&self) -> Vec<String> {
        self.open_ids
            .read()
            .map(|ids| {
                let mut ids: Vec<String> = ids.iter().cloned().collect();
                ids.sort();
                ids
            })
            .unwrap_or_default()
    }

    fn extend(&self, ids: impl IntoIterator<Item = String>) {
        if let Ok(mut set) = self.open_ids.write() {
            set.extend(ids.into_iter().filter(|id| !id.is_empty()));
        }
    }
}

/// 把邮箱解析成 open_id 并加入白名单。失败就退避重试，直到成功或被取消。
pub async fn resolve_emails(
    api: Arc<ApiClient>,
    whitelist: Arc<Whitelist>,
    emails: Vec<String>,
    cancel: CancellationToken,
) {
    if emails.is_empty() {
        return;
    }
    let mut delay = Duration::from_secs(5);
    loop {
        match api.open_ids_by_email(&emails).await {
            Ok(found) => {
                let missing: Vec<&String> = emails
                    .iter()
                    .filter(|email| !found.iter().any(|(e, _)| e.eq_ignore_ascii_case(email)))
                    .collect();
                if !missing.is_empty() {
                    tracing::error!(
                        ?missing,
                        "这些邮箱没有解析出 open_id：确认应用的通讯录数据范围包含他们，且填的是用户资料里的邮箱"
                    );
                }
                tracing::info!(resolved = found.len(), "白名单邮箱解析完成");
                whitelist.extend(found.into_iter().map(|(_, open_id)| open_id));
                return;
            }
            Err(err) => {
                tracing::error!(%err, retry_in_secs = delay.as_secs(), "白名单邮箱解析失败");
            }
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(Duration::from_secs(300));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_non_empty_ids_are_allowed() {
        let list = Whitelist::new(["ou_1".to_owned(), String::new()]);
        assert!(list.allows("ou_1"));
        assert!(!list.allows("ou_2"));
        assert!(!list.allows(""));
        list.extend(["ou_2".to_owned()]);
        assert!(list.allows("ou_2"));
    }
}
