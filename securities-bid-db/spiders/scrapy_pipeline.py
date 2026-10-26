"""Scrapy 管道：调用 db_pipeline 入库 + 打标。"""
from __future__ import annotations

import logging

from db.session import get_session
from pipelines.db_pipeline import save_raw_page, upsert_notice
from tagging.classifier import TaggingPipeline

logger = logging.getLogger(__name__)


class DBPipeline:
    def __init__(self) -> None:
        self.tagger = TaggingPipeline()

    def process_item(self, item, spider):
        notice_item = item["notice"]
        html = item.get("html", "")
        with get_session() as session:
            try:
                from pipelines.db_pipeline import get_or_create_source
                source = get_or_create_source(session, notice_item.source_key)
                raw = save_raw_page(session, source.id, notice_item.original_url, html)
                notice, is_new = upsert_notice(session, notice_item, raw_page_id=raw.id, html=html)
                self.tagger.tag_notice(session, notice)
                logger.info("Saved notice id=%s new=%s title=%s", notice.id, is_new, notice.title[:40])
            except Exception as e:
                logger.exception("Failed to save notice: %s", e)
                raise
        return item
