"""增量调度器：APScheduler 定时任务 + crawl_task/crawl_log 记录。

- 增量抓取：每 6 小时，列表页倒序，遇已入库 ID 连续 N 条即停
- 全量校准：每周日
"""
from __future__ import annotations

import logging
from datetime import datetime

from apscheduler.schedulers.background import BackgroundScheduler
from apscheduler.triggers.cron import CronTrigger
from apscheduler.triggers.interval import IntervalTrigger
from sqlalchemy import select

from config import get_settings
from db.models import CrawlLog, CrawlTask, SourceSite
from db.session import get_session
from sources.loader import load_all_sources

logger = logging.getLogger(__name__)
_settings = get_settings()


def _create_task(session, source_id: int, task_type: str) -> CrawlTask:
    task = CrawlTask(source_id=source_id, task_type=task_type,
                     status="running", started_at=datetime.utcnow())
    session.add(task)
    session.flush()
    return task


def _finish_task(session, task: CrawlTask, fetched: int, new: int, error: str | None) -> None:
    task.status = "failed" if error else "success"
    task.finished_at = datetime.utcnow()
    task.fetched_count = fetched
    task.new_count = new
    task.error_msg = error


def _log(session, task_id: int, source_id: int, level: str, msg: str, url: str = "") -> None:
    session.add(CrawlLog(task_id=task_id, source_id=source_id, level=level,
                         message=msg, url=url))


def run_incremental(source_key: str) -> None:
    """执行单源增量抓取。"""
    from spiders.dynamic_spider import run_dynamic
    from spiders.static_spider import StaticSpider
    from scrapy.crawler import CrawlerProcess
    from scrapy.utils.project import get_project_settings

    with get_session() as session:
        src = session.execute(
            select(SourceSite).where(SourceSite.source_key == source_key)
        ).scalar_one_or_none()
        if not src:
            src = SourceSite(source_key=source_key, name=source_key,
                             source_type="static", entry_url="")
            session.add(src)
            session.flush()
        task = _create_task(session, src.id, "incremental")
        _log(session, task.id, src.id, "INFO", f"Start incremental crawl: {source_key}")

    try:
        cfg = load_all_sources()[source_key]
        if cfg.type == "dynamic":
            saved = run_dynamic(source_key, max_pages=3)
        else:
            saved = _run_static_crawl(source_key)
        with get_session() as session:
            task = session.get(CrawlTask, task.id)
            _finish_task(session, task, saved, saved, None)
            _log(session, task.id, src.id, "INFO", f"Done: fetched={saved}")
    except Exception as e:
        logger.exception("Incremental crawl failed: %s", source_key)
        with get_session() as session:
            task = session.get(CrawlTask, task.id)
            _finish_task(session, task, 0, 0, str(e))
            _log(session, task.id, src.id, "ERROR", str(e))


def _run_static_crawl(source_key: str) -> int:
    from scrapy.crawler import CrawlerProcess
    from scrapy.utils.project import get_project_settings
    from spiders.static_spider import StaticSpider

    settings = get_project_settings()
    settings.set("SPIDER_MODULES", ["spiders"])
    settings.set("ITEM_PIPELINES", {"spiders.scrapy_pipeline.DBPipeline": 300})
    process = CrawlerProcess(settings)
    process.crawl(StaticSpider, source=source_key, max_pages=3)
    process.start()
    return 0  # Scrapy 异步，实际计数由 pipeline 写入


def run_full_calibration() -> None:
    """全量校准：遍历所有启用源。"""
    sources = load_all_sources()
    for key in sources:
        run_incremental(key)


def start_scheduler() -> BackgroundScheduler:
    """启动调度器。"""
    scheduler = BackgroundScheduler()

    # 增量：每 6 小时
    scheduler.add_job(
        run_full_calibration,
        trigger=IntervalTrigger(hours=_settings.scheduler.incremental_interval_hours),
        id="incremental_crawl",
        replace_existing=True,
    )

    # 全量校准：每周日
    scheduler.add_job(
        run_full_calibration,
        trigger=CronTrigger(day_of_week=_settings.scheduler.full_calibration_day,
                            hour=_settings.scheduler.full_calibration_hour),
        id="full_calibration",
        replace_existing=True,
    )

    scheduler.start()
    logger.info("Scheduler started. Incremental every %dh, full on %s",
                _settings.scheduler.incremental_interval_hours,
                _settings.scheduler.full_calibration_day)
    return scheduler


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    sched = start_scheduler()
    try:
        import time
        while True:
            time.sleep(60)
    except (KeyboardInterrupt, SystemExit):
        sched.shutdown()
