"""批量生成数据源 YAML 配置（≥20 券商 + 2 国家级平台）。"""
import yaml
from pathlib import Path

SOURCES_DIR = Path(__file__).resolve().parent

BROKERS = [
    {"key": "citic", "name": "中信证券", "domain": "www.cs.ecitic.com", "path": "/about/bidding/"},
    {"key": "gtja", "name": "国泰君安", "domain": "www.gtja.com", "path": "/aboutus/bidding/"},
    {"key": "haitong", "name": "海通证券", "domain": "www.htsec.com", "path": "/col/col8140/index.html"},
    {"key": "htsc", "name": "华泰证券", "domain": "www.htsc.com.cn", "path": "/aboutUs/bidding/"},
    {"key": "cicc", "name": "中金公司", "domain": "www.cicc.com", "path": "/about/bidding/"},
    {"key": "cmschina", "name": "招商证券", "domain": "www.cmschina.com", "path": "/about/bidding/"},
    {"key": "gf", "name": "广发证券", "domain": "www.gf.com.cn", "path": "/about/bidding/"},
    {"key": "csc", "name": "中信建投", "domain": "www.csc108.com", "path": "/about/bidding/"},
    {"key": "swhysc", "name": "申万宏源", "domain": "www.swhysc.com", "path": "/about/bidding/"},
    {"key": "guosen", "name": "国信证券", "domain": "www.guosen.com.cn", "path": "/about/bidding/"},
    {"key": "orient", "name": "东方证券", "domain": "www.dfzq.com.cn", "path": "/about/bidding/"},
    {"key": "ebscn", "name": "光大证券", "domain": "www.ebscn.com", "path": "/about/bidding/"},
    {"key": "pingan", "name": "平安证券", "domain": "www.pingan.com", "path": "/about/bidding/"},
    {"key": "xyzq", "name": "兴业证券", "domain": "www.xyzq.com.cn", "path": "/about/bidding/"},
    {"key": "cjsc", "name": "长江证券", "domain": "www.95579.com", "path": "/about/bidding/"},
    {"key": "essence", "name": "安信证券", "domain": "www.essence.com.cn", "path": "/about/bidding/"},
    {"key": "dwzq", "name": "东吴证券", "domain": "www.dwzq.com.cn", "path": "/about/bidding/"},
    {"key": "foundersc", "name": "方正证券", "domain": "www.foundersc.com", "path": "/about/bidding/"},
    {"key": "gjzq", "name": "国金证券", "domain": "www.gjzq.com.cn", "path": "/about/bidding/"},
    {"key": "swsc", "name": "西南证券", "domain": "www.swsc.com.cn", "path": "/about/bidding/"},
    {"key": "chinastock", "name": "中国银河", "domain": "www.chinastock.com.cn", "path": "/about/bidding/"},
    {"key": "ctsec", "name": "财通证券", "domain": "www.ctsec.com", "path": "/about/bidding/"},
]

PLATFORMS = [
    {
        "key": "cebpubservice",
        "name": "中国招标投标公共服务平台",
        "domain": "www.cebpubservice.com",
        "path": "/cebpubservice/secondPageNew/index.jhtml",
        "type": "static",
    },
    {
        "key": "ggzy",
        "name": "全国公共资源交易平台",
        "domain": "www.ggzy.gov.cn",
        "path": "/informationList",
        "type": "dynamic",
    },
]


def make_source(key: str, name: str, domain: str, path: str, stype: str = "static") -> dict:
    return {
        "name": name,
        "key": key,
        "type": stype,
        "entry_url": f"https://{domain}{path}",
        "domain": domain,
        "list_pagination": {
            "rule": "query_param",
            "page_param": "pageNo",
            "start_page": 1,
            "max_pages": 50,
            "page_size": 20,
        },
        "list_selectors": {
            "item": "ul.article-list li, div.list-item",
            "title": "a::text",
            "link": "a::attr(href)",
            "publish_time": "span.date::text, time::text",
        },
        "detail_selectors": {
            "title": "h1.title::text, div.article-title::text",
            "project_no": "p:contains('项目编号')::text, td:contains('项目编号') + td::text",
            "publish_time": "span.publish-time::text, div.info time::text",
            "budget": "p:contains('预算')::text, td:contains('预算') + td::text",
            "procurement_unit": "p:contains('采购人')::text, td:contains('采购人') + td::text",
            "agency": "p:contains('代理机构')::text, td:contains('代理') + td::text",
            "content": "div.article-content, div.content",
            "attachments": "div.article-content a::attr(href)",
            "contact": "p:contains('联系人')::text",
            "phone": "p:contains('电话')::text",
        },
        "anti_crawl": {
            "rate_limit_seconds": 3,
            "ua_rotation": True,
            "strategy": "requests_headers",
            "notes": "静态 HTML，无明显反爬，控制频率即可",
        },
        "compliance": {
            "robots": "遵守 robots.txt，仅抓取公开公告栏目",
            "notes": "仅采集无需登录的公开招标/采购公告，不采集个人信息",
        },
    }


def main():
    count = 0
    for b in BROKERS:
        src = make_source(b["key"], b["name"], b["domain"], b["path"], stype="dynamic")
        # 券商官网多为动态渲染
        out = SOURCES_DIR / f"{b['key']}.yml"
        with open(out, "w", encoding="utf-8") as f:
            yaml.dump(src, f, allow_unicode=True, sort_keys=False)
        count += 1
    for p in PLATFORMS:
        src = make_source(p["key"], p["name"], p["domain"], p["path"], stype=p["type"])
        out = SOURCES_DIR / f"{p['key']}.yml"
        with open(out, "w", encoding="utf-8") as f:
            yaml.dump(src, f, allow_unicode=True, sort_keys=False)
        count += 1
    print(f"Generated {count} source configs in {SOURCES_DIR}")


if __name__ == "__main__":
    main()
