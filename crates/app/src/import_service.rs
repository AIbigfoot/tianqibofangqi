//! L1 入库编排（PRD FR-01）。
//!
//! 分工：
//! - **L1（本模块）** 决定顺序与失败隔离：置状态 → 驱动发现 → 回填时长 → 收尾，
//!   并在入库与 `probe` 两个时机写入派生标签（架构文档 §5 的计算点）；
//! - **L4（组合根实现 [`ImportStore]`）** 只负责落库，不参与流程判断。
//!
//! 这样 L4 不需要反向依赖应用层，也不会把「扫描流程」写死在 SQL 层。
//!
//! **例外：在线站点源不做导入期探测**。对本地源，「逐条打开一次」是廉价的本地
//! IO；对在线站点源，每一条都要联网解析（实测约 5 秒/条），几十条的列表会把一次
//! 导入拖成几分钟并占住命令线程，而在线时长对「能不能播」并非必需。因此该源在
//! 导入期只做入库与标签归集，时长留待播放时按需解析。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    DiscoverStat, DiscoveredItem, ItemId, MediaItem, MediaSource, SourceId, SourceKind, SourceState,
};
use cinema_domain::ports::media_source::{DiscoverSink, MediaSourceProvider};
use cinema_domain::tagging::{
    DeriveFacts, DIM_DURATION, DIM_QUALITY, DIM_SOURCE, DIM_SUBTITLE, DIM_WATCH,
};

use crate::tag_service::{apply_derived, builtin_derivers, TagStore};

/// L1 入库所需的持久化能力。
pub trait ImportStore: TagStore + Send + Sync {
    /// 幂等写入一条候选，返回落库后的条目（已存在时返回既有条目，不产生重复行）。
    fn upsert_discovered(&self, src: &MediaSource, candidate: &DiscoveredItem) -> Result<MediaItem>;

    /// 回填 `probe` 得到的时长。
    fn set_item_duration(&self, item_id: &ItemId, duration_ms: Option<i64>) -> Result<()>;

    /// 记录源的扫描状态。
    fn set_source_state(&self, source_id: &SourceId, state: SourceState) -> Result<()>;

    /// 记录 / 清除最近一次扫描失败的原因（PRD FR-01 异常表：路径不可访问时要提示原因）。
    ///
    /// 该信息是「一次扫描的结局」，与源的身份无关，因此与状态分开写入。
    fn set_source_error(&self, source_id: &SourceId, message: Option<&str>) -> Result<()>;

    /// 该条目在某维度上是否已有标签。
    ///
    /// 入库时需要它：重扫不得把已被进度流程写成「在看 / 已看」的观看状态
    /// 回退成「未看」——「观看状态」由进度事实决定，而不是由扫描决定。
    fn has_derived_tag(&self, item_id: &ItemId, dimension_id: &str) -> Result<bool>;
}

/// 一次扫描入库的结果回执。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// 提供者提交的候选项统计（含取消标记）。
    pub stat: DiscoverStat,
    /// 实际落库的条目数（含已存在的合并条目）。
    pub stored: u64,
    /// 本次新探明时长的条目数。
    pub probed: u64,
}

/// 扫描入库编排。
pub struct ImportService;

impl ImportService {
    /// 对一个源执行一次扫描入库。
    ///
    /// 失败隔离（PRD FR-01）：某源扫描失败只把**该源**置为 `scan_failed`，
    /// 其他源与已入库条目不受影响；因此错误向上抛，但不做任何全局回滚。
    pub fn import(
        store: &dyn ImportStore,
        provider: &dyn MediaSourceProvider,
        src: &MediaSource,
    ) -> Result<ImportReport> {
        store.set_source_state(&src.id, SourceState::Scanning)?;

        let sink = DbSink::new(store, src);
        let stat = match provider.discover(src, &sink) {
            Ok(stat) => stat,
            Err(err) => {
                store.set_source_state(&src.id, SourceState::ScanFailed)?;
                // 失败原因一并落库，界面据此在「扫描失败」旁提示可解释的原因
                store.set_source_error(&src.id, Some(err.reason()))?;
                return Err(err);
            }
        };

        let stored = sink.into_stored();

        // 增量：已探明时长的条目不再重复探测，避免每次重扫都打开一遍全部文件；
        // 在线站点源整体跳过该环节（见模块文档：每条解析要联网数秒）
        let probe_during_import = src.kind != SourceKind::OnlineSite;
        let mut probed = 0u64;
        for item in &stored {
            if !probe_during_import || item.duration_ms.is_some() {
                continue;
            }
            let Ok(ref_) = provider.open(item) else {
                continue;
            };
            let Ok(facts) = provider.probe(&ref_) else {
                continue;
            };
            if facts.duration_ms.is_some() {
                store.set_item_duration(&item.id, facts.duration_ms)?;
                probed += 1;
            }
            // `probe()` 是画质 / 时长区间 / 字幕状态的**唯一计算点**（架构文档 §5）：
            // 算一次就落成标签，检索时不必重算。
            let derived_facts = DeriveFacts {
                stream: Some(&facts),
                source_kind: Some(src.kind),
                source_name: Some(src.name.as_str()),
                watched: false,
                has_progress: false,
                ..DeriveFacts::default()
            };
            for dimension in [DIM_DURATION, DIM_QUALITY, DIM_SUBTITLE] {
                apply_derived(store, builtin_derivers(), dimension, &item.id, &derived_facts)?;
            }
            store.reindex_item(&item.id)?;
        }

        // 扫描成功：清除上一次的失败原因，避免界面残留过期的诊断信息
        store.set_source_error(&src.id, None)?;
        store.set_source_state(&src.id, SourceState::Idle)?;
        Ok(ImportReport {
            stat,
            stored: stored.len() as u64,
            probed,
        })
    }
}

/// 把 `discover` 提交的候选项直接落库，并收集落库结果供后续 `probe`。
struct DbSink<'a> {
    store: &'a dyn ImportStore,
    src: &'a MediaSource,
    stored: Mutex<Vec<MediaItem>>,
    cancelled: AtomicBool,
}

impl<'a> DbSink<'a> {
    fn new(store: &'a dyn ImportStore, src: &'a MediaSource) -> Self {
        Self {
            store,
            src,
            stored: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
        }
    }

    fn into_stored(self) -> Vec<MediaItem> {
        self.stored.into_inner().unwrap_or_default()
    }
}

impl DiscoverSink for DbSink<'_> {
    fn submit(&self, item: DiscoveredItem) -> Result<()> {
        let saved = self.store.upsert_discovered(self.src, &item)?;

        // 「来源」在入库即归集：它是源的事实，与文件能否被探测无关
        let source_facts = DeriveFacts {
            stream: None,
            source_kind: Some(self.src.kind),
            source_name: Some(self.src.name.as_str()),
            watched: false,
            has_progress: false,
            ..DeriveFacts::default()
        };
        apply_derived(
            self.store,
            builtin_derivers(),
            DIM_SOURCE,
            &saved.id,
            &source_facts,
        )?;

        // 「观看状态」只在首次入库登记为「未看」：已有观看状态（在看 / 已看）由
        // 进度流程维护，重扫不得回退它。
        let fresh = !self.store.has_derived_tag(&saved.id, DIM_WATCH)?;
        if fresh {
            apply_derived(
                self.store,
                builtin_derivers(),
                DIM_WATCH,
                &saved.id,
                &DeriveFacts::default(),
            )?;
        }

        // 标签写完后重建索引，否则按标签名搜索会查不到刚归集的取值
        self.store.reindex_item(&saved.id)?;

        self.stored
            .lock()
            .map_err(|_| PortError::Backend("入库收集器不可用".into()))?
            .push(saved);
        Ok(())
    }

    /// M1 的扫描由用户手动触发且时长可控，暂无取消入口；端口仍要求可取消，
    /// 因此保留该位并在需要时由上层置位（算法不变）。
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    use cinema_adapters::media_source::LocalFolderProvider;
    use cinema_domain::model::{
        DiscoverStat as Stat, PlayableRef, SourceKind, StreamFacts,
    };
    use cinema_domain::tagging::{
        DURATION_SHORT, QUALITY_1080P, SOURCE_LOCAL, SUBTITLE_NONE, WATCH_UNSEEN,
    };

    /// 记录状态的假仓储：以 `stable_key` 去重，模拟 L4 的幂等 UPSERT。
    #[derive(Default)]
    struct FakeStore {
        items: Mutex<Vec<MediaItem>>,
        states: Mutex<Vec<SourceState>>,
        /// 写入的失败原因：`Some(原因)` 记录，`None` 表示清除。
        errors: Mutex<Vec<Option<String>>>,
        durations: Mutex<Vec<(ItemId, Option<i64>)>>,
        /// 写入的派生标签：`(条目, 维度, 取值)`。
        derived: Mutex<Vec<(String, String, Vec<String>)>>,
        /// 被要求重建索引的条目。
        reindexed: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn derived_of(&self, dimension_id: &str) -> Vec<Vec<String>> {
            self.derived
                .lock()
                .expect("锁不可中毒")
                .iter()
                .filter(|(_, dim, _)| dim == dimension_id)
                .map(|(_, _, values)| values.clone())
                .collect()
        }
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

    impl ImportStore for FakeStore {
        fn upsert_discovered(
            &self,
            src: &MediaSource,
            candidate: &DiscoveredItem,
        ) -> Result<MediaItem> {
            let mut items = self.items.lock().expect("锁不可中毒");
            if let Some(existing) = items.iter().find(|i| i.stable_key == candidate.stable_key) {
                return Ok(existing.clone());
            }
            let mut item = MediaItem::pending_fill(
                format!("it-{}", items.len() + 1),
                src.id.as_str(),
                candidate.title_hint.clone().unwrap_or_else(|| "未命名".into()),
            );
            item.stable_key = candidate.stable_key.clone();
            item.rel_path = candidate.rel_path.clone();
            item.url = candidate.url.clone();
            items.push(item.clone());
            Ok(item)
        }

        fn set_item_duration(&self, item_id: &ItemId, duration_ms: Option<i64>) -> Result<()> {
            self.durations
                .lock()
                .expect("锁不可中毒")
                .push((item_id.clone(), duration_ms));
            Ok(())
        }

        fn set_source_state(&self, _source_id: &SourceId, state: SourceState) -> Result<()> {
            self.states.lock().expect("锁不可中毒").push(state);
            Ok(())
        }

        fn set_source_error(&self, _source_id: &SourceId, message: Option<&str>) -> Result<()> {
            self.errors
                .lock()
                .expect("锁不可中毒")
                .push(message.map(str::to_owned));
            Ok(())
        }

        fn has_derived_tag(&self, item_id: &ItemId, dimension_id: &str) -> Result<bool> {
            Ok(self
                .derived
                .lock()
                .expect("锁不可中毒")
                .iter()
                .any(|(item, dim, _)| item == &item_id.0 && dim == dimension_id))
        }
    }

    /// 目录不存在，用于验证失败隔离。
    struct FailingProvider;

    impl MediaSourceProvider for FailingProvider {
        fn kind(&self) -> SourceKind {
            SourceKind::LocalFolder
        }
        fn discover(&self, _src: &MediaSource, _sink: &dyn DiscoverSink) -> Result<Stat> {
            Err(PortError::NotFound("目录不存在".into()))
        }
        fn open(&self, _item: &MediaItem) -> Result<PlayableRef> {
            Err(PortError::NotFound("无".into()))
        }
        fn probe(&self, _ref_: &PlayableRef) -> Result<StreamFacts> {
            Err(PortError::NotFound("无".into()))
        }
    }

    /// 代理本地目录发现，但 `probe` 返回确定的事实（时长 + 1080P），
    /// 用于验证时长回填与派生标签写入。
    struct DurationProvider {
        inner: LocalFolderProvider,
        duration_ms: i64,
        calls: AtomicUsize,
    }

    impl MediaSourceProvider for DurationProvider {
        fn kind(&self) -> SourceKind {
            self.inner.kind()
        }
        fn discover(&self, src: &MediaSource, sink: &dyn DiscoverSink) -> Result<Stat> {
            self.inner.discover(src, sink)
        }
        fn open(&self, item: &MediaItem) -> Result<PlayableRef> {
            self.inner.open(item)
        }
        fn probe(&self, _ref_: &PlayableRef) -> Result<StreamFacts> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(StreamFacts {
                duration_ms: Some(self.duration_ms),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            })
        }
    }

    fn temp_media_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("cinema-import-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        for name in ["A.Movie.1999.mp4", "B_Movie.mkv", "notes.txt", "poster.jpg"] {
            std::fs::write(dir.join(name), b"x").expect("写样本文件");
        }
        dir
    }

    fn local_source(location: &str) -> MediaSource {
        MediaSource {
            id: SourceId::from("src-1"),
            kind: SourceKind::LocalFolder,
            name: "本地目录".into(),
            location: location.into(),
            exclude_rules: None,
            state: SourceState::Idle,
            last_scan_at: None,
        }
    }

    #[test]
    fn scan_imports_only_video_files_and_probes_duration() {
        let dir = temp_media_dir("happy");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };

        let report = ImportService::import(
            &store,
            &provider,
            &local_source(&dir.to_string_lossy()),
        )
        .expect("扫描应成功");

        assert_eq!(report.stored, 2, "只入库视频文件，忽略 txt/jpg");
        assert_eq!(report.probed, 2, "新条目应回填时长");
        assert_eq!(
            store.states.lock().unwrap().as_slice(),
            [SourceState::Scanning, SourceState::Idle],
            "一次成功扫描应记录 scanning → idle"
        );

        let names: Vec<String> = store
            .items
            .lock()
            .unwrap()
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert!(names.iter().any(|n| n == "A.Movie.1999"), "标题来自文件名: {names:?}");
    }

    #[test]
    fn rescan_does_not_reprobe_known_duration() {
        let dir = temp_media_dir("incremental");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };

        // 第一次：2 条新条目，探测 2 次
        let first = ImportService::import(&store, &provider, &local_source(&dir.to_string_lossy()))
            .expect("首次扫描");
        assert_eq!(first.probed, 2);

        // 让假仓储返回的条目带上已探明时长，模拟重扫时读到的既有行
        {
            let mut items = store.items.lock().unwrap();
            for item in items.iter_mut() {
                item.duration_ms = Some(600_000);
            }
        }

        let calls_before = provider.calls.load(Ordering::SeqCst);
        let second = ImportService::import(&store, &provider, &local_source(&dir.to_string_lossy()))
            .expect("重扫");
        assert_eq!(second.stored, 2, "重扫应合并为同一批条目");
        assert_eq!(second.probed, 0, "已探明时长的条目不再重复探测");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            calls_before,
            "重扫不应再打开文件"
        );
    }

    #[test]
    fn online_source_skips_import_time_probe() {
        let dir = temp_media_dir("online-skip-probe");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };
        // 与本地源同样的条目，只是源类型换成了在线站点源
        let src = MediaSource {
            kind: SourceKind::OnlineSite,
            ..local_source(&dir.to_string_lossy())
        };

        let report = ImportService::import(&store, &provider, &src).expect("扫描应成功");

        assert_eq!(report.stored, 2, "条目仍应正常入库");
        assert_eq!(report.probed, 0, "在线站点源不得在导入期逐条解析时长");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "在线站点源不得在导入期打开任何一条（每条都要联网解析数秒）"
        );
    }

    #[test]
    fn failing_source_is_marked_failed_without_touching_others() {
        let store = FakeStore::default();
        let err = ImportService::import(
            &store,
            &FailingProvider,
            &local_source("Z:/__cinema_missing__"),
        );
        assert!(err.is_err(), "失败必须向上抛");
        assert_eq!(
            store.states.lock().unwrap().as_slice(),
            [SourceState::Scanning, SourceState::ScanFailed],
            "失败隔离：只置该源为 scan_failed"
        );
        assert_eq!(
            store.errors.lock().unwrap().as_slice(),
            [Some("目录不存在".to_owned())],
            "失败原因必须落库，供界面提示"
        );
        assert!(store.items.lock().unwrap().is_empty(), "失败扫描不得留下条目");
    }

    #[test]
    fn successful_rescan_clears_recorded_failure_reason() {
        let dir = temp_media_dir("clear-error");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };

        // 先记录一次失败原因（模拟上一次扫描失败留下的诊断信息）
        store
            .errors
            .lock()
            .unwrap()
            .push(Some("上次失败".to_owned()));

        ImportService::import(&store, &provider, &local_source(&dir.to_string_lossy()))
            .expect("扫描应成功");

        assert_eq!(
            store.errors.lock().unwrap().last(),
            Some(&None),
            "扫描成功后必须清除旧的失败原因"
        );
    }

    #[test]
    fn import_writes_derived_dimensions_and_rebuilds_index() {
        let dir = temp_media_dir("derive");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };

        ImportService::import(&store, &provider, &local_source(&dir.to_string_lossy()))
            .expect("扫描应成功");

        // 两个文件各归集五个「系统自动」维度
        assert_eq!(store.derived_of(DIM_SOURCE).len(), 2, "入库即归集来源");
        assert_eq!(store.derived_of(DIM_WATCH).len(), 2, "首次入库登记观看状态");
        assert_eq!(store.derived_of(DIM_DURATION).len(), 2, "probe 后归集时长区间");
        assert_eq!(store.derived_of(DIM_QUALITY).len(), 2, "probe 后归集画质");
        assert_eq!(store.derived_of(DIM_SUBTITLE).len(), 2, "probe 后归集字幕状态");

        assert_eq!(
            store.derived_of(DIM_DURATION)[0],
            vec![DURATION_SHORT.to_owned()],
            "600 秒（10 分钟）应归入短片"
        );
        assert_eq!(store.derived_of(DIM_QUALITY)[0], vec![QUALITY_1080P.to_owned()]);
        assert_eq!(store.derived_of(DIM_SUBTITLE)[0], vec![SUBTITLE_NONE.to_owned()]);
        assert_eq!(store.derived_of(DIM_WATCH)[0], vec![WATCH_UNSEEN.to_owned()]);
        assert!(
            store.derived_of(DIM_SOURCE)[0].contains(&SOURCE_LOCAL.to_owned()),
            "本地目录应归集为「本地」"
        );

        assert_eq!(
            store.reindexed.lock().unwrap().len(),
            4,
            "每条目在入库与探测后各重建一次检索索引"
        );
    }

    #[test]
    fn rescan_keeps_existing_watch_state() {
        let dir = temp_media_dir("watch");
        let store = FakeStore::default();
        let provider = DurationProvider {
            inner: LocalFolderProvider::new(&dir),
            duration_ms: 600_000,
            calls: AtomicUsize::new(0),
        };
        let src = local_source(&dir.to_string_lossy());

        ImportService::import(&store, &provider, &src).expect("首次扫描");

        let watch_writes = |item_id: &str| {
            store
                .derived
                .lock()
                .unwrap()
                .iter()
                .filter(|(item, dim, _)| item == item_id && dim == DIM_WATCH)
                .count()
        };
        assert_eq!(watch_writes("it-1"), 1, "首次扫描登记一次观看状态");

        ImportService::import(&store, &provider, &src).expect("重扫");

        assert_eq!(
            watch_writes("it-1"),
            1,
            "重扫不得再写观看状态，避免把「在看 / 已看」回退成「未看」"
        );
    }
}
