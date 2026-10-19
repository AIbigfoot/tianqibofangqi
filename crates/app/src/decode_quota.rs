//! 并发解码额度。
//!
//! 架构文档 §7「预览实例与解码额度」明确：预览与多屏**共用同一份**并发解码
//! 额度；额度不足时预览被拒绝并回传原因，避免「预览 + 4 路多屏」同时解码导致丢帧。

/// 解码额度。上限默认 4 路（可调）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeQuota {
    limit: usize,
    in_use: usize,
}

impl DecodeQuota {
    /// 默认上限：4 路。
    pub const DEFAULT_LIMIT: usize = 4;

    pub fn new(limit: usize) -> Self {
        Self { limit, in_use: 0 }
    }

    /// 以默认上限创建。
    pub fn with_default_limit() -> Self {
        Self::new(Self::DEFAULT_LIMIT)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn in_use(&self) -> usize {
        self.in_use
    }

    pub fn available(&self) -> usize {
        self.limit.saturating_sub(self.in_use)
    }

    /// 尝试占用一份额度；额度耗尽时返回 `false`（调用方据此回传可见的降级原因）。
    pub fn try_acquire(&mut self) -> bool {
        if self.in_use < self.limit {
            self.in_use += 1;
            true
        } else {
            false
        }
    }

    /// 释放一份额度。重复释放只会归零，不会下溢。
    pub fn release(&mut self) {
        self.in_use = self.in_use.saturating_sub(1);
    }

    /// 调整上限。上限降低时不影响已占用的路数（不擅自关闭用户已开的窗口）。
    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }
}

impl Default for DecodeQuota {
    fn default() -> Self {
        Self::with_default_limit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_until_limit_then_refuse() {
        let mut quota = DecodeQuota::new(2);
        assert!(quota.try_acquire());
        assert!(quota.try_acquire());
        assert!(!quota.try_acquire(), "超出上限必须拒绝");
        assert_eq!(quota.available(), 0);

        quota.release();
        assert!(quota.try_acquire(), "释放后应可再次占用");
    }

    #[test]
    fn release_never_underflows() {
        let mut quota = DecodeQuota::new(1);
        quota.release();
        quota.release();
        assert_eq!(quota.in_use(), 0);
    }
}
