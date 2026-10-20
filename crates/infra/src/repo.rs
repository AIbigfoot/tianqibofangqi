//! L4 仓储：媒体源 / 条目 / 观看进度的读写。
//!
//! 写入一律幂等：条目以 `stable_key`（源 + 规范化相对路径 / URL）为稳定键 UPSERT，
//! 重复扫描不会产生重复行，也不会覆盖用户数据（标题 / 年份 / 海报 / 锁定字段）。
//! 这正是 PRD FR-01「同一物理文件被重复扫描时合并为一条条目」的落点。

use rusqlite::{params, Connection, OptionalExtension, Row};

use cinema_domain::model::{
    IdentifyState, ItemId, MediaItem, MediaSource, MetaState, Progress, SourceId, SourceKind,
    SourceState,
};

use crate::error::Result;
use crate::now_ms;

// ---------------------------------------------------------------------------
// 枚举 ↔ 数据库文本
// ---------------------------------------------------------------------------

fn kind_to_str(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::LocalFolder => "local_folder",
        SourceKind::LanPath => "lan_path",
        SourceKind::DirectUrl => "direct_url",
        SourceKind::CloudDrive => "cloud_drive",
        SourceKind::OnlineSite => "online_site",
    }
}

/// 未知取值不猜测：回落到本地目录，避免因历史脏数据让整个库读不出来。
fn kind_from_str(raw: &str) -> SourceKind {
    match raw {
        "lan_path" => SourceKind::LanPath,
        "direct_url" => SourceKind::DirectUrl,
        "cloud_drive" => SourceKind::CloudDrive,
        "online_site" => SourceKind::OnlineSite,
        _ => SourceKind::LocalFolder,
    }
}

fn source_state_to_str(state: SourceState) -> &'static str {
    match state {
        SourceState::Idle => "idle",
        SourceState::Scanning => "scanning",
        SourceState::ScanFailed => "scan_failed",
    }
}

fn source_state_from_str(raw: &str) -> SourceState {
    match raw {
        "scanning" => SourceState::Scanning,
        "scan_failed" => SourceState::ScanFailed,
        _ => SourceState::Idle,
    }
}

fn identify_to_str(state: IdentifyState) -> &'static str {
    match state {
        IdentifyState::Identified => "identified",
        IdentifyState::Pending => "pending",
    }
}

fn identify_from_str(raw: &str) -> IdentifyState {
    match raw {
        "identified" => IdentifyState::Identified,
        _ => IdentifyState::Pending,
    }
}

fn meta_to_str(state: MetaState) -> &'static str {
    match state {
        MetaState::Complete => "complete",
        MetaState::PendingFill => "pending_fill",
    }
}

fn meta_from_str(raw: &str) -> MetaState {
    match raw {
        "complete" => MetaState::Complete,
        _ => MetaState::PendingFill,
    }
}

// ---------------------------------------------------------------------------
// media_source
// ---------------------------------------------------------------------------

/// 媒体源仓储。
pub struct MediaSourceRepo;

impl MediaSourceRepo {
    /// 新增或覆盖一条源（按 id）。同名同路径重复添加不产生第二行。
    pub fn upsert(conn: &Connection, src: &MediaSource) -> Result<()> {
        conn.execute(
            "INSERT INTO media_source (id, kind, name, location, exclude_rules, state, last_scan_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 kind = excluded.kind,
                 name = excluded.name,
                 location = excluded.location,
                 exclude_rules = excluded.exclude_rules,
                 state = excluded.state,
                 last_scan_at = excluded.last_scan_at",
            params![
                src.id.as_str(),
                kind_to_str(src.kind),
                src.name,
                src.location,
                src.exclude_rules,
                source_state_to_str(src.state),
                src.last_scan_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &SourceId) -> Result<Option<MediaSource>> {
        let found = conn
            .query_row(
                "SELECT id, kind, name, location, exclude_rules, state, last_scan_at
                 FROM media_source WHERE id = ?1",
                params![id.as_str()],
                read_source,
            )
            .optional()?;
        Ok(found)
    }

    /// 全部源，按创建顺序（SQLite 无隐式顺序，这里按 id 稳定排序）。
    pub fn list(conn: &Connection) -> Result<Vec<MediaSource>> {
        let mut stmt = conn.prepare(
            "SELECT id, kind, name, location, exclude_rules, state, last_scan_at
             FROM media_source ORDER BY id",
        )?;
        let rows = stmt.query_map(params![], read_source)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 记录一次扫描的结局。某源失败只置该源为 `scan_failed`，不影响其他源。
    pub fn set_scan_result(
        conn: &Connection,
        id: &SourceId,
        state: SourceState,
        scanned_at: i64,
    ) -> Result<()> {
        conn.execute(
            "UPDATE media_source SET state = ?2, last_scan_at = ?3 WHERE id = ?1",
            params![id.as_str(), source_state_to_str(state), scanned_at],
        )?;
        Ok(())
    }

    /// 记录 / 清除「最近一次扫描失败原因」（PRD FR-01 异常表：路径不可访问时要提示原因）。
    ///
    /// 该列不在 [`MediaSource`] 模型里：原因属于「一次扫描的结局」，与源的身份无关，
    /// 因此单独读写，避免为了一个诊断字段改动领域模型。
    pub fn set_last_error(conn: &Connection, id: &SourceId, message: Option<&str>) -> Result<()> {
        conn.execute(
            "UPDATE media_source SET last_error = ?2 WHERE id = ?1",
            params![id.as_str(), message],
        )?;
        Ok(())
    }

    pub fn last_error(conn: &Connection, id: &SourceId) -> Result<Option<String>> {
        let found: Option<Option<String>> = conn
            .query_row(
                "SELECT last_error FROM media_source WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.flatten())
    }

    /// 移除源及其全部条目（库内记录），返回被清掉的条目数。
    ///
    /// 条目由外键 `ON DELETE CASCADE` 一并移除；`item_fts` 没有触发器，
    /// 因此先按源删索引、再删源，并放在同一事务里。
    /// **只删库内记录，不动磁盘文件**。「保留用户数据并标记文件缺失」的路径由 L1 决定，
    /// 不在仓储内隐式处理。
    pub fn remove(conn: &Connection, id: &SourceId) -> Result<usize> {
        let tx = conn.unchecked_transaction()?;
        let items = tx.execute(
            "DELETE FROM item_fts WHERE item_id IN (SELECT id FROM media_item WHERE source_id = ?1)",
            params![id.as_str()],
        )?;
        tx.execute("DELETE FROM media_source WHERE id = ?1", params![id.as_str()])?;
        tx.commit()?;
        Ok(items)
    }
}

fn read_source(row: &Row<'_>) -> rusqlite::Result<MediaSource> {
    let kind: String = row.get(1)?;
    let state: String = row.get(5)?;
    Ok(MediaSource {
        id: SourceId(row.get(0)?),
        kind: kind_from_str(&kind),
        name: row.get(2)?,
        location: row.get(3)?,
        exclude_rules: row.get(4)?,
        state: source_state_from_str(&state),
        last_scan_at: row.get(6)?,
    })
}

// ---------------------------------------------------------------------------
// media_item
// ---------------------------------------------------------------------------

/// 媒体条目仓储。
pub struct MediaItemRepo;

impl MediaItemRepo {
    /// 幂等 UPSERT，返回条目标识。
    ///
    /// 命中 `stable_key` 时只更新「跟着文件走」的字段（相对路径 / URL / 大小 / 修改时间），
    /// 并清空 `missing_at`（文件回来了）。标题、年份、海报、锁定字段等可能承载用户
    /// 修改的字段一律保留 —— 这是 PRD FR-01「已识别的元数据与用户标签不被覆盖」。
    pub fn upsert(conn: &Connection, item: &MediaItem) -> Result<ItemId> {
        let ts = now_ms();
        let id: String = conn.query_row(
            "INSERT INTO media_item (
                 id, source_id, stable_key, rel_path, url, title, year, duration_ms, rating,
                 overview, poster_id, size_bytes, mtime, identify_state, meta_state, field_locks,
                 missing_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                       NULL, ?17, ?17)
             ON CONFLICT(stable_key) DO UPDATE SET
                 rel_path = excluded.rel_path,
                 url = excluded.url,
                 size_bytes = excluded.size_bytes,
                 mtime = excluded.mtime,
                 missing_at = NULL,
                 updated_at = excluded.updated_at
             RETURNING id",
            params![
                item.id.as_str(),
                item.source_id.as_str(),
                item.stable_key,
                item.rel_path,
                item.url,
                item.title,
                item.year,
                item.duration_ms,
                item.rating.map(f64::from),
                item.overview,
                item.poster_id,
                item.size_bytes.map(|v| v as i64),
                item.mtime,
                identify_to_str(item.identify_state),
                meta_to_str(item.meta_state),
                item.field_locks,
                ts,
            ],
            |row| row.get::<_, String>(0),
        )?;
        Ok(ItemId(id))
    }

    pub fn get(conn: &Connection, id: &ItemId) -> Result<Option<MediaItem>> {
        let found = conn
            .query_row(
                &format!("SELECT {ITEM_COLUMNS} FROM media_item WHERE id = ?1"),
                params![id.as_str()],
                read_item,
            )
            .optional()?;
        Ok(found)
    }

    /// 条目清单。`source_id` 为 `None` 时返回全库，供海报墙使用。
    pub fn list(conn: &Connection, source_id: Option<&SourceId>) -> Result<Vec<MediaItem>> {
        let (sql, args): (String, Vec<String>) = match source_id {
            Some(id) => (
                format!("SELECT {ITEM_COLUMNS} FROM media_item WHERE source_id = ?1 ORDER BY id"),
                vec![id.0.clone()],
            ),
            None => (
                format!("SELECT {ITEM_COLUMNS} FROM media_item ORDER BY id"),
                Vec::new(),
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), read_item)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 探测得到的时长回填（`probe` 的结果，系统自动不可编辑）。
    pub fn set_duration(conn: &Connection, id: &ItemId, duration_ms: Option<i64>) -> Result<()> {
        conn.execute(
            "UPDATE media_item SET duration_ms = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.as_str(), duration_ms, now_ms()],
        )?;
        Ok(())
    }

    /// 回填入库时间。仅用于**导入**：`upsert` 的插入路径以当前时间为入库时间，
    /// 而「最近添加」排序要按原库的入库时间还原（PRD FR-07）。
    pub fn set_created_at(conn: &Connection, id: &ItemId, created_at: i64) -> Result<()> {
        conn.execute(
            "UPDATE media_item SET created_at = ?2 WHERE id = ?1",
            params![id.as_str(), created_at],
        )?;
        Ok(())
    }

    /// 该稳定键是否已有条目（导入时判定「本机是否已有此条目」，据此决定用户数据归属）。
    pub fn id_by_stable_key(conn: &Connection, stable_key: &str) -> Result<Option<ItemId>> {
        let found: Option<String> = conn
            .query_row(
                "SELECT id FROM media_item WHERE stable_key = ?1",
                params![stable_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.map(ItemId))
    }

    /// 标记 / 清除「文件缺失」（PRD 异常表：条目保留但置灰，不进入播放）。
    pub fn set_missing(conn: &Connection, id: &ItemId, missing: bool) -> Result<()> {
        let value = if missing { Some(now_ms()) } else { None };
        conn.execute(
            "UPDATE media_item SET missing_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.as_str(), value, now_ms()],
        )?;
        Ok(())
    }

    pub fn is_missing(conn: &Connection, id: &ItemId) -> Result<bool> {
        let value: Option<Option<i64>> = conn
            .query_row(
                "SELECT missing_at FROM media_item WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(matches!(value, Some(Some(_))))
    }

    /// 全部「文件缺失」条目的 id。一次查询取全量，避免海报墙逐条目回查。
    pub fn missing_ids(conn: &Connection) -> Result<Vec<ItemId>> {
        let mut stmt = conn.prepare("SELECT id FROM media_item WHERE missing_at IS NOT NULL")?;
        let rows = stmt.query_map(params![], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(ItemId(row?));
        }
        Ok(out)
    }

    pub fn count(conn: &Connection) -> Result<u64> {
        let n: i64 = conn.query_row("SELECT count(*) FROM media_item", params![], |row| row.get(0))?;
        Ok(n as u64)
    }

    /// 某源下的条目数（「库管理」列源时展示，也让用户判断清除的影响面）。
    pub fn count_by_source(conn: &Connection, source_id: &SourceId) -> Result<u64> {
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM media_item WHERE source_id = ?1",
            params![source_id.as_str()],
            |row| row.get(0),
        )?;
        Ok(n as u64)
    }

    /// 批量移除条目，返回实际删除的条数。
    ///
    /// **只删库内记录，绝不动磁盘文件**——这是「清除入库」与「删除视频」的分界线。
    /// `item_fts` 是独立的 FTS5 表（没有触发器随条目级联），必须同批删除，
    /// 否则搜索仍会命中已清除的条目。整批放在一个事务里：中途失败不留半截数据。
    pub fn remove_many(conn: &Connection, ids: &[ItemId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let tx = conn.unchecked_transaction()?;
        let mut removed = 0usize;
        {
            let mut del_index = tx.prepare("DELETE FROM item_fts WHERE item_id = ?1")?;
            let mut del_item = tx.prepare("DELETE FROM media_item WHERE id = ?1")?;
            for id in ids {
                del_index.execute(params![id.as_str()])?;
                removed += del_item.execute(params![id.as_str()])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }
}

/// 读列顺序必须与 [`ITEM_COLUMNS`] 一致。
const ITEM_COLUMNS: &str = "id, source_id, stable_key, rel_path, url, title, year, duration_ms, \
                            rating, overview, poster_id, size_bytes, mtime, identify_state, \
                            meta_state, field_locks, created_at";

fn read_item(row: &Row<'_>) -> rusqlite::Result<MediaItem> {
    let identify: String = row.get(13)?;
    let meta: String = row.get(14)?;
    Ok(MediaItem {
        id: ItemId(row.get(0)?),
        source_id: SourceId(row.get(1)?),
        stable_key: row.get(2)?,
        rel_path: row.get(3)?,
        url: row.get(4)?,
        title: row.get(5)?,
        year: row.get(6)?,
        duration_ms: row.get(7)?,
        rating: row.get::<_, Option<f64>>(8)?.map(|v| v as f32),
        overview: row.get(9)?,
        poster_id: row.get(10)?,
        size_bytes: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        mtime: row.get(12)?,
        identify_state: identify_from_str(&identify),
        meta_state: meta_from_str(&meta),
        field_locks: row.get(15)?,
        created_at: row.get(16)?,
    })
}

// ---------------------------------------------------------------------------
// playback_state
// ---------------------------------------------------------------------------

/// 观看进度仓储。
///
/// 进度按**条目**维度保存，与屏位无关 —— 多屏不新增表（架构文档 §8）。
/// 是否**值得**写入由 L2 [`cinema_domain::rules::ResumePolicy`] 决定，仓储只负责落库。
pub struct PlaybackRepo;

impl PlaybackRepo {
    pub fn put(conn: &Connection, item_id: &ItemId, progress: Progress) -> Result<()> {
        conn.execute(
            "INSERT INTO playback_state (item_id, position_ms, duration_ms, watched, updated_at)
             VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(item_id) DO UPDATE SET
                 position_ms = excluded.position_ms,
                 duration_ms = COALESCE(excluded.duration_ms, playback_state.duration_ms),
                 updated_at = excluded.updated_at",
            params![
                item_id.as_str(),
                progress.position_ms,
                progress.duration_ms,
                now_ms()
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, item_id: &ItemId) -> Result<Option<Progress>> {
        let found = conn
            .query_row(
                "SELECT position_ms, duration_ms FROM playback_state WHERE item_id = ?1",
                params![item_id.as_str()],
                |row| {
                    Ok(Progress {
                        position_ms: row.get(0)?,
                        duration_ms: row.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    pub fn set_watched(conn: &Connection, item_id: &ItemId, watched: bool) -> Result<()> {
        conn.execute(
            "UPDATE playback_state SET watched = ?2, updated_at = ?3 WHERE item_id = ?1",
            params![item_id.as_str(), i64::from(watched), now_ms()],
        )?;
        Ok(())
    }

    pub fn is_watched(conn: &Connection, item_id: &ItemId) -> Result<bool> {
        let value: Option<i64> = conn
            .query_row(
                "SELECT watched FROM playback_state WHERE item_id = ?1",
                params![item_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.unwrap_or(0) != 0)
    }

    /// 最近一次进度更新时间；从未播放返回 `None`（「最近观看」排序使用）。
    pub fn last_updated_at(conn: &Connection, item_id: &ItemId) -> Result<Option<i64>> {
        let value: Option<Option<i64>> = conn
            .query_row(
                "SELECT updated_at FROM playback_state WHERE item_id = ?1",
                params![item_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    /// 清除进度（用户选「从头播放」或手动拖到片尾时覆盖既有进度）。
    pub fn clear(conn: &Connection, item_id: &ItemId) -> Result<()> {
        conn.execute(
            "DELETE FROM playback_state WHERE item_id = ?1",
            params![item_id.as_str()],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cinema_domain::model::MediaSource;

    fn db() -> crate::db::Database {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("迁移应成功");
        db
    }

    fn sample_source() -> MediaSource {
        MediaSource {
            id: SourceId::from("src-1"),
            kind: SourceKind::LocalFolder,
            name: "本地目录".into(),
            location: "D:/media".into(),
            exclude_rules: None,
            state: SourceState::Idle,
            last_scan_at: None,
        }
    }

    fn sample_item(key: &str, title: &str) -> MediaItem {
        let mut item = MediaItem::pending_fill("it-1", "src-1", title);
        item.stable_key = key.into();
        item.rel_path = Some("a/b.mp4".into());
        item
    }

    #[test]
    fn source_roundtrip() {
        let db = db();
        MediaSourceRepo::upsert(db.conn(), &sample_source()).expect("写入源");

        let loaded = MediaSourceRepo::get(db.conn(), &SourceId::from("src-1"))
            .expect("读取源")
            .expect("源应存在");
        assert_eq!(loaded.kind, SourceKind::LocalFolder);
        assert_eq!(loaded.location, "D:/media");

        MediaSourceRepo::set_scan_result(db.conn(), &SourceId::from("src-1"), SourceState::ScanFailed, 123)
            .expect("记录扫描结果");
        assert_eq!(
            MediaSourceRepo::get(db.conn(), &SourceId::from("src-1"))
                .unwrap()
                .unwrap()
                .state,
            SourceState::ScanFailed
        );
    }

    #[test]
    fn item_upsert_is_idempotent_by_stable_key() {
        let db = db();
        MediaSourceRepo::upsert(db.conn(), &sample_source()).expect("写入源");

        let first = MediaItemRepo::upsert(db.conn(), &sample_item("src-1|a/b.mp4", "第一部"))
            .expect("首次入库");

        // 同一物理文件再次扫描：必须合并为同一条，不新增行
        let mut again = sample_item("src-1|a/b.mp4", "被改写的标题");
        again.rel_path = Some("a/c.mp4".into()); // 目录改名
        let second = MediaItemRepo::upsert(db.conn(), &again).expect("重复入库");

        assert_eq!(first, second, "同一稳定键必须复用同一 id");
        assert_eq!(MediaItemRepo::count(db.conn()).expect("统计"), 1, "不得产生重复条目");

        let stored = MediaItemRepo::get(db.conn(), &first).unwrap().unwrap();
        assert_eq!(stored.title, "第一部", "重复扫描不得覆盖既有标题");
        assert_eq!(stored.rel_path.as_deref(), Some("a/c.mp4"), "路径应跟随文件更新");
    }

    #[test]
    fn missing_flag_roundtrip() {
        let db = db();
        MediaSourceRepo::upsert(db.conn(), &sample_source()).unwrap();
        let id = MediaItemRepo::upsert(db.conn(), &sample_item("k1", "t")).unwrap();

        assert!(!MediaItemRepo::is_missing(db.conn(), &id).unwrap());
        MediaItemRepo::set_missing(db.conn(), &id, true).unwrap();
        assert!(MediaItemRepo::is_missing(db.conn(), &id).unwrap());
        assert_eq!(MediaItemRepo::missing_ids(db.conn()).unwrap(), vec![id.clone()]);
        // 重新扫描到该文件时应清除缺失标记
        MediaItemRepo::upsert(db.conn(), &sample_item("k1", "t")).unwrap();
        assert!(!MediaItemRepo::is_missing(db.conn(), &id).unwrap());
        assert!(MediaItemRepo::missing_ids(db.conn()).unwrap().is_empty());
    }

    #[test]
    fn progress_roundtrip_and_clear() {
        let db = db();
        MediaSourceRepo::upsert(db.conn(), &sample_source()).unwrap();
        let id = MediaItemRepo::upsert(db.conn(), &sample_item("k1", "t")).unwrap();

        assert!(PlaybackRepo::get(db.conn(), &id).unwrap().is_none());

        PlaybackRepo::put(
            db.conn(),
            &id,
            Progress {
                position_ms: 360_000,
                duration_ms: Some(600_000),
            },
        )
        .unwrap();
        let stored = PlaybackRepo::get(db.conn(), &id).unwrap().unwrap();
        assert_eq!(stored.position_ms, 360_000);
        assert_eq!(stored.duration_ms, Some(600_000));

        // 续播规则可直接作用于仓储读出的进度
        assert!(
            cinema_domain::rules::ResumePolicy::resume_offer(
                PlaybackRepo::get(db.conn(), &id).unwrap(),
                None
            )
            .is_some(),
            "360s/600s 应给出续播提示"
        );

        PlaybackRepo::clear(db.conn(), &id).unwrap();
        assert!(PlaybackRepo::get(db.conn(), &id).unwrap().is_none());
    }

    #[test]
    fn duration_backfill_keeps_duration_when_put_without_it() {
        let db = db();
        MediaSourceRepo::upsert(db.conn(), &sample_source()).unwrap();
        let id = MediaItemRepo::upsert(db.conn(), &sample_item("k1", "t")).unwrap();

        PlaybackRepo::put(
            db.conn(),
            &id,
            Progress {
                position_ms: 60_000,
                duration_ms: Some(600_000),
            },
        )
        .unwrap();
        // 后续心跳只带位置，不带时长：不得把已有时长覆盖为空
        PlaybackRepo::put(
            db.conn(),
            &id,
            Progress {
                position_ms: 90_000,
                duration_ms: None,
            },
        )
        .unwrap();
        let stored = PlaybackRepo::get(db.conn(), &id).unwrap().unwrap();
        assert_eq!(stored.duration_ms, Some(600_000), "时长应保留");
        assert_eq!(stored.position_ms, 90_000);
    }
}
