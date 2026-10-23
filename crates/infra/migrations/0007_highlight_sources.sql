-- 看点缓存增加「额外资料条数」（简介 / 标签 / 联网结果），用于界面标注来源。
-- 旧缓存是 120 字左右的短版，一并清空，按 200–300 字的新规则重生成。
ALTER TABLE item_highlight ADD COLUMN extra_count INTEGER NOT NULL DEFAULT 0;
DELETE FROM item_highlight;
