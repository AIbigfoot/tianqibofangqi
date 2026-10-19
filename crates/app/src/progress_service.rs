//! L1 进度编排（PRD FR-04「进度记忆与续播」）。
//!
//! 分工与 [`crate::import_service`] 一致：**L1 决定「何时写」，L4 只负责「怎么写」**。
//! 三条判定门槛（30 秒 / 2% / 95%）全部来自 L2 [`ResumePolicy`]，
//! 本层不复制任何规则，避免「换个界面就漂移」。
//!
//! 进度变化同时是「观看状态」维度的归集时机（架构文档 §5）：达 95% → 已看，
//! 有有效进度 → 在看。该写入同样走 [`apply_derived`]，取值规则不在本层硬编码。

use cinema_domain::error::Result;
use cinema_domain::model::{ItemId, Progress};
use cinema_domain::rules::{ResumeOffer, ResumePolicy};
use cinema_domain::tagging::{DeriveFacts, DIM_WATCH};

use crate::tag_service::{apply_derived, builtin_derivers, TagStore};

/// L1 进度所需的持久化能力。
pub trait ProgressStore: TagStore + Send + Sync {
    /// 覆盖写入某条目的进度。
    fn put(&self, item_id: &ItemId, progress: Progress) -> Result<()>;

    /// 读取某条目的进度；从未播放过返回 `None`。
    fn get(&self, item_id: &ItemId) -> Result<Option<Progress>>;

    /// 标记「已看」（PRD FR-04：达 95% 视为已看，之后不再提示续播）。
    fn set_watched(&self, item_id: &ItemId, watched: bool) -> Result<()>;
}

/// 进度编排。
pub struct ProgressService;

impl ProgressService {
    /// 播放中上报位置。
    ///
    /// 只有满足记录门槛（超过 30 秒 **且** 超过总时长 2%）才落库，
    /// 返回本次是否写入 —— UI 可据此判断「本条目尚无有效进度」。
    pub fn report(
        store: &dyn ProgressStore,
        item_id: &ItemId,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<bool> {
        if !ResumePolicy::should_record(position_ms, duration_ms) {
            return Ok(false);
        }
        store.put(
            item_id,
            Progress {
                position_ms,
                duration_ms,
            },
        )?;
        // 观看状态跟着进度事实走：达 95% 记「已看」，否则有有效进度即「在看」
        write_watch_state(store, item_id, ResumePolicy::is_watched(position_ms, duration_ms))?;
        Ok(true)
    }

    /// 打开条目时给出续播提示；无进度 / 太靠前 / 已看三种情况都返回 `None`。
    pub fn resume_point(
        store: &dyn ProgressStore,
        item_id: &ItemId,
        duration_ms: Option<i64>,
    ) -> Result<Option<ResumeOffer>> {
        let saved = store.get(item_id)?;
        // 时长的回退顺序（本次探测 → 历史记录）由 L2 规则统一处理，本层不重复
        Ok(ResumePolicy::resume_offer(saved, duration_ms))
    }

    /// 播放收尾（关闭窗口 / 退出播放页时调用）。
    ///
    /// 达到 95% 视为已看并登记，避免下次继续提示「续播」。
    pub fn finish(
        store: &dyn ProgressStore,
        item_id: &ItemId,
        position_ms: i64,
        duration_ms: Option<i64>,
    ) -> Result<()> {
        let recorded = Self::report(store, item_id, position_ms, duration_ms)?;
        if recorded && ResumePolicy::is_watched(position_ms, duration_ms) {
            store.set_watched(item_id, true)?;
        }
        Ok(())
    }
}

/// 把观看状态归集到「观看状态」维度，并让检索索引跟上标签变化。
fn write_watch_state(store: &dyn ProgressStore, item_id: &ItemId, watched: bool) -> Result<()> {
    let facts = DeriveFacts {
        watched,
        has_progress: true,
        ..DeriveFacts::default()
    };
    apply_derived(store, builtin_derivers(), DIM_WATCH, item_id, &facts)?;
    store.reindex_item(item_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use cinema_domain::model::Progress as P;
    use cinema_domain::tagging::{WATCH_WATCHED, WATCH_WATCHING};

    /// 内存假仓储：模拟 `playback_state` 表的一行 + 派生标签写入。
    #[derive(Default)]
    struct FakeStore {
        row: Mutex<(Option<Progress>, bool)>,
        /// 写入的派生标签：`(维度, 取值)`。
        derived: Mutex<Vec<(String, Vec<String>)>>,
        /// 被要求重建索引的次数。
        reindexed: Mutex<usize>,
    }

    impl FakeStore {
        fn seeded(position_ms: i64, duration_ms: Option<i64>) -> Self {
            Self {
                row: Mutex::new((
                    Some(P {
                        position_ms,
                        duration_ms,
                    }),
                    false,
                )),
                ..Default::default()
            }
        }

        fn row(&self) -> (Option<Progress>, bool) {
            *self.row.lock().expect("锁不可中毒")
        }

        fn watch_values(&self) -> Vec<Vec<String>> {
            self.derived
                .lock()
                .expect("锁不可中毒")
                .iter()
                .filter(|(dim, _)| dim == DIM_WATCH)
                .map(|(_, values)| values.clone())
                .collect()
        }
    }

    impl TagStore for FakeStore {
        fn replace_derived_tags(
            &self,
            _item_id: &ItemId,
            dimension_id: &str,
            values: &[String],
        ) -> Result<()> {
            self.derived
                .lock()
                .expect("锁不可中毒")
                .push((dimension_id.to_owned(), values.to_vec()));
            Ok(())
        }

        fn reindex_item(&self, _item_id: &ItemId) -> Result<()> {
            *self.reindexed.lock().expect("锁不可中毒") += 1;
            Ok(())
        }
    }

    impl ProgressStore for FakeStore {
        fn put(&self, _item_id: &ItemId, progress: Progress) -> Result<()> {
            self.row.lock().expect("锁不可中毒").0 = Some(progress);
            Ok(())
        }

        fn get(&self, _item_id: &ItemId) -> Result<Option<Progress>> {
            Ok(self.row.lock().expect("锁不可中毒").0)
        }

        fn set_watched(&self, _item_id: &ItemId, watched: bool) -> Result<()> {
            self.row.lock().expect("锁不可中毒").1 = watched;
            Ok(())
        }
    }

    fn item() -> ItemId {
        ItemId::from("it-1")
    }

    const HOUR: i64 = 3_600_000;

    #[test]
    fn progress_is_not_recorded_before_thirty_seconds() {
        let store = FakeStore::default();
        let wrote = ProgressService::report(&store, &item(), 20_000, Some(HOUR)).expect("上报");
        assert!(!wrote, "不到 30 秒不得记录");
        assert_eq!(store.row().0, None, "太靠前的进度不得落库");
    }

    #[test]
    fn progress_is_not_recorded_within_first_two_percent() {
        let store = FakeStore::default();
        // 66 秒 / 3600 秒 ≈ 1.8%，低于 2%
        let wrote = ProgressService::report(&store, &item(), 66_000, Some(HOUR)).expect("上报");
        assert!(!wrote, "未超过总时长 2% 不得记录");
    }

    #[test]
    fn progress_is_recorded_past_both_thresholds() {
        let store = FakeStore::default();
        let wrote = ProgressService::report(&store, &item(), 300_000, Some(HOUR)).expect("上报");
        assert!(wrote, "超过 30 秒且超过 2% 必须记录");
        assert_eq!(store.row().0.expect("应已落库").position_ms, 300_000);
    }

    #[test]
    fn no_duration_means_no_recording() {
        let store = FakeStore::default();
        // 总时长未知时无法判断 2%，按 PRD 不记录
        let wrote = ProgressService::report(&store, &item(), 300_000, None).expect("上报");
        assert!(!wrote);
    }

    #[test]
    fn resume_point_is_offered_with_percent() {
        let store = FakeStore::seeded(1_800_000, Some(HOUR));
        let offer = ProgressService::resume_point(&store, &item(), Some(HOUR))
            .expect("读取")
            .expect("中途退出必须提供续播");
        assert_eq!(offer.position_ms, 1_800_000);
        assert_eq!(offer.percent, 50, "1800s / 3600s = 50%");
    }

    #[test]
    fn resume_point_is_suppressed_when_watched_or_early() {
        // 已看：达 95%
        let watched = FakeStore::seeded(3_500_000, Some(HOUR));
        assert_eq!(
            ProgressService::resume_point(&watched, &item(), Some(HOUR)).expect("读取"),
            None,
            "达 95% 视为已看，不再提示续播"
        );

        // 太靠前：仅有 20 秒
        let early = FakeStore::seeded(20_000, Some(HOUR));
        assert_eq!(
            ProgressService::resume_point(&early, &item(), Some(HOUR)).expect("读取"),
            None,
            "太靠前的进度不提示续播"
        );

        // 从未播放
        let fresh = FakeStore::default();
        assert_eq!(
            ProgressService::resume_point(&fresh, &item(), Some(HOUR)).expect("读取"),
            None
        );
    }

    #[test]
    fn finish_marks_watched_only_past_ninety_five_percent() {
        let store = FakeStore::default();
        ProgressService::finish(&store, &item(), 3_500_000, Some(HOUR)).expect("收尾");
        assert!(store.row().1, "达 95% 必须登记为已看");

        let store = FakeStore::default();
        ProgressService::finish(&store, &item(), 1_800_000, Some(HOUR)).expect("收尾");
        assert!(!store.row().1, "中途退出不得标记已看");
        assert_eq!(
            store.row().0.expect("应已落库").position_ms,
            1_800_000,
            "中途退出应留下可续播的进度"
        );
    }

    #[test]
    fn progress_updates_watch_state_and_rebuilds_index() {
        let store = FakeStore::default();
        ProgressService::report(&store, &item(), 300_000, Some(HOUR)).expect("上报");
        assert_eq!(
            store.watch_values(),
            vec![vec![WATCH_WATCHING.to_owned()]],
            "有有效进度即归入「在看」"
        );

        ProgressService::finish(&store, &item(), 3_500_000, Some(HOUR)).expect("收尾");
        assert_eq!(
            store.watch_values().last().expect("应有取值"),
            &vec![WATCH_WATCHED.to_owned()],
            "达 95% 应归入「已看」"
        );
        assert!(
            *store.reindexed.lock().expect("锁不可中毒") >= 2,
            "观看状态变化后必须重建检索索引"
        );
    }

    #[test]
    fn unreported_progress_writes_no_watch_state() {
        let store = FakeStore::default();
        ProgressService::report(&store, &item(), 20_000, Some(HOUR)).expect("上报");
        assert!(
            store.watch_values().is_empty(),
            "未达记录门槛不得写入观看状态"
        );
    }
}
