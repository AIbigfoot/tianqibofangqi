//! 外部元数据补全：JavLibrary 与 hanime.tv。
//!
//! 两个源都只做「补充」：调用方拿到 [`Enrichment`] 后只填库内仍为空的字段，
//! 已有值不覆盖。
//!
//! - **JavLibrary**：按番号抓取详情页（需要条目已有番号）。站点由 Cloudflare
//!   保护，直连被拦截时会返回明确错误。
//! - **hanime.tv**：站点提供一个免鉴权的完整作品目录 JSON，本地按标题（含
//!   日文 / 中文 / 英文别名）匹配，因此不需要搜索接口，也不会被限流。

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use cinema_domain::error::{PortError, Result};
use scraper::{Html, Selector};
use serde_json::Value;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/122.0.0.0 Safari/537.36";
const TIMEOUT_SECS: u64 = 20;
/// 单次响应上限（hanime 目录约 4.5MB）。
const MAX_BODY: u64 = 16 * 1024 * 1024;
/// hanime 目录缓存有效期。
const CATALOG_TTL: Duration = Duration::from_secs(30 * 60);
/// 目录接口：返回全部作品，查询参数会被忽略。
const HANIME_CATALOG_URL: &str = "https://guest.freeanimehentai.net/api/v11/search_hvs";
/// DMM 剧照之类的外部图片最多取 3 张。
pub const MAX_SCREENSHOTS: usize = 3;

/// 外部源补全结果。所有字段都是「可选补充」，由调用方决定填哪些。
#[derive(Debug, Clone, Default)]
pub struct Enrichment {
    pub title: Option<String>,
    pub title_cn: Option<String>,
    pub overview: Option<String>,
    pub release_date: Option<String>,
    pub duration_ms: Option<i64>,
    pub rating: Option<f32>,
    pub cover_url: Option<String>,
    /// 剧照 / 样品图地址（由调用方下载后落盘）。
    pub screenshots: Vec<String>,
    pub categories: Vec<String>,
    pub series: Vec<String>,
    pub maker: Vec<String>,
    pub director: Vec<String>,
    pub actors: Vec<String>,
    pub detail_url: Option<String>,
    pub external_code: Option<String>,
}

/// 番号规范化：只保留字母数字并大写。
///
/// 站点之间写法差异很大（`SSIS-001` / `ssis001` / `h_1234abcd`），统一成
/// 紧凑大写串后再比较，避免因为连字符或下划线而匹配失败。
pub fn normalize_code(raw: &str) -> Option<String> {
    let code: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_uppercase())
        .collect();
    (!code.is_empty()).then_some(code)
}

/// 标题清洗：去掉站点后缀、字幕/画质标记与结尾序号，用于搜索外部作品库。
///
/// 例：`某作品 [中文字幕] - Hanime1.me` → `某作品`；
/// `某作品 (1)` → `某作品`。只做保守清洗，保留作品名本身。
pub fn clean_title(raw: &str) -> String {
    let mut text = raw.trim().to_owned();

    // 站点常见的 ` - 站点名` / ` | 站点名` 后缀
    for sep in [" - ", " | ", " – ", " — "] {
        if let Some(at) = text.rfind(sep) {
            let tail = text[at + sep.len()..].trim();
            if tail.chars().count() <= 24
                && ["hanime", "javdb", "javlibrary", "dmm", "fanza", "missav"]
                    .iter()
                    .any(|name| tail.to_lowercase().contains(name))
            {
                text.truncate(at);
            }
        }
    }

    // 方括号 / 圆角括号里的标记：字幕、无码、画质、集数
    for (open, close) in [('[', ']'), ('【', '】'), ('(', ')'), ('（', '）')] {
        let mut out = String::with_capacity(text.len());
        let mut depth = 0usize;
        for ch in text.chars() {
            if ch == open {
                depth += 1;
                continue;
            }
            if ch == close && depth > 0 {
                depth -= 1;
                continue;
            }
            if depth == 0 {
                out.push(ch);
            }
        }
        text = out;
    }

    // 结尾的序号与分隔符
    let mut cleaned = text.trim().to_owned();
    loop {
        let trimmed = cleaned
            .trim_end_matches(|ch: char| {
                ch.is_whitespace() || ch == '-' || ch == '_' || ch == '·' || ch == '.'
            })
            .to_owned();
        if trimmed == cleaned {
            break;
        }
        cleaned = trimmed;
    }

    // 结尾单独的 OVA / OAD / SP 标记
    for suffix in ["OVA", "OAD", "SP"] {
        if let Some(head) = cleaned.strip_suffix(suffix) {
            let head = head.trim_end_matches(|ch: char| ch == '-' || ch == '_' || ch == ' ');
            if !head.is_empty() {
                cleaned = head.to_owned();
            }
        }
    }

    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// JavLibrary：按番号抓取详情页。无需登录，但站点位于 Cloudflare 之后。
///
/// 搜索页命中唯一结果时会直接返回详情页；否则在结果列表里挑番号一致的一条。
pub fn from_javlibrary(code: &str) -> Result<Option<Enrichment>> {
    let code = code.trim();
    if code.is_empty() {
        return Ok(None);
    }
    let url = format!(
        "https://www.javlibrary.com/en/vl_searchbyid.php?keyword={}",
        encode_component(code)
    );
    let (final_url, html) = get_html(&url)?;
    if is_cloudflare_challenge(&html) {
        return Err(PortError::Backend(
            "JavLibrary 被 Cloudflare 拦截：当前网络需能直连该站（或换用可访问的网络）".to_owned(),
        ));
    }

    let doc = Html::parse_document(&html);
    if has_selector(&doc, "#video_title") {
        return Ok(parse_javlibrary_detail(&doc, code, &final_url));
    }

    // 结果列表：按番号找对应条目，再取详情
    let Some(href) = javlibrary_result_link(&doc, code)? else {
        return Ok(None);
    };
    let detail_url = absolute_url("https://www.javlibrary.com/en/", &href);
    let (final_url, html) = get_html(&detail_url)?;
    if is_cloudflare_challenge(&html) {
        return Err(PortError::Backend(
            "JavLibrary 被 Cloudflare 拦截：当前网络需能直连该站".to_owned(),
        ));
    }
    let doc = Html::parse_document(&html);
    Ok(parse_javlibrary_detail(&doc, code, &final_url))
}

fn parse_javlibrary_detail(doc: &Html, code: &str, detail_url: &str) -> Option<Enrichment> {
    let title = first_text(doc, "#video_title h3 a").or_else(|| first_text(doc, "#video_title"));
    let code_text = first_text(doc, "#video_id .text")
        .or_else(|| Some(code.trim().to_owned()))
        .filter(|value| !value.is_empty());
    let date = first_text(doc, "#video_date .text");
    let duration_ms = first_text(doc, "#video_length .text")
        .map(|text| parse_minutes_ms(&text))
        .filter(|value| *value > 0);
    let mut maker: Vec<String> = Vec::new();
    if let Some(name) = first_text(doc, "#video_maker .text a") {
        maker.push(name);
    }
    if let Some(name) = first_text(doc, "#video_label .text a") {
        if !maker.iter().any(|existing| existing == &name) {
            maker.push(name);
        }
    }
    let director: Vec<String> = all_texts(doc, "#video_director .text a");
    let categories: Vec<String> = all_texts(doc, "#video_genres a")
        .into_iter()
        .filter(|name| !name.eq_ignore_ascii_case("HD"))
        .collect();
    let actors: Vec<String> = all_texts(doc, "#video_cast a");
    let rating = first_text(doc, "#video_review .score")
        .and_then(|text| parse_rating(&text))
        .map(|value| if value <= 5.0 { value * 2.0 } else { value });
    let cover_url = attribute(doc, "#video_jacket_img", "src").map(|src| {
        if src.starts_with("//") {
            format!("https:{src}")
        } else {
            src
        }
    });

    Some(Enrichment {
        title,
        title_cn: None,
        overview: None,
        release_date: date,
        duration_ms,
        rating,
        cover_url,
        screenshots: Vec::new(),
        categories: dedup(categories),
        series: Vec::new(),
        maker: dedup(maker),
        director: dedup(director),
        actors: dedup(actors),
        detail_url: Some(detail_url.to_owned()),
        external_code: code_text,
    })
}

fn javlibrary_result_link(doc: &Html, code: &str) -> Result<Option<String>> {
    let wanted = normalize_code(code);
    let video_sel = Selector::parse("div.video")
        .map_err(|err| PortError::Backend(format!("JavLibrary 页面解析失败：{err}")))?;
    let id_sel = Selector::parse(".id")
        .map_err(|err| PortError::Backend(format!("JavLibrary 页面解析失败：{err}")))?;
    let link_sel = Selector::parse("a")
        .map_err(|err| PortError::Backend(format!("JavLibrary 页面解析失败：{err}")))?;

    for video in doc.select(&video_sel) {
        let id_text = video
            .select(&id_sel)
            .next()
            .map(|element| element.text().collect::<String>())
            .unwrap_or_default();
        if normalize_code(&id_text) != wanted {
            continue;
        }
        if let Some(href) = video
            .select(&link_sel)
            .next()
            .and_then(|link| link.value().attr("href"))
        {
            return Ok(Some(href.to_owned()));
        }
    }
    Ok(None)
}

fn has_selector(doc: &Html, css: &str) -> bool {
    Selector::parse(css)
        .map(|selector| doc.select(&selector).next().is_some())
        .unwrap_or(false)
}

fn first_text(doc: &Html, css: &str) -> Option<String> {
    let selector = Selector::parse(css).ok()?;
    doc.select(&selector)
        .next()
        .map(|element| element.text().collect::<String>().trim().to_owned())
        .filter(|text| !text.is_empty())
}

fn all_texts(doc: &Html, css: &str) -> Vec<String> {
    let Ok(selector) = Selector::parse(css) else {
        return Vec::new();
    };
    doc.select(&selector)
        .filter_map(|element| {
            let text = element.text().collect::<String>().trim().to_owned();
            (!text.is_empty()).then_some(text)
        })
        .collect()
}

fn attribute(doc: &Html, css: &str, name: &str) -> Option<String> {
    let selector = Selector::parse(css).ok()?;
    doc.select(&selector)
        .next()
        .and_then(|element| element.value().attr(name))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_minutes_ms(text: &str) -> i64 {
    let digits: String = text.chars().filter(char::is_ascii_digit).collect();
    digits
        .parse::<i64>()
        .ok()
        .map(|minutes| minutes * 60_000)
        .unwrap_or(0)
}

fn parse_rating(text: &str) -> Option<f32> {
    let number: String = text
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    number.parse::<f32>().ok().filter(|value| *value > 0.0)
}

fn is_cloudflare_challenge(html: &str) -> bool {
    html.contains("Just a moment")
        || html.contains("cf-chl")
        || html.contains("challenge-platform")
}

fn absolute_url(base_dir: &str, href: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        href.to_owned()
    } else if let Some(rest) = href.strip_prefix("//") {
        format!("https://{rest}")
    } else if href.starts_with('/') {
        format!("https://www.javlibrary.com{href}")
    } else {
        format!("{base_dir}{href}")
    }
}

/// hanime.tv：从整库目录里按标题（含多语言别名）匹配作品。
pub fn from_hanime(title: &str) -> Result<Option<Enrichment>> {
    let keyword = clean_title(title);
    if keyword.is_empty() {
        return Ok(None);
    }
    let wanted = normalize_title(&keyword);
    if wanted.chars().count() < 4 {
        return Ok(None);
    }

    let catalog = hanime_catalog()?;
    let mut best: Option<(i32, &Value)> = None;
    for item in &catalog {
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let search_titles = item
            .get("search_titles")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut score = string_similarity(&clean_title(name), &keyword);
        if normalize_title(search_titles).contains(&wanted) {
            score = score.max(90);
        }
        if score < 80 {
            continue;
        }
        if best.map(|(current, _)| score > current).unwrap_or(true) {
            best = Some((score, item));
        }
    }
    let Some((_, item)) = best else {
        return Ok(None);
    };

    let slug = item.get("slug").and_then(Value::as_str).unwrap_or_default();
    let overview = item
        .get("description")
        .and_then(Value::as_str)
        .map(html_to_text)
        .filter(|text| !text.is_empty());
    let release_date = item
        .get("released_at")
        .and_then(Value::as_str)
        .map(|value| value.get(0..10).unwrap_or(value).to_owned());
    let cover_url = item
        .get("cover_url")
        .and_then(Value::as_str)
        .or_else(|| item.get("poster_url").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let categories = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .take(12)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let maker = item
        .get("brand")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .into_iter()
        .collect();

    Ok(Some(Enrichment {
        title: item
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        title_cn: None,
        overview,
        release_date,
        duration_ms: None,
        rating: None,
        cover_url,
        screenshots: Vec::new(),
        categories: dedup(categories),
        series: Vec::new(),
        maker,
        director: Vec::new(),
        actors: Vec::new(),
        detail_url: (!slug.is_empty()).then(|| format!("https://hanime.tv/videos/hentai/{slug}")),
        external_code: None,
    }))
}

/// 拉取整库目录；进程内缓存 30 分钟，避免每次补全都下载 4MB。
fn hanime_catalog() -> Result<Vec<Value>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<Value>)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(guard) = cache.lock() {
        if let Some((fetched_at, items)) = guard.as_ref() {
            if fetched_at.elapsed() < CATALOG_TTL {
                return Ok(items.clone());
            }
        }
    }

    let body = get_text(HANIME_CATALOG_URL)?;
    let json: Value = serde_json::from_str(&body)
        .map_err(|err| PortError::Backend(format!("hanime.tv 目录响应无法解析：{err}")))?;
    let items = json
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(items);
    }
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), items.clone()));
    }
    Ok(items)
}

/// 下载一张图片（封面 / 剧照）。调用方负责落盘与容错。
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = agent()
        .get(url)
        .call()
        .map_err(|err| PortError::Backend(format!("请求 {url} 失败：{err}")))?;
    let mut bytes = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(MAX_BODY)
        .read_to_end(&mut bytes)
        .map_err(|err| PortError::Backend(format!("读取 {url} 响应失败：{err}")))?;
    Ok(bytes)
}

fn get_text(url: &str) -> Result<String> {
    let resp = agent()
        .get(url)
        .call()
        .map_err(|err| PortError::Backend(format!("请求 {url} 失败：{err}")))?;
    let mut body = String::new();
    use std::io::Read;
    resp.into_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|err| PortError::Backend(format!("读取 {url} 响应失败：{err}")))?;
    Ok(body)
}

fn get_html(url: &str) -> Result<(String, String)> {
    let resp = match agent().get(url).call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(403, resp)) => {
            let mut body = String::new();
            use std::io::Read;
            let _ = resp.into_reader().take(MAX_BODY).read_to_string(&mut body);
            if is_cloudflare_challenge(&body) {
                return Err(PortError::Backend(
                    "JavLibrary 被 Cloudflare 拦截：当前网络需能直连该站（或换用可访问的网络）"
                        .to_owned(),
                ));
            }
            return Err(PortError::Backend(format!("请求 {url} 失败：HTTP 403")));
        }
        Err(err) => return Err(PortError::Backend(format!("请求 {url} 失败：{err}"))),
    };
    let final_url = resp.get_url().to_owned();
    let mut body = String::new();
    use std::io::Read;
    resp.into_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|err| PortError::Backend(format!("读取 {url} 响应失败：{err}")))?;
    Ok((final_url, body))
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent(USER_AGENT)
        .build()
}

/// 查询参数百分号编码（只保留 RFC 3986 的非保留字符）。
fn encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 归一化标题后比较：完全一致 100 分，互相包含 60 分，其余按公共前缀给分。
fn string_similarity(left: &str, right: &str) -> i32 {
    let a = normalize_title(left);
    let b = normalize_title(right);
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        return 100;
    }
    if a.contains(&b) || b.contains(&a) {
        return 60;
    }
    let common = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .count();
    if common == 0 {
        0
    } else {
        (common as i32 * 100) / a.chars().count().max(b.chars().count()) as i32
    }
}

fn normalize_title(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

/// 去掉简介里的 HTML 标签并解码常见实体，压缩空白。
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let text = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(2000).collect()
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for value in values {
        let text = value.trim();
        if text.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == text) {
            out.push(text.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_normalization_ignores_separators() {
        assert_eq!(normalize_code("SSIS-001").as_deref(), Some("SSIS001"));
        assert_eq!(normalize_code("h_1234abcd").as_deref(), Some("H1234ABCD"));
        assert_eq!(normalize_code("  "), None);
    }

    #[test]
    fn title_cleaning_drops_site_noise() {
        assert_eq!(clean_title("作品名 [中文字幕] - Hanime1.me"), "作品名");
        assert_eq!(clean_title("作品名 (1)"), "作品名");
        assert_eq!(clean_title("【无码】作品名 OVA"), "作品名");
        assert_eq!(clean_title("普通标题"), "普通标题");
    }

    #[test]
    fn title_similarity_prefers_exact_match() {
        assert_eq!(string_similarity("作品 名", "作品名"), 100);
        assert!(string_similarity("作品名 第二季", "作品名") >= 60);
        assert!(string_similarity("完全不同", "作品名") < 40);
    }

    #[test]
    fn html_to_text_strips_tags() {
        assert_eq!(html_to_text("<p>Hello &amp; world</p>"), "Hello & world");
    }

    #[test]
    fn minutes_conversion() {
        assert_eq!(parse_minutes_ms("120 分"), 7_200_000);
        assert_eq!(parse_minutes_ms("—"), 0);
    }
}
