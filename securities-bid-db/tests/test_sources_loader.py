"""数据源配置加载单元测试。"""
from pathlib import Path

from sources.loader import load_all_sources, load_source


def test_load_all_sources_count():
    sources = load_all_sources()
    assert len(sources) >= 22


def test_load_source_fields():
    cfg = load_source("cebpubservice")
    assert cfg.name
    assert cfg.entry_url
    assert cfg.list_pagination.page_param
    assert cfg.detail_selectors.title
    assert cfg.anti_crawl.rate_limit_seconds >= 3
    assert cfg.compliance.robots


def test_all_sources_valid():
    sources = load_all_sources()
    assert len(sources) >= 22
    for key, cfg in sources.items():
        assert cfg.entry_url, f"{key} missing entry_url"
        assert cfg.anti_crawl.rate_limit_seconds >= 1
