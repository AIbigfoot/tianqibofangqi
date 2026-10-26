"""打标分类器：规则优先，LLM 兜底，写入 notice_tag。"""
from __future__ import annotations

import logging
from decimal import Decimal

from sqlalchemy import select
from sqlalchemy.orm import Session

from config import get_settings
from db.models import Notice, NoticeTag, Tag
from tagging.rules import RuleTagger, TagResult

logger = logging.getLogger(__name__)
_settings = get_settings()


def _get_or_create_tag(session: Session, dimension: str, tag_key: str,
                       name: str, level: int = 1, parent_id: int | None = None) -> Tag:
    stmt = select(Tag).where(Tag.dimension == dimension, Tag.tag_key == tag_key)
    tag = session.execute(stmt).scalar_one_or_none()
    if tag:
        return tag
    tag = Tag(dimension=dimension, tag_key=tag_key, name=name, level=level, parent_id=parent_id)
    session.add(tag)
    session.flush()
    return tag


def _add_notice_tag(session: Session, notice_id: int, tag: Tag,
                    source: str = "rule", confidence: float = 1.0,
                    keywords: list[str] | None = None) -> None:
    stmt = select(NoticeTag).where(
        NoticeTag.notice_id == notice_id, NoticeTag.tag_id == tag.id
    )
    if session.execute(stmt).scalar_one_or_none():
        return
    session.add(NoticeTag(
        notice_id=notice_id, tag_id=tag.id, source=source,
        confidence=confidence, matched_keywords=keywords or [],
    ))


class TaggingPipeline:
    """对公告执行规则打标，规则未命中时可选 LLM 兜底。"""

    def __init__(self) -> None:
        self.rules = RuleTagger()
        self._llm = None  # 延迟加载

    @property
    def llm(self):
        if self._llm is None:
            from tagging.llm_classifier import MockLLMClassifier
            self._llm = MockLLMClassifier()
        return self._llm

    def tag_notice(self, session: Session, notice: Notice) -> bool:
        """对单条公告打标，返回是否需要人工复核。"""
        budget_val = float(notice.budget_value) if notice.budget_value else None
        result: TagResult = self.rules.tag(
            text=notice.content or "",
            budget_value=budget_val,
            procurement_unit=notice.procurement_unit or "",
            notice_title=notice.title or "",
        )

        review_required = False

        # 业务模块
        if result.module_l1_id:
            l1_tag = _get_or_create_tag(
                session, "module", result.module_l1_id, result.module_l1, level=1)
            _add_notice_tag(session, notice.id, l1_tag, "rule",
                            result.confidence, result.matched_keywords)
            if result.module_l2_id:
                l2_tag = _get_or_create_tag(
                    session, "module", result.module_l2_id, result.module_l2,
                    level=2, parent_id=l1_tag.id)
                _add_notice_tag(session, notice.id, l2_tag, "rule", result.confidence)
        else:
            # 规则未命中 → LLM 兜底
            if _settings.tagging.enable_llm:
                llm_result = self.llm.classify(notice.title or "", notice.content or "")
                if llm_result.confidence >= _settings.tagging.llm_confidence_threshold:
                    l1_tag = _get_or_create_tag(
                        session, "module", llm_result.module_id, llm_result.module_name)
                    _add_notice_tag(session, notice.id, l1_tag, "llm",
                                    llm_result.confidence, llm_result.evidence)
                else:
                    review_required = True
            else:
                review_required = True

        # 金额分档
        if result.amount_tier_id:
            t_tag = _get_or_create_tag(session, "amount_tier", result.amount_tier_id, result.amount_tier)
            _add_notice_tag(session, notice.id, t_tag, "rule")

        # 采购方式
        if result.procurement_method:
            pm_tag = _get_or_create_tag(session, "procurement_method",
                                        result.procurement_method, result.procurement_method)
            _add_notice_tag(session, notice.id, pm_tag, "rule")

        # 公告类型
        if result.notice_type:
            nt_tag = _get_or_create_tag(session, "notice_type", result.notice_type, result.notice_type)
            _add_notice_tag(session, notice.id, nt_tag, "rule")

        # 组织形式
        if result.org_form:
            org_tag = _get_or_create_tag(session, "org_form", result.org_form, result.org_form)
            _add_notice_tag(session, notice.id, org_tag, "rule")

        # 项目属性
        if result.project_attribute:
            attr_tag = _get_or_create_tag(session, "project_attribute",
                                          result.project_attribute, result.project_attribute)
            _add_notice_tag(session, notice.id, attr_tag, "rule")

        # 供应商类型
        if result.supplier_type:
            sup_tag = _get_or_create_tag(session, "supplier_type", result.supplier_type, result.supplier_type)
            _add_notice_tag(session, notice.id, sup_tag, "rule")

        # 地域
        if result.region:
            reg_tag = _get_or_create_tag(session, "region", result.region, result.region)
            _add_notice_tag(session, notice.id, reg_tag, "rule")

        notice.review_required = review_required
        return review_required
