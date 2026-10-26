# 证券公司公开招标信息特色数据库 - 产品需求文档

## Overview
- **Summary**: 构建一个面向证券行业的公开招标/采购公告采集与分析系统，实现多源采集、清洗去重、多维打标、结构化入库、增量更新与变更追踪、多维检索与市场动态分析。
- **Purpose**: 为证券行业研究、竞品分析、供应商洞察提供可溯源、可对比的招投标数据底座，支持市场动态洞察（谁在采购、采购什么方向、预算量级、发布节奏、同类项目价格区间、供应商/中标结果分布）。
- **Target Users**: 券商战略/采购/IT 部门、行业研究人员、ToB 销售/BD、数据分析师。

## Goals
1. 全量 + 增量收集各证券公司及其招标代理机构发布的公开招标/采购公告（P0 覆盖 ≥20 家券商官网 + 中国招标投标公共服务平台 + 全国公共资源交易平台）。
2. 按「业务模块 + 采购金额体量 + 采购方式 + 时间 + 地域 + 项目属性」等多维标签组织数据。
3. 保留公告原文、附件与变更历史，形成可溯源、可对比的特色数据库。
4. 支持市场动态洞察：发布量趋势、预算分布、券商热度排行、模块占比、同类项目单价对比、中标供应商集中度。

## Non-Goals
- 不采集需登录/付费的内容；不绕过验证码与付费墙。
- 不采集个人信息超出公告原文范围（如投标单位联系人若属公开公告内字段则保留，否则不采集）。
- 不做实时行情推送；不做自然语言问答机器人。
- 不替换真实站点 URL——示例爬虫以可配置模板 + 1 个真实静态站 + 1 个真实动态站方式交付，其余 ≥18 家券商以 YAML 配置清单方式提供（含入口 URL、选择器假设）。
- 不内置 LLM API Key；LLM 分类器以可插拔接口 + 结构化 Prompt + JSON Schema 形式提供，调用由用户自行接入。

## Background & Context
- 证券行业 IT 采购年规模数百亿级，公开招标散落在各券商官网「采购/招标公告」栏目及公共资源交易平台，缺乏统一聚合与结构化视图。
- 同类产品（剑鱼标讯、千里马）为商业付费聚合站，数据维度与券商行业纵深不足。
- 参考同类爬虫经验：动态站点优先复用真实浏览器上下文发起 API 请求；全量采集后置标签化；分页必须显式传入翻页参数；日志输出可决策字段（success/msg/data 是否为空）。

## Functional Requirements

### 采集层
- **FR-1 多源采集**: 支持 Scrapy（静态站）+ Playwright（动态渲染/反爬站）+ requests（API 站）三种采集通道，统一输出 Pydantic 模型。
- **FR-2 配置化数据源**: 每个源以 YAML 定义入口 URL、列表页翻页规则、详情页选择器/字段映射、反爬策略、robots 合规说明，新增源不改代码。
- **FR-3 P0 数据源清单**: 输出 ≥20 家券商 + 2 个国家级平台的 YAML 配置（含栏目 URL、页面结构假设、反爬特征与应对、合规说明），并实现其中 1 个静态站 + 1 个动态站的完整可运行爬虫。
- **FR-4 详情字段解析**: 解析公告 ID、标题、采购单位、招标代理、项目名称、项目编号、公告类型、采购方式、业务模块、采购内容摘要、预算金额（原值+数值+币种）、最高限价、资金来源、标段/包件、发布时间、报名截止、投标截止、开标时间、项目地点、投标人资格、联系人、电话、邮箱、附件列表、原文 URL、正文全文、变更标记、关联公告 ID。

### 清洗与去重
- **FR-5 金额解析**: 处理「万元/元/¥/人民币/含税」等单位换算，输出数值 + 单位 + 原值 + 币种。
- **FR-6 时间解析**: 统一发布时间/截止时间为 ISO 格式，支持「2024年1月1日」「2024-01-01」「2024/01/01 09:00」等格式。
- **FR-7 去重**: 源站唯一键去重 + 标题指纹（SimHash）+ 正文相似度阈值；同项目编号串联为 project 主档。

### 打标层
- **FR-8 业务模块标签**: 一级 18 类 + 二级子类（IT 基础设施→服务器/存储/网络设备/云资源/维保 等），关键词词典 + 正则打标。
- **FR-9 金额分档标签**: <50万、50-100万、100-300万、300-500万、500-1000万、1000-3000万、3000万-1亿、>1亿、未披露。
- **FR-10 其他维度标签**: 采购方式、公告类型、组织形式（自行/委托代理）、时间（年月/季度/财年）、地域（省/市/服务范围）、项目属性（新建/扩容/续保/替换/试点/研究课题）、供应商类型倾向（国产化/原厂授权）。
- **FR-11 LLM 兜底分类**: 规则未命中时调用可插拔 LLM 分类器，输出结构化结果 + 置信度 + 命中依据；低置信度进入人工复核队列。

### 存储层
- **FR-12 数据库 DDL**: PostgreSQL 完整 DDL，含 source_site、raw_page、notice、notice_field、attachment、tag、notice_tag、project、notice_change_log、bid_result、crawl_task、crawl_log、dedup_fingerprint。
- **FR-13 索引与分区**: 项目编号、发布时间、预算金额、业务模块、单位索引；按发布时间月分区；去重指纹索引。
- **FR-14 视图与物化视图**: 如「近 90 天各券商 IT 类采购预算 TOP」等分析视图。
- **FR-15 对象存储**: 附件与页面快照落地到本地对象存储目录（模拟 S3/MinIO），保存本地路径。

### 更新与变更追踪
- **FR-16 增量调度**: 列表页按发布时间倒序，遇到已入库 ID 连续 N 条即停；全量校准每周一次；APScheduler/Celery 定时配置。
- **FR-17 变更检测**: 同一公告二次抓取后字段 diff，生成变更记录并打标（延期/金额变更/中标结果补充/废标）。
- **FR-18 告警**: 变更告警支持邮件或企微 Webhook。
- **FR-19 爬虫鲁棒性**: 断点续爬、失败重试、限速随机间隔、代理池接口、UA 轮换。

### 检索与分析
- **FR-20 检索 API**: 多条件筛选（单位、模块、金额区间、时间、采购方式、关键词全文检索）、分页、导出 Excel。
- **FR-21 分析看板查询**: 发布量趋势、预算金额分布、各券商热度排行、模块占比、平均预算与周期、同类项目单价对比、中标供应商集中度；提供 3-5 个示例 SQL 与接口实现。

### 工程与合规
- **FR-22 工程分层**: spiders/parsers/pipelines/tagging/db/api/scheduler 清晰分层，带 pytest 用例。
- **FR-23 部署**: Docker Compose 一键启动（PostgreSQL + 可选 ES + 爬虫调度 + API）。
- **FR-24 合规**: 遵守 robots、单站 ≥3s 限速、数据来源标注与免责说明。

## Non-Functional Requirements
- **NFR-1 可配置性**: 新增数据源仅需新增 YAML，不改 Python 代码。
- **NFR-2 可追溯**: 每条 notice 保留 raw_page 快照与原文 URL，字段级变更有 notice_change_log。
- **NFR-3 可观测**: 日志、crawl_task/crawl_log 记录任务状态与耗时，失败告警。
- **NFR-4 可扩展性**: 打标分类器可插拔（规则/LLM/人工复核），存储层可替换（PG → PG+ES）。
- **NFR-5 合规性**: 仅公开信息、robots 友好、限速、不绕过反爬。

## Constraints
- **Technical**: Python 3.11；PostgreSQL 15+；Scrapy；Playwright；Pydantic v2；SQLAlchemy 2.x；FastAPI（API 层）；APScheduler。
- **Business**: 仅采集无需登录的公开公告；不采集个人信息超出公告原文范围。
- **Dependencies**: PostgreSQL 实例（本地 Docker）；Playwright 浏览器驱动；对象存储用本地目录模拟。

## Assumptions
- A1: 示例真实站点选择「中国招标投标公共服务平台」静态列表页 + 某券商官网动态渲染页作为 P0 实现样例；其余券商以 YAML 配置清单（含 URL 与选择器假设）提供，不保证全部实时可爬（站点结构会变）。
- A2: LLM 分类器接口以协议 + Mock 实现交付，不内置真实 LLM 调用与 Key。
- A3: 对象存储以本地 `/data/bid_attachments` 目录模拟，不依赖 MinIO/S3 真实服务。
- A4: 金额解析以「万元」为统一归一单位，保留原值与单位。
- A5: 去重 SimHash 实现内置，正文相似度阈值默认 0.85。
- A6: 公告 ID 优先取源站原生 ID，无则取 URL 路径 hash。

## Acceptance Criteria

### AC-1: 数据源配置清单完整性
- **Type**: `rule`
- **Given**: P0 数据源 YAML 目录存在
- **When**: 统计 YAML 文件中定义的数据源数量
- **Then**: 券商数据源 ≥20 家 + 国家级平台 ≥2 个，每个源含入口 URL、翻页规则、字段映射、反爬策略、合规说明
- **Pass Condition**: `grep -c "name:" sources/*.yml` 计数 ≥22，且抽样 3 个源的 YAML 字段齐全
- **Evidence**: sources 目录文件列表与 YAML 内容

### AC-2: 静态站爬虫端到端可运行
- **Type**: `rule`
- **Given**: 静态站爬虫（如中国招标投标公共服务平台或等价公开站）已实现
- **When**: 执行 `scrapy crawl <static_spider>` 或等价命令
- **Then**: 成功抓取 ≥1 条公告，解析出标题、项目编号、发布时间、正文，写入 PostgreSQL notice 表，raw_page 有快照
- **Pass Condition**: notice 表新增记录数 ≥1 且 raw_page 对应 snapshot_path 非空
- **Evidence**: 爬虫日志 + DB 查询结果

### AC-3: 动态站爬虫端到端可运行
- **Type**: `rule`
- **Given**: Playwright 动态站爬虫已实现
- **When**: 执行动态站爬虫
- **Then**: 成功渲染并抓取 ≥1 条公告，字段解析完整，入库
- **Pass Condition**: notice 表新增该源记录 ≥1
- **Evidence**: 爬虫日志 + DB 查询结果

### AC-4: 数据库 DDL 可执行
- **Type**: `rule`
- **Given**: db/schema.sql 存在
- **When**: 在空 PostgreSQL 执行 `psql -f db/schema.sql`
- **Then**: 全部 13 张核心表创建成功，含索引、分区、视图
- **Pass Condition**: `\dt` 列出 ≥13 表，`\di` 列出索引，月分区表存在
- **Evidence**: psql 执行输出

### AC-5: 金额解析正确性
- **Type**: `rule`
- **Given**: 金额解析模块
- **When**: 输入「1,200万元」「¥500,000元」「人民币叁佰万元整」「300万（含税）」
- **Then**: 输出数值分别为 12000000、500000、3000000、3000000，单位为元，原值保留
- **Pass Condition**: 4 条用例数值全部正确
- **Evidence**: pytest 测试输出

### AC-6: 业务模块打标覆盖率
- **Type**: `rule`
- **Given**: 规则打标器 + 关键词词典
- **When**: 对含「服务器采购」「网络安全等级保护」「核心交易系统升级」「办公电脑」的文本打标
- **Then**: 分别命中 IT基础设施-服务器、网络安全、核心业务系统、办公与协同
- **Pass Condition**: 4 条用例一级标签全部正确
- **Evidence**: pytest 测试输出

### AC-7: 去重与项目串联
- **Type**: `rule`
- **Given**: 两条同项目编号不同公告类型的公告
- **When**: 入库去重
- **Then**: 生成同一 project 主档，notice 关联 project_id，标题指纹重复的第二条被标记为重复或合并
- **Pass Condition**: project 表记录数 = 1，两条 notice.project_id 相同
- **Evidence**: DB 查询

### AC-8: 变更检测与告警
- **Type**: `rule`
- **Given**: 已入库公告再次抓取且投标截止时间变更
- **When**: 执行变更检测
- **Then**: notice_change_log 生成一条变更记录，类型为「延期」，触发告警回调（日志/企微 mock）
- **Pass Condition**: notice_change_log 新增记录且 change_type=延期
- **Evidence**: DB 查询 + 告警日志

### AC-9: 检索 API 多条件筛选
- **Type**: `rule`
- **Given**: FastAPI 服务启动，DB 有测试数据
- **When**: GET /api/notices?unit=中信证券&module=IT基础设施&amount_min=1000000&amount_max=10000000
- **Then**: 返回符合条件的公告列表，分页正确，HTTP 200
- **Pass Condition**: 响应 status=200 且 items 中所有记录满足筛选条件
- **Evidence**: curl 输出

### AC-10: 分析看板查询接口
- **Type**: `rule`
- **Given**: API 服务启动
- **When**: GET /api/analytics/module-share?days=90
- **Then**: 返回近 90 天各业务模块采购金额占比与项目数
- **Pass Condition**: 响应含 module、amount、count 字段且金额合计可校验
- **Evidence**: curl 输出

### AC-11: 工程分层与测试
- **Type**: `rubric`
- **Dimension**: 代码分层清晰度与测试覆盖
- **Scale**: 1-5
- **Anchors**: 1 = 单层脚本无分层无测试；3 = 有 spiders/pipelines/db 分层，核心模块有少量测试；5 = 完整 spiders/parsers/pipelines/tagging/db/api/scheduler 分层，金额解析/打标/去重/变更检测均有 pytest 用例且通过
- **Pass Threshold**: >= 4
- **Evidence**: 目录结构 + pytest 输出

### AC-12: Docker Compose 可启动
- **Type**: `rule`
- **Given**: docker-compose.yml 存在
- **When**: `docker compose up -d db` 启动 PostgreSQL
- **Then**: 容器健康，可连接并执行 DDL
- **Pass Condition**: `docker compose ps` 显示 db running
- **Evidence**: docker compose ps 输出

### AC-13: 合规说明完整性
- **Type**: `rule`
- **Given**: 项目根目录含合规说明文档或 README 合规章节
- **When**: 检查内容
- **Then**: 包含 robots 遵守、单站 ≥3s 限速、不绕过验证码/付费墙、数据来源标注、免责声明
- **Pass Condition**: 5 项合规要点均出现
- **Evidence**: 文档内容

## Open Questions
- [ ] 用户是否需要真实对接 LLM（如提供 API Key）？默认以 Mock 实现交付，接口预留。
- [ ] 对象存储是否需要真实 MinIO？默认本地目录模拟。
- [ ] 是否需要 Elasticsearch 全文检索？默认 PostgreSQL `tsvector` 全文检索，ES 作为可选。
