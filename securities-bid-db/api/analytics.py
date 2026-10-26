"""分析看板查询 API：发布量趋势、模块占比、券商排行、金额分布、供应商集中度。"""
from __future__ import annotations

from datetime import datetime, timedelta

from fastapi import APIRouter, Query
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from db.models import BidResult, Notice, NoticeTag, Tag
from db.session import get_session

router = APIRouter(prefix="/api/analytics", tags=["analytics"])


@router.get("/publish-trend")
def publish_trend(granularity: str = Query("day", pattern="^(day|week|month)$"),
                  days: int = 90):
    """发布量趋势。"""
    with get_session() as session:
        since = datetime.utcnow() - timedelta(days=days)
        if granularity == "day":
            group = func.date_trunc("day", Notice.publish_time)
        elif granularity == "week":
            group = func.date_trunc("week", Notice.publish_time)
        else:
            group = func.date_trunc("month", Notice.publish_time)
        stmt = select(
            group.label("period"),
            func.count().label("count"),
            func.coalesce(func.sum(Notice.budget_value), 0).label("budget"),
        ).where(Notice.publish_time >= since).group_by(group).order_by(group)
        rows = session.execute(stmt).all()
        return {"granularity": granularity, "days": days,
                "items": [{"period": r.period.isoformat(), "count": r.count,
                           "budget": float(r.budget) if r.budget else 0} for r in rows]}


@router.get("/module-share")
def module_share(days: int = 90):
    """各业务模块金额占比与项目数。"""
    with get_session() as session:
        since = datetime.utcnow() - timedelta(days=days)
        stmt = select(
            Tag.name.label("module"),
            func.count(Notice.id).label("count"),
            func.coalesce(func.sum(Notice.budget_value), 0).label("amount"),
        ).select_from(Notice).join(
            NoticeTag, NoticeTag.notice_id == Notice.id
        ).join(Tag, Tag.id == NoticeTag.tag_id).where(
            Tag.dimension == "module", Tag.level == 1,
            Notice.publish_time >= since,
        ).group_by(Tag.name).order_by(func.sum(Notice.budget_value).desc())
        rows = session.execute(stmt).all()
        total = sum(float(r.amount) for r in rows) or 1
        return {"days": days, "total_amount": total,
                "items": [{"module": r.module, "count": r.count,
                           "amount": float(r.amount) if r.amount else 0,
                           "share": float(r.amount) / total if r.amount else 0}
                          for r in rows]}


@router.get("/broker-ranking")
def broker_ranking(days: int = 90, limit: int = 20):
    """各券商采购热度排行。"""
    with get_session() as session:
        since = datetime.utcnow() - timedelta(days=days)
        stmt = select(
            Notice.procurement_unit.label("broker"),
            func.count().label("project_count"),
            func.coalesce(func.sum(Notice.budget_value), 0).label("total_budget"),
        ).where(Notice.procurement_unit.isnot(None),
                Notice.publish_time >= since).group_by(
            Notice.procurement_unit).order_by(func.sum(Notice.budget_value).desc()).limit(limit)
        rows = session.execute(stmt).all()
        return {"days": days, "items": [
            {"broker": r.broker, "project_count": r.project_count,
             "total_budget": float(r.total_budget) if r.total_budget else 0}
            for r in rows]}


@router.get("/amount-distribution")
def amount_distribution(days: int = 90):
    """预算金额分档分布。"""
    with get_session() as session:
        since = datetime.utcnow() - timedelta(days=days)
        stmt = select(
            Tag.name.label("tier"),
            func.count(Notice.id).label("count"),
            func.coalesce(func.sum(Notice.budget_value), 0).label("amount"),
        ).select_from(Notice).join(
            NoticeTag, NoticeTag.notice_id == Notice.id
        ).join(Tag, Tag.id == NoticeTag.tag_id).where(
            Tag.dimension == "amount_tier", Notice.publish_time >= since,
        ).group_by(Tag.name).order_by(Tag.name)
        rows = session.execute(stmt).all()
        return {"days": days, "items": [
            {"tier": r.tier, "count": r.count,
             "amount": float(r.amount) if r.amount else 0} for r in rows]}


@router.get("/supplier-concentration")
def supplier_concentration(days: int = 365, top: int = 10):
    """中标供应商集中度（TOP N）。"""
    with get_session() as session:
        stmt = select(
            BidResult.winner.label("supplier"),
            func.count().label("win_count"),
            func.coalesce(func.sum(BidResult.bid_amount), 0).label("total_amount"),
        ).where(BidResult.winner.isnot(None)).group_by(
            BidResult.winner).order_by(func.sum(BidResult.bid_amount).desc()).limit(top)
        rows = session.execute(stmt).all()
        return {"top": top, "items": [
            {"supplier": r.supplier, "win_count": r.win_count,
             "total_amount": float(r.total_amount) if r.total_amount else 0}
            for r in rows]}


# 挂载到主 app
from api.main import app  # noqa: E402
app.include_router(router)
