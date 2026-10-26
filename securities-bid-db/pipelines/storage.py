"""附件与页面快照存储（本地目录模拟对象存储）。"""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
from urllib.parse import urljoin

import requests

from config import get_settings

_settings = get_settings()


def ensure_dirs() -> None:
    Path(_settings.storage.attachments_dir).mkdir(parents=True, exist_ok=True)
    Path(_settings.storage.snapshots_dir).mkdir(parents=True, exist_ok=True)


def save_snapshot(url: str, html: str) -> str:
    """保存页面快照，返回本地相对路径。"""
    ensure_dirs()
    name = hashlib.md5(url.encode("utf-8")).hexdigest() + ".html"
    path = Path(_settings.storage.snapshots_dir) / name
    path.write_text(html, encoding="utf-8")
    return str(path)


def save_attachment(source_url: str, name: str, content: bytes) -> str:
    """保存附件到本地，返回本地路径。"""
    ensure_dirs()
    safe_name = re.sub(r"[^\w\.\-]", "_", name) or "attachment"
    path = Path(_settings.storage.attachments_dir) / safe_name
    path.write_bytes(content)
    return str(path)


def download_attachment(url: str, base_url: str = "", timeout: int = 30) -> bytes | None:
    """下载附件内容。"""
    full_url = urljoin(base_url, url) if base_url else url
    try:
        resp = requests.get(full_url, timeout=timeout, stream=True)
        if resp.status_code == 200:
            return resp.content
    except Exception:
        return None
    return None


import re  # noqa: E402
