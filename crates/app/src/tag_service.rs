//! L1 标签能力收口。
//!
//! 这里回答三个问题，且只回答这三个：
//! 1. **标签怎么写** —— [`TagStore`] 把落库交给组合根实现；
//! 2. **配置从哪来** —— [`dimension_def`] 与 [`builtin_derivers`] 是「维度定义 /
//!    派生器」两份内置配置的唯一来源；
//! 3. **派生标签按什么求值** —— [`apply_derived`] 读维度声明的 `derive_rule`，
//!    从注册表取实现，因此「新增派生维度只改配置」而不改调用方。
//!
//! 计算时机（架构文档 §5）：`probe()` 是画质 / 时长区间 / 字幕状态的唯一计算点；
//! 「来源」「观看状态」分别在入库与进度变化时归集。三处调用点都在本层。

use std::sync::OnceLock;

use cinema_domain::error::Result;
use cinema_domain::model::ItemId;
use cinema_domain::ports::tag_dimension::{TagDimensionDef, TagSource};
use cinema_domain::tagging::{derive_values, BuiltinDerivers, DeriveFacts, DeriverRegistry};

/// 标签写入能力。实现由**组合根**提供（L4 只提供 SQL，不做流程判断）。
///
/// 两个方法总是一起被调用：派生标签变化后必须重建该条目的检索索引，
/// 否则 FTS 里的标签名会与库内不一致。
pub trait TagStore: Send + Sync {
    /// 重算某维度上的**派生**标签：覆盖该维度上 `origin=derived` 的旧值，
    /// 不动用户手动添加或已锁定的标签。
    fn replace_derived_tags(
        &self,
        item_id: &ItemId,
        dimension_id: &str,
        values: &[String],
    ) -> Result<()>;

    /// 重建该条目的全文检索行（标题 / 简介 / 标签名变化后调用）。
    fn reindex_item(&self, item_id: &ItemId) -> Result<()>;
}

/// 内置维度定义（配置的唯一来源），进程内只解析一次。
fn builtin_dimensions() -> &'static [TagDimensionDef] {
    static DIMS: OnceLock<Vec<TagDimensionDef>> = OnceLock::new();
    DIMS.get_or_init(TagDimensionDef::builtin_defaults)
}

/// 按 id 取维度定义。
pub fn dimension_def(dimension_id: &str) -> Option<TagDimensionDef> {
    builtin_dimensions()
        .iter()
        .find(|d| d.id == dimension_id)
        .cloned()
}

/// 内置派生器注册表（配置的唯一来源）。
///
/// 派生器是「事实 → 取值」的纯函数，没有 IO，因此不需要由组合根注入：
/// 它就是系统内置配置的一部分，与 [`dimension_def`] 同源。
pub fn builtin_derivers() -> &'static dyn DeriverRegistry {
    static REGISTRY: BuiltinDerivers = BuiltinDerivers;
    &REGISTRY
}

/// 按维度声明的派生规则求值并落库。
///
/// 维度未登记、维度非 `derived`、或 `derive_rule` 未注册时都**不写入任何东西**——
/// 「系统自动维度」这一属性完全由配置决定，调用方无需判断。
pub fn apply_derived<S: TagStore + ?Sized>(
    store: &S,
    registry: &dyn DeriverRegistry,
    dimension_id: &str,
    item_id: &ItemId,
    facts: &DeriveFacts<'_>,
) -> Result<()> {
    let Some(def) = dimension_def(dimension_id) else {
        return Ok(());
    };
    if def.source != TagSource::Derived {
        return Ok(());
    }
    let values = derive_values(&def, registry, facts);
    store.replace_derived_tags(item_id, dimension_id, &values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinema_domain::tagging::{DIM_DURATION, DIM_GENRE, RULE_DURATION_BUCKET};

    #[test]
    fn dimension_def_reads_builtin_config() {
        let duration = dimension_def(DIM_DURATION).expect("内置维度应可取");
        assert_eq!(duration.source, TagSource::Derived);
        assert_eq!(duration.derive_rule.as_deref(), Some(RULE_DURATION_BUCKET));

        // 非派生维度照常可取，但不会被 apply_derived 写入
        let genre = dimension_def(DIM_GENRE).expect("内置维度应可取");
        assert_eq!(genre.source, TagSource::Metadata);

        assert!(dimension_def("不存在的维度").is_none());
    }

    #[test]
    fn builtin_registry_covers_every_derived_dimension() {
        let registry = builtin_derivers();
        for def in TagDimensionDef::builtin_defaults() {
            if def.source != TagSource::Derived {
                continue;
            }
            let rule = def.derive_rule.as_deref().expect("派生维度必须声明规则");
            assert!(registry.get(rule).is_some(), "维度 {} 的规则必须有实现", def.id);
        }
    }
}
