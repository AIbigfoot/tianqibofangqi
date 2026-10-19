//! 数据可迁移（PRD FR-07）的对外回执。
//!
//! 导出包**文件格式**由 L4 定义（结构化 JSON 清单 + 资源目录），本模块只声明
//! 导入 / 导出结束后需要回传给 L1 与 L0 的计数与事实。纯数据，零 IO。

use crate::model::ItemId;

/// 一次导出的统计（导出包内容的条目数，用于界面提示与验证）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportOutcome {
    pub sources: usize,
    pub items: usize,
    /// 导出的**用户**标签关系数（派生标签属可重建数据，不导出）。
    pub user_tags: usize,
    pub collections: usize,
    pub settings: usize,
    pub subtitle_tracks: usize,
}

/// 导入完成后仍需按进度事实重算「观看状态」的条目。
///
/// 观看状态是派生维度：导入时先还原进度事实（`playback_state`），再由 L1 调用
/// 同一份派生规则重算，避免「已看」在检索区丢失。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredProgress {
    pub item_id: ItemId,
    pub watched: bool,
}

/// 一次导入的回执。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportOutcome {
    pub sources: usize,
    pub items: usize,
    /// 导入后按**本机**路径判定不可访问、被标记「缺失」的条目数。
    pub items_missing: usize,
    pub user_tags: usize,
    pub collections: usize,
    pub settings: usize,
    pub subtitle_tracks: usize,
    /// 已还原进度的条目（本地 id），供 L1 重算观看状态。
    pub restored_progress: Vec<RestoredProgress>,
}
