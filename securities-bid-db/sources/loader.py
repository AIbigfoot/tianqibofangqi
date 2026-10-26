"""数据源 YAML 配置加载器。"""
from __future__ import annotations

from pathlib import Path
from typing import Any

import yaml
from pydantic import BaseModel, Field

SOURCES_DIR = Path(__file__).resolve().parent


class ListPagination(BaseModel):
    rule: str = "query_param"
    page_param: str = "pageNo"
    start_page: int = 1
    max_pages: int = 50
    page_size: int = 20


class ListSelectors(BaseModel):
    item: str = ""
    title: str = ""
    link: str = ""
    publish_time: str = ""


class DetailSelectors(BaseModel):
    title: str = ""
    project_no: str = ""
    publish_time: str = ""
    budget: str = ""
    procurement_unit: str = ""
    agency: str = ""
    content: str = ""
    attachments: str = ""
    contact: str = ""
    phone: str = ""


class AntiCrawl(BaseModel):
    rate_limit_seconds: int = 3
    ua_rotation: bool = True
    strategy: str = "requests_headers"
    notes: str = ""


class Compliance(BaseModel):
    robots: str = ""
    notes: str = ""


class SourceConfig(BaseModel):
    name: str
    key: str
    type: str = "static"
    entry_url: str
    domain: str = ""
    list_pagination: ListPagination = Field(default_factory=ListPagination)
    list_selectors: ListSelectors = Field(default_factory=ListSelectors)
    detail_selectors: DetailSelectors = Field(default_factory=DetailSelectors)
    anti_crawl: AntiCrawl = Field(default_factory=AntiCrawl)
    compliance: Compliance = Field(default_factory=Compliance)


def load_source(key: str) -> SourceConfig:
    path = SOURCES_DIR / f"{key}.yml"
    if not path.exists():
        raise FileNotFoundError(f"数据源配置不存在: {path}")
    with open(path, "r", encoding="utf-8") as f:
        raw: dict[str, Any] = yaml.safe_load(f)
    return SourceConfig(**raw)


def load_all_sources() -> dict[str, SourceConfig]:
    sources: dict[str, SourceConfig] = {}
    for path in SOURCES_DIR.glob("*.yml"):
        with open(path, "r", encoding="utf-8") as f:
            raw = yaml.safe_load(f)
        if not isinstance(raw, dict) or "key" not in raw:
            continue
        sources[raw["key"]] = SourceConfig(**raw)
    return sources
