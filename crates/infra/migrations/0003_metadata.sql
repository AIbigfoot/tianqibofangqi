-- 0003_metadata：JavDB 刮削元数据（FR-02 详情页）
--
-- 设计取舍：
-- - 标量与状态直接扩展 media_item（重扫的 UPSERT 不写这些列，避免覆盖刮削结果）；
-- - 截图 / 评论是一对多，单独建表并由外键级联；
-- - 演员 / 导演 / 片商 / 系列 / 类别复用通用标签维度，不另建表。

ALTER TABLE media_item ADD COLUMN external_code TEXT;        -- 番号，如 SSIS-001
ALTER TABLE media_item ADD COLUMN title_origin TEXT;         -- 原文标题
ALTER TABLE media_item ADD COLUMN release_date TEXT;         -- YYYY-MM-DD
ALTER TABLE media_item ADD COLUMN detail_url TEXT;           -- 数据源详情页，重刮用
ALTER TABLE media_item ADD COLUMN trailer_file TEXT;         -- trailers/ 下的本地文件名
ALTER TABLE media_item ADD COLUMN trailer_url TEXT;          -- 预告片来源 URL
ALTER TABLE media_item ADD COLUMN trailer_fetched_at INTEGER;
ALTER TABLE media_item ADD COLUMN scraped_at INTEGER;
ALTER TABLE media_item ADD COLUMN scrape_state TEXT NOT NULL DEFAULT 'pending';
ALTER TABLE media_item ADD COLUMN scrape_error TEXT;

CREATE INDEX idx_media_item_code ON media_item (external_code);

CREATE TABLE item_screenshot (
    item_id    TEXT    NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    ord        INTEGER NOT NULL,
    file_name  TEXT    NOT NULL,           -- shots/ 下文件名
    source_url TEXT,
    PRIMARY KEY (item_id, ord)
);

CREATE TABLE item_comment (
    item_id    TEXT    NOT NULL REFERENCES media_item (id) ON DELETE CASCADE,
    ord        INTEGER NOT NULL,            -- 已按赞数降序后的展示序号
    author     TEXT,
    score      INTEGER,
    content    TEXT    NOT NULL,
    likes      INTEGER NOT NULL DEFAULT 0,
    posted_at  TEXT,
    PRIMARY KEY (item_id, ord)
);
