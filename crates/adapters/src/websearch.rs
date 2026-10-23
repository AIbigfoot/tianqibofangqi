//! 联网检索影片资料：DeepSeek 等 OpenAI 兼容接口本身不能浏览网页，
//! 这里由本机先取搜索结果页，抽取标题与摘要文本，再交给大模型综合。
//!
//! 只用搜索结果的文字摘要（不逐页抓正文）：摘要已覆盖口碑、题材与卖点，
//! 且请求量小、速度快，不会因为站点改版或反爬让整条推荐链路失败。

use std::io::Read;
use std::time::Duration;

use scraper::{Html, Selector};

use cinema_domain::error::{PortError, Result};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/122.0.0.0 Safari/537.36";
/// 搜索结果页上限：正常约 100KB，超出视为异常响应。
const MAX_HTML: u64 = 1024 * 1024;
/// 最多取几条结果：10 条左右会让提示词过长，6 条足够覆盖口碑与简介。
const MAX_SNIPPETS: usize = 6;
/// 单条摘要长度上限（字符）。
const MAX_SNIPPET_CHARS: usize = 220;

/// 按关键词联网检索，返回「标题：摘要」形式的资料片段；检索失败回落到空列表。
///
/// `require_code` 用于过滤：搜索结果必须出现这个番号（忽略大小写与连接符）才保留。
/// Bing 对这类影片站点常返回相邻番号或同名缩写，不过滤会把无关内容喂给模型。
pub fn search_snippets(query: &str, require_code: Option<&str>) -> Result<Vec<String>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let url = format!(
        "https://cn.bing.com/search?q={}&setlang=zh-CN",
        encode_query(query)
    );
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(USER_AGENT)
        .build();
    let resp = agent
        .get(&url)
        .set("Accept-Language", "zh-CN,zh;q=0.9")
        .call()
        .map_err(|e| PortError::Backend(format!("联网检索失败：{e}")))?;
    let mut html = String::new();
    resp.into_reader()
        .take(MAX_HTML)
        .read_to_string(&mut html)
        .map_err(|e| PortError::Backend(format!("读取检索结果失败：{e}")))?;
    let mut snippets = parse_results(&html);
    if let Some(code) = require_code.map(normalize_code).filter(|code| !code.is_empty()) {
        snippets.retain(|snippet| normalize_code(snippet).contains(&code));
    }
    Ok(snippets)
}

/// 番号归一化：只保留 ASCII 字母数字并转小写，`PFES-081` 与 `pfes 081` 视为同一个。
fn normalize_code(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 解析 Bing 结果列表：每条取标题 + 摘要段落，去重后最多 6 条。
fn parse_results(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let Ok(item) = Selector::parse("li.b_algo") else {
        return Vec::new();
    };
    let Ok(title) = Selector::parse("h2") else {
        return Vec::new();
    };
    let Ok(paragraph) = Selector::parse("p") else {
        return Vec::new();
    };

    let mut out: Vec<String> = Vec::new();
    for block in doc.select(&item) {
        let mut parts: Vec<String> = Vec::new();
        if let Some(heading) = block.select(&title).next() {
            let text = clean_text(&heading.text().collect::<String>());
            if !text.is_empty() {
                parts.push(text);
            }
        }
        for para in block.select(&paragraph) {
            let text = clean_text(&para.text().collect::<String>());
            // 过短的段落多是导航或日期，不构成资料
            if text.chars().count() >= 12 {
                parts.push(text);
            }
        }
        if parts.is_empty() {
            continue;
        }
        let snippet = truncate_chars(&parts.join("："), MAX_SNIPPET_CHARS);
        if !out.iter().any(|existing| existing == &snippet) {
            out.push(snippet);
        }
        if out.len() >= MAX_SNIPPETS {
            break;
        }
    }
    out
}

fn clean_text(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// 查询串百分号编码：只保留 ASCII 字母数字与常见安全符号，其余按 UTF-8 字节编码。
fn encode_query(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
