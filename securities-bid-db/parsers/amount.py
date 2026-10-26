"""金额解析器：处理万元/元/¥/人民币/含税/中文大写，统一输出元数值。"""
from __future__ import annotations

import re
from decimal import Decimal, InvalidOperation

from .models import Amount

# 中文数字映射
_CN_NUM = {
    "零": 0, "〇": 0, "一": 1, "壹": 1, "二": 2, "贰": 2, "两": 2,
    "三": 3, "叁": 3, "四": 4, "肆": 4, "五": 5, "伍": 5,
    "六": 6, "陆": 6, "七": 7, "柒": 7, "八": 8, "捌": 8,
    "九": 9, "玖": 9,
}
_CN_UNIT = {"十": 10, "拾": 10, "百": 100, "佰": 100, "千": 1000, "仟": 1000}
_CN_BIG = {"万": 10000, "萬": 10000, "亿": 100000000, "億": 100000000}

_AMOUNT_PATTERNS = [
    # 阿拉伯数字 + 单位
    re.compile(r"([\d,]+(?:\.\d+)?)\s*(万元|万元人民币|万)"),
    re.compile(r"([\d,]+(?:\.\d+)?)\s*(元|元人民币|圆)"),
    # ¥ / 人民币 符号
    re.compile(r"[¥￥]\s*([\d,]+(?:\.\d+)?)"),
    re.compile(r"人民币\s*([\d,]+(?:\.\d+)?)\s*(万元|万|元)?"),
]


def _cn_to_int(s: str) -> int | None:
    """将中文数字字符串转为整数（支持万/亿）。"""
    if not s:
        return None
    total = 0
    section = 0
    current = 0
    for ch in s:
        if ch in _CN_NUM:
            current = _CN_NUM[ch]
        elif ch in _CN_UNIT:
            section += (current if current else 1) * _CN_UNIT[ch]
            current = 0
        elif ch in _CN_BIG:
            section += current
            total += (section if section else 1) * _CN_BIG[ch]
            section = 0
            current = 0
        elif ch == "点":
            break
    return total + section + current


def parse_amount(text: str) -> Amount:
    """从文本中解析金额，返回 Amount（统一单位为元）。"""
    if not text:
        return Amount()
    original = text.strip()
    value: Decimal | None = None
    unit = "元"

    # 1. 先尝试中文大写金额（壹佰万元整 / 叁佰万元）
    cn_match = re.search(
        r"([零〇一二两三四五六七八九十百千万亿壹贰叁肆伍陆柒捌玖拾佰仟萬億]+)\s*(万|元|圆)?",
        text,
    )
    # 仅当包含中文数字字符（非纯单位）时才走中文解析，避免「万」被误判为 1 万
    if cn_match and any(c in _CN_NUM for c in cn_match.group(1)):
        cn_num_str = cn_match.group(1)
        cn_unit = cn_match.group(2)
        num = _cn_to_int(cn_num_str)
        if num is not None:
            value = Decimal(num)
            if cn_unit == "万":
                value *= 10000
                unit = "元"
            elif cn_unit in ("元", "圆") or cn_unit is None:
                unit = "元"

    # 2. 阿拉伯数字模式
    if value is None:
        for pat in _AMOUNT_PATTERNS:
            m = pat.search(text)
            if m:
                num_str = m.group(1).replace(",", "")
                try:
                    num = Decimal(num_str)
                except InvalidOperation:
                    continue
                unit_word = m.group(2) if m.lastindex and m.lastindex >= 2 else None
                if unit_word in ("万元", "万元人民币", "万"):
                    value = num * 10000
                else:
                    value = num
                unit = "元"
                break

    # 3. 纯数字（兜底，视为元）
    if value is None:
        pure = re.search(r"[\d,]+(?:\.\d+)?", text)
        if pure:
            try:
                value = Decimal(pure.group().replace(",", ""))
                unit = "元"
            except InvalidOperation:
                pass

    return Amount(original=original, value=value, unit=unit, currency="CNY")
