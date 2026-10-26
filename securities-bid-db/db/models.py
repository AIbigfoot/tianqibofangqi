"""SQLAlchemy 2.x ORM 模型 - 对应 db/schema.sql 全部表。"""
from __future__ import annotations

from datetime import datetime
from decimal import Decimal

from sqlalchemy import (
    BigInteger, Boolean, Column, DateTime, ForeignKey, Integer, Numeric,
    SmallInteger, String, Text, UniqueConstraint, Index, JSON,
)
from sqlalchemy.dialects.postgresql import JSONB, TSVECTOR
from sqlalchemy.orm import declarative_base, relationship

Base = declarative_base()


class SourceSite(Base):
    __tablename__ = "source_site"
    id = Column(BigInteger, primary_key=True)
    source_key = Column(String(64), unique=True, nullable=False)
    name = Column(String(128), nullable=False)
    source_type = Column(String(16), nullable=False)
    entry_url = Column(Text, nullable=False)
    domain = Column(String(255))
    config = Column(JSONB)
    enabled = Column(Boolean, default=True)
    rate_limit = Column(Integer, default=3)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    updated_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class RawPage(Base):
    __tablename__ = "raw_page"
    id = Column(BigInteger, primary_key=True)
    source_id = Column(BigInteger, ForeignKey("source_site.id"), nullable=False)
    url = Column(Text, nullable=False)
    http_status = Column(Integer)
    content_type = Column(String(128))
    raw_html = Column(Text)
    snapshot_path = Column(Text)
    fetched_at = Column(DateTime(timezone=True), nullable=False, default=datetime.utcnow)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class Notice(Base):
    __tablename__ = "notice"
    id = Column(BigInteger, primary_key=True, autoincrement=True)
    source_id = Column(BigInteger, ForeignKey("source_site.id"), nullable=False)
    notice_id = Column(String(255), nullable=False)
    title = Column(Text, nullable=False)
    project_no = Column(String(128))
    project_name = Column(Text)
    procurement_unit = Column(String(255))
    agency = Column(String(255))
    notice_type = Column(String(32))
    procurement_method = Column(String(32))
    org_form = Column(String(16))
    budget_original = Column(Text)
    budget_value = Column(Numeric(18, 2))
    budget_currency = Column(String(8), default="CNY")
    max_price = Column(Numeric(18, 2))
    fund_source = Column(String(128))
    package_count = Column(Integer)
    publish_time = Column(DateTime(timezone=True), primary_key=True)  # 分区键
    signup_deadline = Column(DateTime(timezone=True))
    bid_deadline = Column(DateTime(timezone=True))
    open_time = Column(DateTime(timezone=True))
    project_location = Column(String(255))
    bidder_qualification = Column(Text)
    contact_person = Column(String(128))
    contact_phone = Column(String(64))
    contact_email = Column(String(128))
    content = Column(Text)
    content_tsv = Column(TSVECTOR)
    original_url = Column(Text)
    snapshot_path = Column(Text)
    is_change = Column(Boolean, default=False)
    change_mark = Column(String(64))
    related_notice_ids = Column(JSONB)
    project_id = Column(BigInteger)
    review_required = Column(Boolean, default=False)
    raw_page_id = Column(BigInteger, ForeignKey("raw_page.id"))
    dedup_hash = Column(String(64))
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    updated_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class NoticeField(Base):
    __tablename__ = "notice_field"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger, nullable=False)
    field_key = Column(String(64), nullable=False)
    field_value = Column(Text)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class Attachment(Base):
    __tablename__ = "attachment"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger, nullable=False)
    name = Column(String(255))
    download_url = Column(Text)
    local_path = Column(Text)
    file_type = Column(String(32))
    file_size = Column(BigInteger)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class Tag(Base):
    __tablename__ = "tag"
    id = Column(BigInteger, primary_key=True)
    dimension = Column(String(64), nullable=False)
    tag_key = Column(String(128), nullable=False)
    name = Column(String(128), nullable=False)
    parent_id = Column(BigInteger, ForeignKey("tag.id"))
    level = Column(SmallInteger, default=1)
    sort_order = Column(Integer, default=0)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    __table_args__ = (UniqueConstraint("dimension", "tag_key"),)


class NoticeTag(Base):
    __tablename__ = "notice_tag"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger, nullable=False)
    tag_id = Column(BigInteger, ForeignKey("tag.id"), nullable=False)
    source = Column(String(16), default="rule")
    confidence = Column(Numeric(4, 3), default=1.0)
    matched_keywords = Column(JSONB)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    __table_args__ = (UniqueConstraint("notice_id", "tag_id"),)


class Project(Base):
    __tablename__ = "project"
    id = Column(BigInteger, primary_key=True)
    project_no = Column(String(128), unique=True)
    project_name = Column(Text)
    procurement_unit = Column(String(255))
    total_budget = Column(Numeric(18, 2))
    status = Column(String(32))
    region = Column(String(64))
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    updated_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class NoticeChangeLog(Base):
    __tablename__ = "notice_change_log"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger, nullable=False)
    field_name = Column(String(64), nullable=False)
    old_value = Column(Text)
    new_value = Column(Text)
    change_type = Column(String(32))
    detected_at = Column(DateTime(timezone=True), default=datetime.utcnow)
    alerted = Column(Boolean, default=False)


class BidResult(Base):
    __tablename__ = "bid_result"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger, nullable=False)
    project_id = Column(BigInteger)
    winner = Column(String(255))
    bid_amount = Column(Numeric(18, 2))
    rank = Column(Integer)
    is_candidate = Column(Boolean, default=False)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class CrawlTask(Base):
    __tablename__ = "crawl_task"
    id = Column(BigInteger, primary_key=True)
    source_id = Column(BigInteger, ForeignKey("source_site.id"), nullable=False)
    task_type = Column(String(16), nullable=False)
    status = Column(String(16), default="pending")
    started_at = Column(DateTime(timezone=True))
    finished_at = Column(DateTime(timezone=True))
    fetched_count = Column(Integer, default=0)
    new_count = Column(Integer, default=0)
    error_msg = Column(Text)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class CrawlLog(Base):
    __tablename__ = "crawl_log"
    id = Column(BigInteger, primary_key=True)
    task_id = Column(BigInteger, ForeignKey("crawl_task.id"))
    source_id = Column(BigInteger)
    level = Column(String(16))
    url = Column(Text)
    message = Column(Text)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)


class DedupFingerprint(Base):
    __tablename__ = "dedup_fingerprint"
    id = Column(BigInteger, primary_key=True)
    notice_id = Column(BigInteger)
    source_id = Column(BigInteger)
    title_hash = Column(String(64))
    content_hash = Column(String(64))
    fingerprint = Column(String(64), unique=True, nullable=False)
    created_at = Column(DateTime(timezone=True), default=datetime.utcnow)
