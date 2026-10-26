"""统一的 Pydantic 数据模型 - 爬虫解析层输出。"""
from __future__ import annotations

from datetime import datetime
from decimal import Decimal
from typing import Any

from pydantic import BaseModel, Field


class Amount(BaseModel):
    """金额子模型：原值 + 数值 + 单位 + 币种。"""
    original: str = ""                     # 原值，如 "1,200万元"
    value: Decimal | None = None           # 数值（统一为元）
    unit: str = "元"                        # 归一单位
    currency: str = "CNY"

    class Config:
        json_encoders = {Decimal: float}


class AttachmentItem(BaseModel):
    name: str = ""
    download_url: str = ""
    local_path: str = ""
    file_type: str = ""
    file_size: int = 0


class NoticeItem(BaseModel):
    """公告统一数据模型（详情页解析输出）。"""
    source_key: str
    notice_id: str                         # 源站唯一键
    title: str
    project_no: str | None = None
    project_name: str | None = None
    procurement_unit: str | None = None    # 采购单位
    agency: str | None = None              # 招标代理机构
    notice_type: str | None = None         # 公告类型
    procurement_method: str | None = None  # 采购方式
    org_form: str | None = None            # 自行招标/委托代理
    budget: Amount = Field(default_factory=Amount)
    max_price: Amount | None = None
    fund_source: str | None = None
    package_count: int | None = None
    publish_time: datetime | None = None
    signup_deadline: datetime | None = None
    bid_deadline: datetime | None = None
    open_time: datetime | None = None
    project_location: str | None = None
    bidder_qualification: str | None = None
    contact_person: str | None = None
    contact_phone: str | None = None
    contact_email: str | None = None
    content: str = ""                      # 正文全文
    original_url: str = ""
    snapshot_path: str | None = None
    is_change: bool = False
    change_mark: str | None = None
    related_notice_ids: list[str] = Field(default_factory=list)
    attachments: list[AttachmentItem] = Field(default_factory=list)
    extra_fields: dict[str, Any] = Field(default_factory=dict)

    class Config:
        json_encoders = {Decimal: float}
