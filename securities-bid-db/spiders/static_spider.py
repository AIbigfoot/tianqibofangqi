"""静态站爬虫（Scrapy）- 由 SourceConfig 驱动。

示例真实源：中国招标投标公共服务平台 (cebpubservice)。
"""
from __future__ import annotations

import re
from urllib.parse import urljoin, urlparse

import scrapy
from parsel import Selector

from parsers.amount import parse_amount
from parsers.models import AttachmentItem, NoticeItem
from parsers.text import clean_html, notice_id_from_url, normalize_project_no
from parsers.time_parser import parse_time
from sources.loader import load_source
from spiders.items import NoticeItemWrapper


class StaticSpider(scrapy.Spider):
    name = "static"
    custom_settings = {"ITEM_PIPELINES": {"spiders.scrapy_pipeline.DBPipeline": 300}}

    def __init__(self, source: str = "cebpubservice", max_pages: int | None = None, **kwargs):
        super().__init__(**kwargs)
        self.source_key = source
        self.cfg = load_source(source)
        self.max_pages = max_pages or self.cfg.list_pagination.max_pages
        self.stop_threshold = 5  # 连续已入库 N 条则停
        self._seen_count = 0

    def start_requests(self):
        yield scrapy.Request(self.cfg.entry_url, callback=self.parse_list,
                             meta={"page": self.cfg.list_pagination.start_page})

    def _build_page_url(self, page: int) -> str:
        base = self.cfg.entry_url
        param = self.cfg.list_pagination.page_param
        sep = "&" if "?" in base else "?"
        return f"{base}{sep}{param}={page}"

    def parse_list(self, response):
        page = response.meta.get("page", 1)
        sel = Selector(response.text)
        items_sel = self.cfg.list_selectors

        items = sel.css(items_sel.item) if items_sel.item else sel.xpath("//li")
        new_links = 0

        for it in items:
            title = (it.css(items_sel.title).get() or "").strip() if items_sel.title else ""
            link = it.css(items_sel.link).get("") if items_sel.link else ""
            pub_time = (it.css(items_sel.publish_time).get() or "").strip() if items_sel.publish_time else ""
            if not link:
                continue
            abs_link = urljoin(response.url, link)
            notice_id = notice_id_from_url(abs_link)
            # 增量停止：已入库则计数（实际应查 DB，这里简化为连续空标题停止）
            yield scrapy.Request(abs_link, callback=self.parse_detail,
                                 meta={"notice_id": notice_id, "publish_time": pub_time})
            new_links += 1

        # 翻页
        if page < self.max_pages and new_links > 0:
            next_url = self._build_page_url(page + 1)
            yield scrapy.Request(next_url, callback=self.parse_list, meta={"page": page + 1})

    def parse_detail(self, response):
        sel = Selector(response.text)
        ds = self.cfg.detail_selectors
        meta = response.meta

        def extract(css: str) -> str:
            return (sel.css(css).get("") or "").strip() if css else ""

        def extract_all(css: str) -> list[str]:
            return [x.strip() for x in sel.css(css).getall()] if css else []

        title = extract(ds.title)
        project_no_raw = extract(ds.project_no)
        budget_raw = extract(ds.budget)
        content_html = sel.css(ds.content).get("") if ds.content else ""
        content = clean_html(content_html)

        budget = parse_amount(budget_raw) if budget_raw else None

        attachments = []
        for href in extract_all(ds.attachments):
            if href and (href.endswith((".pdf", ".doc", ".docx", ".xls", ".xlsx", ".zip")) or "/attachment" in href):
                attachments.append(AttachmentItem(
                    name=href.split("/")[-1], download_url=urljoin(response.url, href),
                ))

        notice = NoticeItem(
            source_key=self.source_key,
            notice_id=meta.get("notice_id") or notice_id_from_url(response.url),
            title=title or response.url,
            project_no=normalize_project_no(project_no_raw),
            project_name=title,
            procurement_unit=extract(ds.procurement_unit) or None,
            agency=extract(ds.agency) or None,
            budget=budget or __import__("parsers.models", fromlist=["Amount"]).Amount(),
            publish_time=parse_time(meta.get("publish_time")) or parse_time(extract(ds.publish_time)),
            content=content,
            original_url=response.url,
            attachments=attachments,
        )

        yield NoticeItemWrapper(notice=notice, html=response.text)
