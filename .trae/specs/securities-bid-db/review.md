# 证券公司公开招标信息特色数据库 - 独立审查

## 审查检查点

- [x] CP-1: 数据源配置清单完整性（AC-1）
  - **Type**: `rule`
  - **Covers**: AC-1
  - **Evidence**: sources/*.yml 共 24 个（≥22）；抽样 citic/chinastock/htsc 均含 5 个必填字段

- [x] CP-2: 数据库 DDL 可执行（AC-4）
  - **Type**: `rule`
  - **Covers**: AC-4
  - **Evidence**: 13 张核心表全部存在；notice 分区 37 个；索引 40 个，视图 4 个

- [x] CP-3: 金额解析正确性（AC-5）
  - **Type**: `rule`
  - **Covers**: AC-5
  - **Evidence**: pytest tests/test_amount.py 5 passed

- [x] CP-4: 业务模块打标（AC-6）
  - **Type**: `rule`
  - **Covers**: AC-6
  - **Evidence**: pytest tests/test_tagging.py 6 passed

- [x] CP-5: 去重与项目串联（AC-7）
  - **Type**: `rule`
  - **Covers**: AC-7
  - **Evidence**: project_no=CITIC-2024-IT-001 → project_id=2, count=2

- [x] CP-6: 变更检测与告警（AC-8）
  - **Type**: `rule`
  - **Covers**: AC-8
  - **Evidence**: notice_change_log 中 change_type=延期 1 条（field=bid_deadline）

- [x] CP-7: 检索 API（AC-9）
  - **Type**: `rule`
  - **Covers**: AC-9
  - **Evidence**: GET /api/notices?unit=中信证券 → 200, total=2

- [x] CP-8: 分析看板 API（AC-10）
  - **Type**: `rule`
  - **Covers**: AC-10
  - **Evidence**: 5 个接口均返回 200

- [x] CP-9: 工程分层与测试（AC-11）
  - **Type**: `rubric`
  - **Covers**: AC-11
  - **Scale**: 1-5
  - **Score**: 5/5
  - **Evidence**: 7 层目录齐全；pytest tests/ 29 passed；核心模块均有测试

- [x] CP-10: Docker Compose（AC-12）
  - **Type**: `rule`
  - **Covers**: AC-12
  - **Evidence**: docker-compose.yml services = [db, api, scheduler]

- [x] CP-11: 合规说明（AC-13）
  - **Type**: `rule`
  - **Covers**: AC-13
  - **Evidence**: README 含 robots/限速/验证码/数据来源/免责 5 项

## Review History

### Review R1
- **Result**: `pass`
- **Evidence**: 11 个检查点全部通过；29 个 pytest 用例通过；DDL/API/变更检测/去重串联均实际运行验证
- **Actionable Findings**: F-1（low，advisory）：CP-2 检查查询模式应排除分区子表而非所有 notice_% 表，但 13 张核心表确实存在，非实现缺陷
- **Overall**: pass，无阻塞性问题
