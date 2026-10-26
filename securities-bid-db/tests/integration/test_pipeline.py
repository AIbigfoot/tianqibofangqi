"""端到端集成测试：fixture HTML → 解析 → 打标（无 DB 依赖部分）。"""
from pathlib import Path

from parsel import Selector

from parsers.amount import parse_amount
from parsers.models import NoticeItem
from parsers.text import clean_html, normalize_project_no
from parsers.time_parser import parse_time
from tagging.rules import RuleTagger

FIXTURE = Path(__file__).parent.parent / "fixtures" / "sample_notice.html"


def test_parse_fixture():
    html = FIXTURE.read_text(encoding="utf-8")
    sel = Selector(html)

    title = sel.css("h1.title::text").get("").strip()
    project_no_raw = sel.css("p:contains('项目编号')::text").get("")
    budget_raw = sel.css("p:contains('预算')::text").get("")
    content_html = sel.css("div.article-content").get("")
    content = clean_html(content_html)

    assert "服务器采购" in title
    assert normalize_project_no(project_no_raw) == "CITIC-2024-IT-001"

    budget = parse_amount(budget_raw)
    assert budget.value == 12000000

    pub_time = parse_time(sel.css("span.publish-time::text").get(""))
    assert pub_time.year == 2024 and pub_time.month == 6

    notice = NoticeItem(
        source_key="test", notice_id="test-001", title=title,
        project_no=normalize_project_no(project_no_raw),
        budget=budget, content=content,
        publish_time=pub_time,
    )
    assert notice.project_no == "CITIC-2024-IT-001"

    tagger = RuleTagger()
    result = tagger.tag(text=content, budget_value=float(budget.value),
                        procurement_unit="XX证券", notice_title=title)
    assert result.module_l1 == "IT基础设施与云"
    assert result.amount_tier == "1000-3000万"


def test_parse_deadline():
    html = FIXTURE.read_text(encoding="utf-8")
    sel = Selector(html)
    deadline_raw = sel.css("p:contains('投标截止')::text").get("")
    t = parse_time(deadline_raw)
    assert t.month == 7 and t.day == 1
