"""动态站爬虫（Playwright）- 渲染动态页并提取公告。

示例真实源：某券商官网采购栏目（如 citic 中信证券）。
复用浏览器上下文发起详情请求，避免 token 失效。
"""
from __future__ import annotations

import argparse
import logging
from urllib.parse import urljoin

from parsel import Selector
from playwright.sync_api import sync_playwright

from db.session import get_session
from parsers.amount import parse_amount
from parsers.models import AttachmentItem, NoticeItem
from parsers.text import clean_html, notice_id_from_url, normalize_project_no
from parsers.time_parser import parse_time
from pipelines.db_pipeline import save_raw_page, upsert_notice
from sources.loader import load_source
from spiders.utils import default_headers, rate_limit
from tagging.classifier import TaggingPipeline

logger = logging.getLogger(__name__)


def run_dynamic(source_key: str = "citic", max_pages: int = 3) -> int:
    cfg = load_source(source_key)
    tagger = TaggingPipeline()
    saved = 0

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        context = browser.new_context(user_agent=default_headers()["User-Agent"])
        page = context.new_page()

        ls = cfg.list_selectors
        page.goto(cfg.entry_url, wait_until="networkidle", timeout=30000)

        # 提取列表项
        items = page.query_selector_all(ls.item) if ls.item else []
        for it in items:
            try:
                link_el = it.query_selector("a") if not ls.link else it
                link = link_el.get_attribute("href") if link_el else ""
                if not link:
                    continue
                abs_link = urljoin(cfg.entry_url, link)
                title = (it.inner_text() or "").strip()[:200]
                # 复用浏览器上下文访问详情页
                detail_page = context.new_page()
                detail_page.goto(abs_link, wait_until="domcontentloaded", timeout=30000)
                html = detail_page.content()
                _process_detail(source_key, cfg, abs_link, html, title, tagger)
                detail_page.close()
                saved += 1
                rate_limit(cfg.anti_crawl.rate_limit_seconds)
            except Exception as e:
                logger.warning("Failed to process item: %s", e)
                continue

        browser.close()
    return saved


def _process_detail(source_key, cfg, url, html, fallback_title, tagger):
    sel = Selector(html)
    ds = cfg.detail_selectors

    def extract(css: str) -> str:
        return (sel.css(css).get("") or "").strip() if css else ""

    title = extract(ds.title) or fallback_title
    content_html = sel.css(ds.content).get("") if ds.content else ""
    content = clean_html(content_html)
    budget_raw = extract(ds.budget)
    budget = parse_amount(budget_raw) if budget_raw else None

    attachments = []
    if ds.attachments:
        for href in sel.css(ds.attachments).getall():
            if href and any(href.lower().endswith(ext) for ext in
                            (".pdf", ".doc", ".docx", ".xls", ".xlsx", ".zip")):
                attachments.append(AttachmentItem(
                    name=href.split("/")[-1], download_url=urljoin(url, href),
                ))

    notice = NoticeItem(
        source_key=source_key,
        notice_id=notice_id_from_url(url),
        title=title,
        project_no=normalize_project_no(extract(ds.project_no)),
        project_name=title,
        procurement_unit=extract(ds.procurement_unit) or None,
        agency=extract(ds.agency) or None,
        budget=budget or __import__("parsers.models", fromlist=["Amount"]).Amount(),
        publish_time=parse_time(extract(ds.publish_time)),
        content=content,
        original_url=url,
        attachments=attachments,
    )

    with get_session() as session:
        from pipelines.db_pipeline import get_or_create_source
        source = get_or_create_source(session, source_key)
        raw = save_raw_page(session, source.id, url, html)
        n, _ = upsert_notice(session, notice, raw_page_id=raw.id, html=html)
        tagger.tag_notice(session, n)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default="citic")
    parser.add_argument("--max-pages", type=int, default=3)
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO)
    n = run_dynamic(args.source, args.max_pages)
    print(f"Saved {n} notices")
