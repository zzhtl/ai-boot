//! 单实例锁。
//!
//! 同一个飞书应用同时有两个进程连着长连接时，每条事件只会随机推给其中一个，
//! 表现就是「偶尔丢消息」。锁随进程退出自动释放，不会残留。

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context as _, bail};

const LOCK_FILE: &str = "ai-boot.lock";

/// 持有期间独占；drop 即释放。
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

pub fn acquire(dir: &Path) -> anyhow::Result<InstanceLock> {
    let path = dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("打开锁文件 {} 失败", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceLock { _file: file }),
        Err(TryLockError::WouldBlock) => bail!(
            "已有 ai-boot 实例在运行（{} 被占用）。同一个飞书应用只能有一个实例",
            path.display()
        ),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("锁定 {} 失败", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_instance_is_refused_until_the_first_exits() {
        let dir = tempfile::tempdir().expect("临时目录");
        let first = acquire(dir.path()).expect("首个实例");
        let err = acquire(dir.path()).expect_err("第二个实例必须被拒");
        assert!(err.to_string().contains("已有"), "{err:#}");
        drop(first);
        acquire(dir.path()).expect("首个实例退出后可以再启动");
    }
}
