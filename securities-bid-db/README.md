# 证券公司公开招标信息特色数据库

面向证券行业的公开招标/采购公告采集与分析系统。实现多源采集 → 清洗去重 → 多维打标 → 结构化入库 → 增量更新与变更追踪 → 多维检索与市场动态分析。

## 功能特性

- **多源采集**：Scrapy（静态站）+ Playwright（动态渲染）+ requests（API 站），YAML 配置化，新增源不改代码
- **清洗去重**：金额/时间/文本统一解析，源站唯一键 + 标题 SimHash + 正文相似度去重
- **多维打标**：业务模块（18 类 + 二级子类）、金额分档、采购方式、公告类型、地域、项目属性等；规则优先 + LLM 兜底
- **结构化存储**：PostgreSQL 13 张核心表，月分区，索引齐全，视图/物化视图
- **增量与变更追踪**：增量抓取遇已入库 ID 连续 N 条即停；字段级 diff 生成变更历史与告警
- **检索与分析 API**：多条件筛选、全文检索、Excel 导出、5 大看板指标

## 快速开始

```bash
# 1. 安装依赖
pip install -r requirements.txt
playwright install chromium

# 2. 启动 PostgreSQL（Docker）
docker compose up -d db

# 3. 初始化数据库
psql -h localhost -U biduser -d bid_db -f db/schema.sql

# 4. 运行静态站爬虫
python -m spiders.run_static --source cebpubservice

# 5. 运行动态站爬虫
python -m spiders.run_dynamic --source citic

# 6. 启动 API 服务
python -m api.main

# 7. 启动调度器
python -m scheduler.scheduler
```

## 目录结构

```
securities-bid-db/
├── spiders/        # 爬虫（静态/动态/API）
├── parsers/        # 解析器（金额/时间/文本/Pydantic 模型）
├── pipelines/      # 入库/去重/项目串联/变更检测/附件存储
├── tagging/        # 标签字典/规则打标/LLM 分类器
├── db/             # DDL/SQLAlchemy ORM 模型/分析 SQL
├── api/            # FastAPI 检索与分析接口
├── scheduler/      # APScheduler 调度器/告警
├── sources/        # 数据源 YAML 配置
├── config/         # 全局配置
├── tests/          # pytest 单元与集成测试
├── data/           # 附件与页面快照（对象存储模拟）
└── docs/           # 部署/运维/验收文档
```

## 合规与伦理

本系统严格遵守以下合规原则：

1. **遵守 robots.txt**：采集前检查目标站 robots.txt，不抓取禁止路径。
2. **限速采集**：单站请求间隔 ≥ 3 秒（可配置），并加入随机抖动，避免对源站造成压力。
3. **不绕过反爬**：不破解验证码、不绕过付费墙、不模拟登录获取非公开内容。
4. **仅公开信息**：仅采集无需登录的公开招标/采购公告，不采集超出公告原文范围的个人信息。
5. **数据来源标注**：每条数据保留 `source_site` 与 `原文 URL`，可溯源。
6. **免责声明**：本系统数据仅用于行业研究与市场分析，不得用于非法用途；数据版权归原始发布方所有。

## 验收阶段

详见 [docs/acceptance.md](docs/acceptance.md)。
