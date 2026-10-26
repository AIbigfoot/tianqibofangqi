# 验收清单

## 阶段1：架构与基础

- [ ] 目录结构含 spiders/parsers/pipelines/tagging/db/api/scheduler/sources/tests/data
- [ ] requirements.txt 含 Scrapy/Playwright/SQLAlchemy/FastAPI/APScheduler 等依赖
- [ ] sources/*.yml 数量 ≥ 22（20 券商 + 2 平台）
- [ ] 每个 YAML 含 entry_url/list_pagination/detail_selectors/anti_crawl/compliance
- [ ] tagging/dictionary.yaml 含 18 个一级业务模块 + 二级子类 + 金额 9 档
- [ ] db/schema.sql 含 13 张核心表，在空 PG 执行成功
- [ ] notice 表为月分区表，存在月分区
- [ ] README 合规章节含 robots/限速/不绕过验证码/数据来源/免责 5 项

## 阶段2：爬虫全链路

- [ ] parsers/models.py NoticeItem 含全部 FR-4 字段
- [ ] 金额解析：1200万元→12000000、¥500000元→500000、叁佰万元→3000000、300万含税→3000000
- [ ] 时间解析支持「年月日」「-」「/」格式
- [ ] 静态站爬虫可运行并入库 ≥1 条公告
- [ ] 动态站爬虫可渲染并入库 ≥1 条公告
- [ ] raw_page 保存快照路径非空
- [ ] 去重：同源同 notice_id 不重复入库
- [ ] 项目串联：同 project_no 的公告关联同一 project_id
- [ ] 规则打标：服务器采购→IT基础设施、等保→网络安全、核心交易→核心业务系统、办公电脑→办公与协同
- [ ] LLM Mock 分类器返回符合 JSON Schema 的结果

## 阶段3：调度、变更、API

- [ ] APScheduler 注册增量（每 6h）与全量（每周日）任务
- [ ] crawl_task/crawl_log 记录任务状态
- [ ] 变更检测：投标截止时间变更生成 change_type=延期 的 notice_change_log
- [ ] 告警回调可触发（日志/企微 mock）
- [ ] GET /api/notices 多条件筛选返回 HTTP 200 且结果符合条件
- [ ] GET /api/notices/{id} 返回详情
- [ ] GET /api/notices/export 返回 xlsx 文件
- [ ] 5 个分析接口均返回 HTTP 200：publish-trend/module-share/broker-ranking/amount-distribution/supplier-concentration

## 阶段4：部署与测试

- [ ] `docker compose config` 校验通过
- [ ] `docker compose up -d db` 后 PG 健康可连接
- [ ] pytest 核心用例全部通过（金额/时间/打标/去重/变更检测/配置加载）
- [ ] 集成测试：fixture HTML → 解析 → 入库 → 打标 → API 查询 全链路通过
