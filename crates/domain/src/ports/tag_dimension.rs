//! P-05 `TagDimension` — 标签维度端口。
//!
//! 标签维度**不是代码里的枚举**，而是「配置 + 数据驱动」的定义：新增维度只需登记
//! 一条 [`TagDimensionDef`] 并声明其取值方式。检索组合逻辑（同维度 OR、跨维度 AND）
//! 实现一次、作用于全部维度，因此新增维度天然获得正确语义（硬约束 C6）。

use crate::error::Result;

/// 维度取值方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    /// 单值选择。
    Single,
    /// 多值选择（同维度内 OR）。
    Multi,
}

/// 维度来源（架构文档 §4 P-05 的 `source`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSource {
    /// 来自 P-02 元数据。
    Metadata,
    /// 系统按事实归集（如时长 / 清晰度 / 字幕状态），对应 FR-03「系统自动，不可手动编辑」。
    Derived,
    /// 用户创建。
    User,
}

/// 一条标签维度定义。字段与文档示例一一对应：
/// `{ "id":"genre", "label":"内容类型", "selector":"multi", "source":"metadata",
///    "expand":"value", "order":1, "editable":true, "lockable":true }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDimensionDef {
    pub id: String,
    pub label: String,
    pub selector: Selector,
    pub source: TagSource,
    /// 展开方式。文档示例只给出 `"value"`；此处保留为字符串，避免在 M0 编造其他取值。
    pub expand: String,
    pub order: u32,
    pub editable: bool,
    pub lockable: bool,
    /// `derived` 维度的取值规则（表达式或注册的派生器标识）。非 `derived` 时为 `None`。
    pub derive_rule: Option<String>,
}

impl TagDimensionDef {
    /// 文档中的 `genre` 示例，作为内置默认维度。
    pub fn genre() -> Self {
        Self {
            id: "genre".to_owned(),
            label: "内容类型".to_owned(),
            selector: Selector::Multi,
            source: TagSource::Metadata,
            expand: "value".to_owned(),
            order: 1,
            editable: true,
            lockable: true,
            derive_rule: None,
        }
    }

    fn builtin(
        id: &str,
        label: &str,
        source: TagSource,
        order: u32,
        derive_rule: Option<&str>,
    ) -> Self {
        // 「系统自动」维度不可手动编辑、不可锁定；其余维度均由用户维护
        let auto = source == TagSource::Derived;
        Self {
            id: id.to_owned(),
            label: label.to_owned(),
            selector: Selector::Multi,
            source,
            expand: "value".to_owned(),
            order,
            editable: !auto,
            lockable: !auto,
            derive_rule: derive_rule.map(str::to_owned),
        }
    }

    /// PRD FR-03「标签体系」的九个内置维度，顺序与文档表格一致。
    ///
    /// 这是**配置**：新增一个维度只需在此（或数据源的等价位）增一行，
    /// 检索组合逻辑作用于全部维度，因此检索代码无需改动（硬约束 C6）。
    pub fn builtin_defaults() -> Vec<TagDimensionDef> {
        use crate::tagging::{
            DIM_CUSTOM, DIM_DURATION, DIM_QUALITY, DIM_REGION, DIM_SOURCE, DIM_SUBTITLE, DIM_WATCH,
            DIM_YEAR, RULE_DURATION_BUCKET, RULE_RESOLUTION_TIER, RULE_SOURCE_SCOPE,
            RULE_SUBTITLE_STATE, RULE_WATCH_STATE,
        };

        vec![
            // 第一项复用文档示例，避免同一维度写两份定义
            Self::genre(),
            Self::builtin(DIM_REGION, "地区与语言", TagSource::Metadata, 2, None),
            Self::builtin(DIM_YEAR, "年份与年代", TagSource::Metadata, 3, None),
            Self::builtin(DIM_DURATION, "时长区间", TagSource::Derived, 4, Some(RULE_DURATION_BUCKET)),
            Self::builtin(DIM_QUALITY, "画质与规格", TagSource::Derived, 5, Some(RULE_RESOLUTION_TIER)),
            Self::builtin(DIM_SOURCE, "来源", TagSource::Derived, 6, Some(RULE_SOURCE_SCOPE)),
            Self::builtin(DIM_WATCH, "观看状态", TagSource::Derived, 7, Some(RULE_WATCH_STATE)),
            Self::builtin(DIM_SUBTITLE, "字幕状态", TagSource::Derived, 8, Some(RULE_SUBTITLE_STATE)),
            Self::builtin(DIM_CUSTOM, "自定义标签", TagSource::User, 9, None),
        ]
    }
}

/// 标签维度注册表。新增维度即调用 [`TagDimensionRegistry::register`]，
/// 检索交互代码不需要任何改动。
pub trait TagDimensionRegistry: Send + Sync {
    /// 全部已登记维度，按 `order` 升序。
    fn dimensions(&self) -> Vec<TagDimensionDef>;

    /// 按 id 取维度。
    fn get(&self, id: &str) -> Option<TagDimensionDef>;

    /// 登记一条维度。id 重复应返回 `Unsupported`，不得静默覆盖。
    fn register(&mut self, def: TagDimensionDef) -> Result<()>;
}
