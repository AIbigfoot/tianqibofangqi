# 数据源 YAML 配置说明

每个数据源一个 `.yml` 文件，字段语义如下：

| 字段 | 说明 |
|------|------|
| `name` | 数据源名称（券商/平台） |
| `key` | 唯一标识，用于爬虫启动参数 |
| `type` | `static`(Scrapy) / `dynamic`(Playwright) / `api`(requests) |
| `entry_url` | 列表页入口 URL |
| `domain` | 域名，用于相对 URL 拼接 |
| `list_pagination` | 翻页规则：`rule`(query_param/path/cursor)、`page_param`、`start_page`、`max_pages`、`page_size` |
| `list_selectors` | 列表页选择器：`item`(每条公告)、`title`、`link`、`publish_time` |
| `detail_selectors` | 详情页字段选择器：title/project_no/publish_time/budget/procurement_unit/agency/content/attachments/contact/phone |
| `anti_crawl` | 反爬策略：rate_limit_seconds、ua_rotation、strategy、notes |
| `compliance` | 合规：robots、notes |

## 页面结构假设

- 列表页为公告条目列表，每条含标题、链接、发布时间
- 详情页为公告正文，含项目编号、预算、采购人、代理机构、联系人等字段
- 选择器使用 parsel/CSS 语法（与 Scrapy 兼容），`a::attr(href)` 取属性，`::text` 取文本

## 新增数据源

1. 复制任意 `*.yml` 为新文件
2. 修改 name/key/entry_url/domain
3. 调整 list_selectors 与 detail_selectors 以匹配目标站点结构
4. 无需修改 Python 代码

## 注意

券商官网栏目 URL 与页面结构可能随时间变化，配置中的选择器为假设值，实际使用前需根据站点当前结构校验调整。
