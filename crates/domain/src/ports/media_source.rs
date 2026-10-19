//! P-01 `MediaSourceProvider` — 媒体源类型端口。
//!
//! L3 实现：`LocalFolderProvider` / `LanPathProvider` / `DirectUrlProvider` /
//! （后续）`CloudDriveProvider`。

use crate::error::Result;
use crate::model::{DiscoveredItem, MediaItem, MediaSource, PlayableRef, SourceKind, StreamFacts};

/// 发现过程的回执出口。提供者在 `discover` 中通过该出口提交候选项，
/// 并从中读取取消信号。
///
/// 约束（架构文档 §4 P-01）：
/// - **增量与幂等**：重复调用时按稳定键合并，不重复入库、不覆盖用户数据。
/// - **可取消**：收到取消信号后停止扫描，但已提交条目保留。
pub trait DiscoverSink: Send + Sync {
    /// 提交一个候选项。返回 `Err` 表示该条目被拒绝，但不中断整个扫描。
    fn submit(&self, item: DiscoveredItem) -> Result<()>;

    /// 是否已请求取消。
    fn is_cancelled(&self) -> bool;
}

/// 媒体源类型端口。
///
/// 「发现」与「解析」分离：`discover` 只枚举候选项，不做解析与入库。
pub trait MediaSourceProvider: Send + Sync {
    /// 该提供者对应的源类型。
    fn kind(&self) -> SourceKind;

    /// 枚举候选项：只做「发现」，不做解析与入库。
    fn discover(
        &self,
        src: &MediaSource,
        sink: &dyn DiscoverSink,
    ) -> Result<crate::model::DiscoverStat>;

    /// 打开播放句柄：本地返回路径，在线返回可播放地址（必要时带请求头）。
    fn open(&self, item: &MediaItem) -> Result<PlayableRef>;

    /// 探测流事实：时长 / 分辨率 / 音轨 / 内嵌字幕清单。
    fn probe(&self, ref_: &PlayableRef) -> Result<StreamFacts>;
}
