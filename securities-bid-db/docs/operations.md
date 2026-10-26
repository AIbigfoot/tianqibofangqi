# 运维手册

## 日常巡检

```bash
# 容器状态
docker compose ps

# API 健康检查
curl http://localhost:8000/api/health

# 最近爬虫任务
docker compose exec db psql -U biduser -d bid_db -c \
  "SELECT id, source_id, task_type, status, started_at, fetched_count, error_msg \
   FROM crawl_task ORDER BY id DESC LIMIT 10;"

# 公告总量
docker compose exec db psql -U biduser -d bid_db -c \
  "SELECT count(*) FROM notice;"
```

## 故障处理

| 现象 | 排查 | 处理 |
|------|------|------|
| 爬虫任务失败 | 查 crawl_log.error_msg | 检查源站 URL/选择器是否变更 |
| DB 连接失败 | `docker compose logs db` | 确认 PG 健康，重启 api/scheduler |
| 抓取为空 | 检查列表选择器是否匹配 | 更新对应 sources/*.yml 的选择器 |
| 磁盘满 | 清理 data/attachments 旧文件 | 归档或扩展存储 |

## 增量与全量

- 增量默认每 6 小时，可在 `config/settings.yaml` 调整 `incremental_interval_hours`
- 全量校准每周日，调整 `full_calibration_day/hour`

## 新增数据源

1. 在 `sources/` 新增 `{key}.yml`（参考已有配置）
2. 校验：`python -c "from sources.loader import load_source; load_source('{key}')"`
3. 试跑：`python -m spiders.run_static --source {key}` 或 `run_dynamic`

## 数据备份

```bash
# 逻辑备份
docker compose exec db pg_dump -U biduser bid_db > backup_$(date +%Y%m%d).sql
```
