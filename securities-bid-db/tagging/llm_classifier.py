"""可插拔 LLM 分类器接口 + Mock 实现。

真实接入时，继承 BaseLLMClassifier 并实现 classify 方法，配置 LLM API Key。
"""
from __future__ import annotations

import json
from dataclasses import dataclass, field
from typing import Any, Protocol

# 结构化 Prompt 模板
LLM_PROMPT_TEMPLATE = """你是一个证券行业招标公告分类专家。请根据公告标题与正文，判断其业务模块。

公告标题：{title}
公告正文：{content}

业务模块一级分类（请选择最匹配的一个）：
- IT基础设施与云
- 信创与国产化
- 网络安全
- 数据与大数据
- 人工智能与算法
- 行情与交易系统
- 核心业务系统
- 办公与协同
- 财务与人力
- 研究与资讯
- 营销与客服
- 风控与合规
- 运维外包与人力外包
- 机房与弱电工程
- 办公装修与物业
- 安保与后勤
- 设备与耗材
- 咨询与审计
- 其他

请严格按以下 JSON Schema 输出：
{schema}
"""

# JSON Schema
LLM_OUTPUT_SCHEMA = {
    "type": "object",
    "properties": {
        "module_id": {"type": "string", "description": "业务模块一级 ID，如 IT_INFRA"},
        "module_name": {"type": "string", "description": "业务模块一级名称"},
        "confidence": {"type": "number", "description": "置信度 0-1"},
        "evidence": {"type": "array", "items": {"type": "string"},
                     "description": "命中依据关键词列表"},
    },
    "required": ["module_id", "module_name", "confidence", "evidence"],
}


@dataclass
class LLMClassifyResult:
    module_id: str
    module_name: str
    confidence: float
    evidence: list[str] = field(default_factory=list)


class BaseLLMClassifier(Protocol):
    def classify(self, title: str, content: str) -> LLMClassifyResult: ...


# 模块 ID → 名称 映射（与 dictionary.yaml 一致）
_MODULE_MAP = {
    "IT_INFRA": "IT基础设施与云", "XINCHUANG": "信创与国产化",
    "SECURITY": "网络安全", "DATA": "数据与大数据",
    "AI": "人工智能与算法", "TRADING": "行情与交易系统",
    "CORE": "核心业务系统", "OFFICE": "办公与协同",
    "FINANCE_HR": "财务与人力", "RESEARCH": "研究与资讯",
    "MARKETING": "营销与客服", "RISK": "风控与合规",
    "OUTSOURCING": "运维外包与人力外包", "ROOM": "机房与弱电工程",
    "DECORATION": "办公装修与物业", "SECURITY_GUARD": "安保与后勤",
    "EQUIPMENT": "设备与耗材", "CONSULTING": "咨询与审计",
    "OTHER": "其他",
}


class MockLLMClassifier:
    """Mock LLM 分类器：基于简单关键词模拟 LLM 输出，用于本地测试。"""

    _KEYWORDS = {
        "IT_INFRA": ["服务器", "存储", "交换机", "云", "网络设备"],
        "SECURITY": ["安全", "等保", "防火墙", "漏洞"],
        "AI": ["人工智能", "AI", "大模型", "机器学习"],
        "DATA": ["数据", "大数据", "数据中台"],
        "TRADING": ["交易", "行情"],
        "CORE": ["核心", "集中交易", "清算"],
        "OFFICE": ["办公", "电脑", "OA"],
        "CONSULTING": ["咨询", "审计"],
    }

    def classify(self, title: str, content: str) -> LLMClassifyResult:
        text = f"{title} {content}"
        for mid, kws in self._KEYWORDS.items():
            hit = [k for k in kws if k in text]
            if hit:
                return LLMClassifyResult(
                    module_id=mid, module_name=_MODULE_MAP[mid],
                    confidence=0.75, evidence=hit,
                )
        return LLMClassifyResult(
            module_id="OTHER", module_name="其他", confidence=0.4, evidence=[],
        )


def build_prompt(title: str, content: str) -> str:
    """构建 LLM 分类 Prompt。"""
    return LLM_PROMPT_TEMPLATE.format(
        title=title, content=content[:2000],
        schema=json.dumps(LLM_OUTPUT_SCHEMA, ensure_ascii=False, indent=2),
    )
