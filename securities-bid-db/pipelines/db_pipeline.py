"""数据库入库 Pipeline：NoticeItem → ORM 入库 + 去重 + 项目串联。"""
from __future__ import annotations

import logging
from datetime import datetime
from decimal import Decimal

from sqlalchemy import select
from sqlalchemy.orm import Session

from config import get_settings
from db.models import (
    Attachment, DedupFingerprint, Notice, NoticeField, Project, RawPage, SourceSite,
)
from parsers.models import NoticeItem
from parsers.text import title_fingerprint
from pipelines.dedup import combined_fingerprint, is_duplicate, simhash_text
from pipelines.project_linker import get_or_create_project
from pipelines.storage import save_snapshot

logger = logging.getLogger(__name__)
_settings = get_settings()


def get_or_create_source(session: Session, source_key: str, name: str = "",
                         source_type: str = "static", entry_url: str = "") -> SourceSite:
    stmt = select(SourceSite).where(SourceSite.source_key == source_key)
    src = session.execute(stmt).scalar_one_or_none()
    if src:
        return src
    src = SourceSite(source_key=source_key, name=name or source_key,
                     source_type=source_type, entry_url=entry_url)
    session.add(src)
    session.flush()
    return src


def save_raw_page(session: Session, source_id: int, url: str, html: str,
                  http_status: int = 200, content_type: str = "text/html") -> RawPage:
    snapshot_path = save_snapshot(url, html)
    raw = RawPage(source_id=source_id, url=url, http_status=http_status,
                  content_type=content_type, raw_html=html[:500000],
                  snapshot_path=snapshot_path)
    session.add(raw)
    session.flush()
    return raw


def upsert_notice(session: Session, item: NoticeItem, raw_page_id: int | None = None,
                  html: str = "") -> tuple[Notice, bool]:
    """入库公告。返回 (notice, is_new)。"""
    source = get_or_create_source(session, item.source_key)
    raw_page = raw_page_id

    # 去重指纹
    title_h = title_fingerprint(item.title)
    content_h = simhash_text(item.content or item.title)
    fp = combined_fingerprint(source.id, item.notice_id, title_h, str(content_h))

    # 检查是否已存在（源站唯一键）
    stmt = select(Notice).where(
        Notice.source_id == source.id, Notice.notice_id == item.notice_id
    )
    existing = session.execute(stmt).scalar_one_or_none()

    publish_time = item.publish_time or datetime.utcnow()

    if existing:
        # 更新
        existing.title = item.title
        existing.project_no = item.project_no
        existing.project_name = item.project_name
        existing.procurement_unit = item.procurement_unit
        existing.agency = item.agency
        existing.notice_type = item.notice_type
        existing.procurement_method = item.procurement_method
        existing.org_form = item.org_form
        existing.budget_original = item.budget.original
        existing.budget_value = item.budget.value
        existing.max_price = item.max_price.value if item.max_price else None
        existing.fund_source = item.fund_source
        existing.package_count = item.package_count
        existing.publish_time = publish_time
        existing.signup_deadline = item.signup_deadline
        existing.bid_deadline = item.bid_deadline
        existing.open_time = item.open_time
        existing.project_location = item.project_location
        existing.bidder_qualification = item.bidder_qualification
        existing.contact_person = item.contact_person
        existing.contact_phone = item.contact_phone
        existing.contact_email = item.contact_email
        existing.content = item.content
        existing.original_url = item.original_url
        existing.snapshot_path = item.snapshot_path
        existing.is_change = item.is_change
        existing.change_mark = item.change_mark
        existing.related_notice_ids = item.related_notice_ids
        existing.dedup_hash = fp
        if raw_page:
            existing.raw_page_id = raw_page
        existing.updated_at = datetime.utcnow()
        return existing, False

    # 标题 SimHash 去重（同源内）
    stmt2 = select(DedupFingerprint.content_hash).where(
        DedupFingerprint.source_id == source.id
    )
    existing_hashes = [int(h) for (h,) in session.execute(stmt2).all() if h]
    if is_duplicate(content_h, existing_hashes):
        logger.info("Duplicate by content similarity: %s", item.title)
        # 仍入库但标记（实际可选择跳过，这里保留以便追踪）

    notice = Notice(
        source_id=source.id,
        notice_id=item.notice_id,
        title=item.title,
        project_no=item.project_no,
        project_name=item.project_name,
        procurement_unit=item.procurement_unit,
        agency=item.agency,
        notice_type=item.notice_type,
        procurement_method=item.procurement_method,
        org_form=item.org_form,
        budget_original=item.budget.original,
        budget_value=item.budget.value,
        max_price=item.max_price.value if item.max_price else None,
        fund_source=item.fund_source,
        package_count=item.package_count,
        publish_time=publish_time,
        signup_deadline=item.signup_deadline,
        bid_deadline=item.bid_deadline,
        open_time=item.open_time,
        project_location=item.project_location,
        bidder_qualification=item.bidder_qualification,
        contact_person=item.contact_person,
        contact_phone=item.contact_phone,
        contact_email=item.contact_email,
        content=item.content,
        original_url=item.original_url,
        snapshot_path=item.snapshot_path,
        is_change=item.is_change,
        change_mark=item.change_mark,
        related_notice_ids=item.related_notice_ids or None,
        raw_page_id=raw_page,
        dedup_hash=fp,
    )
    session.add(notice)
    session.flush()

    # 写入去重指纹
    session.add(DedupFingerprint(
        notice_id=notice.id, source_id=source.id,
        title_hash=title_h, content_hash=str(content_h), fingerprint=fp,
    ))

    # 项目串联
    project = get_or_create_project(
        session, item.project_no, item.project_name, item.procurement_unit,
        item.project_location,
    )
    if project:
        notice.project_id = project.id
        if notice.budget_value:
            project.total_budget = (project.total_budget or 0) + notice.budget_value

    # 附件
    for att in item.attachments:
        session.add(Attachment(
            notice_id=notice.id, name=att.name, download_url=att.download_url,
            local_path=att.local_path, file_type=att.file_type, file_size=att.file_size,
        ))

    # 扩展字段
    for k, v in item.extra_fields.items():
        session.add(NoticeField(notice_id=notice.id, field_key=k, field_value=str(v)))

    return notice, True
