//! L4 数据可迁移（PRD FR-07）：导出包的读写与合并导入。
//!
//! 分工与仓储一致：本模块只做「读库 → 落成包」「读包 → 写库」，**不做流程判断**。
//! 版本兼容判定由 L2 规则给出、由 L1 编排执行；本模块只提供
//! [`packaged_schema_version`] 供 L1 读出包内登记的版本号。
//!
//! 包格式（前进式、离线可校验）：
//! ```text
//! <导出目录>/
//!   manifest.json                        结构化清单（源 / 条目 / 标签 / 进度 / 收藏 / 设置 / 字幕轨道）
//!   resources/subtitles/<轨道 id>.json   已生成实时字幕的字幕句
//!   resources/posters/<海报 id>          海报资源（当前无元数据源，通常为空）
//! ```
//!
//! 合并规则（PRD FR-07 / 架构 §8）：
//! - 按稳定键合并，**同一 stable_key 冲突以本机用户数据为准**（`ON CONFLICT DO NOTHING`）；
//! - 派生数据（派生标签 / FTS 索引 / 海报缓存）**不入包**，导入后按事实重算；
//! - 源位置不匹配的条目**标记为「缺失」而不是丢弃**；
//! - 整个导入在**一个事务**内完成，任一步失败即回滚，不留半成品。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use cinema_domain::model::{
    IdentifyState, ItemId, MediaItem, MetaState, Progress, SourceKind, SubtitleCue, SubtitleKind,
    SubtitleTrack,
};
use cinema_domain::ports::tag_dimension::{Selector, TagDimensionDef, TagSource};
use cinema_domain::portability::{ExportOutcome, ImportOutcome, RestoredProgress};

use crate::error::{InfraError, Result};
use crate::repo::{MediaItemRepo, MediaSourceRepo, PlaybackRepo};
use crate::subtitle_repo::SubtitleTrackRepo;
use crate::tag_repo::{DimensionRepo, TagRepo};

/// 导出包标记：导入时据此拒绝非本应用的目录。
const FORMAT: &str = "cinema-library-export";
/// 包格式版本（与库 schema 版本分开：格式用于自描述，schema 版本用于迁移判定）。
const FORMAT_VERSION: u32 = 1;
/// 资源子目录（相对导出目录）。
const RES_SUBTITLE_DIR: &str = "resources/subtitles";
const RES_POSTER_DIR: &str = "resources/posters";

// ---------------------------------------------------------------------------
// 包内结构（结构化 JSON 清单）
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    format: String,
    format_version: u32,
    /// 导出时本机库的 schema 版本，供导入侧做前进式判定。
    schema_version: i64,
    exported_at: i64,
    sources: Vec<ManifestSource>,
    items: Vec<ManifestItem>,
    dimensions: Vec<ManifestDimension>,
    tags: Vec<ManifestTag>,
    item_tags: Vec<ManifestItemTag>,
    blacklist: Vec<ManifestBlacklist>,
    progress: Vec<ManifestProgress>,
    collections: Vec<ManifestCollection>,
    collection_items: Vec<ManifestCollectionItem>,
    settings: Vec<ManifestSetting>,
    subtitle_tracks: Vec<ManifestTrack>,
    posters: Vec<ManifestPoster>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestSource {
    id: String,
    kind: String,
    name: String,
    location: String,
    exclude_rules: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestItem {
    id: String,
    source_id: String,
    stable_key: String,
    rel_path: Option<String>,
    url: Option<String>,
    title: String,
    year: Option<i32>,
    duration_ms: Option<i64>,
    rating: Option<f32>,
    overview: Option<String>,
    poster_id: Option<String>,
    size_bytes: Option<u64>,
    mtime: Option<i64>,
    identify_state: String,
    meta_state: String,
    field_locks: Option<String>,
    created_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestDimension {
    id: String,
    label: String,
    selector: String,
    source: String,
    order: u32,
    editable: bool,
    lockable: bool,
    derive_rule: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestTag {
    id: String,
    dimension_id: String,
    name: String,
    aliases: Vec<String>,
    locked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestItemTag {
    item_id: String,
    tag_id: String,
    origin: String,
    locked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestBlacklist {
    item_id: String,
    tag_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestProgress {
    item_id: String,
    position_ms: i64,
    duration_ms: Option<i64>,
    watched: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestCollection {
    id: String,
    name: String,
    sort_mode: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestCollectionItem {
    collection_id: String,
    item_id: String,
    order_idx: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestSetting {
    key: String,
    value: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestTrack {
    id: String,
    item_id: String,
    kind: String,
    lang: Option<String>,
    source_engine: Option<String>,
    locked: bool,
    /// 字幕句资源文件的相对路径；无句时为 `null`。
    blob_ref: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestPoster {
    poster_id: String,
    /// 海报资源文件的相对路径。
    file: String,
}

/// 字幕句资源文件。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubtitleResource {
    cues: Vec<CueResource>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CueResource {
    start_ms: i64,
    end_ms: i64,
    text: String,
    lang: Option<String>,
}

// ---------------------------------------------------------------------------
// 读取包内的版本号（供 L1 做前进式判定；只读一个字段，不解析整包）
// ---------------------------------------------------------------------------

/// 读出导出包登记的 schema 版本。
pub fn packaged_schema_version(src: &Path) -> Result<i64> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct VersionProbe {
        schema_version: i64,
    }
    let text = read_manifest_text(src)?;
    let probe: VersionProbe = serde_json::from_str(&text)
        .map_err(|e| InfraError::Format(format!("导出包清单不可解析：{e}")))?;
    Ok(probe.schema_version)
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn read_manifest_text(dir: &Path) -> Result<String> {
    let path = manifest_path(dir);
    if !path.is_file() {
        return Err(InfraError::Format(format!(
            "不是导出包：缺少 {}",
            path.display()
        )));
    }
    Ok(std::fs::read_to_string(&path)?)
}

fn read_manifest(dir: &Path) -> Result<Manifest> {
    let text = read_manifest_text(dir)?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| InfraError::Format(format!("导出包清单不可解析：{e}")))?;
    if manifest.format != FORMAT {
        return Err(InfraError::Format(format!(
            "不是本应用的导出包（format={}）",
            manifest.format
        )));
    }
    Ok(manifest)
}

fn fmt_err(e: serde_json::Error) -> InfraError {
    InfraError::Format(format!("导出包序列化失败：{e}"))
}

// ---------------------------------------------------------------------------
// 枚举 ↔ 文本（与仓储的落库取值一致）
// ---------------------------------------------------------------------------

fn selector_to_str(selector: Selector) -> &'static str {
    match selector {
        Selector::Single => "single",
        Selector::Multi => "multi",
    }
}

fn selector_from_str(raw: &str) -> Selector {
    match raw {
        "single" => Selector::Single,
        _ => Selector::Multi,
    }
}

fn tag_source_to_str(source: TagSource) -> &'static str {
    match source {
        TagSource::Metadata => "metadata",
        TagSource::Derived => "derived",
        TagSource::User => "user",
    }
}

fn tag_source_from_str(raw: &str) -> TagSource {
    match raw {
        "derived" => TagSource::Derived,
        "user" => TagSource::User,
        _ => TagSource::Metadata,
    }
}

fn source_kind_to_str(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::LocalFolder => "local_folder",
        SourceKind::LanPath => "lan_path",
        SourceKind::DirectUrl => "direct_url",
        SourceKind::CloudDrive => "cloud_drive",
        SourceKind::OnlineSite => "online_site",
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
// 导出
// ---------------------------------------------------------------------------

/// 把库内**用户数据**写入导出目录（结构化 JSON 清单 + 资源）。
///
/// 不入包的内容：派生标签与 FTS 索引（可重建）、扫描中的状态与失败原因（运行期状态）、
/// 条目缺失标记（本机条件）。因此导出包可在任意机器上离线校验与还原。
pub fn export_package(conn: &Connection, dest: &Path, poster_dir: Option<&Path>) -> Result<ExportOutcome> {
    let schema_version = crate::db::migration::MigrationRunner::current_version(conn)?;
    let sources = read_sources(conn)?;
    let items = read_items(conn)?;
    let dimensions = read_dimensions(conn)?;
    let tags = read_tags(conn)?;
    let item_tags = read_item_tags(conn)?;
    let blacklist = read_blacklist(conn)?;
    let progress = read_progress(conn)?;
    let collections = read_collections(conn)?;
    let collection_items = read_collection_items(conn)?;
    let settings = read_settings(conn)?;
    let tracks = read_generated_tracks(conn)?;

    std::fs::create_dir_all(dest)?;
    let sub_dir = dest.join(RES_SUBTITLE_DIR);
    std::fs::create_dir_all(&sub_dir)?;

    // 已生成实时字幕：字幕句单独落成资源文件，清单里只留引用
    let mut subtitle_tracks = Vec::with_capacity(tracks.len());
    for (track, cues) in tracks {
        let file_name = format!("{}.json", track.id);
        let resource = SubtitleResource {
            cues: cues
                .iter()
                .map(|cue| CueResource {
                    start_ms: cue.start_ms,
                    end_ms: cue.end_ms,
                    text: cue.text.clone(),
                    lang: cue.lang.clone(),
                })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&resource).map_err(fmt_err)?;
        std::fs::write(sub_dir.join(&file_name), text)?;
        subtitle_tracks.push(ManifestTrack {
            blob_ref: Some(format!("{RES_SUBTITLE_DIR}/{file_name}")),
            ..track
        });
    }

    let posters = export_posters(&items, poster_dir, dest)?;

    let manifest = Manifest {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        schema_version,
        exported_at: crate::now_ms(),
        sources,
        items,
        dimensions,
        tags,
        item_tags,
        blacklist,
        progress,
        collections,
        collection_items,
        settings,
        subtitle_tracks,
        posters,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(fmt_err)?;
    std::fs::write(manifest_path(dest), text)?;

    Ok(ExportOutcome {
        sources: manifest.sources.len(),
        items: manifest.items.len(),
        user_tags: manifest.item_tags.len(),
        collections: manifest.collections.len(),
        settings: manifest.settings.len(),
        subtitle_tracks: manifest.subtitle_tracks.len(),
    })
}

/// 海报资源：本地缓存存在才复制（当前无元数据源，通常为空）。失败不阻断导出。
fn export_posters(
    items: &[ManifestItem],
    poster_dir: Option<&Path>,
    dest: &Path,
) -> Result<Vec<ManifestPoster>> {
    let mut out = Vec::new();
    let Some(src_dir) = poster_dir else {
        return Ok(out);
    };
    let dst_dir = dest.join(RES_POSTER_DIR);
    for item in items {
        let Some(poster_id) = item.poster_id.as_deref().filter(|s| !s.trim().is_empty()) else {
            continue;
        };
        let from = src_dir.join(poster_id);
        if !from.is_file() {
            continue;
        }
        std::fs::create_dir_all(&dst_dir)?;
        let rel = format!("{RES_POSTER_DIR}/{poster_id}");
        std::fs::copy(&from, dest.join(&rel))?;
        out.push(ManifestPoster {
            poster_id: poster_id.to_owned(),
            file: rel,
        });
    }
    Ok(out)
}

fn read_sources(conn: &Connection) -> Result<Vec<ManifestSource>> {
    Ok(MediaSourceRepo::list(conn)?
        .into_iter()
        .map(|s| ManifestSource {
            id: s.id.0,
            kind: source_kind_to_str(s.kind).to_owned(),
            name: s.name,
            location: s.location,
            exclude_rules: s.exclude_rules,
        })
        .collect())
}

fn read_items(conn: &Connection) -> Result<Vec<ManifestItem>> {
    Ok(MediaItemRepo::list(conn, None)?
        .into_iter()
        .map(|i| ManifestItem {
            id: i.id.0,
            source_id: i.source_id.0,
            stable_key: i.stable_key,
            rel_path: i.rel_path,
            url: i.url,
            title: i.title,
            year: i.year,
            duration_ms: i.duration_ms,
            rating: i.rating,
            overview: i.overview,
            poster_id: i.poster_id,
            size_bytes: i.size_bytes,
            mtime: i.mtime,
            identify_state: identify_to_str(i.identify_state).to_owned(),
            meta_state: meta_to_str(i.meta_state).to_owned(),
            field_locks: i.field_locks,
            created_at: i.created_at,
        })
        .collect())
}

fn read_dimensions(conn: &Connection) -> Result<Vec<ManifestDimension>> {
    Ok(DimensionRepo::list(conn)?
        .into_iter()
        .map(|d| ManifestDimension {
            id: d.id,
            label: d.label,
            selector: selector_to_str(d.selector).to_owned(),
            source: tag_source_to_str(d.source).to_owned(),
            order: d.order,
            editable: d.editable,
            lockable: d.lockable,
            derive_rule: d.derive_rule,
        })
        .collect())
}

/// 全部标签取值。逐维度经 [`TagRepo::list_by_dimension`] 读出（别名解码复用仓储实现）。
fn read_tags(conn: &Connection) -> Result<Vec<ManifestTag>> {
    let mut out = Vec::new();
    for dim in DimensionRepo::list(conn)? {
        for tag in TagRepo::list_by_dimension(conn, &dim.id)? {
            out.push(ManifestTag {
                id: tag.id,
                dimension_id: tag.dimension_id,
                name: tag.name,
                aliases: tag.aliases,
                locked: tag.locked,
            });
        }
    }
    Ok(out)
}

/// 用户标签关系：只导出 `origin='user'` 或已锁定的行。
///
/// 派生标签由系统按事实重算（架构 §8「派生数据可重建」），不入包。
fn read_item_tags(conn: &Connection) -> Result<Vec<ManifestItemTag>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, tag_id, origin, locked FROM item_tag
         WHERE origin = 'user' OR locked = 1
         ORDER BY item_id, tag_id",
    )?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestItemTag {
            item_id: row.get(0)?,
            tag_id: row.get(1)?,
            origin: row.get(2)?,
            locked: row.get::<_, i64>(3)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_blacklist(conn: &Connection) -> Result<Vec<ManifestBlacklist>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, tag_id FROM item_tag_blacklist ORDER BY item_id, tag_id",
    )?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestBlacklist {
            item_id: row.get(0)?,
            tag_id: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_progress(conn: &Connection) -> Result<Vec<ManifestProgress>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, position_ms, duration_ms, watched FROM playback_state ORDER BY item_id",
    )?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestProgress {
            item_id: row.get(0)?,
            position_ms: row.get(1)?,
            duration_ms: row.get(2)?,
            watched: row.get::<_, i64>(3)? != 0,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_collections(conn: &Connection) -> Result<Vec<ManifestCollection>> {
    let mut stmt = conn.prepare("SELECT id, name, sort_mode FROM collection ORDER BY id")?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestCollection {
            id: row.get(0)?,
            name: row.get(1)?,
            sort_mode: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_collection_items(conn: &Connection) -> Result<Vec<ManifestCollectionItem>> {
    let mut stmt = conn.prepare(
        "SELECT collection_id, item_id, order_idx FROM collection_item ORDER BY collection_id, order_idx",
    )?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestCollectionItem {
            collection_id: row.get(0)?,
            item_id: row.get(1)?,
            order_idx: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn read_settings(conn: &Connection) -> Result<Vec<ManifestSetting>> {
    let mut stmt = conn.prepare("SELECT key, value FROM setting ORDER BY key")?;
    let rows = stmt.query_map(params![], |row| {
        Ok(ManifestSetting {
            key: row.get(0)?,
            value: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 已生成实时字幕轨道（含字幕句）。外挂 / 内嵌轨道依赖本机文件，不入包。
fn read_generated_tracks(conn: &Connection) -> Result<Vec<(ManifestTrack, Vec<SubtitleCue>)>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM subtitle_track WHERE kind = 'generated' ORDER BY item_id, id",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let track = SubtitleTrackRepo::get(conn, &id)?
            .ok_or_else(|| InfraError::Format(format!("字幕轨道读取失败：{id}")))?;
        out.push((
            ManifestTrack {
                id: track.id,
                item_id: track.item_id.0,
                kind: "generated".to_owned(),
                lang: track.lang,
                source_engine: track.source_engine,
                locked: track.locked,
                blob_ref: None,
            },
            track.cues,
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

/// 把导出包合并进本机库。**全部在一个事务内**，任一步失败即回滚。
///
/// 合并口径：
/// - 清单里已有同 id 的源 / 标签关系 / 进度 / 设置 / 字幕轨道时，**保留本机数据**；
/// - 条目按稳定键合并，用户字段（标题 / 锁定字段等）沿用本机，仅路径字段跟随包；
/// - 无法按本机路径访问的条目标记为「缺失」。
pub fn import_package(conn: &Connection, src: &Path, poster_dir: Option<&Path>) -> Result<ImportOutcome> {
    let manifest = read_manifest(src)?;
    let tx = conn.unchecked_transaction()?;

    // 1. 维度定义：幂等播种，不覆盖本机已改配置
    let dims: Vec<TagDimensionDef> = manifest
        .dimensions
        .iter()
        .map(|d| TagDimensionDef {
            id: d.id.clone(),
            label: d.label.clone(),
            selector: selector_from_str(&d.selector),
            source: tag_source_from_str(&d.source),
            expand: "value".to_owned(),
            order: d.order,
            editable: d.editable,
            lockable: d.lockable,
            derive_rule: d.derive_rule.clone(),
        })
        .collect();
    DimensionRepo::seed(&tx, &dims)?;

    // 2. 标签取值：复用「同义合并」规则，得到本机统一 id
    let mut tag_map: BTreeMap<String, String> = BTreeMap::new();
    for tag in &manifest.tags {
        let id = TagRepo::ensure(&tx, &tag.dimension_id, &tag.name, &tag.aliases)?;
        if tag.locked {
            TagRepo::set_locked(&tx, &id, true)?;
        }
        tag_map.insert(tag.id.clone(), id);
    }

    // 3. 源：同 id 已存在则保留本机配置（位置可能与本机实际不一致，交由条目判定）
    for source in &manifest.sources {
        insert_source_if_absent(&tx, source)?;
    }
    let local_sources = source_locations(&tx)?;

    // 4. 条目：按稳定键合并，返回本机 id 并建立「包 id → 本机 id」映射
    let mut item_map: BTreeMap<String, String> = BTreeMap::new();
    let mut items_missing = 0usize;
    for item in &manifest.items {
        let existed = MediaItemRepo::id_by_stable_key(&tx, &item.stable_key)?.is_some();
        let local_id = MediaItemRepo::upsert(&tx, &item_to_model(item))?;
        if !existed {
            if let Some(created_at) = item.created_at {
                MediaItemRepo::set_created_at(&tx, &local_id, created_at)?;
            }
        }
        // 源位置不匹配（本机读不到文件）→ 标记缺失，但条目保留
        if !reachable(&local_sources, item) {
            MediaItemRepo::set_missing(&tx, &local_id, true)?;
            items_missing += 1;
        }
        item_map.insert(item.id.clone(), local_id.0);
    }

    // 5. 用户标签关系：本机已有同 (条目, 标签) 时保留本机
    let mut user_tags = 0usize;
    for relation in &manifest.item_tags {
        let (Some(local_item), Some(local_tag)) =
            (item_map.get(&relation.item_id), tag_map.get(&relation.tag_id))
        else {
            continue;
        };
        insert_item_tag_if_absent(&tx, local_item, local_tag, &relation.origin, relation.locked)?;
        user_tags += 1;
    }

    // 6. 标签黑名单（用户显式反对的自动标签）
    for blocked in &manifest.blacklist {
        let (Some(local_item), Some(local_tag)) =
            (item_map.get(&blocked.item_id), tag_map.get(&blocked.tag_id))
        else {
            continue;
        };
        insert_blacklist_if_absent(&tx, local_item, local_tag)?;
    }

    // 7. 进度与已看：本机已有进度时不覆盖
    let mut restored_progress = Vec::new();
    for entry in &manifest.progress {
        let Some(local_item) = item_map.get(&entry.item_id) else {
            continue;
        };
        let id = ItemId(local_item.clone());
        if PlaybackRepo::get(&tx, &id)?.is_none() {
            PlaybackRepo::put(
                &tx,
                &id,
                Progress {
                    position_ms: entry.position_ms,
                    duration_ms: entry.duration_ms,
                },
            )?;
            PlaybackRepo::set_watched(&tx, &id, entry.watched)?;
        }
        restored_progress.push(RestoredProgress {
            item_id: id.clone(),
            watched: PlaybackRepo::is_watched(&tx, &id)?,
        });
    }

    // 8. 收藏
    for collection in &manifest.collections {
        insert_collection_if_absent(&tx, collection)?;
    }
    let mut collections = 0usize;
    for entry in &manifest.collection_items {
        let Some(local_item) = item_map.get(&entry.item_id) else {
            continue;
        };
        insert_collection_item_if_absent(&tx, &entry.collection_id, local_item, entry.order_idx)?;
        collections += 1;
    }

    // 9. 设置：本机已有同键时保留本机
    let mut settings = 0usize;
    for entry in &manifest.settings {
        insert_setting_if_absent(&tx, entry)?;
        settings += 1;
    }

    // 10. 已生成字幕轨道：本机已有同 id 时保留本机
    let mut subtitle_tracks = 0usize;
    for track in &manifest.subtitle_tracks {
        let Some(local_item) = item_map.get(&track.item_id) else {
            continue;
        };
        if SubtitleTrackRepo::get(&tx, &track.id)?.is_some() {
            continue;
        }
        let cues = match track.blob_ref.as_deref() {
            Some(rel) => read_subtitle_resource(src, rel)?,
            None => Vec::new(),
        };
        SubtitleTrackRepo::upsert(
            &tx,
            &SubtitleTrack {
                id: track.id.clone(),
                item_id: ItemId(local_item.clone()),
                kind: SubtitleKind::Generated,
                lang: track.lang.clone(),
                source_engine: track.source_engine.clone(),
                locked: track.locked,
                cues,
            },
        )?;
        subtitle_tracks += 1;
    }

    // 11. 海报资源：尽力复制，缺失不阻断（当前无元数据源时为空）
    import_posters(&manifest.posters, src, poster_dir);

    tx.commit()?;

    Ok(ImportOutcome {
        sources: manifest.sources.len(),
        items: manifest.items.len(),
        items_missing,
        user_tags,
        collections,
        settings,
        subtitle_tracks,
        restored_progress,
    })
}

fn item_to_model(item: &ManifestItem) -> MediaItem {
    MediaItem {
        id: ItemId(item.id.clone()),
        source_id: cinema_domain::model::SourceId(item.source_id.clone()),
        stable_key: item.stable_key.clone(),
        rel_path: item.rel_path.clone(),
        url: item.url.clone(),
        title: item.title.clone(),
        year: item.year,
        duration_ms: item.duration_ms,
        rating: item.rating,
        overview: item.overview.clone(),
        poster_id: item.poster_id.clone(),
        size_bytes: item.size_bytes,
        mtime: item.mtime,
        identify_state: identify_from_str(&item.identify_state),
        meta_state: meta_from_str(&item.meta_state),
        field_locks: item.field_locks.clone(),
        created_at: item.created_at,
    }
}

/// 本机各源的位置与类型（判定条目是否可访问）。
fn source_locations(conn: &Connection) -> Result<BTreeMap<String, (SourceKind, String)>> {
    Ok(MediaSourceRepo::list(conn)?
        .into_iter()
        .map(|s| (s.id.0, (s.kind, s.location)))
        .collect())
}

/// 条目在本机是否可访问。在线源（直链 / 云盘）离线不可校验，一律视为可访问。
fn reachable(sources: &BTreeMap<String, (SourceKind, String)>, item: &ManifestItem) -> bool {
    let Some((kind, location)) = sources.get(&item.source_id) else {
        return false;
    };
    match kind {
        SourceKind::LocalFolder | SourceKind::LanPath => {
            let mut path = PathBuf::from(location);
            if let Some(rel) = item.rel_path.as_deref().filter(|s| !s.trim().is_empty()) {
                path = path.join(rel);
            }
            path.is_file()
        }
        SourceKind::DirectUrl | SourceKind::CloudDrive | SourceKind::OnlineSite => true,
    }
}

fn read_subtitle_resource(dir: &Path, rel: &str) -> Result<Vec<SubtitleCue>> {
    let path = dir.join(rel);
    if !path.is_file() {
        return Err(InfraError::Format(format!(
            "导出包缺少字幕资源：{rel}"
        )));
    }
    let text = std::fs::read_to_string(&path)?;
    let resource: SubtitleResource = serde_json::from_str(&text)
        .map_err(|e| InfraError::Format(format!("字幕资源不可解析（{rel}）：{e}")))?;
    Ok(resource
        .cues
        .into_iter()
        .map(|cue| SubtitleCue {
            start_ms: cue.start_ms,
            end_ms: cue.end_ms,
            text: cue.text,
            lang: cue.lang,
        })
        .collect())
}

/// 海报回填：尽力而为，任一步失败只跳过（不影响导入结果）。
fn import_posters(posters: &[ManifestPoster], src: &Path, poster_dir: Option<&Path>) {
    let Some(dst_dir) = poster_dir else {
        return;
    };
    for poster in posters {
        let from = src.join(&poster.file);
        if !from.is_file() {
            continue;
        }
        if std::fs::create_dir_all(dst_dir).is_err() {
            continue;
        }
        let _ = std::fs::copy(&from, dst_dir.join(&poster.poster_id));
    }
}

// ---------------------------------------------------------------------------
// 写入原语（本模块专用：均为「本机数据优先」的幂等写入）
// ---------------------------------------------------------------------------

fn insert_source_if_absent(conn: &Connection, source: &ManifestSource) -> Result<()> {
    conn.execute(
        "INSERT INTO media_source (id, kind, name, location, exclude_rules, state, last_scan_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'idle', NULL)
         ON CONFLICT(id) DO NOTHING",
        params![
            source.id,
            source.kind,
            source.name,
            source.location,
            source.exclude_rules
        ],
    )?;
    Ok(())
}

fn insert_item_tag_if_absent(
    conn: &Connection,
    item_id: &str,
    tag_id: &str,
    origin: &str,
    locked: bool,
) -> Result<()> {
    conn.execute(
        "INSERT INTO item_tag (item_id, tag_id, origin, locked) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(item_id, tag_id) DO NOTHING",
        params![item_id, tag_id, origin, i64::from(locked)],
    )?;
    Ok(())
}

fn insert_blacklist_if_absent(conn: &Connection, item_id: &str, tag_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO item_tag_blacklist (item_id, tag_id) VALUES (?1, ?2)
         ON CONFLICT(item_id, tag_id) DO NOTHING",
        params![item_id, tag_id],
    )?;
    Ok(())
}

fn insert_collection_if_absent(conn: &Connection, collection: &ManifestCollection) -> Result<()> {
    conn.execute(
        "INSERT INTO collection (id, name, sort_mode) VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO NOTHING",
        params![collection.id, collection.name, collection.sort_mode],
    )?;
    Ok(())
}

fn insert_collection_item_if_absent(
    conn: &Connection,
    collection_id: &str,
    item_id: &str,
    order_idx: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO collection_item (collection_id, item_id, order_idx) VALUES (?1, ?2, ?3)
         ON CONFLICT(collection_id, item_id) DO NOTHING",
        params![collection_id, item_id, order_idx],
    )?;
    Ok(())
}

fn insert_setting_if_absent(conn: &Connection, setting: &ManifestSetting) -> Result<()> {
    conn.execute(
        "INSERT INTO setting (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO NOTHING",
        params![setting.key, setting.value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cinema_domain::model::{SourceId, SourceState};
    use cinema_domain::ports::tag_dimension::TagDimensionDef;

    use crate::repo::MediaSourceRepo;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("cinema-pkg-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn migrated_db() -> crate::db::Database {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("迁移应成功");
        DimensionRepo::seed(db.conn(), &TagDimensionDef::builtin_defaults()).expect("播种维度");
        db
    }

    /// 造一份带用户数据的样本库：源 + 条目（文件真实存在）+ 用户标签 + 进度（已看）+ 收藏 + 设置 + 生成字幕。
    fn seeded_db() -> (crate::db::Database, PathBuf) {
        let media_dir = temp_dir("media");
        std::fs::write(media_dir.join("影片.mp4"), b"x").expect("写影片文件");

        let db = migrated_db();
        let conn = db.conn();
        MediaSourceRepo::upsert(
            conn,
            &cinema_domain::model::MediaSource {
                id: SourceId::from("src-1"),
                kind: SourceKind::LocalFolder,
                name: "我的电影".into(),
                location: media_dir.to_string_lossy().into_owned(),
                exclude_rules: None,
                state: SourceState::Idle,
                last_scan_at: None,
            },
        )
        .expect("写源");

        let mut item = MediaItem::pending_fill("it-1", "src-1", "影片");
        item.stable_key = "src-1|影片.mp4".into();
        item.rel_path = Some("影片.mp4".into());
        item.year = Some(1999);
        let id = MediaItemRepo::upsert(conn, &item).expect("写条目");

        let tag = TagRepo::ensure(conn, cinema_domain::tagging::DIM_GENRE, "科幻", &[]).expect("建标签");
        insert_item_tag_if_absent(conn, id.as_str(), &tag, "user", true).expect("挂用户标签");

        PlaybackRepo::put(
            conn,
            &id,
            Progress {
                position_ms: 500_000,
                duration_ms: Some(600_000),
            },
        )
        .expect("写进度");
        PlaybackRepo::set_watched(conn, &id, true).expect("标记已看");

        insert_collection_if_absent(
            conn,
            &ManifestCollection {
                id: "col-1".into(),
                name: "周末片单".into(),
                sort_mode: "manual".into(),
            },
        )
        .expect("写收藏");
        insert_collection_item_if_absent(conn, "col-1", id.as_str(), 0).expect("写收藏项");
        insert_setting_if_absent(
            conn,
            &ManifestSetting {
                key: "multi_screen.max_screens".into(),
                value: "3".into(),
            },
        )
        .expect("写设置");
        SubtitleTrackRepo::upsert(
            conn,
            &SubtitleTrack {
                id: "tr-1".into(),
                item_id: id.clone(),
                kind: SubtitleKind::Generated,
                lang: Some("zh".into()),
                source_engine: Some("local-whisper".into()),
                locked: false,
                cues: vec![SubtitleCue {
                    start_ms: 0,
                    end_ms: 1000,
                    text: "第一句".into(),
                    lang: Some("zh".into()),
                }],
            },
        )
        .expect("写生成字幕");

        (db, media_dir)
    }

    #[test]
    fn export_then_import_restores_user_data_into_fresh_db() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export");
        let report = export_package(origin.conn(), &pkg, None).expect("导出");
        assert_eq!(report.items, 1);
        assert_eq!(report.user_tags, 1, "只导出用户标签关系");
        assert_eq!(report.subtitle_tracks, 1);

        // 导入到全新库
        let fresh = migrated_db();
        let outcome = import_package(fresh.conn(), &pkg, None).expect("导入");
        assert_eq!(outcome.items, 1);
        assert_eq!(outcome.subtitle_tracks, 1);
        assert_eq!(outcome.restored_progress.len(), 1);

        let conn = fresh.conn();
        // 用户标签还原
        let tag = fresh_tag_id(conn, cinema_domain::tagging::DIM_GENRE, "科幻");
        assert!(PartialEq::eq(&tag.is_some(), &true), "标签取值应还原");
        let restarted = MediaItemRepo::id_by_stable_key(conn, "src-1|影片.mp4")
            .expect("查条目")
            .expect("条目应还原");
        // 进度与已看还原
        let progress = PlaybackRepo::get(conn, &restarted).expect("读进度").expect("应有进度");
        assert_eq!(progress.position_ms, 500_000);
        assert!(PlaybackRepo::is_watched(conn, &restarted).expect("读已看"));
        // 收藏还原
        assert_eq!(read_collections(conn).unwrap().len(), 1);
        assert_eq!(read_collection_items(conn).unwrap().len(), 1);
        // 设置还原
        assert_eq!(
            crate::subtitle_repo::SettingRepo::get(conn, "multi_screen.max_screens")
                .unwrap()
                .as_deref(),
            Some("3")
        );
        // 生成字幕还原（字幕句无损）
        let tracks = SubtitleTrackRepo::list_by_item(conn, &restarted).expect("列字幕");
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].cues.len(), 1);
        assert_eq!(tracks[0].cues[0].text, "第一句");

        // 入库时间还原（最近添加排序依赖它）
        let stored = MediaItemRepo::get(conn, &restarted).unwrap().unwrap();
        assert_eq!(stored.created_at, MediaItemRepo::get(origin.conn(), &restarted).ok().flatten().and_then(|i| i.created_at), "入库时间按包还原");
    }

    /// 查某维度下是否存在指定名称的标签。
    fn fresh_tag_id(conn: &Connection, dimension: &str, name: &str) -> Option<String> {
        TagRepo::list_by_dimension(conn, dimension)
            .unwrap()
            .into_iter()
            .find(|t| t.name == name)
            .map(|t| t.id)
    }

    #[test]
    fn import_marks_unreachable_items_missing_instead_of_dropping() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export-missing");
        export_package(origin.conn(), &pkg, None).expect("导出");

        // 目标库：同名 id 的源但指向不存在的目录 → 条目标记缺失
        let fresh = migrated_db();
        MediaSourceRepo::upsert(
            fresh.conn(),
            &cinema_domain::model::MediaSource {
                id: SourceId::from("src-1"),
                kind: SourceKind::LocalFolder,
                name: "别的机器上的目录".into(),
                location: "Z:/不存在".into(),
                exclude_rules: None,
                state: SourceState::Idle,
                last_scan_at: None,
            },
        )
        .expect("写源");

        let outcome = import_package(fresh.conn(), &pkg, None).expect("导入");
        assert_eq!(outcome.items, 1, "条目不得丢弃");
        assert_eq!(outcome.items_missing, 1, "读不到文件应标记缺失");

        let id = MediaItemRepo::id_by_stable_key(fresh.conn(), "src-1|影片.mp4")
            .unwrap()
            .unwrap();
        assert!(MediaItemRepo::is_missing(fresh.conn(), &id).unwrap());
    }

    #[test]
    fn import_keeps_local_user_data_on_stable_key_conflict() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export-conflict");
        export_package(origin.conn(), &pkg, None).expect("导出");

        // 目标库已有同稳定键条目，且本机进度与包内不同
        let fresh = migrated_db();
        MediaSourceRepo::upsert(
            fresh.conn(),
            &cinema_domain::model::MediaSource {
                id: SourceId::from("src-1"),
                kind: SourceKind::LocalFolder,
                name: "本机目录".into(),
                location: "Z:/不存在".into(),
                exclude_rules: None,
                state: SourceState::Idle,
                last_scan_at: None,
            },
        )
        .expect("写源");
        let mut local = MediaItem::pending_fill("it-local", "src-1", "本机标题");
        local.stable_key = "src-1|影片.mp4".into();
        let local_id = MediaItemRepo::upsert(fresh.conn(), &local).expect("写本机条目");
        PlaybackRepo::put(
            fresh.conn(),
            &local_id,
            Progress {
                position_ms: 120_000,
                duration_ms: Some(600_000),
            },
        )
        .expect("写本机进度");

        import_package(fresh.conn(), &pkg, None).expect("导入");

        // 冲突以用户数据为准：本机标题与进度都不得被包覆盖
        let stored = MediaItemRepo::get(fresh.conn(), &local_id).unwrap().unwrap();
        assert_eq!(stored.title, "本机标题");
        let progress = PlaybackRepo::get(fresh.conn(), &local_id).unwrap().unwrap();
        assert_eq!(progress.position_ms, 120_000, "本机进度优先");
    }

    #[test]
    fn packaged_version_is_readable_and_non_package_is_rejected() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export-version");
        export_package(origin.conn(), &pkg, None).expect("导出");
        let version = packaged_schema_version(&pkg).expect("读版本");
        assert_eq!(version, crate::db::migration::MIGRATIONS.last().unwrap().version);

        // 非本应用的目录必须被拒绝（不静默改写）
        let bogus = temp_dir("export-bogus");
        std::fs::write(bogus.join("manifest.json"), b"{\"schemaVersion\":9}").unwrap();
        assert!(packaged_schema_version(&bogus).is_ok(), "版本探测只读一个字段");

        let empty = temp_dir("export-empty");
        assert!(read_manifest(&empty).is_err(), "缺少清单必须报错");
        let wrong = temp_dir("export-wrong");
        std::fs::write(wrong.join("manifest.json"), b"{\"format\":\"other\"}").unwrap();
        assert!(read_manifest(&wrong).is_err(), "标记不匹配必须报错");
    }

    #[test]
    fn export_records_no_volatile_state() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export-volatile");
        export_package(origin.conn(), &pkg, None).expect("导出");
        let text = std::fs::read_to_string(pkg.join("manifest.json")).unwrap();
        assert!(!text.contains("lastError"), "失败原因属运行期状态，不入包");
        assert!(!text.contains("lastScanAt"), "扫描时间不入包");
        assert!(!text.contains("missingAt"), "缺失标记属本机条件，不入包");
        assert!(!text.contains("\"state\""), "扫描中的状态不入包");
    }

    #[test]
    fn import_is_atomic_when_a_subtitle_resource_is_missing() {
        let (origin, _media) = seeded_db();
        let pkg = temp_dir("export-broken");
        export_package(origin.conn(), &pkg, None).expect("导出");
        // 破坏包：删掉字幕资源文件 → 导入必须整体失败且不留半成品
        std::fs::remove_file(pkg.join(RES_SUBTITLE_DIR).join("tr-1.json")).expect("删资源");

        let fresh = migrated_db();
        assert!(import_package(fresh.conn(), &pkg, None).is_err(), "缺资源必须失败");
        assert_eq!(
            MediaItemRepo::count(fresh.conn()).expect("统计"),
            0,
            "失败必须回滚：不得留下已写入的条目"
        );
        assert!(read_settings(fresh.conn()).unwrap().is_empty(), "失败必须回滚：设置也不得写入");
    }
}
