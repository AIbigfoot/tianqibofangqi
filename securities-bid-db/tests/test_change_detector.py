"""变更检测单元测试。"""
from datetime import datetime
from unittest.mock import MagicMock

from db.models import Notice
from pipelines.change_detector import detect_changes


def _make_session():
    """mock session，收集 add 的对象。"""
    session = MagicMock()
    added = []
    session.add.side_effect = lambda obj: added.append(obj)
    return session, added


def test_detect_deadline_change():
    notice = Notice(id=1, bid_deadline=datetime(2024, 1, 1, 9, 0))
    session, added = _make_session()
    new_vals = {"bid_deadline": datetime(2024, 2, 1, 9, 0)}
    logs = detect_changes(session, notice, new_vals)
    assert len(logs) == 1
    assert logs[0].field_name == "bid_deadline"
    assert logs[0].change_type == "延期"


def test_detect_budget_change():
    notice = Notice(id=1, budget_value="1000000")
    session, added = _make_session()
    new_vals = {"budget_value": "2000000"}
    logs = detect_changes(session, notice, new_vals)
    assert len(logs) == 1
    assert logs[0].change_type == "金额变更"


def test_no_change():
    notice = Notice(id=1, title="服务器采购", budget_value="1000000")
    session, added = _make_session()
    new_vals = {"title": "服务器采购", "budget_value": "1000000"}
    logs = detect_changes(session, notice, new_vals)
    assert len(logs) == 0
