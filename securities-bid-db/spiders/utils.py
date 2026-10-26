"""爬虫共用工具：UA 轮换、限速、请求头构造。"""
from __future__ import annotations

import random
import time
from urllib.parse import urljoin

from config import get_settings

_settings = get_settings()


def get_random_ua() -> str:
    return random.choice(_settings.user_agents) if _settings.user_agents else (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"
    )


def default_headers(referer: str = "") -> dict[str, str]:
    h = {
        "User-Agent": get_random_ua(),
        "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8",
        "Accept-Encoding": "gzip, deflate",
        "Connection": "keep-alive",
    }
    if referer:
        h["Referer"] = referer
    return h


def rate_limit(base_seconds: int | None = None) -> None:
    """限速 + 随机抖动。"""
    base = base_seconds or _settings.crawl.default_rate_limit_seconds
    lo, hi = _settings.crawl.random_jitter_range
    time.sleep(base + random.uniform(lo, hi))


def abs_url(href: str, base_url: str) -> str:
    return urljoin(base_url, href)
