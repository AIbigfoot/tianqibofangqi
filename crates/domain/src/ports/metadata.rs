//! P-02 `MetadataProvider` — 元数据与海报来源端口。

use crate::error::Result;
use crate::model::{Metadata, MetadataCandidate, ParseHint, ProviderRef};

/// 元数据提供者端口。
///
/// 约束（架构文档 §4 P-02）：
/// - **可关闭**：全部提供者被关闭时，入库仍必须成功，条目落为「元数据待补全」
///   并使用占位海报。因此实现方在不可用时应当返回空候选或 `Unsupported`，
///   而**不得**让入库流程失败。
/// - **不覆盖用户数据**：任何提供者写入都必须经 [`crate::rules::FieldLock`] 过滤。
/// - **失败不阻塞**：抓取为异步补齐流程，重试与退避在 L1 统一控制，不在提供者内实现。
pub trait MetadataProvider: Send + Sync {
    /// 提供者名称，用于注册表与 UI 展示。
    fn name(&self) -> &str;

    /// 多提供者并存时的顺序（值越小越优先）。
    fn priority(&self) -> u8;

    /// 依据解析提示给出候选。
    fn candidates(&self, hint: &ParseHint) -> Result<Vec<MetadataCandidate>>;

    /// 按提供者内部引用抓取元数据（含封面候选与简介）。
    fn fetch(&self, id: &ProviderRef) -> Result<Metadata>;
}
