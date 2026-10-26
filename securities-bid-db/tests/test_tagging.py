"""规则打标单元测试。"""
from tagging.rules import RuleTagger

tagger = RuleTagger()


def test_module_server():
    r = tagger.tag("本项目为服务器采购项目，采购内容包括机架式服务器", budget_value=1500000)
    assert r.module_l1 == "IT基础设施与云"
    assert r.module_l2 == "服务器"


def test_module_security():
    r = tagger.tag("网络安全等级保护测评服务项目", budget_value=500000)
    assert r.module_l1 == "网络安全"


def test_module_core():
    r = tagger.tag("核心交易系统升级改造项目", budget_value=5000000)
    assert r.module_l1 == "核心业务系统"


def test_module_office():
    r = tagger.tag("办公电脑采购项目", budget_value=300000)
    assert r.module_l1 == "办公与协同"


def test_amount_tier():
    r = tagger.tag("服务器", budget_value=500000)
    assert r.amount_tier == "50-100万"
    r = tagger.tag("服务器", budget_value=1500000)
    assert r.amount_tier == "100-300万"
    r = tagger.tag("服务器", budget_value=8000000)
    assert r.amount_tier == "500-1000万"
    r = tagger.tag("服务器", budget_value=50000000)
    assert r.amount_tier == "3000万-1亿"
    r = tagger.tag("服务器", budget_value=200000000)
    assert r.amount_tier == ">1亿"


def test_amount_unknown():
    r = tagger.tag("服务器", budget_value=None)
    assert r.amount_tier == "未披露"
