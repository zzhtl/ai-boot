//! 模型目录每 24 小时查询一次，持久化后跨服务重启复用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ai_bot_agent::{AgentBackend, ModelOption};
use serde::{Deserialize, Serialize};

use crate::store::now_ms;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    checked_at_ms: i64,
    models: Vec<ModelOption>,
}

pub struct ModelCache {
    file: PathBuf,
    snapshot: Mutex<Option<Snapshot>>,
}

impl ModelCache {
    pub fn new(file: PathBuf) -> Self {
        let snapshot = std::fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok());
        Self {
            file,
            snapshot: Mutex::new(snapshot),
        }
    }

    pub fn choices(&self) -> Vec<ModelOption> {
        self.snapshot
            .lock()
            .expect("模型缓存锁")
            .as_ref()
            .map(|snapshot| snapshot.models.clone())
            .unwrap_or_default()
    }

    fn wait(&self, now: i64) -> Duration {
        let millis = self
            .snapshot
            .lock()
            .expect("模型缓存锁")
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .checked_at_ms
                    .saturating_add(DAY_MS)
                    .saturating_sub(now)
                    .max(0)
            })
            .unwrap_or(0);
        Duration::from_millis(millis as u64)
    }

    async fn refresh(&self, backend: &dyn AgentBackend, now: i64) -> anyhow::Result<()> {
        if !self.wait(now).is_zero() {
            return Ok(());
        }
        let result = backend.models().await;
        let models = match &result {
            Ok(models) => models.iter().take(5).cloned().collect(),
            Err(_) => self.choices(),
        };
        let snapshot = Snapshot {
            checked_at_ms: now,
            models,
        };
        // 失败也记录本次检查时间，保留上次可用目录，避免反复请求。
        *self.snapshot.lock().expect("模型缓存锁") = Some(snapshot.clone());
        let temporary = self.file.with_extension("json.tmp");
        tokio::fs::write(&temporary, serde_json::to_vec(&snapshot)?).await?;
        tokio::fs::rename(temporary, &self.file).await?;
        result?;
        Ok(())
    }

    pub fn start(self: &Arc<Self>, backend: Arc<dyn AgentBackend>) {
        let cache = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(cache.wait(now_ms())).await;
                if let Err(err) = cache.refresh(&*backend, now_ms()).await {
                    tracing::warn!(%err, "更新每日模型缓存失败，保留已有目录");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_bot_agent::{AgentError, BackendInfo, BackendKind, TurnHandle, TurnRequest};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Backend {
        calls: AtomicUsize,
        fail: AtomicBool,
    }
    #[async_trait::async_trait]
    impl AgentBackend for Backend {
        fn kind(&self) -> BackendKind {
            BackendKind::Codex
        }
        async fn models(&self) -> Result<Vec<ModelOption>, AgentError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(AgentError::Preflight("暂时失败".into()));
            }
            Ok((0..7)
                .map(|n| ModelOption {
                    id: format!("model-{call}-{n}"),
                    name: format!("模型 {n}"),
                })
                .collect())
        }
        async fn start_turn(&self, _: TurnRequest) -> Result<TurnHandle, AgentError> {
            panic!("缓存不触发分析")
        }
        async fn preflight(&self) -> Result<BackendInfo, AgentError> {
            panic!("未调用")
        }
    }

    #[tokio::test]
    async fn cache_queries_once_per_day_and_survives_restarts() {
        let dir = tempfile::tempdir().expect("目录");
        let file = dir.path().join("codex-models.json");
        let backend = Backend {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let cache = ModelCache::new(file.clone());
        cache.refresh(&backend, 100).await.expect("首次刷新");
        assert_eq!(cache.choices().len(), 5);
        cache.refresh(&backend, 1000).await.expect("当日复用");
        let rebooted = ModelCache::new(file);
        rebooted.refresh(&backend, 1000).await.expect("重启复用");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        rebooted
            .refresh(&backend, 100 + DAY_MS)
            .await
            .expect("次日刷新");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(rebooted.choices()[0].id, "model-1-0");
    }

    #[tokio::test]
    async fn failed_refresh_retains_the_previous_choices_without_repeated_queries() {
        let dir = tempfile::tempdir().expect("目录");
        let file = dir.path().join("codex-models.json");
        let backend = Backend {
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        };
        let cache = ModelCache::new(file.clone());
        cache.refresh(&backend, 100).await.expect("首次刷新");
        let previous = cache.choices();
        backend.fail.store(true, Ordering::SeqCst);
        assert!(cache.refresh(&backend, 100 + DAY_MS).await.is_err());
        assert_eq!(cache.choices(), previous);
        let rebooted = ModelCache::new(file);
        rebooted
            .refresh(&backend, 1000 + DAY_MS)
            .await
            .expect("失败后当日复用");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(rebooted.choices(), previous);
    }
}
