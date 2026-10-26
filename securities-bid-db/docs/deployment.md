# 部署文档

## 环境要求
- Docker 24+ / Docker Compose v2
- 内存 ≥ 4GB，磁盘 ≥ 20GB（附件与快照）

## 一键启动

```bash
cd securities-bid-db

# 1. 启动 PostgreSQL（自动初始化 schema）
docker compose up -d db

# 2. 等待健康检查通过
docker compose ps

# 3. 启动 API 与调度器
docker compose up -d api scheduler

# 4. 验证
curl http://localhost:8000/api/health
```

## 本地开发启动（无 Docker）

```bash
pip install -r requirements.txt
playwright install chromium

# 启动本地 PG 后初始化
psql -h localhost -U biduser -d bid_db -f db/schema.sql

# 运行静态爬虫
python -m spiders.run_static --source cebpubservice

# 运行动态爬虫
python -m spiders.run_dynamic --source citic

# 启动 API
python -m api.main

# 启动调度器
python -m scheduler.scheduler
```

## 配置项

复制 `.env.example` 为 `.env` 并按需修改：
- `DATABASE_URL`：数据库连接
- `WECOM_WEBHOOK_URL`：企微告警
- `SMTP_*`：邮件告警

详见 `config/settings.yaml`。
