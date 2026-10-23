//! 前进式数据库迁移框架。
//!
//! 规则（架构文档 §8「迁移规则」）：
//! - **前进式**：只允许 `vN → vN+1`，逐级前进，禁止跳版；低版本数据文件一律引导升级。
//! - **迁移前备份**：执行迁移前自动复制数据库文件为带版本后缀的备份，保留最近 2 份；
//!   迁移失败则回滚并提示。
//! - **派生数据可重建**：派生标签 / FTS 索引 / 海报缓存允许清空重算。

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{InfraError, Result};
use crate::now_ms;

/// 一个迁移单元：只前进，带回滚的事务边界。
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// 目标版本号。必须严格等于「当前版本 + 1」。
    pub version: i64,
    pub name: &'static str,
    /// 迁移脚本原文；其哈希会写入 `schema_migration.checksum`。
    pub sql: &'static str,
}

/// 全部迁移，按版本升序。
///
/// 新增版本时只需在数组末尾追加一条，并把 `sql` 指向新的 `.sql` 文件。
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_init",
        sql: include_str!("../../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        name: "0002_scan_error",
        sql: include_str!("../../migrations/0002_scan_error.sql"),
    },
    Migration {
        version: 3,
        name: "0003_metadata",
        sql: include_str!("../../migrations/0003_metadata.sql"),
    },
    Migration {
        version: 4,
        name: "0004_highlight",
        sql: include_str!("../../migrations/0004_highlight.sql"),
    },
    Migration {
        version: 5,
        name: "0005_highlight_regen",
        sql: include_str!("../../migrations/0005_highlight_regen.sql"),
    },
    Migration {
        version: 6,
        name: "0006_highlight_regen2",
        sql: include_str!("../../migrations/0006_highlight_regen2.sql"),
    },
    Migration {
        version: 7,
        name: "0007_highlight_sources",
        sql: include_str!("../../migrations/0007_highlight_sources.sql"),
    },
    Migration {
        version: 8,
        name: "0008_highlight_regen3",
        sql: include_str!("../../migrations/0008_highlight_regen3.sql"),
    },
    Migration {
        version: 9,
        name: "0009_source_roots",
        sql: include_str!("../../migrations/0009_source_roots.sql"),
    },
];

/// 旧库自愈：`0003_metadata` 曾被改写成最终形态，但已登记 version=3 的库不会重跑，
/// 于是 `media_item` 可能缺 `external_code` 等列（启动时 SELECT 直接报错）。
/// 这里按「列是否存在」幂等判断，缺了就补跑一次 0003（ALTER + 建表都是新对象，可重复执行）。
pub fn ensure_metadata_schema(conn: &Connection) -> Result<()> {
    let present: i64 = conn.query_row(
        "SELECT count(*) FROM pragma_table_info('media_item') WHERE name = 'external_code'",
        [],
        |row| row.get(0),
    )?;
    if present > 0 {
        return Ok(());
    }
    conn.execute_batch(include_str!("../../migrations/0003_metadata.sql"))?;
    Ok(())
}

/// 迁移执行器。
pub struct MigrationRunner;

impl MigrationRunner {
    /// 当前 schema 版本；迁移表不存在时视为 0（全新数据库）。
    pub fn current_version(conn: &Connection) -> Result<i64> {
        let exists: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_migration'",
            rusqlite::params![],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Ok(0);
        }
        let version: Option<i64> = conn.query_row(
            "SELECT MAX(version) FROM schema_migration",
            rusqlite::params![],
            |row| row.get(0),
        )?;
        Ok(version.unwrap_or(0))
    }

    /// 确保版本表存在。
    fn ensure_table(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migration (
                 version    INTEGER PRIMARY KEY,
                 applied_at INTEGER NOT NULL,
                 checksum   TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    /// 应用全部待执行的迁移，返回最终版本号。
    ///
    /// `db_path` 为空（内存库）时跳过备份步骤。
    pub fn run(conn: &Connection, db_path: &Path, migrations: &[Migration]) -> Result<i64> {
        Self::ensure_table(conn)?;
        let current = Self::current_version(conn)?;

        let mut pending: Vec<&Migration> = migrations
            .iter()
            .filter(|m| m.version > current)
            .collect();
        pending.sort_by_key(|m| m.version);
        if pending.is_empty() {
            return Ok(current);
        }

        // 前进式：必须逐级 +1，禁止跳版
        let mut expected = current + 1;
        for migration in &pending {
            if migration.version != expected {
                return Err(InfraError::Migration(format!(
                    "迁移必须逐级前进：期望 v{expected}，实际发现 v{}（{}）",
                    migration.version, migration.name
                )));
            }
            expected += 1;
        }

        // 迁移前备份，保留最近 2 份
        if !db_path.as_os_str().is_empty() {
            Self::backup(db_path, current)?;
        }

        for migration in pending {
            if let Err(err) = Self::apply_one(conn, migration) {
                // 回滚已在 apply_one 内尝试；此处再兜底一次，避免留下半开事务
                let _ = conn.execute_batch("ROLLBACK;");
                return Err(err);
            }
        }

        Ok(expected - 1)
    }

    /// 单条迁移：在显式事务中执行脚本并登记版本行。
    fn apply_one(conn: &Connection, migration: &Migration) -> Result<()> {
        let script = format!(
            "BEGIN;\n{}\nINSERT INTO schema_migration (version, applied_at, checksum) \
             VALUES ({}, {}, '{}');\nCOMMIT;",
            migration.sql,
            migration.version,
            now_ms(),
            checksum(migration.sql),
        );
        conn.execute_batch(&script)?;
        Ok(())
    }

    /// 迁移前把数据库文件复制为 `{文件名}.v{版本}.{时间戳}.bak`，并只保留最近 `KEEP_BACKUPS` 份。
    fn backup(db_path: &Path, from_version: i64) -> Result<()> {
        if !db_path.exists() {
            // 首次创建：还没有任何用户数据可备份
            return Ok(());
        }
        let stem = db_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cinema.db");
        let backup_path = db_path.with_file_name(format!("{stem}.v{from_version}.{}.bak", now_ms()));
        std::fs::copy(db_path, &backup_path)?;
        Self::prune_backups(db_path, Self::KEEP_BACKUPS)?;
        Ok(())
    }

    /// 保留最近 2 份备份，其余删除。
    const KEEP_BACKUPS: usize = 2;

    fn prune_backups(db_path: &Path, keep: usize) -> Result<()> {
        let dir = db_path.parent().filter(|p| !p.as_os_str().is_empty());
        let dir = dir.unwrap_or_else(|| Path::new("."));
        let stem = db_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cinema.db");
        let prefix = format!("{stem}.v");

        let mut backups: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|s| s.to_str())
                    .map(|name| name.starts_with(&prefix) && name.ends_with(".bak"))
                    .unwrap_or(false)
            })
            .collect();

        // 按修改时间倒序，保留最近的 keep 份
        backups.sort_by_key(|path| std::fs::metadata(path).and_then(|m| m.modified()).ok());
        backups.reverse();
        for stale in backups.into_iter().skip(keep) {
            let _ = std::fs::remove_file(stale);
        }
        Ok(())
    }
}

/// FNV-1a 64 位哈希，十六进制。仅用于迁移完整性登记，不做安全用途。
fn checksum(sql: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in sql.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("cinema-migration-test-{tag}-{unique}.db"))
    }

    #[test]
    fn fresh_database_reaches_latest_version() {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        assert_eq!(db.schema_version().expect("读取版本"), 0, "全新库版本为 0");

        let version = db.migrate().expect("迁移应成功");
        assert_eq!(version, MIGRATIONS.last().map(|m| m.version).unwrap_or(0));
        assert_eq!(db.schema_version().expect("读取版本"), version);
    }

    #[test]
    fn migrate_is_idempotent() {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        let first = db.migrate().expect("首次迁移");
        let second = db.migrate().expect("重复迁移");
        assert_eq!(first, second, "重复迁移不得改变版本");
    }

    #[test]
    fn migration_creates_documented_tables() {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("迁移应成功");

        for table in ["media_source", "media_item", "tag_dimension", "item_tag", "playback_state"] {
            let count: i64 = db
                .conn()
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .expect("查询表存在性");
            assert_eq!(count, 1, "应存在表 {table}");
        }
    }

    #[test]
    fn version_gap_is_rejected() {
        let db = crate::db::Database::open_in_memory().expect("打开内存库");
        db.migrate().expect("先到最新版");
        let latest = MIGRATIONS.last().expect("至少一条迁移").version;

        // 构造一个跳版（最新版 + 2）的迁移，必须被拒绝
        let jumping = [Migration {
            version: latest + 2,
            name: "0099_jump",
            sql: "CREATE TABLE should_not_exist(id TEXT);",
        }];
        let empty_path = Path::new("");
        let err = MigrationRunner::run(db.conn(), empty_path, &jumping);
        assert!(err.is_err(), "跳版必须被拒绝");
        assert_eq!(db.schema_version().expect("版本不变"), latest);
    }

    #[test]
    fn backup_is_created_before_migration_and_pruned() {
        let path = temp_db_path("backup");
        let _ = std::fs::remove_file(&path);

        {
            let db = crate::db::Database::open(&path).expect("打开磁盘库");
            db.migrate().expect("首次迁移");
        }

        // 第二次「迁移」需要新版本；这里用一个比最新版更高的合成迁移触发备份逻辑。
        let latest = MIGRATIONS.last().expect("至少一条迁移").version;
        let mut migrations: Vec<Migration> = MIGRATIONS.to_vec();
        migrations.push(Migration {
            version: latest + 1,
            name: "0099_noop",
            sql: "CREATE TABLE IF NOT EXISTS t99(id TEXT);",
        });
        let conn = rusqlite::Connection::open(&path).expect("重开连接");
        let version = MigrationRunner::run(&conn, &path, &migrations).expect("应用合成迁移");
        assert_eq!(version, latest + 1);
        drop(conn);

        let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let dir = path.parent().unwrap_or(Path::new("."));
        let backups: Vec<_> = std::fs::read_dir(dir)
            .expect("列目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(&format!("{stem}.v")) && name.ends_with(".bak"))
            .collect();
        assert!(!backups.is_empty(), "迁移前必须生成备份");
        assert!(backups.len() <= MigrationRunner::KEEP_BACKUPS, "备份只保留最近 2 份");

        // 清理测试产物
        let _ = std::fs::remove_file(&path);
        for name in backups {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }

    /// 往「上一版本」的样本库里写入各类真实用户数据：媒体源、条目、用户标签、
    /// 观看进度、已生成字幕轨道、设置项。
    fn seed_previous_version_data(conn: &rusqlite::Connection) {
        conn.execute_batch(
            "INSERT INTO media_source (id, kind, name, location, state, last_scan_at)
                 VALUES ('src-1', 'lan_path', '客厅 NAS', 'smb://NAS/媒体', 'scan_failed', 1700000000000);
             INSERT INTO media_item (id, source_id, stable_key, rel_path, title, created_at, updated_at)
                 VALUES ('it-1', 'src-1', 'src-1|电影/样本.mp4', '电影/样本.mp4', '样本影片', 1700000000000, 1700000000000);
             INSERT INTO tag_dimension (id, label, selector, source_kind, order_idx)
                 VALUES ('dim-scene', '场景', 'multi', 'user', 9);
             INSERT INTO tag (id, dimension_id, name) VALUES ('tag-1', 'dim-scene', '客厅');
             INSERT INTO item_tag (item_id, tag_id, origin) VALUES ('it-1', 'tag-1', 'user');
             INSERT INTO playback_state (item_id, position_ms, duration_ms, watched, updated_at)
                 VALUES ('it-1', 615000, 5400000, 0, 1700000000000);
             INSERT INTO subtitle_track (id, item_id, kind, lang, source_engine, created_at, uri_or_blob)
                 VALUES ('sub-1', 'it-1', 'generated', 'zh', 'builtin-scripted', 1700000000000, 'it-1:0:1000:你好');
             INSERT INTO setting (key, value) VALUES ('realtime_subtitle.enabled', 'true');",
        )
        .expect("写入样本用户数据");
    }

    /// M5 验收（架构 §10）：用**上一版本**的数据库样本升级到当前版本后，
    /// 用户标签 / 观看进度 / 已生成字幕等用户数据必须全部存活。
    #[test]
    fn upgrade_from_previous_version_preserves_user_data() {
        let path = temp_db_path("upgrade");
        let _ = std::fs::remove_file(&path);

        let latest = MIGRATIONS.last().expect("至少一条迁移").version;
        let previous = latest - 1;
        assert!(previous >= 1, "升级用例要求至少存在一个上一版本");

        // 1) 造样本：只跑到上一版本，并写入用户数据
        {
            let db = crate::db::Database::open(&path).expect("打开磁盘库");
            let version = MigrationRunner::run(db.conn(), Path::new(""), &MIGRATIONS[..previous as usize])
                .expect("先建到上一版本");
            assert_eq!(version, previous, "样本库必须停在上一版本");
            seed_previous_version_data(db.conn());
        }

        // 2) 走真实升级路径（带文件路径 → 触发迁移前备份）
        let conn = rusqlite::Connection::open(&path).expect("重开连接");
        let version = MigrationRunner::run(&conn, &path, MIGRATIONS).expect("升级应成功");
        assert_eq!(version, latest, "升级后必须停在最新版本");

        // 3) 版本登记：逐级各一行
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM schema_migration",
                rusqlite::params![],
                |row| row.get(0),
            )
            .expect("统计版本登记行");
        assert_eq!(rows, MIGRATIONS.len() as i64, "每级迁移各登记一行");

        // 4) 新列已就位
        let has_column: i64 = conn
            .query_row(
                "SELECT count(*) FROM pragma_table_info('media_source') WHERE name = 'last_error'",
                rusqlite::params![],
                |row| row.get(0),
            )
            .expect("查询新列");
        assert_eq!(has_column, 1, "当前版本必须已加上 last_error 列");

        // 5) 用户数据全存活
        let (kind, name, location, state): (String, String, String, String) = conn
            .query_row(
                "SELECT kind, name, location, state FROM media_source WHERE id = 'src-1'",
                rusqlite::params![],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("媒体源必须存活");
        assert_eq!(kind, "lan_path", "源类型不得被升级改写");
        assert_eq!(name, "客厅 NAS");
        assert_eq!(location, "smb://NAS/媒体");
        assert_eq!(state, "scan_failed", "源状态不得被升级重置");

        let (title, rel_path): (String, String) = conn
            .query_row(
                "SELECT title, rel_path FROM media_item WHERE id = 'it-1'",
                rusqlite::params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("条目必须存活");
        assert_eq!(title, "样本影片");
        assert_eq!(rel_path, "电影/样本.mp4");

        let user_tags: i64 = conn
            .query_row(
                "SELECT count(*) FROM item_tag WHERE item_id = 'it-1' AND origin = 'user'",
                rusqlite::params![],
                |row| row.get(0),
            )
            .expect("统计用户标签");
        assert_eq!(user_tags, 1, "用户标签必须存活");

        let (position_ms, duration_ms): (i64, i64) = conn
            .query_row(
                "SELECT position_ms, duration_ms FROM playback_state WHERE item_id = 'it-1'",
                rusqlite::params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("观看进度必须存活");
        assert_eq!((position_ms, duration_ms), (615_000, 5_400_000));

        let (engine, blob): (String, String) = conn
            .query_row(
                "SELECT source_engine, uri_or_blob FROM subtitle_track WHERE id = 'sub-1'",
                rusqlite::params![],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("已生成字幕必须存活");
        assert_eq!(engine, "builtin-scripted");
        assert_eq!(blob, "it-1:0:1000:你好");

        let enabled: String = conn
            .query_row(
                "SELECT value FROM setting WHERE key = 'realtime_subtitle.enabled'",
                rusqlite::params![],
                |row| row.get(0),
            )
            .expect("设置项必须存活");
        assert_eq!(enabled, "true");

        drop(conn);

        // 6) 迁移前备份存在，且只保留最近 2 份
        let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let dir = path.parent().unwrap_or(Path::new("."));
        let backups: Vec<_> = std::fs::read_dir(dir)
            .expect("列目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.starts_with(&format!("{stem}.v")) && name.ends_with(".bak"))
            .collect();
        assert!(!backups.is_empty(), "升级前必须生成备份");
        assert!(
            backups.len() <= MigrationRunner::KEEP_BACKUPS,
            "备份只保留最近 2 份"
        );

        // 清理测试产物
        let _ = std::fs::remove_file(&path);
        for name in backups {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}
