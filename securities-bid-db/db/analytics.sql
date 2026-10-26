-- 分析看板示例 SQL

-- 1. 近 90 天各业务模块采购金额占比
SELECT
    t.name AS module,
    COUNT(n.id) AS project_count,
    SUM(n.budget_value) AS total_amount
FROM notice n
JOIN notice_tag nt ON nt.notice_id = n.id
JOIN tag t ON t.id = nt.tag_id
WHERE t.dimension = 'module' AND t.level = 1
  AND n.publish_time >= NOW() - INTERVAL '90 days'
GROUP BY t.name
ORDER BY total_amount DESC;

-- 2. 近 90 天各券商采购热度排行（项目数 + 预算）
SELECT
    procurement_unit AS broker,
    COUNT(*) AS project_count,
    SUM(budget_value) AS total_budget
FROM notice
WHERE procurement_unit IS NOT NULL
  AND publish_time >= NOW() - INTERVAL '90 days'
GROUP BY procurement_unit
ORDER BY total_budget DESC
LIMIT 20;

-- 3. 预算金额分档分布
SELECT
    t.name AS tier,
    COUNT(n.id) AS count,
    SUM(n.budget_value) AS amount
FROM notice n
JOIN notice_tag nt ON nt.notice_id = n.id
JOIN tag t ON t.id = nt.tag_id
WHERE t.dimension = 'amount_tier'
GROUP BY t.name
ORDER BY t.name;

-- 4. 中标供应商集中度（CR5）
SELECT
    winner AS supplier,
    COUNT(*) AS win_count,
    SUM(bid_amount) AS total_amount
FROM bid_result
WHERE winner IS NOT NULL
GROUP BY winner
ORDER BY total_amount DESC
LIMIT 10;

-- 5. 发布量趋势（按日）
SELECT
    DATE(publish_time) AS publish_date,
    COUNT(*) AS notice_count,
    SUM(budget_value) AS total_budget
FROM notice
WHERE publish_time >= NOW() - INTERVAL '90 days'
GROUP BY DATE(publish_time)
ORDER BY publish_date;
