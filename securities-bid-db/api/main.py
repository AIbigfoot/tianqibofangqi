"""FastAPI 检索 API：多条件筛选、分页、Excel 导出。"""
from __future__ import annotations

import io
from datetime import datetime
from decimal import Decimal
from typing import Optional

import openpyxl
from fastapi import FastAPI, HTTPException, Query
from fastapi.responses import StreamingResponse
from sqlalchemy import func, or_, select
from sqlalchemy.orm import Session

from db.models import Notice, NoticeTag, Tag
from db.session import get_session

app = FastAPI(title="证券公司公开招标信息特色数据库 API", version="0.1.0")


def _notice_to_dict(n: Notice) -> dict:
    return {
        "id": n.id,
        "source_key": n.source_id,
        "notice_id": n.notice_id,
        "title": n.title,
        "project_no": n.project_no,
        "procurement_unit": n.procurement_unit,
        "agency": n.agency,
        "notice_type": n.notice_type,
        "procurement_method": n.procurement_method,
        "budget_original": n.budget_original,
        "budget_value": float(n.budget_value) if n.budget_value else None,
        "publish_time": n.publish_time.isoformat() if n.publish_time else None,
        "bid_deadline": n.bid_deadline.isoformat() if n.bid_deadline else None,
        "project_location": n.project_location,
        "original_url": n.original_url,
        "is_change": n.is_change,
    }


@app.get("/api/notices")
def list_notices(
    unit: Optional[str] = None,
    module: Optional[str] = None,
    amount_min: Optional[float] = None,
    amount_max: Optional[float] = None,
    date_from: Optional[str] = None,
    date_to: Optional[str] = None,
    procurement_method: Optional[str] = None,
    keyword: Optional[str] = None,
    page: int = Query(1, ge=1),
    page_size: int = Query(20, ge=1, le=100),
):
    with get_session() as session:
        stmt = select(Notice)
        count_stmt = select(func.count()).select_from(Notice)

        if unit:
            stmt = stmt.where(Notice.procurement_unit.ilike(f"%{unit}%"))
            count_stmt = count_stmt.where(Notice.procurement_unit.ilike(f"%{unit}%"))
        if amount_min is not None:
            stmt = stmt.where(Notice.budget_value >= amount_min)
            count_stmt = count_stmt.where(Notice.budget_value >= amount_min)
        if amount_max is not None:
            stmt = stmt.where(Notice.budget_value <= amount_max)
            count_stmt = count_stmt.where(Notice.budget_value <= amount_max)
        if date_from:
            d = datetime.fromisoformat(date_from)
            stmt = stmt.where(Notice.publish_time >= d)
            count_stmt = count_stmt.where(Notice.publish_time >= d)
        if date_to:
            d = datetime.fromisoformat(date_to)
            stmt = stmt.where(Notice.publish_time <= d)
            count_stmt = count_stmt.where(Notice.publish_time <= d)
        if procurement_method:
            stmt = stmt.where(Notice.procurement_method == procurement_method)
            count_stmt = count_stmt.where(Notice.procurement_method == procurement_method)
        if module:
            stmt = stmt.join(NoticeTag, NoticeTag.notice_id == Notice.id).join(
                Tag, Tag.id == NoticeTag.tag_id
            ).where(Tag.dimension == "module", Tag.name == module)
        if keyword:
            # 全文检索 via tsvector
            tsq = func.plainto_tsquery("simple", keyword)
            stmt = stmt.where(Notice.content_tsv.op("@@")(tsq))
            count_stmt = count_stmt.where(Notice.content_tsv.op("@@")(tsq))

        total = session.execute(count_stmt).scalar_one()
        stmt = stmt.order_by(Notice.publish_time.desc()).offset(
            (page - 1) * page_size).limit(page_size)
        rows = session.execute(stmt).scalars().all()

        return {
            "total": total,
            "page": page,
            "page_size": page_size,
            "items": [_notice_to_dict(n) for n in rows],
        }


@app.get("/api/notices/export")
def export_notices(
    unit: Optional[str] = None,
    module: Optional[str] = None,
    amount_min: Optional[float] = None,
    amount_max: Optional[float] = None,
    keyword: Optional[str] = None,
):
    """导出 Excel。"""
    wb = openpyxl.Workbook()
    ws = wb.active
    ws.title = "招标公告"
    headers = ["ID", "标题", "项目编号", "采购单位", "公告类型", "采购方式",
               "预算原值", "预算(元)", "发布时间", "投标截止", "原文链接"]
    ws.append(headers)
    with get_session() as session:
        stmt = select(Notice)
        if unit:
            stmt = stmt.where(Notice.procurement_unit.ilike(f"%{unit}%"))
        if amount_min is not None:
            stmt = stmt.where(Notice.budget_value >= amount_min)
        if amount_max is not None:
            stmt = stmt.where(Notice.budget_value <= amount_max)
        if keyword:
            tsq = func.plainto_tsquery("simple", keyword)
            stmt = stmt.where(Notice.content_tsv.op("@@")(tsq))
        rows = session.execute(stmt).scalars().all()
        for n in rows:
            ws.append([
                n.id, n.title, n.project_no, n.procurement_unit, n.notice_type,
                n.procurement_method, n.budget_original,
                float(n.budget_value) if n.budget_value else None,
                n.publish_time.isoformat() if n.publish_time else None,
                n.bid_deadline.isoformat() if n.bid_deadline else None,
                n.original_url,
            ])

    buf = io.BytesIO()
    wb.save(buf)
    buf.seek(0)
    return StreamingResponse(
        buf,
        media_type="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        headers={"Content-Disposition": "attachment; filename=notices.xlsx"},
    )


@app.get("/api/notices/{notice_id}")
def get_notice(notice_id: int):
    with get_session() as session:
        n = session.execute(
            select(Notice).where(Notice.id == notice_id)
        ).scalar_one_or_none()
        if not n:
            raise HTTPException(status_code=404, detail="Notice not found")
        return _notice_to_dict(n)


@app.get("/api/health")
def health():
    return {"status": "ok"}


if __name__ == "__main__":
    import uvicorn
    from config import get_settings
    s = get_settings()
    uvicorn.run(app, host=s.api.host, port=s.api.port)
