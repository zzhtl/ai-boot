//! 定时维护，每天一次：
//! - 保留期：信息最多留 30 天。会话创建满 30 天，删掉它的全部数据：库里的提问、
//!   答案、原始消息、写回记录，工作目录里的聊天记录和附件（图片、文件），以及 Claude
//!   配置目录里这个会话的记录。之后再引用它的卡片追问就开新会话，附件重新下载。
//!   不属于任何会话的旧数据、库里已经没有记录的残留目录也一并清掉，最后整理数据库
//!   文件，让删掉的内容不留在空闲页里。
//! - 备份：清理之后 `VACUUM INTO` 出一份完整的库，只留这一份——留多份的话，旧备份里
//!   会有已经超过保留期的数据。
//! - 清掉崩溃留下的运行目录和临时目录。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ai_boot_agent::claude::project_slug;
use tokio_util::sync::CancellationToken;

use crate::store::{Store, now_ms};

const EVERY: Duration = Duration::from_secs(24 * 3600);
/// 启动后先等一会儿再做第一次，别和启动恢复抢。
const FIRST_DELAY: Duration = Duration::from_secs(120);
const RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);
/// 刚收到消息的过期会话先不删：消息还没来得及建轮次，删了就没人应答了。
const IN_FLIGHT: Duration = Duration::from_secs(600);
const BACKUPS_KEPT: usize = 1;
/// 运行目录、临时目录里超过这个时间的算残留。
const LEFTOVER_AGE: Duration = Duration::from_secs(24 * 3600);

pub struct Maintenance {
    pub store: Store,
    pub data_dir: PathBuf,
    /// claude 用的配置目录（`CLAUDE_CONFIG_DIR`，复用本机登录时是 `~/.claude`）。
    /// 只删机器人会话工作目录对应的 `projects/` 子目录，不碰别的。
    pub claude_config_dir: Option<PathBuf>,
}

pub async fn run(maintenance: Maintenance, cancel: CancellationToken) {
    let mut wait = FIRST_DELAY;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(wait) => {}
        }
        maintenance.once(now_ms()).await;
        wait = EVERY;
    }
}

impl Maintenance {
    pub async fn once(&self, now: i64) {
        self.retention(now).await;
        self.backup(now).await;
        for dir in ["run", "tmp"] {
            sweep(&self.data_dir.join(dir), LEFTOVER_AGE).await;
        }
    }

    async fn retention(&self, now: i64) {
        let millis = |d: Duration| i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        let cutoff = now - millis(RETENTION);
        let expired = match self
            .store
            .expired_conversations(cutoff, now - millis(IN_FLIGHT))
            .await
        {
            Ok(expired) => expired,
            Err(err) => {
                tracing::warn!("{err:#}");
                Vec::new()
            }
        };
        let sessions = self.data_dir.join("sessions");
        let mut deleted = 0_u64;
        for id in expired {
            let workdir = sessions.join(&id);
            remove(&workdir).await;
            if let Some(config) = &self.claude_config_dir {
                remove(&config.join("projects").join(project_slug(&workdir))).await;
            }
            match self.store.purge_conversation(&id).await {
                Ok(()) => {
                    deleted += 1;
                    tracing::info!(conversation = %id, "会话超过保留期，已删除全部数据");
                }
                Err(err) => tracing::warn!("{err:#}"),
            }
        }
        match self.store.purge_orphans(cutoff, now).await {
            Ok(rows) => deleted += rows,
            Err(err) => tracing::warn!("{err:#}"),
        }
        // 库里已经没有记录的残留：崩溃或手工删库留下的工作目录，以及 Claude 那边
        // 对应的会话目录（只动机器人工作目录对应的那些，不碰用户自己的项目）。库里还有的
        // 不算残留：上面跳过的在跑、刚来消息的过期会话，目录根的修改时间在第一轮之后
        // 就不变了，只看时间会把它们正在用的目录删掉
        match self.store.conversation_ids().await {
            Ok(alive) => {
                sweep_except(&sessions, "", RETENTION, &alive).await;
                if let Some(config) = &self.claude_config_dir {
                    let prefix = format!("{}-", project_slug(&sessions));
                    let alive: HashSet<String> = alive
                        .iter()
                        .map(|id| project_slug(&sessions.join(id)))
                        .collect();
                    sweep_except(&config.join("projects"), &prefix, RETENTION, &alive).await;
                }
            }
            Err(err) => tracing::warn!("{err:#}，这次不清扫残留目录"),
        }
        if deleted > 0
            && let Err(err) = self.store.compact().await
        {
            tracing::warn!("{err:#}");
        }
    }

    async fn backup(&self, now: i64) {
        let dir = self.data_dir.join("backup");
        if let Err(err) = tokio::fs::create_dir_all(&dir).await {
            tracing::warn!(%err, "创建备份目录失败");
            return;
        }
        let date = crate::context::beijing_date(now).replace('-', "");
        let target = dir.join(format!("ai-boot-{date}.db"));
        if tokio::fs::try_exists(&target).await.unwrap_or(false) {
            return;
        }
        if let Err(err) = self.store.backup_into(&target).await {
            tracing::warn!("{err:#}");
            return;
        }
        // 只留最近几份：文件名里是日期，按名字排序就是按时间
        let mut backups: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("ai-boot-") && n.ends_with(".db"))
                })
                .collect(),
            Err(_) => return,
        };
        backups.sort();
        let excess = backups.len().saturating_sub(BACKUPS_KEPT);
        for old in backups.into_iter().take(excess) {
            if let Err(err) = tokio::fs::remove_file(&old).await {
                tracing::warn!(%err, file = %old.display(), "删除旧备份失败");
            }
        }
    }
}

async fn remove(path: &Path) {
    if let Err(err) = tokio::fs::remove_dir_all(path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%err, path = %path.display(), "清理目录失败");
    }
}

/// 删掉目录下超过 `age` 没动过的子目录和文件。
async fn sweep(dir: &Path, age: Duration) {
    sweep_except(dir, "", age, &HashSet::new()).await;
}

/// 同 [`sweep`]，只看名字以 `prefix` 开头的，`keep` 里的名字不动。
async fn sweep_except(dir: &Path, prefix: &str, age: Duration, keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.filter_map(Result::ok).filter(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        name.starts_with(prefix) && !keep.contains(&name)
    }) {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|elapsed| elapsed > age);
        if !old {
            continue;
        }
        let path = entry.path();
        let result = if path.is_dir() {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        if let Err(err) = result {
            tracing::debug!(%err, path = %path.display(), "清理残留失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{FinishedTurn, NewConversation, NewInput, Origin, TurnStatus};

    async fn open(dir: &Path) -> Store {
        std::fs::create_dir_all(dir).expect("目录");
        Store::open(dir).await.expect("数据库")
    }

    #[test]
    fn project_dirs_follow_the_cli_naming() {
        assert_eq!(
            project_slug(Path::new("/var/lib/ai-boot/sessions/0199a-b")),
            "-var-lib-ai-boot-sessions-0199a-b"
        );
        assert_eq!(
            project_slug(Path::new("/home/u/.claude")),
            "-home-u--claude"
        );
    }

    async fn conversation(store: &Store, id: &str, at: i64) {
        store
            .create_conversation(&NewConversation {
                id,
                chat_id: "oc_1",
                chat_type: "group",
                origin: Origin::NewThread,
                thread_id: None,
                root_message_id: id,
                owner_open_id: "ou_1",
                backend: "claude",
                now_ms: at,
            })
            .await
            .expect("会话");
    }

    async fn input(store: &Store, id: &str, at: i64) {
        store
            .insert_input(&NewInput {
                message_id: id,
                chat_id: "oc_1",
                chat_type: "group",
                sender_open_id: "ou_1",
                payload: "{\"机密\":\"聊天内容\"}",
                received_at_ms: at,
            })
            .await
            .expect("消息");
    }

    /// 信息最多留 30 天：按创建时间算，满了就删掉库里和磁盘上的全部数据，
    /// 在跑的、刚来消息的等下一次；备份只留清理之后的那一份。
    #[tokio::test]
    async fn everything_older_than_the_retention_period_is_deleted() {
        let dir = tempfile::tempdir().expect("临时目录");
        let data = dir.path().join("data");
        let claude = dir.path().join("claude");
        let store = open(&data).await;
        let day = 24 * 3600 * 1000_i64;
        let now = 100 * day;
        conversation(&store, "old", now - 31 * day).await;
        conversation(&store, "recent", now - 2 * day).await;
        conversation(&store, "busy", now - 40 * day).await;
        // busy：老会话，刚来了一条消息（还没建轮次）
        input(&store, "om_busy", now - 1000).await;
        store
            .assign_conversation("om_busy", "busy")
            .await
            .expect("分派");
        // old：上个月一直在追问，最后一轮是 3 天前——照样按创建时间删
        input(&store, "om_old", now - 31 * day).await;
        store
            .create_turn("old", "t-old", "om_old", now - 3 * day)
            .await
            .expect("轮次");
        store
            .finish_turn(&FinishedTurn {
                turn_id: "t-old",
                conversation_id: "old",
                status: TurnStatus::Succeeded,
                model: "opus",
                answer_json: Some("{}"),
                error_kind: None,
                error: None,
                tokens: 0,
                tool_calls: 0,
                duration_ms: 0,
                session_id: None,
                session_tokens: None,
                history_cursor_ms: None,
                context_tokens: None,
                now_ms: now - 3 * day,
            })
            .await
            .expect("结束");
        // 没分到会话的旧消息（白名单外的私聊）
        input(&store, "om_denied", now - 35 * day).await;
        for id in ["old", "recent", "busy"] {
            let workdir = data.join("sessions").join(id);
            std::fs::create_dir_all(workdir.join("attachments/1")).expect("工作目录");
            std::fs::write(workdir.join("attachments/1/00-image.webp"), b"x").expect("附件");
            std::fs::create_dir_all(claude.join("projects").join(project_slug(&workdir)))
                .expect("Claude 会话");
        }
        // busy 的目录根在第一轮之后就不再变：修改时间早就超过保留期了，也不能当残留删
        let long_ago = SystemTime::now() - Duration::from_secs(40 * 24 * 3600);
        let busy = data.join("sessions").join("busy");
        for dir in [
            busy.clone(),
            claude.join("projects").join(project_slug(&busy)),
        ] {
            std::fs::File::open(&dir)
                .and_then(|f| f.set_modified(long_ago))
                .expect("改修改时间");
        }
        // 用户自己的 Claude 项目不能碰
        let own = claude.join("projects").join("-home-u-code");
        std::fs::create_dir_all(&own).expect("用户项目");

        let maintenance = Maintenance {
            store: store.clone(),
            data_dir: data.clone(),
            claude_config_dir: Some(claude.clone()),
        };
        maintenance.once(now).await;

        let exists = |id: &str| data.join("sessions").join(id).exists();
        let claude_exists = |id: &str| {
            claude
                .join("projects")
                .join(project_slug(&data.join("sessions").join(id)))
                .exists()
        };
        assert!(
            !exists("old") && !claude_exists("old"),
            "超过保留期的文件全删"
        );
        assert!(
            store.turn_record("t-old").await.expect("查询").is_none(),
            "提问和答案也删"
        );
        assert!(store.conversation("old").await.expect("查询").is_none());
        assert!(
            store.input_payload("om_old").await.expect("查询").is_none(),
            "原始消息也删"
        );
        assert!(
            store
                .input_payload("om_denied")
                .await
                .expect("查询")
                .is_none()
        );
        assert!(exists("recent") && claude_exists("recent"));
        assert!(
            exists("busy") && claude_exists("busy"),
            "刚来了消息的等下一次"
        );
        assert!(store.conversation("busy").await.expect("查询").is_some());
        assert!(own.exists(), "用户自己的 Claude 项目不动");
        // 备份：清理之后出的，里面没有过期数据
        let backups: Vec<_> = std::fs::read_dir(data.join("backup"))
            .expect("备份目录")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(backups.len(), 1);
        let restored = dir.path().join("restored");
        std::fs::create_dir_all(&restored).expect("目录");
        std::fs::copy(backups[0].path(), restored.join("ai-boot.db")).expect("拷贝");
        let restored = Store::open(&restored).await.expect("备份能打开");
        assert!(
            restored
                .conversation("recent")
                .await
                .expect("查询")
                .is_some()
        );
        assert!(restored.conversation("old").await.expect("查询").is_none());
        // 删掉的内容不留在数据库文件里
        let file = std::fs::read(data.join("ai-boot.db")).expect("数据库文件");
        let needle = "聊天内容".as_bytes();
        let leftovers = file.windows(needle.len()).filter(|w| *w == needle).count();
        assert_eq!(leftovers, 1, "只剩 busy 那条消息");
    }

    #[tokio::test]
    async fn only_the_last_backups_are_kept() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = open(dir.path()).await;
        let maintenance = Maintenance {
            store,
            data_dir: dir.path().to_path_buf(),
            claude_config_dir: None,
        };
        let day = 24 * 3600 * 1000_i64;
        for i in 0..10 {
            maintenance.backup(1_790_000_000_000 + i * day).await;
        }
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("backup"))
            .expect("备份目录")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), BACKUPS_KEPT);
        let newest = crate::context::beijing_date(1_790_000_000_000 + 9 * day).replace('-', "");
        assert!(names[0].contains(&newest), "留下的是最新那份：{names:?}");
    }
}
