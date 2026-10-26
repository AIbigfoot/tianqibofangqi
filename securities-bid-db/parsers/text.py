"""文本清洗工具：HTML 去标签、空白归一、项目编号归一化。"""
from __future__ import annotations

import hashlib
import re

from lxml import html as lxml_html


def clean_html(html_text: str) -> str:
    """去除 HTML 标签，保留纯文本，归一空白。"""
    if not html_text:
        return ""
    try:
        doc = lxml_html.fromstring(html_text)
        text = doc.text_content()
    except Exception:
        text = re.sub(r"<[^>]+>", "", html_text)
    text = re.sub(r"&nbsp;", " ", text)
    text = re.sub(r"[ \t]+", " ", text)
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip()


def normalize_whitespace(text: str) -> str:
    return re.sub(r"\s+", " ", text or "").strip()


# 项目编号归一化：去除空白、统一大小写、去除常见前缀括号
_PROJECT_NO_RE = re.compile(r"[（(]?([A-Za-z0-9][A-Za-z0-9\-_]{4,})[)）]?")


def normalize_project_no(text: str | None) -> str | None:
    """归一化项目编号：取第一个字母数字串，去空白。"""
    if not text:
        return None
    m = _PROJECT_NO_RE.search(text)
    if m:
        return m.group(1).strip().upper()
    cleaned = re.sub(r"\s+", "", text)
    return cleaned or None


def title_fingerprint(title: str) -> str:
    """标题指纹：去标点空白后 MD5。"""
    t = re.sub(r"[\s\W]+", "", title or "").lower()
    return hashlib.md5(t.encode("utf-8")).hexdigest()


def notice_id_from_url(url: str) -> str:
    """从 URL 生成公告 ID（当源站无原生 ID 时）。"""
    return hashlib.md5(url.encode("utf-8")).hexdigest()[:16]
