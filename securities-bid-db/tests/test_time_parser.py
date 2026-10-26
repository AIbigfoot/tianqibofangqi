"""时间解析单元测试。"""
from parsers.time_parser import parse_time


def test_chinese_date():
    t = parse_time("2024年1月1日")
    assert t.year == 2024 and t.month == 1 and t.day == 1


def test_iso_datetime():
    t = parse_time("2024-01-01 09:00:00")
    assert t.hour == 9 and t.minute == 0


def test_slash_date():
    t = parse_time("2024/01/01")
    assert t.month == 1 and t.day == 1


def test_dot_date():
    t = parse_time("2024.06.15")
    assert t.month == 6 and t.day == 15


def test_none():
    assert parse_time("") is None
