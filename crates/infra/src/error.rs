//! 基础设施层错误。

use std::fmt;

use cinema_domain::error::PortError;

#[derive(Debug)]
pub enum InfraError {
    /// SQLite 返回的错误。
    Sqlite(rusqlite::Error),
    /// 文件系统错误（建目录、备份、清理）。
    Io(std::io::Error),
    /// 迁移规则被违反：跳版、校验失败、回滚。
    Migration(String),
    /// 导入包格式不合法：清单缺失、标记不匹配、资源不可读。
    Format(String),
    /// 由领域层端口错误上抛。
    Domain(PortError),
}

impl fmt::Display for InfraError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InfraError::Sqlite(e) => write!(f, "sqlite: {e}"),
            InfraError::Io(e) => write!(f, "io: {e}"),
            InfraError::Migration(m) => write!(f, "migration: {m}"),
            InfraError::Format(m) => write!(f, "format: {m}"),
            InfraError::Domain(e) => write!(f, "domain: {e}"),
        }
    }
}

impl std::error::Error for InfraError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            InfraError::Sqlite(e) => Some(e),
            InfraError::Io(e) => Some(e),
            InfraError::Domain(e) => Some(e),
            InfraError::Migration(_) | InfraError::Format(_) => None,
        }
    }
}

impl From<rusqlite::Error> for InfraError {
    fn from(value: rusqlite::Error) -> Self {
        InfraError::Sqlite(value)
    }
}

impl From<std::io::Error> for InfraError {
    fn from(value: std::io::Error) -> Self {
        InfraError::Io(value)
    }
}

impl From<PortError> for InfraError {
    fn from(value: PortError) -> Self {
        InfraError::Domain(value)
    }
}

pub type Result<T> = std::result::Result<T, InfraError>;
