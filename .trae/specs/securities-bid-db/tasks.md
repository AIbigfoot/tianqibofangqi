# 证券公司公开招标信息特色数据库 - 实施计划

## 实施完成证据汇总（2026-10-26）

全部 16 个任务已完成，端到端验证通过：

| 验收项 | 证据 |
|--------|------|
| 数据源配置 | `sources/*.yml` 共 24 个（22 券商 + 2 平台），`load_all_sources()` 全部加载成功 |
| 数据库 DDL | `db/schema.sql` 在 PostgreSQL 16 执行成功，13 张核心表 + 37 个月分区，索引/视图/触发器齐全 |
| 金额解析 | 4 条用例全过：1200万元→12000000、¥500000→500000、叁佰万元→3000000、300万含税→3000000 |
| 规则打标 | 服务器→IT基础设施、等保→网络安全、核心交易→核心业务系统、办公电脑→办公与协同 |
| 入库/去重/串联 | 同源同 notice_id 不重复；同 project_no 两条公告关联同一 project_id；预算汇总 2380 万 |
| 变更检测 | 投标截止延期生成 change_type=延期 的 notice_change_log，告警日志输出 |
| 检索 API | `/api/notices` 多条件筛选 200、`/api/notices/{id}` 详情 200、`/api/notices/export` 返回 xlsx |
| 分析看板 | 5 个接口均 200：module-share/broker-ranking/amount-distribution/publish-trend/supplier-concentration |
| 测试 | `pytest tests/` 29 passed |
| 模块导入 | Scrapy/Playwright/APScheduler 全部模块导入成功 |

## 阶段1：架构、目录结构、数据源清单、标签体系、数据库 DDL

### Task 1: 项目脚手架与目录结构
- **Status**: `completed`
- **Priority**: high
- **Depends On**: None
- **Description**:
  - 创建 `/workspace/securities-bid-db` 项目根目录，建立分层目录：`spiders/`, `parsers/`, `pipelines/`, `tagging/`, `db/`, `api/`, `scheduler/`, `sources/`, `tests/`, `data/`（附件与快照）
  - 创建 `pyproject.toml`/`requirements.txt`（Python 3.11，Scrapy, Playwright, requests, SQLAlchemy 2.x, Pydantic v2, FastAPI, APScheduler, python-dotenv, PyYAML, openpyxl, simhash）
  - 创建全局配置 `config/settings.yaml`（DB 连接、限速、代理池、UA 列表、对象存储路径、告警配置）
  - 创建 `README.md` 含项目说明与合规章节
- **Acceptance Criteria Addressed**: AC-11, AC-13
- **Test Requirements**:
  - `rule` TR-1.1: 目录结构存在且包含 8 个核心子目录；`ls` 验证
  - `rule` TR-1.2: `requirements.txt` 含全部依赖且 `pip check` 通过（若已安装）
  - `rule` TR-1.3: README 合规章节包含 robots/限速/不绕过验证码/数据来源/免责 5 项

### Task 2: 数据源 YAML 配置清单（≥20 券商 + 2 国家级平台）
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 1
- **Description**:
  - 在 `sources/` 下为每家券商/平台创建 YAML，字段：name, type(static/dynamic/api), entry_url, list_pagination(rule, param, max_pages), detail_selectors(title, project_no, publish_time, budget, content, attachments...), anti_crawl(strategy, rate_limit, ua_rotation), compliance(robots, notes)
  - 券商清单（≥20）：中信证券、国泰君安、海通证券、华泰证券、中金公司、招商证券、广发证券、中信建投、申万宏源、国信证券、东方证券、光大证券、平安证券、兴业证券、长江证券、安信证券、东吴证券、方正证券、国金证券、西南证券、中国银河、财通证券（取 ≥20）
  - 国家级平台：中国招标投标公共服务平台（cebpubservice.com）、全国公共资源交易平台（ggzy.gov.cn）
  - 提供 `sources/README.md` 说明 YAML 字段语义与页面结构假设
- **Acceptance Criteria Addressed**: AC-1
- **Test Requirements**:
  - `rule` TR-2.1: `sources/*.yml` 文件数 ≥22
  - `rule` TR-2.2: 抽样 3 个 YAML 均含 entry_url/list_pagination/detail_selectors/anti_crawl/compliance 字段
  - `rule` TR-2.3: 配置加载器能正确解析全部 YAML 为 SourceConfig 对象（pytest）

### Task 3: 标签体系字典与打标规则
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 1
- **Description**:
  - `tagging/dictionary.yaml`：业务模块一级 18 类 + 二级子类关键词词典
  - 金额分档规则（<50万 至 >1亿 共 9 档 + 未披露）
  - 采购方式、公告类型、组织形式、项目属性、供应商类型倾向 枚举字典
  - `tagging/rules.py`：基于关键词词典 + 正则的规则打标器，输出一级+二级标签
- **Acceptance Criteria Addressed**: AC-6
- **Test Requirements**:
  - `rule` TR-3.1: 4 条示例文本（服务器采购/等保/核心交易系统/办公电脑）打标一级标签正确
  - `rule` TR-3.2: 金额分档对 50万/150万/800万/5000万/2亿 分别命中对应档位

### Task 4: 数据库完整 DDL
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 1
- **Description**:
  - `db/schema.sql`：13 张核心表 DDL
    - source_site, raw_page, notice, notice_field, attachment, tag, notice_tag, project, notice_change_log, bid_result, crawl_task, crawl_log, dedup_fingerprint
  - 索引：项目编号、发布时间、预算金额、业务模块、采购单位、(source_id+notice_id) 唯一
  - 分区：notice 按 publish_time 月分区（声明式分区）
  - 视图/物化视图：近 90 天各券商 IT 类采购预算 TOP、模块金额占比、中标供应商集中度
  - `db/models.py`：SQLAlchemy 2.x ORM 模型对应全部表
- **Acceptance Criteria Addressed**: AC-4
- **Test Requirements**:
  - `rule` TR-4.1: 在空 PG 执行 schema.sql 成功，`\dt` ≥13 表
  - `rule` TR-4.2: notice 表为分区表且存在月分区
  - `rule` TR-4.3: SQLAlchemy 模型可创建会话并查询 source_site 空表

## 阶段2：YAML 配置 + 静态站 + 动态站完整爬虫（解析/入库/去重/打标全链路）

### Task 5: Pydantic 数据模型与配置加载
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 1, Task 2
- **Description**:
  - `parsers/models.py`：NoticeItem Pydantic 模型（含全部 FR-4 字段），金额子模型 Amount(original, value, unit, currency)
  - `sources/loader.py`：加载 YAML 为 SourceConfig Pydantic 模型，校验必填字段
- **Acceptance Criteria Addressed**: AC-2, AC-3
- **Test Requirements**:
  - `rule` TR-5.1: NoticeItem 可从 dict 构造，缺失必填字段抛 ValidationError
  - `rule` TR-5.2: SourceConfig 加载全部 YAML 无校验错误

### Task 6: 解析器（金额/时间/文本清洗）
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 5
- **Description**:
  - `parsers/amount.py`：金额解析，支持「万元/元/¥/人民币/含税/中文大写」，统一输出元数值
  - `parsers/time_parser.py`：多种中文日期格式 → ISO datetime
  - `parsers/text.py`：HTML 标签清洗、空白归一、项目编号归一化正则
- **Acceptance Criteria Addressed**: AC-5
- **Test Requirements**:
  - `rule` TR-6.1: 金额解析 4 条用例（1200万元/¥500000元/叁佰万元整/300万含税）数值正确
  - `rule` TR-6.2: 时间解析「2024年1月1日」「2024-01-01 09:00」「2024/01/01」输出 ISO 格式

### Task 7: 静态站爬虫（Scrapy）端到端
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 5, Task 6, Task 9
- **Description**:
  - `spiders/static_spider.py`：基于 SourceConfig 驱动的通用 Scrapy 爬虫，列表页翻页、详情页解析
  - 实现中国招标投标公共服务平台（或等价公开静态站）为示例真实源
  - 列表页遇到已入库 notice_id 连续 N=5 条停止翻页（增量逻辑）
  - 输出 NoticeItem，经 pipeline 入库、去重、打标
- **Acceptance Criteria Addressed**: AC-2
- **Test Requirements**:
  - `rule` TR-7.1: 爬虫可运行并抓取 ≥1 条公告入库（网络可达时）
  - `rule` TR-7.2: 使用 fixtures 本地 HTML 可离线解析出完整 NoticeItem

### Task 8: 动态站爬虫（Playwright）端到端
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 5, Task 6, Task 9
- **Description**:
  - `spiders/dynamic_spider.py`：Playwright 渲染动态页，等待列表加载，提取详情链接
  - 实现某券商官网采购栏目为示例真实动态源
  - 复用浏览器上下文发起详情请求（避免 token 失效）
  - UA 轮换、随机间隔、代理池接口预留
- **Acceptance Criteria Addressed**: AC-3
- **Test Requirements**:
  - `rule` TR-8.1: 动态渲染后可提取 ≥1 条公告链接
  - `rule` TR-8.2: 离线 fixtures 可解析动态详情页字段

### Task 9: 数据库 Pipeline（入库/去重/项目串联）
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 4, Task 5, Task 6
- **Description**:
  - `pipelines/db_pipeline.py`：NoticeItem → ORM 入库，upsert 策略
  - `pipelines/dedup.py`：源站唯一键去重 + 标题 SimHash + 正文相似度（阈值 0.85）；写 dedup_fingerprint
  - `pipelines/project_linker.py`：按项目编号（归一化）+ 单位 + 标题关键词 串联为 project 主档，notice.project_id 关联
  - `pipelines/storage.py`：附件下载 + 页面快照保存到 data/ 目录，记录本地路径
- **Acceptance Criteria Addressed**: AC-7
- **Test Requirements**:
  - `rule` TR-9.1: 两条同项目编号公告入库后 project 表 1 条，两 notice.project_id 相同
  - `rule` TR-9.2: 标题高度相似的两条公告第二条被标记重复（dedup_fingerprint 命中）
  - `rule` TR-9.3: 附件与快照落地到 data/ 且 DB 记录本地路径

### Task 10: 打标 Pipeline（规则 + LLM 接口）
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 3, Task 9
- **Description**:
  - `tagging/classifier.py`：规则打标器封装，对 NoticeItem 输出业务模块、金额分档等全部维度标签，写入 notice_tag
  - `tagging/llm_classifier.py`：可插拔 LLM 分类接口，含结构化 Prompt 与 JSON Schema；提供 MockLLMClassifier（规则未命中时返回候选标签 + 置信度）
  - 低置信度（<0.6）写入人工复核队列（notice 标记 review_required）
- **Acceptance Criteria Addressed**: AC-6
- **Test Requirements**:
  - `rule` TR-10.1: 规则打标对 4 条示例文本输出正确一级标签
  - `rule` TR-10.2: MockLLMClassifier 返回符合 JSON Schema 的结果，低置信度标记 review_required

## 阶段3：增量调度、变更检测、告警、API 与看板

### Task 11: 增量调度器（APScheduler）
- **Status**: `completed`
- **Priority**: medium
- **Depends On**: Task 7, Task 8
- **Description**:
  - `scheduler/scheduler.py`：APScheduler 定时任务，增量抓取（每 6 小时）+ 全量校准（每周日）
  - `db` 表 crawl_task/crawl_log：记录任务 ID、源、状态、耗时、抓取数、错误
  - 断点续爬：记录最后成功的列表页游标
- **Acceptance Criteria Addressed**: AC-8
- **Test Requirements**:
  - `rule` TR-11.1: 调度器可注册并触发一次增量任务，crawl_log 有记录
  - `rule` TR-11.2: 任务失败写入 crawl_log 错误信息

### Task 12: 变更检测与告警
- **Status**: `completed`
- **Priority**: medium
- **Depends On**: Task 9
- **Description**:
  - `pipelines/change_detector.py`：二次抓取同一 notice 后字段 diff，生成 notice_change_log（字段、旧值、新值、变更类型：延期/金额变更/中标补充/废标）
  - `scheduler/alert.py`：告警分发器，支持邮件（SMTP 配置）与企微 Webhook；默认输出日志 mock
- **Acceptance Criteria Addressed**: AC-8
- **Test Requirements**:
  - `rule` TR-12.1: 同一 notice 投标截止时间变更后生成 change_type=延期 的记录
  - `rule` TR-12.2: 告警回调被调用（mock 断言）

### Task 13: 检索 API（FastAPI）+ Excel 导出
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 9, Task 10
- **Description**:
  - `api/main.py`：FastAPI 应用
  - `GET /api/notices`：多条件筛选（unit, module, amount_min/max, date_from/to, procurement_method, keyword 全文检索 via PG tsvector），分页
  - `GET /api/notices/{id}`：详情
  - `GET /api/notices/export`：导出 Excel（openpyxl）
- **Acceptance Criteria Addressed**: AC-9
- **Test Requirements**:
  - `rule` TR-13.1: `/api/notices` 多条件筛选返回符合条件记录，HTTP 200
  - `rule` TR-13.2: `/api/notices/export` 返回 xlsx 文件且可打开

### Task 14: 分析看板查询 API
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 13
- **Description**:
  - `api/analytics.py`：看板指标接口
    - `/api/analytics/publish-trend`：发布量趋势（按日/周/月）
    - `/api/analytics/module-share`：各业务模块金额占比与项目数
    - `/api/analytics/broker-ranking`：各券商采购热度排行（项目数+预算）
    - `/api/analytics/amount-distribution`：预算金额分档分布
    - `/api/analytics/supplier-concentration`：中标供应商集中度（CR5）
  - `db/analytics.sql`：3-5 个示例 SQL
- **Acceptance Criteria Addressed**: AC-10
- **Test Requirements**:
  - `rule` TR-14.1: 5 个分析接口均返回 HTTP 200 且字段完整
  - `rule` TR-14.2: module-share 金额合计与 notice 表预算合计可校验（误差 < 1%）

## 阶段4：部署文档、运维手册、验收清单 + 测试

### Task 15: Docker Compose + 部署文档 + 运维手册 + 验收清单
- **Status**: `completed`
- **Priority**: medium
- **Depends On**: Task 13, Task 14
- **Description**:
  - `docker-compose.yml`：PostgreSQL 15 + API 服务 + 爬虫调度器（可选 ES）
  - `Dockerfile`：爬虫与 API 镜像
  - `docs/deployment.md`：部署步骤
  - `docs/operations.md`：运维手册（日常巡检、故障处理、扩容）
  - `docs/acceptance.md`：分阶段验收清单（对应 4 阶段）
- **Acceptance Criteria Addressed**: AC-12, AC-13
- **Test Requirements**:
  - `rule` TR-15.1: `docker compose config` 校验通过
  - `rule` TR-15.2: `docker compose up -d db` 后 PG 可连接
  - `rule` TR-15.3: 验收清单覆盖 4 阶段且每阶段有可勾选项

### Task 16: pytest 测试套件与集成验证
- **Status**: `completed`
- **Priority**: high
- **Depends On**: Task 1-15
- **Description**:
  - `tests/`：单元测试（金额解析、时间解析、规则打标、去重、变更检测、配置加载）
  - `tests/integration/`：端到端集成测试（fixture HTML → 解析 → 入库 → 打标 → API 查询）
  - fixtures：静态站与动态站的本地 HTML 样本
  - 运行 `pytest` 全部通过
- **Acceptance Criteria Addressed**: AC-11
- **Test Requirements**:
  - `rule` TR-16.1: `pytest` 运行，核心模块用例全部通过
  - `rubric` TR-16.2: 代码分层清晰度；scale 1-5；anchors 1/3/5；threshold >= 4；evidence = 目录结构审查 + 测试通过率
