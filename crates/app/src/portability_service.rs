//! L1 数据可迁移编排（PRD FR-07）。
//!
//! 分工与前几个服务一致：**L1 决定「能不能导、导完还要补什么」，L4 只负责「怎么读写包」**。
//! 具体地，本层只多做两件流程判断：
//! 1. **版本兼容**：包内登记的 schema 版本高于本机时必须拒绝并提示，不得静默改写
//!    （前进式迁移禁止跳版，规则见 [`cinema_domain::rules::package_compatibility`]）；
//! 2. **派生数据重建**：进度还原后，「观看状态」这一派生维度必须按同一份派生规则重算，
//!    否则「已看」会在检索区丢失。重算放在 [`import_package`] **返回之后**执行，
//!    避免在组合根持有库锁的情况下再嵌套取锁。
//!
//! 导出包内的**用户数据**由 L4 写好后，本层不再加工。

use cinema_domain::error::{PortError, Result};
use cinema_domain::portability::{ExportOutcome, ImportOutcome};
use cinema_domain::rules::{package_compatibility, PackageCompatibility};
use cinema_domain::tagging::{DeriveFacts, DIM_WATCH};

use crate::tag_service::{apply_derived, builtin_derivers, TagStore};

/// L1 数据可迁移所需的持久化能力（由组合根实现）。
pub trait PortabilityStore: TagStore + Send + Sync {
    /// 把库内用户数据导出到目录。
    fn export_package(&self, dest: &str) -> Result<ExportOutcome>;

    /// 读出导出包登记的 schema 版本（只读一个字段，不解析整包）。
    fn packaged_schema_version(&self, src: &str) -> Result<i64>;

    /// 把导出包合并进本机库（单事务、失败回滚）。
    fn import_package(&self, src: &str) -> Result<ImportOutcome>;

    /// 本机库当前的 schema 版本。
    fn local_schema_version(&self) -> Result<i64>;
}

/// 数据可迁移编排。
pub struct PortabilityService;

impl PortabilityService {
    /// 导出当前库的用户数据。目录由 L0 选定。
    pub fn export(store: &dyn PortabilityStore, dest: &str) -> Result<ExportOutcome> {
        store.export_package(dest)
    }

    /// 导入一个导出包。
    ///
    /// 导入前先做**前进式版本判定**：包版本高于本机即拒绝（提示升级应用），
    /// 同版本或更旧版本照常导入；导入成功后按还原的进度事实重算「观看状态」。
    pub fn import(store: &dyn PortabilityStore, src: &str) -> Result<ImportOutcome> {
        let packaged = store.packaged_schema_version(src)?;
        let local = store.local_schema_version()?;
        if package_compatibility(packaged, local) == PackageCompatibility::Newer {
            return Err(PortError::Unsupported(format!(
                "导出包来自更新的应用版本（包 v{packaged} > 本机 v{local}），请先升级应用再导入"
            )));
        }

        let outcome = store.import_package(src)?;

        // 观看状态是派生维度：进度与「已看」已由 L4 还原，这里按同一份规则重算取值
        for entry in &outcome.restored_progress {
            let facts = DeriveFacts {
                watched: entry.watched,
                has_progress: true,
                ..DeriveFacts::default()
            };
            apply_derived(store, builtin_derivers(), DIM_WATCH, &entry.item_id, &facts)?;
            store.reindex_item(&entry.item_id)?;
        }

        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use cinema_domain::model::ItemId;
    use cinema_domain::portability::RestoredProgress;

    /// 内存假仓储：只记录调用与返回，验证 L1 的流程判断（不落库）。
    #[derive(Default)]
    struct FakeStore {
        packaged: i64,
        local: i64,
        /// `import_package` 被调用的次数。
        imports: Mutex<usize>,
        /// `import_package` 返回时附带的已还原进度。
        restored: Vec<RestoredProgress>,
        /// 写入的派生标签：`(条目, 维度, 取值)`。
        derived: Mutex<Vec<(String, String, Vec<String>)>>,
        /// 被要求重建索引的条目。
        reindexed: Mutex<Vec<String>>,
    }

    impl TagStore for FakeStore {
        fn replace_derived_tags(
            &self,
            item_id: &ItemId,
            dimension_id: &str,
            values: &[String],
        ) -> Result<()> {
            self.derived.lock().expect("锁不可中毒").push((
                item_id.0.clone(),
                dimension_id.to_owned(),
                values.to_vec(),
            ));
            Ok(())
        }

        fn reindex_item(&self, item_id: &ItemId) -> Result<()> {
            self.reindexed
                .lock()
                .expect("锁不可中毒")
                .push(item_id.0.clone());
            Ok(())
        }
    }

    impl PortabilityStore for FakeStore {
        fn export_package(&self, _dest: &str) -> Result<ExportOutcome> {
            Ok(ExportOutcome::default())
        }

        fn packaged_schema_version(&self, _src: &str) -> Result<i64> {
            Ok(self.packaged)
        }

        fn import_package(&self, _src: &str) -> Result<ImportOutcome> {
            *self.imports.lock().expect("锁不可中毒") += 1;
            Ok(ImportOutcome {
                restored_progress: self.restored.clone(),
                ..ImportOutcome::default()
            })
        }

        fn local_schema_version(&self) -> Result<i64> {
            Ok(self.local)
        }
    }

    fn store_with(packaged: i64, local: i64) -> FakeStore {
        FakeStore {
            packaged,
            local,
            ..Default::default()
        }
    }

    #[test]
    fn newer_package_is_rejected_without_touching_the_library() {
        let store = store_with(3, 2);
        let err = PortabilityService::import(&store, "包目录").expect_err("包版本更高必须拒绝");
        assert!(
            err.reason().contains("升级"),
            "提示应引导用户升级应用：{}",
            err.reason()
        );
        assert_eq!(
            *store.imports.lock().unwrap(),
            0,
            "拒绝时不得读取或改写库内容"
        );
    }

    #[test]
    fn same_and_older_packages_are_imported() {
        for (packaged, local) in [(2, 2), (1, 2)] {
            let store = store_with(packaged, local);
            PortabilityService::import(&store, "包目录").expect("同版本 / 更旧版本应可导入");
            assert_eq!(*store.imports.lock().unwrap(), 1);
        }
    }

    #[test]
    fn import_recomputes_watch_state_from_restored_progress() {
        let mut store = store_with(2, 2);
        store.restored = vec![
            RestoredProgress {
                item_id: ItemId::from("it-watched"),
                watched: true,
            },
            RestoredProgress {
                item_id: ItemId::from("it-watching"),
                watched: false,
            },
        ];

        PortabilityService::import(&store, "包目录").expect("导入");

        let writes = store.derived.lock().expect("锁不可中毒").clone();
        assert_eq!(writes.len(), 2, "每条还原进度的条目都要重算观看状态");
        assert_eq!(writes[0].1, DIM_WATCH, "只重算观看状态维度");
        assert_eq!(writes[0].2, vec!["已看".to_owned()], "落库时 watched 即为「已看」");
        assert_eq!(
            writes[1].2,
            vec!["在看".to_owned()],
            "有进度但未看完应重算为「在看」"
        );
        assert_eq!(
            store.reindexed.lock().unwrap().len(),
            2,
            "观看状态变化后必须重建检索索引"
        );
    }
}
