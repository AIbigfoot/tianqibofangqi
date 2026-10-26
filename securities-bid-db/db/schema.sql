-- ============================================================
-- 证券公司公开招标信息特色数据库 - 完整 DDL
-- PostgreSQL 15+
-- ============================================================

CREATE EXTENSION IF NOT EXISTS "pg_trgm";
CREATE EXTENSION IF NOT EXISTS "btree_gin";

-- ------------------------------------------------------------
-- 1. source_site 数据源配置
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS source_site (
    id              BIGSERIAL PRIMARY KEY,
    source_key      VARCHAR(64) UNIQUE NOT NULL,
    name            VARCHAR(128) NOT NULL,
    source_type     VARCHAR(16) NOT NULL,          -- static/dynamic/api
    entry_url       TEXT NOT NULL,
    domain          VARCHAR(255),
    config          JSONB,                          -- 完整 YAML 配置
    enabled         BOOLEAN DEFAULT TRUE,
    rate_limit      INTEGER DEFAULT 3,
    created_at      TIMESTAMPTZ DEFAULT now(),
    updated_at      TIMESTAMPTZ DEFAULT now()
);
COMMENT ON TABLE source_site IS '数据源配置';

-- ------------------------------------------------------------
-- 2. raw_page 原始抓取快照
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS raw_page (
    id              BIGSERIAL PRIMARY KEY,
    source_id       BIGINT NOT NULL REFERENCES source_site(id),
    url             TEXT NOT NULL,
    http_status     INTEGER,
    content_type    VARCHAR(128),
    raw_html        TEXT,
    snapshot_path   TEXT,                           -- 本地快照文件路径
    fetched_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_raw_page_source_url ON raw_page(source_id, url);
COMMENT ON TABLE raw_page IS '原始页面快照';

-- ------------------------------------------------------------
-- 3. notice 公告主表（按月分区）
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS notice (
    id                  BIGSERIAL,
    source_id           BIGINT NOT NULL REFERENCES source_site(id),
    notice_id           VARCHAR(255) NOT NULL,       -- 源站唯一键
    title               TEXT NOT NULL,
    project_no          VARCHAR(128),
    project_name        TEXT,
    procurement_unit    VARCHAR(255),                -- 采购单位
    agency              VARCHAR(255),                -- 招标代理机构
    notice_type         VARCHAR(32),                 -- 公告类型
    procurement_method  VARCHAR(32),                 -- 采购方式
    org_form            VARCHAR(16),                 -- 自行招标/委托代理
    budget_original     TEXT,                         -- 预算原值
    budget_value        NUMERIC(18,2),               -- 预算数值（元）
    budget_currency     VARCHAR(8) DEFAULT 'CNY',
    max_price           NUMERIC(18,2),               -- 最高限价
    fund_source         VARCHAR(128),                -- 资金来源
    package_count       INTEGER,                      -- 包件数量
    publish_time        TIMESTAMPTZ,
    signup_deadline     TIMESTAMPTZ,
    bid_deadline        TIMESTAMPTZ,
    open_time           TIMESTAMPTZ,
    project_location    VARCHAR(255),
    bidder_qualification TEXT,
    contact_person      VARCHAR(128),
    contact_phone       VARCHAR(64),
    contact_email       VARCHAR(128),
    content             TEXT,                         -- 正文全文
    content_tsv         TSVECTOR,                     -- 全文检索向量
    original_url        TEXT,
    snapshot_path       TEXT,
    is_change           BOOLEAN DEFAULT FALSE,
    change_mark         VARCHAR(64),
    related_notice_ids  JSONB,                        -- 关联公告 ID 列表
    project_id          BIGINT,                       -- 关联 project（可空，后串联）
    review_required     BOOLEAN DEFAULT FALSE,
    raw_page_id         BIGINT REFERENCES raw_page(id),
    dedup_hash          VARCHAR(64),
    created_at          TIMESTAMPTZ DEFAULT now(),
    updated_at          TIMESTAMPTZ DEFAULT now(),
    PRIMARY KEY (id, publish_time)
) PARTITION BY RANGE (publish_time);

-- 月分区（自动创建 2024-2026 分区）
DO $$
DECLARE
    y INT;
    m INT;
    start_date DATE;
    end_date DATE;
    pname TEXT;
BEGIN
    FOR y IN 2024..2026 LOOP
        FOR m IN 1..12 LOOP
            start_date := make_date(y, m, 1);
            end_date := start_date + INTERVAL '1 month';
            pname := format('notice_%s_%s', y, lpad(m::text, 2, '0'));
            EXECUTE format(
                'CREATE TABLE IF NOT EXISTS %I PARTITION OF notice FOR VALUES FROM (%L) TO (%L)',
                pname, start_date, end_date
            );
        END LOOP;
    END LOOP;
END $$;

-- 默认分区（兜底）
CREATE TABLE IF NOT EXISTS notice_default PARTITION OF notice DEFAULT;

CREATE INDEX IF NOT EXISTS idx_notice_source_noticeid ON notice(source_id, notice_id);
CREATE INDEX IF NOT EXISTS idx_notice_project_no ON notice(project_no);
CREATE INDEX IF NOT EXISTS idx_notice_publish_time ON notice(publish_time DESC);
CREATE INDEX IF NOT EXISTS idx_notice_budget ON notice(budget_value);
CREATE INDEX IF NOT EXISTS idx_notice_unit ON notice(procurement_unit);
CREATE INDEX IF NOT EXISTS idx_notice_project_id ON notice(project_id);
CREATE INDEX IF NOT EXISTS idx_notice_dedup_hash ON notice(dedup_hash);
CREATE INDEX IF NOT EXISTS idx_notice_content_tsv ON notice USING gin(content_tsv);

COMMENT ON TABLE notice IS '公告主表（按月分区）';

-- ------------------------------------------------------------
-- 4. notice_field KV 扩展字段
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS notice_field (
    id          BIGSERIAL PRIMARY KEY,
    notice_id   BIGINT NOT NULL,
    field_key   VARCHAR(64) NOT NULL,
    field_value TEXT,
    created_at  TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_notice_field_notice ON notice_field(notice_id, field_key);
COMMENT ON TABLE notice_field IS '公告 KV 扩展字段';

-- ------------------------------------------------------------
-- 5. attachment 附件
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS attachment (
    id              BIGSERIAL PRIMARY KEY,
    notice_id       BIGINT NOT NULL,
    name            VARCHAR(255),
    download_url    TEXT,
    local_path      TEXT,
    file_type       VARCHAR(32),
    file_size       BIGINT,
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_attachment_notice ON attachment(notice_id);
COMMENT ON TABLE attachment IS '公告附件';

-- ------------------------------------------------------------
-- 6. tag 标签字典
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tag (
    id          BIGSERIAL PRIMARY KEY,
    dimension   VARCHAR(64) NOT NULL,              -- module/amount_tier/procurement_method/...
    tag_key     VARCHAR(128) NOT NULL,
    name        VARCHAR(128) NOT NULL,
    parent_id   BIGINT REFERENCES tag(id),
    level       SMALLINT DEFAULT 1,
    sort_order  INTEGER DEFAULT 0,
    created_at  TIMESTAMPTZ DEFAULT now(),
    UNIQUE(dimension, tag_key)
);
CREATE INDEX IF NOT EXISTS idx_tag_dimension ON tag(dimension);
COMMENT ON TABLE tag IS '标签字典';

-- ------------------------------------------------------------
-- 7. notice_tag 公告-标签多对多
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS notice_tag (
    id          BIGSERIAL PRIMARY KEY,
    notice_id   BIGINT NOT NULL,
    tag_id      BIGINT NOT NULL REFERENCES tag(id),
    source      VARCHAR(16) DEFAULT 'rule',        -- rule/llm/manual
    confidence  REAL DEFAULT 1.0,
    matched_keywords JSONB,
    created_at  TIMESTAMPTZ DEFAULT now(),
    UNIQUE(notice_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_notice_tag_notice ON notice_tag(notice_id);
CREATE INDEX IF NOT EXISTS idx_notice_tag_tag ON notice_tag(tag_id);
COMMENT ON TABLE notice_tag IS '公告标签关联';

-- ------------------------------------------------------------
-- 8. project 项目主档（串联招标→变更→中标）
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS project (
    id                  BIGSERIAL PRIMARY KEY,
    project_no          VARCHAR(128) UNIQUE,        -- 归一化项目编号
    project_name        TEXT,
    procurement_unit    VARCHAR(255),
    total_budget        NUMERIC(18,2),
    status              VARCHAR(32),                -- bidding/changed/awarded/cancelled
    region              VARCHAR(64),
    created_at          TIMESTAMPTZ DEFAULT now(),
    updated_at          TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_project_unit ON project(procurement_unit);
COMMENT ON TABLE project IS '项目主档';

-- ------------------------------------------------------------
-- 9. notice_change_log 字段级变更历史
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS notice_change_log (
    id              BIGSERIAL PRIMARY KEY,
    notice_id       BIGINT NOT NULL,
    field_name      VARCHAR(64) NOT NULL,
    old_value       TEXT,
    new_value       TEXT,
    change_type     VARCHAR(32),                    -- 延期/金额变更/中标补充/废标/其他
    detected_at     TIMESTAMPTZ DEFAULT now(),
    alerted         BOOLEAN DEFAULT FALSE
);
CREATE INDEX IF NOT EXISTS idx_change_log_notice ON notice_change_log(notice_id);
CREATE INDEX IF NOT EXISTS idx_change_log_type ON notice_change_log(change_type);
COMMENT ON TABLE notice_change_log IS '公告字段级变更历史';

-- ------------------------------------------------------------
-- 10. bid_result 中标结果
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS bid_result (
    id              BIGSERIAL PRIMARY KEY,
    notice_id       BIGINT NOT NULL,
    project_id      BIGINT,
    winner          VARCHAR(255),                   -- 中标人
    bid_amount      NUMERIC(18,2),
    rank            INTEGER,                        -- 排名
    is_candidate    BOOLEAN DEFAULT FALSE,          -- 是否候选人
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_bid_result_notice ON bid_result(notice_id);
CREATE INDEX IF NOT EXISTS idx_bid_result_winner ON bid_result(winner);
COMMENT ON TABLE bid_result IS '中标结果';

-- ------------------------------------------------------------
-- 11. crawl_task 爬虫任务
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS crawl_task (
    id              BIGSERIAL PRIMARY KEY,
    source_id       BIGINT NOT NULL REFERENCES source_site(id),
    task_type       VARCHAR(16) NOT NULL,           -- incremental/full
    status          VARCHAR(16) DEFAULT 'pending',  -- pending/running/success/failed
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    fetched_count   INTEGER DEFAULT 0,
    new_count       INTEGER DEFAULT 0,
    error_msg       TEXT,
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_crawl_task_source ON crawl_task(source_id, status);
COMMENT ON TABLE crawl_task IS '爬虫任务';

-- ------------------------------------------------------------
-- 12. crawl_log 爬虫日志
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS crawl_log (
    id              BIGSERIAL PRIMARY KEY,
    task_id         BIGINT REFERENCES crawl_task(id),
    source_id       BIGINT,
    level           VARCHAR(16),
    url             TEXT,
    message         TEXT,
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_crawl_log_task ON crawl_log(task_id);
COMMENT ON TABLE crawl_log IS '爬虫日志';

-- ------------------------------------------------------------
-- 13. dedup_fingerprint 去重指纹
-- ------------------------------------------------------------
CREATE TABLE IF NOT EXISTS dedup_fingerprint (
    id              BIGSERIAL PRIMARY KEY,
    notice_id       BIGINT,
    source_id       BIGINT,
    title_hash      VARCHAR(64),                    -- 标题 SimHash
    content_hash    VARCHAR(64),                    -- 正文 SimHash
    fingerprint     VARCHAR(64) UNIQUE NOT NULL,    -- 综合指纹
    created_at      TIMESTAMPTZ DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_dedup_title ON dedup_fingerprint(title_hash);
COMMENT ON TABLE dedup_fingerprint IS '去重指纹';

-- ============================================================
-- 视图与物化视图
-- ============================================================

-- 近 90 天各券商 IT 类采购预算 TOP
CREATE OR REPLACE VIEW v_broker_it_budget_top AS
SELECT
    n.procurement_unit AS broker,
    COUNT(*) AS project_count,
    SUM(n.budget_value) AS total_budget
FROM notice n
JOIN notice_tag nt ON nt.notice_id = n.id
JOIN tag t ON t.id = nt.tag_id
WHERE t.dimension = 'module'
  AND t.name = 'IT基础设施与云'
  AND n.publish_time >= now() - INTERVAL '90 days'
  AND n.budget_value IS NOT NULL
GROUP BY n.procurement_unit
ORDER BY total_budget DESC;

-- 各业务模块金额占比
CREATE OR REPLACE VIEW v_module_share AS
SELECT
    t.name AS module,
    COUNT(*) AS project_count,
    SUM(n.budget_value) AS total_amount
FROM notice n
JOIN notice_tag nt ON nt.notice_id = n.id
JOIN tag t ON t.id = nt.tag_id
WHERE t.dimension = 'module' AND t.level = 1
  AND n.budget_value IS NOT NULL
GROUP BY t.name
ORDER BY total_amount DESC;

-- 中标供应商集中度
CREATE OR REPLACE VIEW v_supplier_concentration AS
SELECT
    br.winner AS supplier,
    COUNT(*) AS win_count,
    SUM(br.bid_amount) AS total_amount
FROM bid_result br
WHERE br.winner IS NOT NULL
GROUP BY br.winner
ORDER BY total_amount DESC;

-- 发布量趋势（按日）
CREATE OR REPLACE VIEW v_publish_trend AS
SELECT
    DATE(publish_time) AS publish_date,
    COUNT(*) AS notice_count,
    SUM(budget_value) AS total_budget
FROM notice
WHERE publish_time IS NOT NULL
GROUP BY DATE(publish_time)
ORDER BY publish_date DESC;

-- 全文检索向量更新触发器
CREATE OR REPLACE FUNCTION notice_tsv_update() RETURNS trigger AS $$
BEGIN
    NEW.content_tsv := to_tsvector('simple',
        COALESCE(NEW.title, '') || ' ' || COALESCE(NEW.project_name, '') || ' ' ||
        COALESCE(NEW.content, ''));
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_notice_tsv ON notice;
CREATE TRIGGER trg_notice_tsv
    BEFORE INSERT OR UPDATE ON notice
    FOR EACH ROW EXECUTE FUNCTION notice_tsv_update();
