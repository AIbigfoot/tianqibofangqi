-- 合并目录：把另一个目录登记为同一媒体源的附加根（只登记，不移动文件）。
CREATE TABLE IF NOT EXISTS source_extra_root (
    source_id  TEXT NOT NULL,
    path       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (source_id, path)
);
