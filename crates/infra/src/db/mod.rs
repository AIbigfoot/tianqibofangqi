//! 数据库句柄与迁移入口。

pub mod migration;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::error::Result;

/// SQLite 数据库句柄。
///
/// 打开时即启用 WAL 与外键约束；迁移由 [`migration::MigrationRunner`] 负责。
pub struct Database {
    conn: Connection,
    path: PathBuf,
}

impl Database {
    /// 打开（或创建）磁盘数据库。父目录不存在时自动创建。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(&path)?;
        let db = Self { conn, path };
        db.configure()?;
        Ok(db)
    }

    /// 内存数据库，仅用于测试与迁移框架自测。
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn,
            path: PathBuf::new(),
        };
        db.configure()?;
        Ok(db)
    }

    /// WAL + 外键 + 适度同步（架构 §8 指定 SQLite + WAL）。
    fn configure(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL;")?;
        Ok(())
    }

    /// 执行所有待应用迁移，返回迁移后的版本号。
    pub fn migrate(&self) -> Result<i64> {
        migration::MigrationRunner::run(&self.conn, &self.path, migration::MIGRATIONS)
    }

    /// 当前 schema 版本。
    pub fn schema_version(&self) -> Result<i64> {
        migration::MigrationRunner::current_version(&self.conn)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
