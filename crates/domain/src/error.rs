//! 端口统一错误类型。
//!
//! 分类刻意保持粗糙：领域层不关心底层是 IO、网络还是子进程，只关心
//! 「能否继续」以及「是不是可解释的降级原因」。`QuotaExceeded` 对应
//! PRD 共享规则中「额度不足时预览被拒绝并回传原因」的可见降级。

use std::fmt;

/// 端口调用失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// 源 / 条目 / 句柄 / 提供者引用不存在。
    NotFound(String),
    /// 能力不支持：例如某引擎被关闭、某源类型未实现。
    Unsupported(String),
    /// 资源额度不足：预览与多屏共享并发上限，超限时拒绝。
    QuotaExceeded(String),
    /// 底层失败：IO、网络、解码、子进程异常。
    Backend(String),
}

impl PortError {
    /// 用于 UI 归类降级原因的一句话说明。
    pub fn reason(&self) -> &str {
        match self {
            PortError::NotFound(m)
            | PortError::Unsupported(m)
            | PortError::QuotaExceeded(m)
            | PortError::Backend(m) => m,
        }
    }
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = match self {
            PortError::NotFound(_) => "not_found",
            PortError::Unsupported(_) => "unsupported",
            PortError::QuotaExceeded(_) => "quota_exceeded",
            PortError::Backend(_) => "backend",
        };
        write!(f, "{tag}: {}", self.reason())
    }
}

impl std::error::Error for PortError {}

/// 端口层通用返回值。
pub type Result<T> = std::result::Result<T, PortError>;
