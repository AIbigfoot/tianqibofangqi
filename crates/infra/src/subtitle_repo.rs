//! L4 字幕轨道与设置仓储。
//!
//! 与 [`crate::repo`] / [`crate::tag_repo`] 的分工一致：**仓储只做 SQL，不做流程判断**。
//! 「入口是否出现」「什么时候降级」「失败后给哪三个出口」这类判定全部在 L1/L2，
//! 本文件只负责把 [`SubtitleTrack`] 与设置读写落库。
//!
//! 编码说明：字幕句存放在 `subtitle_track.uri_or_blob` 这一列里，采用
//! **换行分句 + Tab 分字段**的行文本（文本内的 `\`、Tab、换行做转义），
//! 而不是 JSON —— 目的是不为基础设施层引入额外序列化依赖，同时保证往返无损。

use rusqlite::{params, Connection, OptionalExtension, Row};

use cinema_domain::model::{ItemId, SubtitleCue, SubtitleKind, SubtitleTrack};

use crate::error::Result;
use crate::now_ms;

// ---------------------------------------------------------------------------
// 枚举 ↔ 数据库文本
// ---------------------------------------------------------------------------

fn kind_to_str(kind: SubtitleKind) -> &'static str {
    match kind {
        SubtitleKind::External => "external",
        SubtitleKind::Embedded => "embedded",
        SubtitleKind::Generated => "generated",
    }
}

fn kind_from_str(raw: &str) -> SubtitleKind {
    match raw {
        "external" => SubtitleKind::External,
        "embedded" => SubtitleKind::Embedded,
        _ => SubtitleKind::Generated,
    }
}

// ---------------------------------------------------------------------------
// 字幕句文本编码
// ---------------------------------------------------------------------------

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// 字幕句 → 行文本。
fn cues_encode(cues: &[SubtitleCue]) -> String {
    cues.iter()
        .map(|cue| {
            format!(
                "{}\t{}\t{}\t{}",
                cue.start_ms,
                cue.end_ms,
                cue.lang.as_deref().unwrap_or(""),
                escape(&cue.text)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 行文本 → 字幕句；无法解析的行整行忽略（不猜测）。
fn cues_decode(raw: &str) -> Vec<SubtitleCue> {
    raw.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let start_ms: i64 = parts.next()?.trim().parse().ok()?;
            let end_ms: i64 = parts.next()?.trim().parse().ok()?;
            let lang = parts.next().unwrap_or("").trim();
            let text = unescape(parts.next().unwrap_or(""));
            Some(SubtitleCue {
                start_ms,
                end_ms,
                text,
                lang: (!lang.is_empty()).then(|| lang.to_owned()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// subtitle_track
// ---------------------------------------------------------------------------

/// 字幕轨道仓储。
pub struct SubtitleTrackRepo;

impl SubtitleTrackRepo {
    /// 写入或更新一条轨道。`created_at` 首次写入时为当前时间，更新时保持不变。
    pub fn upsert(conn: &Connection, track: &SubtitleTrack) -> Result<()> {
        conn.execute(
            "INSERT INTO subtitle_track
                 (id, item_id, kind, lang, source_engine, locked, created_at, uri_or_blob)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                 item_id       = excluded.item_id,
                 kind          = excluded.kind,
                 lang          = excluded.lang,
                 source_engine = excluded.source_engine,
                 locked        = excluded.locked,
                 uri_or_blob   = excluded.uri_or_blob",
            params![
                track.id,
                track.item_id.as_str(),
                kind_to_str(track.kind),
                track.lang,
                track.source_engine,
                i64::from(track.locked),
                now_ms(),
                cues_encode(&track.cues),
            ],
        )?;
        Ok(())
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<SubtitleTrack>> {
        let found = conn
            .query_row(
                "SELECT id, item_id, kind, lang, source_engine, locked, uri_or_blob
                 FROM subtitle_track WHERE id = ?1",
                params![id],
                read_track,
            )
            .optional()?;
        Ok(found)
    }

    /// 某条目的全部轨道。响应「复用已保存轨道」：按写入顺序返回，含全部字幕句。
    pub fn list_by_item(conn: &Connection, item_id: &ItemId) -> Result<Vec<SubtitleTrack>> {
        let mut stmt = conn.prepare(
            "SELECT id, item_id, kind, lang, source_engine, locked, uri_or_blob
             FROM subtitle_track WHERE item_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map(params![item_id.as_str()], read_track)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 某条目是否已有指定类型的轨道。
    ///
    /// L1 用它判定「入口是否出现」（有无外挂/内嵌轨道）与派生字幕状态。
    pub fn exists_kind(
        conn: &Connection,
        item_id: &ItemId,
        kind: SubtitleKind,
    ) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM subtitle_track WHERE item_id = ?1 AND kind = ?2",
            params![item_id.as_str(), kind_to_str(kind)],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn delete(conn: &Connection, id: &str) -> Result<()> {
        conn.execute("DELETE FROM subtitle_track WHERE id = ?1", params![id])?;
        Ok(())
    }
}

fn read_track(row: &Row<'_>) -> rusqlite::Result<SubtitleTrack> {
    let kind: String = row.get(2)?;
    let blob: Option<String> = row.get(6)?;
    Ok(SubtitleTrack {
        id: row.get(0)?,
        item_id: ItemId(row.get(1)?),
        kind: kind_from_str(&kind),
        lang: row.get(3)?,
        source_engine: row.get(4)?,
        locked: row.get::<_, i64>(5)? != 0,
        cues: blob.map(|raw| cues_decode(&raw)).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// setting
// ---------------------------------------------------------------------------

/// 设置仓储：`setting(key, value)`，`value` 为 JSON 文本。
///
/// 承载功能开关、隐私授权、引擎选择等「默认关闭 / 默认不授权」的显式配置。
pub struct SettingRepo;

impl SettingRepo {
    pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
        let found = conn
            .query_row(
                "SELECT value FROM setting WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(found)
    }

    pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// 读布尔设置；键不存在或取值无法识别时返回 `default`（不猜测、不静默开启）。
    pub fn get_bool(conn: &Connection, key: &str, default: bool) -> Result<bool> {
        match Self::get(conn, key)? {
            None => Ok(default),
            Some(raw) => Ok(match raw.trim() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => default,
            }),
        }
    }

    /// 写布尔设置，落库为合法 JSON 字面量 `true` / `false`。
    pub fn set_bool(conn: &Connection, key: &str, value: bool) -> Result<()> {
        Self::set(conn, key, if value { "true" } else { "false" })
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cinema_domain::model::{MediaItem, MediaSource, SourceId, SourceKind, SourceState};

    use crate::repo::{MediaItemRepo, MediaSourceRepo};

    fn db() -> crate::db::Database {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("迁移应成功");
        db
    }

    fn add_item(db: &crate::db::Database, id: &str) -> ItemId {
        MediaSourceRepo::upsert(
            db.conn(),
            &MediaSource {
                id: SourceId::from("src-1"),
                kind: SourceKind::LocalFolder,
                name: "我的电影".into(),
                location: "D:/media".into(),
                exclude_rules: None,
                state: SourceState::Idle,
                last_scan_at: None,
            },
        )
        .expect("写入源");
        let mut item = MediaItem::pending_fill(id, "src-1", "样本");
        item.stable_key = format!("src-1|{id}.mp4");
        MediaItemRepo::upsert(db.conn(), &item).expect("写入条目")
    }

    fn track(id: &str, item: &ItemId, kind: SubtitleKind, cues: Vec<SubtitleCue>) -> SubtitleTrack {
        SubtitleTrack {
            id: id.into(),
            item_id: item.clone(),
            kind,
            lang: Some("zh".into()),
            source_engine: Some("local-whisper".into()),
            locked: false,
            cues,
        }
    }

    #[test]
    fn subtitle_track_roundtrip_preserves_cues() {
        let db = db();
        let item = add_item(&db, "it-1");
        let cues = vec![
            SubtitleCue {
                start_ms: 0,
                end_ms: 1000,
                text: "第一行\n第二行".into(),
                lang: Some("en".into()),
            },
            SubtitleCue {
                start_ms: 1000,
                end_ms: 2000,
                text: "含\t制表符与\\反斜杠".into(),
                lang: Some("zh".into()),
            },
        ];
        let saved = track("tr-1", &item, SubtitleKind::Generated, cues.clone());
        SubtitleTrackRepo::upsert(db.conn(), &saved).expect("写入轨道");

        let loaded = SubtitleTrackRepo::get(db.conn(), "tr-1")
            .expect("读取轨道")
            .expect("轨道存在");
        assert_eq!(loaded, saved, "换行与制表符必须无损往返");

        // 重复 upsert 不改变 created_at 之外的内容，且不产生第二行
        SubtitleTrackRepo::upsert(db.conn(), &saved).expect("再次写入");
        let all = SubtitleTrackRepo::list_by_item(db.conn(), &item).expect("列轨道");
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn exists_kind_reports_item_track_kinds() {
        let db = db();
        let item = add_item(&db, "it-1");
        assert!(!SubtitleTrackRepo::exists_kind(db.conn(), &item, SubtitleKind::External).unwrap());
        assert!(!SubtitleTrackRepo::exists_kind(db.conn(), &item, SubtitleKind::Generated).unwrap());

        SubtitleTrackRepo::upsert(
            db.conn(),
            &track("tr-gen", &item, SubtitleKind::Generated, Vec::new()),
        )
        .expect("写生成轨道");

        assert!(SubtitleTrackRepo::exists_kind(db.conn(), &item, SubtitleKind::Generated).unwrap());
        assert!(!SubtitleTrackRepo::exists_kind(db.conn(), &item, SubtitleKind::External).unwrap());
    }

    #[test]
    fn external_and_generated_kinds_roundtrip() {
        let db = db();
        let item = add_item(&db, "it-1");
        SubtitleTrackRepo::upsert(
            db.conn(),
            &track("tr-ext", &item, SubtitleKind::External, Vec::new()),
        )
        .expect("写外挂轨道");
        let loaded = SubtitleTrackRepo::get(db.conn(), "tr-ext")
            .unwrap()
            .expect("读外挂轨道");
        assert_eq!(loaded.kind, SubtitleKind::External);
    }

    #[test]
    fn delete_removes_track() {
        let db = db();
        let item = add_item(&db, "it-1");
        SubtitleTrackRepo::upsert(
            db.conn(),
            &track("tr-1", &item, SubtitleKind::Generated, Vec::new()),
        )
        .expect("写入轨道");
        SubtitleTrackRepo::delete(db.conn(), "tr-1").expect("删除轨道");
        assert!(SubtitleTrackRepo::get(db.conn(), "tr-1").unwrap().is_none());
        assert!(SubtitleTrackRepo::list_by_item(db.conn(), &item).unwrap().is_empty());
    }

    #[test]
    fn setting_bool_defaults_when_absent_and_roundtrips() {
        let db = db();
        // 功能开关默认关闭（架构 §9）：键不存在时取默认
        assert!(!SettingRepo::get_bool(db.conn(), "realtime_subtitle.enabled", false).unwrap());
        assert!(SettingRepo::get_bool(db.conn(), "realtime_subtitle.enabled", true).unwrap());

        SettingRepo::set_bool(db.conn(), "realtime_subtitle.enabled", true).expect("开启");
        assert!(SettingRepo::get_bool(db.conn(), "realtime_subtitle.enabled", false).unwrap());
        assert_eq!(
            SettingRepo::get(db.conn(), "realtime_subtitle.enabled").unwrap().as_deref(),
            Some("true"),
            "落库为合法 JSON 字面量"
        );

        SettingRepo::set(db.conn(), "subtitle.engine", "\"local-whisper\"").expect("写引擎选择");
        assert_eq!(
            SettingRepo::get(db.conn(), "subtitle.engine").unwrap().as_deref(),
            Some("\"local-whisper\"")
        );

        // 无法识别的取值回落默认，不得静默开启
        SettingRepo::set(db.conn(), "weird", "maybe").expect("写怪值");
        assert!(!SettingRepo::get_bool(db.conn(), "weird", false).unwrap());
    }
}
