"""金额解析单元测试。"""
from parsers.amount import parse_amount


def test_amount_wan():
    a = parse_amount("1,200万元")
    assert a.value == 12000000
    assert a.unit == "元"


def test_amount_yuan_symbol():
    a = parse_amount("¥500,000元")
    assert a.value == 500000


def test_amount_chinese():
    a = parse_amount("人民币叁佰万元整")
    assert a.value == 3000000


def test_amount_with_tax():
    a = parse_amount("300万（含税）")
    assert a.value == 3000000


def test_amount_none():
    a = parse_amount("")
    assert a.value is None
