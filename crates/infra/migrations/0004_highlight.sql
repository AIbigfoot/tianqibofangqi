-- 0004_highlight：推荐看点的本地缓存
--
-- 推荐横排按「高赞短评 → 一句核心看点」展示；生成一次就落库，
-- 换一批时优先取尚未生成过的条目，避免每次刷新都重复请求大模型。
-- comment_count 记录生成时的评论条数：重新刮削后条数变化即视为过期，
-- 下次被选中时自动重新生成。

CREATE TABLE item_highlight (
    item_id       TEXT    PRIMARY KEY REFERENCES media_item (id) ON DELETE CASCADE,
    highlight     TEXT    NOT NULL,
    comment_count INTEGER NOT NULL DEFAULT 0,
    source        TEXT    NOT NULL DEFAULT 'llm',  -- llm | local
    model         TEXT,
    created_at    INTEGER NOT NULL
);
