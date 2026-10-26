"""去重模块：源站唯一键 + 标题 SimHash + 正文相似度。"""
from __future__ import annotations

import re
from typing import Iterable

from simhash import Simhash

from config import get_settings

_settings = get_settings()


def simhash_text(text: str, f: int = 64) -> int:
    """计算文本 SimHash。"""
    if not text:
        return 0
    tokens = re.findall(r"[\u4e00-\u9fa5A-Za-z0-9]{2,}", text)
    if not tokens:
        tokens = list(text)
    return Simhash(tokens, f=f).value


def hamming_distance(h1: int, h2: int, f: int = 64) -> int:
    x = (h1 ^ h2) & ((1 << f) - 1)
    dist = 0
    while x:
        x &= x - 1
        dist += 1
    return dist


def similarity(h1: int, h2: int, f: int = 64) -> float:
    """SimHash 相似度 = 1 - 汉明距离/f。"""
    return 1.0 - hamming_distance(h1, h2, f) / f


def is_duplicate(new_hash: int, existing_hashes: Iterable[int],
                 threshold: float | None = None) -> bool:
    """判断新指纹是否与已有指纹重复（相似度 ≥ 阈值）。"""
    threshold = threshold or _settings.tagging.similarity_threshold
    for h in existing_hashes:
        if similarity(new_hash, h) >= threshold:
            return True
    return False


def combined_fingerprint(source_id: int, notice_id: str,
                         title_hash: str, content_hash: str) -> str:
    """综合指纹 = source_id + notice_id（源站唯一键优先）。"""
    import hashlib
    raw = f"{source_id}:{notice_id}"
    return hashlib.md5(raw.encode("utf-8")).hexdigest()
