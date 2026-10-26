"""Scrapy Item 包装 NoticeItem。"""
from __future__ import annotations

import scrapy

from parsers.models import NoticeItem


class NoticeItemWrapper(scrapy.Item):
    notice = scrapy.Field()  # 存放 NoticeItem 实例
