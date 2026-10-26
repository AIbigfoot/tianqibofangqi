"""静态站爬虫启动入口。

用法：python -m spiders.run_static --source cebpubservice [--max-pages 5]
"""
from __future__ import annotations

import argparse

from scrapy.crawler import CrawlerProcess
from scrapy.utils.project import get_project_settings

from spiders.static_spider import StaticSpider


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default="cebpubservice", help="数据源 key")
    parser.add_argument("--max-pages", type=int, default=None, help="最大翻页数")
    args = parser.parse_args()

    settings = get_project_settings()
    settings.set("SPIDER_MODULES", ["spiders"])
    settings.set("ITEM_PIPELINES", {"spiders.scrapy_pipeline.DBPipeline": 300})
    settings.set("ROBOTSTXT_OBEY", True)
    settings.set("DOWNLOAD_DELAY", 3)

    process = CrawlerProcess(settings)
    process.crawl(StaticSpider, source=args.source, max_pages=args.max_pages)
    process.start()


if __name__ == "__main__":
    main()
