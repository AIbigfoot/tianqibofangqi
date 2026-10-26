"""基于关键词词典与正则的规则打标器。"""
from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import yaml

DICT_PATH = Path(__file__).resolve().parent / "dictionary.yaml"


@dataclass
class TagResult:
    module_l1: str | None        # 一级业务模块名
    module_l2: str | None        # 二级业务模块名
    module_l1_id: str | None
    module_l2_id: str | None
    amount_tier: str | None      # 金额分档名称
    amount_tier_id: str | None
    procurement_method: str | None
    notice_type: str | None
    org_form: str | None
    project_attribute: str | None
    supplier_type: str | None
    region: str | None           # 地域（省/市）
    matched_keywords: list[str]
    confidence: float            # 规则置信度


class RuleTagger:
    """规则打标器：关键词词典匹配 + 金额分档 + 枚举识别。"""

    def __init__(self, dict_path: Path | str = DICT_PATH) -> None:
        with open(dict_path, "r", encoding="utf-8") as f:
            self.dict: dict[str, Any] = yaml.safe_load(f)
        self._modules = self.dict["business_modules"]
        self._amount_tiers = self.dict["amount_tiers"]

    def tag(self, text: str, budget_value: float | None = None,
            procurement_unit: str = "", notice_title: str = "") -> TagResult:
        full_text = f"{notice_title} {text} {procurement_unit}"
        matched_kw: list[str] = []

        # 1. 业务模块匹配：收集所有命中，取最长关键词命中（最具体）
        mod_l1 = mod_l2 = mod_l1_id = mod_l2_id = None
        best = None  # (kw_len, mod_l1, mod_l1_id, mod_l2, mod_l2_id, kw)
        for m in self._modules:
            for child in m.get("children", []):
                for kw in child.get("keywords", []):
                    if kw and kw in full_text:
                        if best is None or len(kw) > best[0]:
                            best = (len(kw), m["name"], m["id"], child["name"], child["id"], kw)
        if best:
            _, mod_l1, mod_l1_id, mod_l2, mod_l2_id, kw = best
            matched_kw.append(kw)

        # 2. 金额分档
        tier = tier_id = None
        if budget_value is not None:
            for t in self._amount_tiers:
                if t["id"] == "TIER_UNKNOWN":
                    continue
                lo, hi = t["min"], t["max"]
                if lo is not None and budget_value >= lo and (hi is None or budget_value < hi):
                    tier, tier_id = t["name"], t["id"]
                    break
        if tier is None:
            tier, tier_id = "未披露", "TIER_UNKNOWN"

        # 3. 采购方式
        pm = self._match_enum(full_text, self.dict["procurement_methods"])

        # 4. 公告类型
        nt = self._match_notice_type(notice_title or full_text)

        # 5. 组织形式
        org = "委托代理" if re.search(r"代理|招标代理", full_text) else (
            "自行招标" if re.search(r"自行|自主", full_text) else None)

        # 6. 项目属性
        attr = None
        for a in self.dict["project_attributes"]:
            if a in full_text:
                attr = a
                break

        # 7. 供应商类型倾向
        sup = None
        if re.search(r"国产化|信创|国产", full_text):
            sup = "国产化要求"
        elif re.search(r"原厂|授权|原厂商", full_text):
            sup = "原厂授权"

        # 8. 地域（简单省/市提取）
        region = self._extract_region(full_text)

        confidence = 0.9 if mod_l1 else 0.3
        return TagResult(
            module_l1=mod_l1, module_l2=mod_l2,
            module_l1_id=mod_l1_id, module_l2_id=mod_l2_id,
            amount_tier=tier, amount_tier_id=tier_id,
            procurement_method=pm, notice_type=nt, org_form=org,
            project_attribute=attr, supplier_type=sup, region=region,
            matched_keywords=matched_kw, confidence=confidence,
        )

    @staticmethod
    def _match_enum(text: str, enums: list[str]) -> str | None:
        for e in enums:
            if e == "其他":
                continue
            if e in text:
                return e
        return None

    @staticmethod
    def _match_notice_type(text: str) -> str | None:
        if re.search(r"中标候选人", text):
            return "中标候选人"
        if re.search(r"中标结果|中标公告|成交公告", text):
            return "中标结果"
        if re.search(r"废标|流标", text):
            return "废标流标"
        if re.search(r"变更|更正|澄清|延期", text):
            return "变更公告"
        if re.search(r"供应商征集|征集", text):
            return "供应商征集"
        if re.search(r"资格预审", text):
            return "资格预审"
        if re.search(r"招标|采购公告|询价公告|磋商公告|谈判公告", text):
            return "招标公告"
        return None

    @staticmethod
    def _extract_region(text: str) -> str | None:
        provinces = ["北京", "上海", "广东", "深圳", "江苏", "浙江", "山东", "四川",
                     "湖北", "湖南", "福建", "河南", "河北", "安徽", "陕西", "辽宁",
                     "重庆", "天津", "吉林", "黑龙江", "江西", "广西", "云南", "贵州",
                     "山西", "甘肃", "内蒙古", "新疆", "海南", "宁夏", "青海", "西藏"]
        for p in provinces:
            if p in text:
                return p
        return None
