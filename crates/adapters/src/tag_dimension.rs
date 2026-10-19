//! P-05 实现：配置驱动的标签维度注册表。

use cinema_domain::error::{PortError, Result};
use cinema_domain::ports::tag_dimension::{TagDimensionDef, TagDimensionRegistry};

/// 配置驱动的注册表。
///
/// `new()` 即「空实现」（零维度）；`with_builtin_defaults()` 装入文档示例维度。
/// 新增维度只需调用 `register`，检索组合逻辑（同维度 OR、跨维度 AND）
/// 已在 L2 [`cinema_domain::rules::matches_filters`] 实现一次，无需改动。
#[derive(Debug, Clone, Default)]
pub struct ConfigTagDimensionRegistry {
    defs: Vec<TagDimensionDef>,
}

impl ConfigTagDimensionRegistry {
    /// 空实现：不登记任何维度。
    pub fn new() -> Self {
        Self { defs: Vec::new() }
    }

    /// 装入内置默认维度（架构文档 P-05 的 `genre` 示例）。
    pub fn with_builtin_defaults() -> Self {
        let mut registry = Self::new();
        registry
            .register(TagDimensionDef::genre())
            .expect("内置维度 id 不应重复");
        registry
    }
}

impl TagDimensionRegistry for ConfigTagDimensionRegistry {
    fn dimensions(&self) -> Vec<TagDimensionDef> {
        let mut defs = self.defs.clone();
        defs.sort_by_key(|d| d.order);
        defs
    }

    fn get(&self, id: &str) -> Option<TagDimensionDef> {
        self.defs.iter().find(|d| d.id == id).cloned()
    }

    fn register(&mut self, def: TagDimensionDef) -> Result<()> {
        if def.id.trim().is_empty() {
            return Err(PortError::Unsupported("维度 id 不得为空".into()));
        }
        if self.defs.iter().any(|d| d.id == def.id) {
            return Err(PortError::Unsupported(format!("维度 id 重复: {}", def.id)));
        }
        self.defs.push(def);
        Ok(())
    }
}
