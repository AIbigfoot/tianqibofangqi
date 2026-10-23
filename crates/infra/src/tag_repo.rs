//! L4 标签与检索仓储：维度 / 标签 / 条目标签 / 黑名单 / FTS 索引。
//!
//! 与 [`crate::repo`] 的分工一致：**仓储只做 SQL，不做流程判断**。
//! 「派生维度何时计算」「哪些取值被用户反对」这类判定全部在 L1/L2，本文件只负责落库。
//!
//! 通用性（硬约束 C6）：本文件**没有任何按维度硬编码的分支**——维度只是
//! `tag_dimension` 里的行，取值只是 `tag` 里的行，关系只是 `item_tag` 里的行。
//! 因此新增一个维度只需多一行数据，检索代码一行不动。

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension, Row};

use cinema_domain::error::PortError;
use cinema_domain::model::ItemId;
use cinema_domain::ports::tag_dimension::{Selector, TagDimensionDef, TagSource};
use cinema_domain::rules::ItemTags;
use cinema_domain::tagging::TagUsage;

use crate::error::Result;

// ---------------------------------------------------------------------------
// 枚举 ↔ 数据库文本
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

fn source_to_str(source: TagSource) -> &'static str {
    match source {
        TagSource::Metadata => "metadata",
        TagSource::Derived => "derived",
        TagSource::User => "user",
    }
}

fn source_from_str(raw: &str) -> TagSource {
    match raw {
        "derived" => TagSource::Derived,
        "user" => TagSource::User,
        _ => TagSource::Metadata,
    }
}

/// 条目标签来源（`item_tag.origin`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagOrigin {
    User,
    Metadata,
    Derived,
}

impl TagOrigin {
    fn to_str(self) -> &'static str {
        match self {
            TagOrigin::User => "user",
            TagOrigin::Metadata => "metadata",
            TagOrigin::Derived => "derived",
        }
    }
}

/// 别名编码：M0 采用换行分隔的最小形式，避免为基础设施引入额外依赖；
/// 具体编码由 L4 决定，不改变「按统一名称合并、保留原始写法」的规则语义。
fn aliases_encode(items: &[String]) -> Option<String> {
    if items.is_empty() {
        None
    } else {
        Some(items.join("\n"))
    }
}

fn aliases_decode(raw: Option<String>) -> Vec<String> {
    raw.map(|text| {
        text.split('\n')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// 由「维度 + 统一名称」派生稳定的标签 id：同一维度内同名标签永远同一行。
fn tag_id_for(dimension_id: &str, name: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dimension_id.as_bytes().iter().chain(b"|").chain(name.as_bytes()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("tg-{hash:016x}")
}

// ---------------------------------------------------------------------------
// tag_dimension
// ---------------------------------------------------------------------------

/// 维度定义仓储。
pub struct DimensionRepo;

impl DimensionRepo {
    /// 播种内置维度：**幂等**，已存在的 id 不覆盖（用户改过的配置优先保留）。
    pub fn seed(conn: &Connection, defs: &[TagDimensionDef]) -> Result<()> {
        for def in defs {
            conn.execute(
                "INSERT INTO tag_dimension
                     (id, label, selector, source_kind, order_idx, editable, lockable, derive_rule)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO NOTHING",
                params![
                    def.id,
                    def.label,
                    selector_to_str(def.selector),
                    source_to_str(def.source),
                    def.order,
                    i64::from(def.editable),
                    i64::from(def.lockable),
                    def.derive_rule,
                ],
            )?;
        }
        Ok(())
    }

    /// 全部维度，按 `order_idx` 升序（即 PRD 表格顺序）。
    pub fn list(conn: &Connection) -> Result<Vec<TagDimensionDef>> {
        let mut stmt = conn.prepare(
            "SELECT id, label, selector, source_kind, order_idx, editable, lockable, derive_rule
             FROM tag_dimension ORDER BY order_idx, id",
        )?;
        let rows = stmt.query_map(params![], read_dimension)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<TagDimensionDef>> {
        let found = conn
            .query_row(
                "SELECT id, label, selector, source_kind, order_idx, editable, lockable, derive_rule
                 FROM tag_dimension WHERE id = ?1",
                params![id],
                read_dimension,
            )
            .optional()?;
        Ok(found)
    }
}

fn read_dimension(row: &Row<'_>) -> rusqlite::Result<TagDimensionDef> {
    let selector: String = row.get(2)?;
    let source: String = row.get(3)?;
    Ok(TagDimensionDef {
        id: row.get(0)?,
        label: row.get(1)?,
        selector: selector_from_str(&selector),
        source: source_from_str(&source),
        expand: "value".to_owned(),
        order: row.get::<_, i64>(4)? as u32,
        editable: row.get::<_, i64>(5)? != 0,
        lockable: row.get::<_, i64>(6)? != 0,
        derive_rule: row.get(7)?,
    })
}

// ---------------------------------------------------------------------------
// tag
// ---------------------------------------------------------------------------

/// 一个标签取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    pub id: String,
    pub dimension_id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub locked: bool,
}

/// 标签仓储。
pub struct TagRepo;

impl TagRepo {
    /// 保证标签存在，返回其 id。**同义合并**：若同维度内已有标签的名称或别名等于
    /// `name`，则复用该行并把 `name` 记入别名（PRD FR-03「标签合并」）。
    pub fn ensure(conn: &Connection, dimension_id: &str, name: &str, aliases: &[String]) -> Result<String> {
        let canonical = name.trim();
        if canonical.is_empty() {
            return Err(PortError::Unsupported("标签名称不得为空".into()).into());
        }

        // 同维度内按「名称或别名」查找统一名称
        let existing = Self::find_by_name_or_alias(conn, dimension_id, canonical)?;
        if let Some(mut row) = existing {
            let mut merged = row.aliases.clone();
            for extra in std::iter::once(canonical.to_owned()).chain(aliases.iter().cloned()) {
                let extra = extra.trim().to_owned();
                if !extra.is_empty() && extra != row.name && !merged.contains(&extra) {
                    merged.push(extra);
                }
            }
            if merged != row.aliases {
                conn.execute(
                    "UPDATE tag SET aliases = ?2 WHERE id = ?1",
                    params![row.id, aliases_encode(&merged)],
                )?;
                row.aliases = merged;
            }
            return Ok(row.id);
        }

        let id = tag_id_for(dimension_id, canonical);
        conn.execute(
            "INSERT INTO tag (id, dimension_id, name, aliases, locked)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(id) DO NOTHING",
            params![id, dimension_id, canonical, aliases_encode(aliases)],
        )?;
        Ok(id)
    }

    /// 同维度内按统一名称或别名查找。
    fn find_by_name_or_alias(
        conn: &Connection,
        dimension_id: &str,
        name: &str,
    ) -> Result<Option<TagRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, dimension_id, name, aliases, locked FROM tag WHERE dimension_id = ?1",
        )?;
        let rows = stmt.query_map(params![dimension_id], read_tag)?;
        for row in rows {
            let row = row?;
            if row.name == name || row.aliases.iter().any(|a| a == name) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    pub fn get(conn: &Connection, id: &str) -> Result<Option<TagRow>> {
        let found = conn
            .query_row(
                "SELECT id, dimension_id, name, aliases, locked FROM tag WHERE id = ?1",
                params![id],
                read_tag,
            )
            .optional()?;
        Ok(found)
    }

    pub fn list_by_dimension(conn: &Connection, dimension_id: &str) -> Result<Vec<TagRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, dimension_id, name, aliases, locked FROM tag
             WHERE dimension_id = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![dimension_id], read_tag)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 某维度下的取值及使用频次，**按频次降序**（PRD FR-03 异常表：过多时按频次排序）。
    pub fn usage_by_dimension(conn: &Connection, dimension_id: &str) -> Result<Vec<TagUsage>> {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.name, count(it.item_id)
             FROM tag t
             LEFT JOIN item_tag it ON it.tag_id = t.id
             WHERE t.dimension_id = ?1
             GROUP BY t.id, t.name
             ORDER BY count(it.item_id) DESC, t.name",
        )?;
        let rows = stmt.query_map(params![dimension_id], |row| {
            Ok(TagUsage {
                tag_id: row.get(0)?,
                name: row.get(1)?,
                count: row.get::<_, i64>(2)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// 全库删除一个标签取值：清掉所有条目关联与黑名单，再删除标签行本身。
    ///
    /// 返回受影响的条目 id —— 调用方据此重建这些条目的搜索索引（索引里含标签名）。
    pub fn delete_value(conn: &Connection, tag_id: &str) -> Result<Vec<String>> {
        let affected = {
            let mut stmt = conn.prepare("SELECT item_id FROM item_tag WHERE tag_id = ?1")?;
            let rows = stmt.query_map(params![tag_id], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };
        conn.execute("DELETE FROM item_tag WHERE tag_id = ?1", params![tag_id])?;
        conn.execute(
            "DELETE FROM item_tag_blacklist WHERE tag_id = ?1",
            params![tag_id],
        )?;
        conn.execute("DELETE FROM tag WHERE id = ?1", params![tag_id])?;
        Ok(affected)
    }

    /// 用户锁定 / 解锁标签（PRD FR-03：元数据不得覆盖用户已锁定标签）。
    pub fn set_locked(conn: &Connection, tag_id: &str, locked: bool) -> Result<()> {
        conn.execute(
            "UPDATE tag SET locked = ?2 WHERE id = ?1",
            params![tag_id, i64::from(locked)],
        )?;
        Ok(())
    }
}

fn read_tag(row: &Row<'_>) -> rusqlite::Result<TagRow> {
    Ok(TagRow {
        id: row.get(0)?,
        dimension_id: row.get(1)?,
        name: row.get(2)?,
        aliases: aliases_decode(row.get(3)?),
        locked: row.get::<_, i64>(4)? != 0,
    })
}

// ---------------------------------------------------------------------------
// item_tag / item_tag_blacklist
// ---------------------------------------------------------------------------

/// 条目标签仓储。
pub struct ItemTagRepo;

impl ItemTagRepo {
    /// 挂上一个标签。`ON CONFLICT` 不覆盖已锁定行：用户显式选择优先。
    pub fn set(
        conn: &Connection,
        item_id: &ItemId,
        tag_id: &str,
        origin: TagOrigin,
        locked: bool,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO item_tag (item_id, tag_id, origin, locked)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(item_id, tag_id) DO UPDATE SET
                 origin = excluded.origin,
                 locked = max(item_tag.locked, excluded.locked)",
            params![item_id.as_str(), tag_id, origin.to_str(), i64::from(locked)],
        )?;
        Ok(())
    }

    pub fn remove(conn: &Connection, item_id: &ItemId, tag_id: &str) -> Result<()> {
        conn.execute(
            "DELETE FROM item_tag WHERE item_id = ?1 AND tag_id = ?2",
            params![item_id.as_str(), tag_id],
        )?;
        Ok(())
    }

    /// 重算某维度上的**派生**标签：先删除该维度上源为 `derived` 的行，再写入新取值。
    ///
    /// 只动 `origin='derived'` 的行 —— 用户手动添加与已锁定标签不受影响（PRD FR-03
    /// 「元数据源标签不得覆盖用户标签」在派生侧的同一条规则）。
    /// 命中黑名单的取值被跳过（用户显式反对后不再出现）。
    pub fn replace_derived(
        conn: &Connection,
        item_id: &ItemId,
        dimension_id: &str,
        values: &[String],
    ) -> Result<()> {
        conn.execute(
            "DELETE FROM item_tag
             WHERE item_id = ?1 AND origin = 'derived'
               AND tag_id IN (SELECT id FROM tag WHERE dimension_id = ?2)",
            params![item_id.as_str(), dimension_id],
        )?;
        for value in values {
            if value.trim().is_empty() {
                continue;
            }
            let tag_id = TagRepo::ensure(conn, dimension_id, value, &[])?;
            if Self::is_blacklisted(conn, item_id, &tag_id)? {
                continue;
            }
            Self::set(conn, item_id, &tag_id, TagOrigin::Derived, false)?;
        }
        Ok(())
    }

    /// 重算某维度上的**元数据**标签：替换元数据源写入的取值。
    ///
    /// 与 [`Self::replace_derived`] 同构，但只动 `origin='metadata'` 的行：
    /// 用户手动添加（`origin='user'`）、已锁定与黑名单标签都不受影响（PRD FR-03）。
    pub fn replace_metadata(
        conn: &Connection,
        item_id: &ItemId,
        dimension_id: &str,
        values: &[String],
    ) -> Result<()> {
        conn.execute(
            "DELETE FROM item_tag
             WHERE item_id = ?1 AND origin = 'metadata' AND locked = 0
               AND tag_id IN (SELECT id FROM tag WHERE dimension_id = ?2)",
            params![item_id.as_str(), dimension_id],
        )?;
        for value in values {
            if value.trim().is_empty() {
                continue;
            }
            let tag_id = TagRepo::ensure(conn, dimension_id, value, &[])?;
            if Self::is_blacklisted(conn, item_id, &tag_id)? {
                continue;
            }
            // 用户已手动选择同一取值：保留 `origin='user'`，刮削不得降级来源
            let existing: Option<String> = conn
                .query_row(
                    "SELECT origin FROM item_tag WHERE item_id = ?1 AND tag_id = ?2",
                    params![item_id.as_str(), tag_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some("user") {
                continue;
            }
            Self::set(conn, item_id, &tag_id, TagOrigin::Metadata, false)?;
        }
        Ok(())
    }

    /// 用户显式反对某个自动标签 → 进入该条目的标签黑名单。
    pub fn blacklist(conn: &Connection, item_id: &ItemId, tag_id: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO item_tag_blacklist (item_id, tag_id) VALUES (?1, ?2)
             ON CONFLICT(item_id, tag_id) DO NOTHING",
            params![item_id.as_str(), tag_id],
        )?;
        Self::remove(conn, item_id, tag_id)
    }

    pub fn is_blacklisted(conn: &Connection, item_id: &ItemId, tag_id: &str) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM item_tag_blacklist WHERE item_id = ?1 AND tag_id = ?2",
            params![item_id.as_str(), tag_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 该条目在某维度上是否已有标签。
    ///
    /// 入库流程据此避免用「未看」覆盖已被进度流程写成的「在看 / 已看」。
    pub fn has_on_dimension(
        conn: &Connection,
        item_id: &ItemId,
        dimension_id: &str,
    ) -> Result<bool> {
        let count: i64 = conn.query_row(
            "SELECT count(*)
             FROM item_tag it JOIN tag t ON t.id = it.tag_id
             WHERE it.item_id = ?1 AND t.dimension_id = ?2",
            params![item_id.as_str(), dimension_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 某条目的标签：`dimension_id -> 取值集合`，可直接交给
    /// [`cinema_domain::rules::matches_filters`]。
    pub fn of_item(conn: &Connection, item_id: &ItemId) -> Result<ItemTags> {
        let mut stmt = conn.prepare(
            "SELECT td.id, t.name
             FROM item_tag it
             JOIN tag t ON t.id = it.tag_id
             JOIN tag_dimension td ON td.id = t.dimension_id
             WHERE it.item_id = ?1",
        )?;
        let rows = stmt.query_map(params![item_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = ItemTags::new();
        for row in rows {
            let (dimension, name) = row?;
            out.entry(dimension).or_insert_with(Vec::new).push(name);
        }
        Ok(out)
    }

    /// 全库标签：`item_id -> ItemTags`。检索一次读全量，避免逐条目 N+1 查询。
    pub fn all(conn: &Connection) -> Result<BTreeMap<String, ItemTags>> {
        let mut stmt = conn.prepare(
            "SELECT it.item_id, td.id, t.name
             FROM item_tag it
             JOIN tag t ON t.id = it.tag_id
             JOIN tag_dimension td ON td.id = t.dimension_id",
        )?;
        let rows = stmt.query_map(params![], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut out: BTreeMap<String, ItemTags> = BTreeMap::new();
        for row in rows {
            let (item, dimension, name) = row?;
            out.entry(item)
                .or_default()
                .entry(dimension)
                .or_insert_with(Vec::new)
                .push(name);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// item_fts（FR-03 搜索）
// ---------------------------------------------------------------------------

/// FTS 索引仓储。索引内容 = 标题 + 简介 + 标签名（PRD FR-03 搜索框的匹配范围）。
pub struct SearchIndexRepo;

impl SearchIndexRepo {
    /// 依据库内当前内容重建某条目的索引行（幂等）。
    pub fn reindex_item(conn: &Connection, item_id: &ItemId) -> Result<()> {
        let content: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT title, overview FROM media_item WHERE id = ?1",
                params![item_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        // 条目已被删除：索引也不该留下
        let Some((title, overview)) = content else {
            Self::remove_item(conn, item_id)?;
            return Ok(());
        };

        let mut stmt = conn.prepare(
            "SELECT t.name FROM item_tag it JOIN tag t ON t.id = it.tag_id WHERE it.item_id = ?1",
        )?;
        let rows = stmt.query_map(params![item_id.as_str()], |row| row.get::<_, String>(0))?;
        let mut names = Vec::new();
        for row in rows {
            names.push(row?);
        }

        Self::remove_item(conn, item_id)?;
        // 建索引与查询使用同一套中日韩切分，否则子串检索两侧对不上
        conn.execute(
            "INSERT INTO item_fts (item_id, title, overview, tag_names) VALUES (?1, ?2, ?3, ?4)",
            params![
                item_id.as_str(),
                cjk_segment(&title),
                cjk_segment(&overview.unwrap_or_default()),
                cjk_segment(&names.join(" "))
            ],
        )?;
        Ok(())
    }

    pub fn remove_item(conn: &Connection, item_id: &ItemId) -> Result<()> {
        conn.execute(
            "DELETE FROM item_fts WHERE item_id = ?1",
            params![item_id.as_str()],
        )?;
        Ok(())
    }

    /// 文本匹配，返回命中的条目 id。空白查询返回空集合（调用方据此跳过文本约束）。
    ///
    /// 分词按空白切分后逐个加引号并相连，等价于 FTS5 的「全部词都必须出现」，
    /// 避免用户输入的 `-`、`*` 等 FTS 语法字符被当作运算符。
    pub fn match_ids(conn: &Connection, text: &str) -> Result<Vec<ItemId>> {
        let query = fts_query(text);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt =
            conn.prepare("SELECT item_id FROM item_fts WHERE item_fts MATCH ?1")?;
        let rows = stmt.query_map(params![query], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(ItemId(row?));
        }
        Ok(out)
    }
}

/// 把用户输入转成 FTS5 查询串：先做与建索引一致的切分，再逐词加引号并前缀匹配，
/// 词间为隐式 AND。
///
/// 引号避免用户输入里的 `-`、`*` 等字符被当作 FTS 运算符（内部引号加倍转义）。
fn fts_query(text: &str) -> String {
    cjk_segment(text)
        .split_whitespace()
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 中日韩文字切分：在**每个**中日韩字符后插入空格，使其各自成为一个 token。
///
/// 引入这一步是因为 unicode61 分词器把连续的中日韩文字当作一个整体 token
/// （「一段简介」不被拆开），因而「简介」这类子串无法命中。按字切分后，
/// 建索引与查询两侧都变成「单字 token 串」，子串检索即可用相邻 token 的 AND 表达。
/// 非中日韩文本（英文单词、数字）保持原样，前缀匹配继续生效。
/// `item_fts` 属可重建的派生数据，改变切分方式只需重算索引，不动用户数据。
fn cjk_segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for ch in text.chars() {
        out.push(ch);
        if is_cjk(ch) {
            out.push(' ');
        }
    }
    out
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF        // 平假名 / 片假名
        | 0x3400..=0x4DBF      // 中日韩统一表意文字扩展 A
        | 0x4E00..=0x9FFF      // 中日韩统一表意文字
        | 0xF900..=0xFAFF      // 兼容表意文字
        | 0xAC00..=0xD7AF      // 谚文音节
    )
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cinema_domain::model::{MediaItem, MediaSource, SourceId, SourceKind, SourceState};
    use cinema_domain::ports::tag_dimension::TagDimensionDef;
    use cinema_domain::rules::{matches_filters, FilterSelection};
    use cinema_domain::tagging::{DIM_DURATION, DIM_GENRE};

    use crate::repo::{MediaItemRepo, MediaSourceRepo};

    fn db() -> crate::db::Database {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("迁移应成功");
        db
    }

    fn seed_dimensions(db: &crate::db::Database) {
        DimensionRepo::seed(db.conn(), &TagDimensionDef::builtin_defaults()).expect("播种维度");
    }

    fn seed_source(db: &crate::db::Database) {
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
    }

    fn add_item(db: &crate::db::Database, id: &str, title: &str) -> ItemId {
        let mut item = MediaItem::pending_fill(id, "src-1", title);
        item.stable_key = format!("src-1|{id}.mp4");
        item.overview = Some("一段简介".into());
        MediaItemRepo::upsert(db.conn(), &item).expect("写入条目")
    }

    #[test]
    fn dimension_seed_is_idempotent_and_ordered() {
        let db = db();
        seed_dimensions(&db);
        seed_dimensions(&db); // 重复播种不得产生第二行

        let dims = DimensionRepo::list(db.conn()).expect("读取维度");
        assert_eq!(
            dims.len(),
            cinema_domain::ports::tag_dimension::TagDimensionDef::builtin_defaults().len()
        );
        for pair in dims.windows(2) {
            assert!(pair[0].order < pair[1].order, "必须按 order 升序返回");
        }
        assert_eq!(dims[0].id, DIM_GENRE);
    }

    #[test]
    fn synonyms_merge_into_one_tag_with_alias() {
        let db = db();
        seed_dimensions(&db);

        let first = TagRepo::ensure(db.conn(), DIM_GENRE, "科幻", &[]).expect("建标签");
        let merged = TagRepo::ensure(db.conn(), DIM_GENRE, "科幻", &["Sci-Fi".into()])
            .expect("同义合并");
        assert_eq!(first, merged, "同义写法必须落到同一行");

        let other_spelling = TagRepo::ensure(db.conn(), DIM_GENRE, "Sci-Fi", &[]).expect("按别名命中");
        assert_eq!(other_spelling, first, "别名写法也必须合并到统一名称");

        let rows = TagRepo::list_by_dimension(db.conn(), DIM_GENRE).expect("列标签");
        assert_eq!(rows.len(), 1, "同维度内不得出现重复取值");
        assert_eq!(rows[0].name, "科幻", "统一名称是首次登记的名称");
        assert!(rows[0].aliases.contains(&"Sci-Fi".to_owned()), "原始写法保留为别名");
    }

    #[test]
    fn derived_tags_replace_only_derived_rows() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let item = add_item(&db, "it-1", "样本");

        // 用户手动加一个同维度的标签并锁定
        let user_tag = TagRepo::ensure(db.conn(), DIM_DURATION, "我的分类", &[]).expect("用户标签");
        ItemTagRepo::set(db.conn(), &item, &user_tag, TagOrigin::User, true).expect("挂用户标签");

        ItemTagRepo::replace_derived(
            db.conn(),
            &item,
            DIM_DURATION,
            &["短片".to_owned()],
        )
        .expect("写派生标签");
        let tags = ItemTagRepo::of_item(db.conn(), &item).expect("读标签");
        assert_eq!(tags.get(DIM_DURATION).map(|v| v.len()), Some(2), "用户标签与派生标签共存");

        // 重算派生标签：旧派生值被替换，用户标签必须原样保留
        ItemTagRepo::replace_derived(
            db.conn(),
            &item,
            DIM_DURATION,
            &["长片".to_owned()],
        )
        .expect("重算派生标签");
        let tags = ItemTagRepo::of_item(db.conn(), &item).expect("读标签");
        let values = tags.get(DIM_DURATION).expect("该维度有取值");
        assert!(values.contains(&"我的分类".to_owned()), "用户标签不得被派生流程移除");
        assert!(!values.contains(&"短片".to_owned()), "旧派生值应被替换");
        assert!(values.contains(&"长片".to_owned()));
    }

    #[test]
    fn blacklisted_derived_value_is_not_rewritten() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let item = add_item(&db, "it-1", "样本");

        ItemTagRepo::replace_derived(db.conn(), &item, DIM_DURATION, &["短片".to_owned()])
            .expect("首次派生");
        let tag = TagRepo::list_by_dimension(db.conn(), DIM_DURATION).expect("列标签")[0]
            .id
            .clone();
        ItemTagRepo::blacklist(db.conn(), &item, &tag).expect("用户显式反对");

        let tags = ItemTagRepo::of_item(db.conn(), &item).expect("读标签");
        assert!(
            tags.get(DIM_DURATION).map(|v| v.is_empty()).unwrap_or(true),
            "被反对的标签应被移除"
        );

        // 再次派生同一取值：不得复活
        ItemTagRepo::replace_derived(db.conn(), &item, DIM_DURATION, &["短片".to_owned()])
            .expect("再次派生");
        let tags = ItemTagRepo::of_item(db.conn(), &item).expect("读标签");
        assert!(
            tags.get(DIM_DURATION).map(|v| v.is_empty()).unwrap_or(true),
            "进入黑名单的取值不得被重新自动写入"
        );
    }

    #[test]
    fn tag_usage_is_sorted_by_frequency() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let a = add_item(&db, "it-a", "A");
        let b = add_item(&db, "it-b", "B");
        let c = add_item(&db, "it-c", "C");

        let hot = TagRepo::ensure(db.conn(), DIM_GENRE, "热门", &[]).expect("建标签");
        let rare = TagRepo::ensure(db.conn(), DIM_GENRE, "冷门", &[]).expect("建标签");
        for item in [&a, &b] {
            ItemTagRepo::set(db.conn(), item, &hot, TagOrigin::Metadata, false).expect("挂标签");
        }
        ItemTagRepo::set(db.conn(), &c, &rare, TagOrigin::Metadata, false).expect("挂标签");

        let usage = TagRepo::usage_by_dimension(db.conn(), DIM_GENRE).expect("统计");
        assert_eq!(usage[0].name, "热门");
        assert_eq!(usage[0].count, 2, "频次高的排在前面");
        assert_eq!(usage[1].name, "冷门");
        assert_eq!(usage[1].count, 1);
    }

    #[test]
    fn fts_search_matches_title_overview_and_tag_names() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let item = add_item(&db, "it-1", "银翼杀手");
        let tag = TagRepo::ensure(db.conn(), DIM_GENRE, "赛博朋克", &[]).expect("建标签");
        ItemTagRepo::set(db.conn(), &item, &tag, TagOrigin::User, false).expect("挂标签");
        SearchIndexRepo::reindex_item(db.conn(), &item).expect("建索引");

        for query in ["银翼", "简介", "赛博"] {
            let hits = SearchIndexRepo::match_ids(db.conn(), query).expect("检索");
            assert_eq!(hits, vec![item.clone()], "查询 {query} 应命中标题/简介/标签名");
        }
        // 多词为 AND：只命中其中一个词时不得返回
        assert!(SearchIndexRepo::match_ids(db.conn(), "银翼 不存在的词")
            .expect("检索")
            .is_empty());
        // 空查询不加约束
        assert!(SearchIndexRepo::match_ids(db.conn(), "   ").expect("检索").is_empty());
    }

    #[test]
    fn new_dimension_needs_no_retrieval_code_change() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let item = add_item(&db, "it-1", "样本");

        // 只用数据新增一个维度（模拟「改配置」：tag_dimension 增一行）
        DimensionRepo::seed(
            db.conn(),
            &[TagDimensionDef {
                id: "mood".into(),
                label: "心情".into(),
                selector: Selector::Multi,
                source: TagSource::User,
                expand: "value".into(),
                order: 99,
                editable: true,
                lockable: true,
                derive_rule: None,
            }],
        )
        .expect("新增维度");

        let tag = TagRepo::ensure(db.conn(), "mood", "放松", &[]).expect("新维度标签");
        ItemTagRepo::set(db.conn(), &item, &tag, TagOrigin::User, false).expect("挂标签");

        // 检索侧一行未改：筛选语义（同维度 OR / 跨维度 AND）由 L2 统一实现
        let tags = ItemTagRepo::of_item(db.conn(), &item).expect("读标签");
        let mut selection = FilterSelection::new();
        selection.insert("mood".into(), vec!["放松".into()]);
        assert!(matches_filters(&selection, &tags), "新维度天然获得正确筛选语义");

        selection.insert(DIM_GENRE.into(), vec!["不存在的类型".into()]);
        assert!(!matches_filters(&selection, &tags), "跨维度仍为与关系");

        // 该维度的取值也照常出现在检索区清单里
        let usage = TagRepo::usage_by_dimension(db.conn(), "mood").expect("统计");
        assert_eq!(usage[0].name, "放松");
    }

    #[test]
    fn all_item_tags_returns_whole_library_map() {
        let db = db();
        seed_dimensions(&db);
        seed_source(&db);
        let item = add_item(&db, "it-1", "样本");
        let tag = TagRepo::ensure(db.conn(), DIM_GENRE, "动作", &[]).expect("建标签");
        ItemTagRepo::set(db.conn(), &item, &tag, TagOrigin::Metadata, false).expect("挂标签");

        let all = ItemTagRepo::all(db.conn()).expect("全库标签");
        assert_eq!(all.len(), 1);
        assert_eq!(all["it-1"][DIM_GENRE], vec!["动作".to_owned()]);

        // 维度存在性判定：入库流程用它避免覆盖既有观看状态
        assert!(ItemTagRepo::has_on_dimension(db.conn(), &item, DIM_GENRE).unwrap());
        assert!(!ItemTagRepo::has_on_dimension(db.conn(), &item, DIM_DURATION).unwrap());
    }
}
