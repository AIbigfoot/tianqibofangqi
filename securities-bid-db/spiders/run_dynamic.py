"""动态站爬虫启动入口。

用法：python -m spiders.run_dynamic --source citic
"""
from spiders.dynamic_spider import run_dynamic
import argparse
import logging

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", default="citic")
    parser.add_argument("--max-pages", type=int, default=3)
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO)
    n = run_dynamic(args.source, args.max_pages)
    print(f"Saved {n} notices")
