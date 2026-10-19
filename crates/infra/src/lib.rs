//! L4 基础设施层：SQLite 存储与迁移框架。

pub mod db;
pub mod error;
pub mod portability;
pub mod repo;
pub mod subtitle_repo;
pub mod tag_repo;

pub use error::{InfraError, Result};

/// 统一毫秒时钟。
///
/// L4 的所有写入（迁移登记、条目 upsert、进度心跳）都从这里取时间，
/// 避免时间来源分散导致备份排序、增量扫描判断出现不一致。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
