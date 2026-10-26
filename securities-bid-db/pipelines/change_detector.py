"""变更检测：同一公告二次抓取后字段 diff，生成 notice_change_log。"""
from __future__ import annotations

import logging
from datetime import datetime
from typing import Any

from sqlalchemy import select
from sqlalchemy.orm import Session

from db.models import Notice, NoticeChangeLog

logger = logging.getLogger(__name__)

# 变更类型映射
_CHANGE_TYPE_MAP = {
    "bid_deadline": "延期",
    "signup_deadline": "延期",
    "open_time": "延期",
    "budget_value": "金额变更",
    "max_price": "金额变更",
    "notice_type": "中标补充",
}

# 参与 diff 的字段
_DIFF_FIELDS = [
    "title", "project_no", "budget_value", "budget_original", "max_price",
    "bid_deadline", "signup_deadline", "open_time", "procurement_method",
    "notice_type", "package_count", "content",
]


def detect_changes(session: Session, notice: Notice, new_values: dict[str, Any]) -> list[NoticeChangeLog]:
    """对比新值与已入库值，生成变更记录。"""
    logs: list[NoticeChangeLog] = []
    for field in _DIFF_FIELDS:
        old = getattr(notice, field)
        new = new_values.get(field)
        old_str = str(old) if old is not None else ""
        new_str = str(new) if new is not None else ""
        if old_str == new_str:
            continue
        change_type = _CHANGE_TYPE_MAP.get(field, "其他")
        log = NoticeChangeLog(
            notice_id=notice.id, field_name=field,
            old_value=old_str[:1000], new_value=new_str[:1000],
            change_type=change_type, detected_at=datetime.utcnow(),
        )
        session.add(log)
        logs.append(log)
        logger.info("Change detected: notice=%s field=%s type=%s",
                    notice.id, field, change_type)
    if logs:
        notice.is_change = True
        notice.updated_at = datetime.utcnow()
    return logs
