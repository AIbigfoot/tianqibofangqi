//! L1 持久化能力的实现：`ImportStore` + `ProgressStore` + `TagStore` + `SearchStore`。
//!
//! 这些 trait 由 L1（`cinema-app`）声明能力、由**组合根**提供实现，
//! 因此 L4（`cinema-infra`）不需要反向依赖应用层；仓储只做 SQL，流程判断留在 L1。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cinema_app::import_service::ImportStore;
use cinema_app::portability_service::PortabilityStore;
use cinema_app::progress_service::ProgressStore;
use cinema_app::search_service::{ProgressState, SearchStore};
use cinema_app::subtitle_service::SubtitleStore;
use cinema_app::tag_service::TagStore;
use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    DiscoveredItem, ItemId, MediaItem, MediaSource, Progress, SourceId, SourceState, SubtitleKind,
    SubtitleTrack,
};
use cinema_domain::ports::tag_dimension::TagDimensionDef;
use cinema_domain::portability::{ExportOutcome, ImportOutcome};
use cinema_domain::rules::ItemTags;
use cinema_domain::tagging::TagUsage;
use cinema_infra::db::migration::MigrationRunner;
use cinema_infra::db::Database;
use cinema_infra::error::InfraError;
use cinema_infra::repo::{MediaItemRepo, MediaSourceRepo, PlaybackRepo};
use cinema_infra::subtitle_repo::{SettingRepo, SubtitleTrackRepo};
use cinema_infra::tag_repo::{DimensionRepo, ItemTagRepo, SearchIndexRepo, TagRepo};

/// 把 L4 错误折叠成端口错误：领域错误原样上抛，其余归为 `Backend`。
fn port_err(err: InfraError) -> PortError {
    match err {
        InfraError::Domain(p) => p,
        other => PortError::Backend(other.to_string()),
    }
}

/// 由「源 + 稳定键」派生条目 id：同一物理文件任意次扫描都得到同一 id，
/// 避免重复扫描在 `media_item.id` 主键上互相冲突。
fn item_id_for(stable_key: &str) -> ItemId {
    // FNV-1a 64 位
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in stable_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ItemId(format!("it-{hash:016x}"))
}

/// SQLite 支撑的入库 / 进度仓储。
pub struct DbStore {
    db: Arc<Mutex<Database>>,
    /// 海报缓存目录（导出时若有海报则一并复制；当前无元数据源，通常为空）。
    poster_dir: Option<PathBuf>,
}

impl DbStore {
    /// 共享同一份数据库句柄（组合根的其他部分也要用它读海报墙）。
    pub fn shared(db: Arc<Mutex<Database>>, poster_dir: Option<PathBuf>) -> Self {
        Self { db, poster_dir }
    }

    fn with<T>(&self, f: impl FnOnce(&Database) -> Result<T>) -> Result<T> {
        let guard = self
            .db
            .lock()
            .map_err(|_| PortError::Backend("数据库锁不可用".into()))?;
        f(&guard)
    }
}

impl TagStore for DbStore {
    fn replace_derived_tags(
        &self,
        item_id: &ItemId,
        dimension_id: &str,
        values: &[String],
    ) -> Result<()> {
        self.with(|db| {
            ItemTagRepo::replace_derived(db.conn(), item_id, dimension_id, values).map_err(port_err)
        })
    }

    fn reindex_item(&self, item_id: &ItemId) -> Result<()> {
        self.with(|db| SearchIndexRepo::reindex_item(db.conn(), item_id).map_err(port_err))
    }
}

impl ImportStore for DbStore {
    fn upsert_discovered(
        &self,
        src: &MediaSource,
        candidate: &DiscoveredItem,
    ) -> Result<MediaItem> {
        self.with(|db| {
            let conn = db.conn();
            // 外键要求源先存在；重复写入同一源是幂等的
            MediaSourceRepo::upsert(conn, src).map_err(port_err)?;

            let mut item = MediaItem::pending_fill(
                item_id_for(&candidate.stable_key),
                src.id.clone(),
                candidate
                    .title_hint
                    .clone()
                    .unwrap_or_else(|| "未命名".into()),
            );
            item.stable_key = candidate.stable_key.clone();
            item.rel_path = candidate.rel_path.clone();
            item.url = candidate.url.clone();
            // 海报引用（在线站点源在发现时登记已缓存的封面文件名；本地源为 None）
            item.poster_id = candidate.poster_ref.clone();

            let id = MediaItemRepo::upsert(conn, &item).map_err(port_err)?;
            MediaItemRepo::get(conn, &id)
                .map_err(port_err)?
                .ok_or_else(|| PortError::Backend(format!("条目 {id} 写入后读取失败")))
        })
    }

    fn set_item_duration(&self, item_id: &ItemId, duration_ms: Option<i64>) -> Result<()> {
        self.with(|db| {
            MediaItemRepo::set_duration(db.conn(), item_id, duration_ms).map_err(port_err)
        })
    }

    fn set_source_state(&self, source_id: &SourceId, state: SourceState) -> Result<()> {
        self.with(|db| {
            MediaSourceRepo::set_scan_result(
                db.conn(),
                source_id,
                state,
                cinema_infra::now_ms(),
            )
            .map_err(port_err)
        })
    }

    fn set_source_error(&self, source_id: &SourceId, message: Option<&str>) -> Result<()> {
        self.with(|db| MediaSourceRepo::set_last_error(db.conn(), source_id, message).map_err(port_err))
    }

    fn has_derived_tag(&self, item_id: &ItemId, dimension_id: &str) -> Result<bool> {
        self.with(|db| {
            ItemTagRepo::has_on_dimension(db.conn(), item_id, dimension_id).map_err(port_err)
        })
    }
}

impl ProgressStore for DbStore {
    fn put(&self, item_id: &ItemId, progress: Progress) -> Result<()> {
        self.with(|db| PlaybackRepo::put(db.conn(), item_id, progress).map_err(port_err))
    }

    fn get(&self, item_id: &ItemId) -> Result<Option<Progress>> {
        self.with(|db| PlaybackRepo::get(db.conn(), item_id).map_err(port_err))
    }

    fn set_watched(&self, item_id: &ItemId, watched: bool) -> Result<()> {
        self.with(|db| PlaybackRepo::set_watched(db.conn(), item_id, watched).map_err(port_err))
    }
}

impl SearchStore for DbStore {
    fn dimensions(&self) -> Result<Vec<TagDimensionDef>> {
        self.with(|db| DimensionRepo::list(db.conn()).map_err(port_err))
    }

    fn tags_of_dimension(&self, dimension_id: &str) -> Result<Vec<TagUsage>> {
        self.with(|db| TagRepo::usage_by_dimension(db.conn(), dimension_id).map_err(port_err))
    }

    fn items(&self) -> Result<Vec<MediaItem>> {
        self.with(|db| MediaItemRepo::list(db.conn(), None).map_err(port_err))
    }

    fn all_item_tags(&self) -> Result<BTreeMap<String, ItemTags>> {
        self.with(|db| ItemTagRepo::all(db.conn()).map_err(port_err))
    }

    fn match_ids(&self, text: &str) -> Result<Vec<ItemId>> {
        self.with(|db| SearchIndexRepo::match_ids(db.conn(), text).map_err(port_err))
    }

    fn progress_state(&self, item_id: &ItemId) -> Result<ProgressState> {
        self.with(|db| {
            let progress = PlaybackRepo::get(db.conn(), item_id).map_err(port_err)?;
            let watched = PlaybackRepo::is_watched(db.conn(), item_id).map_err(port_err)?;
            let last_watched_at =
                PlaybackRepo::last_updated_at(db.conn(), item_id).map_err(port_err)?;
            Ok(ProgressState {
                watched,
                // 有行即有有效进度：L1 只会写入通过门槛的进度
                has_progress: progress.is_some(),
                last_watched_at,
                position_ms: progress.map(|p| p.position_ms).unwrap_or(0),
                duration_ms: progress.and_then(|p| p.duration_ms),
            })
        })
    }
}

impl SubtitleStore for DbStore {
    fn save_track(&self, track: &SubtitleTrack) -> Result<()> {
        self.with(|db| SubtitleTrackRepo::upsert(db.conn(), track).map_err(port_err))
    }

    fn delete_tracks_of_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<()> {
        self.with(|db| {
            // 先列后删：仓储只提供按 id 删除，类型过滤留在本层
            let tracks = SubtitleTrackRepo::list_by_item(db.conn(), item_id).map_err(port_err)?;
            for track in tracks.into_iter().filter(|t| t.kind == kind) {
                SubtitleTrackRepo::delete(db.conn(), &track.id).map_err(port_err)?;
            }
            Ok(())
        })
    }

    fn list_tracks(&self, item_id: &ItemId) -> Result<Vec<SubtitleTrack>> {
        self.with(|db| SubtitleTrackRepo::list_by_item(db.conn(), item_id).map_err(port_err))
    }

    fn has_track_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<bool> {
        self.with(|db| SubtitleTrackRepo::exists_kind(db.conn(), item_id, kind).map_err(port_err))
    }

    fn setting(&self, key: &str) -> Result<Option<String>> {
        self.with(|db| SettingRepo::get(db.conn(), key).map_err(port_err))
    }

    fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with(|db| SettingRepo::set(db.conn(), key, value).map_err(port_err))
    }
}

impl PortabilityStore for DbStore {
    fn export_package(&self, dest: &str) -> Result<ExportOutcome> {
        let poster_dir = self.poster_dir.clone();
        self.with(|db| {
            cinema_infra::portability::export_package(db.conn(), Path::new(dest), poster_dir.as_deref())
                .map_err(port_err)
        })
    }

    fn packaged_schema_version(&self, src: &str) -> Result<i64> {
        self.with(|_db| {
            cinema_infra::portability::packaged_schema_version(Path::new(src)).map_err(port_err)
        })
    }

    fn import_package(&self, src: &str) -> Result<ImportOutcome> {
        let poster_dir = self.poster_dir.clone();
        self.with(|db| {
            cinema_infra::portability::import_package(db.conn(), Path::new(src), poster_dir.as_deref())
                .map_err(port_err)
        })
    }

    fn local_schema_version(&self) -> Result<i64> {
        self.with(|db| MigrationRunner::current_version(db.conn()).map_err(port_err))
    }
}
