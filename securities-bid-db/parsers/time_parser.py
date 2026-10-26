"""时间解析器：多种中文日期格式 → datetime。"""
from __future__ import annotations

import re
from datetime import datetime

_PATTERNS = [
    # 2024年1月1日 / 2024年01月01日
    (re.compile(r"(\d{4})年(\d{1,2})月(\d{1,2})日(?:\s+(\d{1,2}):(\d{1,2}))?"), "%Y-%m-%d %H:%M"),
    # 2024-01-01 / 2024-01-01 09:00:00
    (re.compile(r"(\d{4})-(\d{1,2})-(\d{1,2})(?:\s+(\d{1,2}):(\d{1,2})(?::(\d{1,2}))?)?"), "%Y-%m-%d %H:%M:%S"),
    # 2024/01/01
    (re.compile(r"(\d{4})/(\d{1,2})/(\d{1,2})(?:\s+(\d{1,2}):(\d{1,2}))?"), "%Y/%m/%d %H:%M"),
    # 2024.01.01
    (re.compile(r"(\d{4})\.(\d{1,2})\.(\d{1,2})(?:\s+(\d{1,2}):(\d{1,2}))?"), "%Y.%m.%d %H:%M"),
]


def parse_time(text: str | None) -> datetime | None:
    """解析中文日期时间字符串，失败返回 None。"""
    if not text:
        return None
    text = text.strip()
    for pat, _ in _PATTERNS:
        m = pat.search(text)
        if not m:
            continue
        groups = m.groups()
        year, month, day = int(groups[0]), int(groups[1]), int(groups[2])
        hour = int(groups[3]) if len(groups) > 3 and groups[3] else 0
        minute = int(groups[4]) if len(groups) > 4 and groups[4] else 0
        second = int(groups[5]) if len(groups) > 5 and groups[5] else 0
        try:
            return datetime(year, month, day, hour, minute, second)
        except ValueError:
            continue
    return None
