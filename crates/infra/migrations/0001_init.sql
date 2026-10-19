-- 0001_init：初始表结构（架构文档 §8「数据模型与迁移」）
--
-- 面向硬约束 C6（增量演进）：对「维度与取值」做通用建模，
-- 新增标签维度或新来源不需要改表、不需要改检索代码。
--
-- 注意：schema_migration 表由迁移框架自行创建，本脚本不重复定义。

-- ---------------------------------------------------------------------------
-- 源与条目
-- ---------------------------------------------------------------------------
CREATE TABLE media_source (
    id            TEXT PRIMARY KEY,
    kind          TEXT    NOT NULL,
    name          TEXT    NOT NULL,
    location      TEXT    NOT NULL,
    exclude_rules TEXT,                      -- JSON
    state         TEXT    NOT NULL DEFAULT 'idle',
    last_scan_at  INTEGER
);

CREATE TABLE media_item (
    id             TEXT PRIMARY KEY,
    source_id      TEXT    NOT NULL REFERENCES media_source (id) ON DELETE CASCADE,
    stable_key     TEXT    NOT NULL UNIQUE,  -- 源 + 规范化相对路径 / 规范化 URL
    rel_path       TEXT,
    url            TEXT,
    title          TEXT    NOT NULL,
    year           INTEGER,
    duration_ms    INTEGER,
    rating         REAL,
    overview       TEXT,
    poster_id      TEXT,
    size_bytes     INTEGER,
    mtime          INTEGER,
    identify_state TEXT    NOT NULL DEFAULT 'pending',
    meta_state     TEXT    NOT NULL DEFAULT 'pending_fill',
    field_locks    TEXT,                     -- JSON
    missing_at     INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX idx_media_item_source ON media_item (source_id);
CREATE INDEX idx_media_item_missing ON media_item (missing_at);

-- 「另一版本」互相标记
CREATE TABLE media_item_version (
    item_id       TEXT NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    other_item_id TEXT NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, other_item_id)
);

-- ---------------------------------------------------------------------------
-- 标签：通用维度模型（新增维度零改表）
-- ---------------------------------------------------------------------------
CREATE TABLE tag_dimension (
    id          TEXT PRIMARY KEY,
    label       TEXT    NOT NULL,
    selector    TEXT    NOT NULL,            -- single | multi
    source_kind TEXT    NOT NULL,            -- metadata | derived | user
    order_idx   INTEGER NOT NULL DEFAULT 0,
    editable    INTEGER NOT NULL DEFAULT 1,
    lockable    INTEGER NOT NULL DEFAULT 1,
    derive_rule TEXT
);

CREATE TABLE tag (
    id           TEXT PRIMARY KEY,
    dimension_id TEXT    NOT NULL REFERENCES tag_dimension (id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    aliases      TEXT,                       -- JSON
    locked       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_tag_dimension ON tag (dimension_id);

CREATE TABLE item_tag (
    item_id TEXT    NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    tag_id  TEXT    NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
    origin  TEXT    NOT NULL,                -- user | metadata | derived
    locked  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (item_id, tag_id)
);

-- 用户明确反对的自动标签进入该条目的黑名单
CREATE TABLE item_tag_blacklist (
    item_id TEXT NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    tag_id  TEXT NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, tag_id)
);

-- ---------------------------------------------------------------------------
-- 字幕与播放
-- ---------------------------------------------------------------------------
CREATE TABLE subtitle_track (
    id            TEXT PRIMARY KEY,
    item_id       TEXT    NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL,           -- external | embedded | generated
    lang          TEXT,
    source_engine TEXT,
    locked        INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    uri_or_blob   TEXT
);
CREATE INDEX idx_subtitle_track_item ON subtitle_track (item_id);

-- 进度按条目维度持久化；多屏不新增表
CREATE TABLE playback_state (
    item_id     TEXT PRIMARY KEY REFERENCES media_item (id) ON DELETE CASCADE,
    position_ms INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER,
    watched     INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL
);

-- ---------------------------------------------------------------------------
-- 收藏、设置、检索
-- ---------------------------------------------------------------------------
CREATE TABLE collection (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    sort_mode TEXT NOT NULL DEFAULT 'manual'
);

CREATE TABLE collection_item (
    collection_id TEXT    NOT NULL REFERENCES collection (id) ON DELETE CASCADE,
    item_id       TEXT    NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    order_idx     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (collection_id, item_id)
);

-- 含功能开关、隐私授权、引擎选择
CREATE TABLE setting (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL                      -- JSON
);

-- 注册表启停与优先级（provider / dimension 条目）
CREATE TABLE engine_registry_config (
    id       TEXT PRIMARY KEY,
    kind     TEXT    NOT NULL,               -- media_source | metadata | subtitle | tag_dimension
    enabled  INTEGER NOT NULL DEFAULT 1,
    priority INTEGER NOT NULL DEFAULT 0,
    config   TEXT
);

-- FTS5 支撑 FR-03 搜索；需要 SQLite 以 FTS5 编译（rusqlite 的 bundled 特性已启用）
CREATE VIRTUAL TABLE item_fts USING fts5 (
    item_id UNINDEXED,
    title,
    overview,
    tag_names
);
