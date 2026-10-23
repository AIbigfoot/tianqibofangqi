//! P-06 `JavdbClient` 的真实实现：JavDB 页面抓取与解析。
//!
//! **为什么用「HTTP + HTML 解析」而不是官方 API**：站点没有公开 API，页面结构
//! 稳定（搜索卡片、详情面板、截图栅格、短评分片），因此把「取页面」与「解析页面」
//! 都收敛在本文件，解析函数是纯函数（`parse_*`），可用固定样本离线测试。
//!
//! **登录态**：标题 / 封面 / 截图 / 标签 / 日期 / 演员无需登录；评论与预告片需要
//! Cookie（站点对匿名用户返回 `/login` 并隐藏评论区）。Cookie 由用户在界面粘贴，
//! 本层只负责带上它并在探测失败时给出可读原因。
//!
//! **预告片本地化**：详情页里的预告片是 HLS（m3u8）。本层逐片下载后合并为单个
//! `.ts`（mpv 可直接播放）。遇到 `#EXT-X-KEY`（加密）或 `#EXT-X-BYTERANGE`
//! （分片偏移）时返回 `Unsupported`，调用方保留远端地址在线播放。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use scraper::{ElementRef, Html, Selector};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{ScrapedComment, ScrapedImage};
use cinema_domain::ports::javdb::{
    JavdbClient, JavdbConfig, LoginState, RemoteDetail, RemoteHit, TrailerKind, TrailerSource,
};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/122.0.0.0 Safari/537.36";
/// 页面 HTML 上限：正常详情页约 60KB，超出视为异常响应。
const MAX_HTML: u64 = 8 * 1024 * 1024;
/// 单张图片上限。
const MAX_BYTES: u64 = 32 * 1024 * 1024;
/// 截图最多保留张数（详情页通常 8–12 张）。
const MAX_SCREENSHOTS: usize = 12;
/// 短评最多保留条数（站点匿名/普通账号只返回少量）。
const MAX_COMMENTS: usize = 10;

/// 真实实现。
pub struct JavdbProvider {
    agent: ureq::Agent,
}

impl JavdbProvider {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(25))
            .user_agent(USER_AGENT)
            .build();
        Self { agent }
    }

    fn get_text(&self, config: &JavdbConfig, url: &str) -> Result<String> {
        let resp = self.send(config, url, None)?;
        let mut out = String::new();
        resp.into_reader()
            .take(MAX_HTML)
            .read_to_string(&mut out)
            .map_err(|e| PortError::Backend(format!("读取响应失败：{e}")))?;
        Ok(out)
    }

    fn get_bytes(&self, config: &JavdbConfig, url: &str) -> Result<Vec<u8>> {
        let resp = self.send(config, url, None)?;
        let mut out = Vec::new();
        resp.into_reader()
            .take(MAX_BYTES)
            .read_to_end(&mut out)
            .map_err(|e| PortError::Backend(format!("下载失败：{e}")))?;
        Ok(out)
    }

    fn send(
        &self,
        config: &JavdbConfig,
        url: &str,
        referer: Option<&str>,
    ) -> Result<ureq::Response> {
        let mut req = self.agent.get(url).set("Accept-Language", "zh-CN,zh;q=0.9");
        let cookie = normalize_cookie(&config.cookie);
        if !cookie.is_empty() {
            req = req.set("Cookie", &cookie);
        }
        if let Some(referer) = referer {
            req = req.set("Referer", referer);
        }
        match req.call() {
            Ok(resp) => Ok(resp),
            Err(ureq::Error::Status(code, _)) => {
                let msg = match code {
                    404 => "数据源返回 404（页面不存在或已下架）".to_owned(),
                    403 | 503 => format!("数据源拒绝访问（HTTP {code}），Cookie 可能失效"),
                    _ => format!("数据源返回 HTTP {code}"),
                };
                Err(PortError::Backend(msg))
            }
            Err(ureq::Error::Transport(t)) => {
                Err(PortError::Backend(format!("连接数据源失败：{t}")))
            }
        }
    }

    /// 解析详情页，并按需补抓「预告片取流地址」与短评。
    fn detail_at(&self, config: &JavdbConfig, url: &str, uid: &str) -> Result<RemoteDetail> {
        let html = self.get_text(config, url)?;
        let mut parsed = parse_detail_html(&html, uid);

        // 预告片：登录后详情页带 <video data-url>，该地址返回 JSON {url: m3u8}。
        if parsed.detail.trailer.is_none() {
            if let Some(endpoint) = parsed.preview_endpoint.as_deref() {
                let endpoint = absolute(&config.base_url, endpoint);
                let mut req = self
                    .agent
                    .get(&endpoint)
                    .set("Accept-Language", "zh-CN,zh;q=0.9")
                    .set("X-Requested-With", "XMLHttpRequest")
                    .set("Referer", url);
                let cookie = normalize_cookie(&config.cookie);
                if !cookie.is_empty() {
                    req = req.set("Cookie", &cookie);
                }
                if let Some(csrf) = parsed.csrf.as_deref() {
                    req = req.set("X-CSRF-Token", csrf);
                }
                if let Ok(resp) = req.call() {
                    let mut body = String::new();
                    if resp.into_reader().take(64 * 1024).read_to_string(&mut body).is_ok() {
                        if let Some(src) = parse_preview_json(&body) {
                            let kind = if src.contains(".m3u8") {
                                TrailerKind::Hls
                            } else {
                                TrailerKind::Direct
                            };
                            parsed.detail.trailer = Some(TrailerSource { url: src, kind });
                        }
                    }
                }
            }
        }

        // 短评：独立分片接口，匿名也能取到（站点限量的部分）。
        let reviews_url = format!(
            "{}/v/{}/reviews/lastest",
            config.base_url.trim_end_matches('/'),
            uid
        );
        if let Ok(html) = self.get_text(config, &reviews_url) {
            parsed.detail.comments = parse_reviews_html(&html);
        }
        // 站点返回的预告片地址可能是站内绝对路径：统一补成完整 URL，下载与在线播放都要用。
        if let Some(trailer) = parsed.detail.trailer.as_mut() {
            trailer.url = absolute(&config.base_url, &trailer.url);
        }
        Ok(parsed.detail)
    }
}

impl Default for JavdbProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl JavdbClient for JavdbProvider {
    fn login_state(&self, config: &JavdbConfig) -> Result<LoginState> {
        if !config.has_cookie() {
            return Ok(LoginState::default());
        }
        let url = format!("{}/", config.base_url.trim_end_matches('/'));
        let html = self.get_text(config, &url)?;
        Ok(parse_login_html(&html))
    }

    fn search(&self, config: &JavdbConfig, code: &str) -> Result<Vec<RemoteHit>> {
        let url = format!(
            "{}/search?q={}&locale=zh-CN",
            config.base_url.trim_end_matches('/'),
            encode_query(code)
        );
        let html = self.get_text(config, &url)?;
        let mut hits = parse_search_html(&html);
        // 站点搜索是模糊匹配：把番号一致的排到最前，其余保持站点顺序。
        hits.sort_by_key(|hit| !cinema_domain::javdb_code::same_code(&hit.code, code));
        Ok(hits)
    }

    fn detail(&self, config: &JavdbConfig, uid: &str) -> Result<RemoteDetail> {
        let url = format!(
            "{}/v/{}?locale=zh-CN",
            config.base_url.trim_end_matches('/'),
            uid
        );
        self.detail_at(config, &url, uid)
    }

    fn fetch_bytes(&self, config: &JavdbConfig, url: &str) -> Result<Vec<u8>> {
        self.get_bytes(config, url)
    }

    fn save_trailer(&self, config: &JavdbConfig, source: &TrailerSource, dest: &Path) -> Result<()> {
        match source.kind {
            TrailerKind::Direct => {
                let bytes = self.get_bytes(config, &source.url)?;
                write_all(dest, &bytes)
            }
            TrailerKind::Hls => self.save_hls(config, &source.url, dest),
        }
    }
}

impl JavdbProvider {
    /// 下载 HLS：支持主播放列表（取第一路）与普通分片列表；加密/分片偏移不支持。
    fn save_hls(&self, config: &JavdbConfig, url: &str, dest: &Path) -> Result<()> {
        let mut playlist_url = url.to_owned();
        let mut text = self.get_text(config, &playlist_url)?;
        if text.contains("#EXT-X-STREAM-INF") {
            let variant = first_uri(&text)
                .ok_or_else(|| PortError::Backend("预告片播放列表为空".into()))?;
            playlist_url = resolve_url(&playlist_url, &variant);
            text = self.get_text(config, &playlist_url)?;
        }
        if text.contains("#EXT-X-KEY") {
            return Err(PortError::Unsupported("预告片为加密流，无法本地保存".into()));
        }
        if text.contains("#EXT-X-BYTERANGE") {
            return Err(PortError::Unsupported(
                "预告片分片使用偏移寻址，暂不支持本地保存".into(),
            ));
        }
        let segments = uris(&text);
        if segments.is_empty() {
            return Err(PortError::Backend("预告片播放列表中没有分片".into()));
        }
        let mut file =
            File::create(dest).map_err(|e| PortError::Backend(format!("创建预告片文件失败：{e}")))?;
        for seg in segments {
            let seg_url = resolve_url(&playlist_url, &seg);
            let bytes = self.get_bytes(config, &seg_url)?;
            file.write_all(&bytes)
                .map_err(|e| PortError::Backend(format!("写入预告片失败：{e}")))?;
        }
        file.flush()
            .map_err(|e| PortError::Backend(format!("写入预告片失败：{e}")))?;
        Ok(())
    }
}

fn write_all(dest: &Path, bytes: &[u8]) -> Result<()> {
    let mut file =
        File::create(dest).map_err(|e| PortError::Backend(format!("创建文件失败：{e}")))?;
    file.write_all(bytes)
        .map_err(|e| PortError::Backend(format!("写入文件失败：{e}")))
}

/// 用户可能粘贴整段 `Cookie: a=b; c=d`，这里剥掉前缀与换行。
fn normalize_cookie(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("Cookie:")
        .trim_start_matches("cookie:")
        .replace(['\r', '\n'], "")
        .trim()
        .to_owned()
}

fn encode_query(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 是否为可播放地址：接受绝对 URL 与站内绝对路径，排除 `#fragment` / `javascript:`。
fn is_playable_url(url: &str) -> bool {
    let url = url.trim();
    !url.is_empty()
        && !url.starts_with('#')
        && !url.starts_with("javascript:")
        && (url.starts_with("http://") || url.starts_with("https://") || url.starts_with('/'))
}

/// `path` 为绝对 URL 时原样返回，否则与站点根拼接。
fn absolute(base_url: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_owned()
    } else {
        format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

/// 相对地址解析：保留播放列表所在目录。
fn resolve_url(playlist_url: &str, uri: &str) -> String {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return uri.to_owned();
    }
    if let Some(prefix) = playlist_url.rfind('/') {
        format!("{}/{}", &playlist_url[..prefix], uri.trim_start_matches('/'))
    } else {
        uri.to_owned()
    }
}

// ---------------------------------------------------------------------------
// 解析（纯函数，可用固定样本测试）
// ---------------------------------------------------------------------------

/// 详情页解析的中间结果：除可见字段外，还带预告片需要的端点与 CSRF。
struct ParsedDetail {
    detail: RemoteDetail,
    preview_endpoint: Option<String>,
    csrf: Option<String>,
}

fn sel(text: &str) -> Selector {
    Selector::parse(text).expect("静态选择器必须合法")
}

fn text_of(el: &ElementRef<'_>) -> String {
    el.text().collect::<String>().trim().to_owned()
}

fn attr(el: &ElementRef<'_>, name: &str) -> Option<String> {
    el.value()
        .attr(name)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_detail_html(html: &str, uid: &str) -> ParsedDetail {
    let doc = Html::parse_document(html);
    let mut detail = RemoteDetail {
        uid: uid.to_owned(),
        ..Default::default()
    };

    // 番号与标题
    if let Some(el) = doc.select(&sel(".video-detail h2.title > strong")).next() {
        detail.code = text_of(&el);
    }
    if let Some(el) = doc.select(&sel(".video-detail .current-title")).next() {
        detail.title = text_of(&el);
    }
    if detail.code.is_empty() {
        if let Some(el) = doc.select(&sel(".movie-panel-info [data-clipboard-text]")).next() {
            detail.code = attr(&el, "data-clipboard-text").unwrap_or_default();
        }
    }
    if detail.title.is_empty() {
        if let Some(el) = doc.select(&sel(".video-detail .origin-title")).next() {
            detail.title = text_of(&el);
        }
    }

    // 封面
    if let Some(el) = doc.select(&sel(".column-video-cover img.video-cover")).next() {
        detail.cover_url = attr(&el, "src");
    }

    // 信息面板：标签 → 值
    let mut panel: BTreeMap<String, String> = BTreeMap::new();
    let mut categories: Vec<String> = Vec::new();
    let mut actors: Vec<String> = Vec::new();
    for block in doc.select(&sel(".movie-panel-info .panel-block")) {
        let Some(label_el) = block.select(&sel("strong")).next() else {
            continue;
        };
        let label = text_of(&label_el)
            .trim_end_matches([':', '：'])
            .trim()
            .to_owned();
        let Some(value_el) = block.select(&sel(".value")).next() else {
            continue;
        };
        let links: Vec<String> = value_el
            .select(&sel("a"))
            .map(|a| text_of(&a))
            .filter(|t| !t.is_empty())
            .collect();
        match label.as_str() {
            "日期" | "Date" => panel.insert("date".into(), text_of(&value_el)),
            "时长" | "時長" | "Duration" => panel.insert("duration".into(), text_of(&value_el)),
            "导演" | "導演" | "Director" => panel.insert("director".into(), text_of(&value_el)),
            "片商" | "Maker" | "Studio" => panel.insert("maker".into(), text_of(&value_el)),
            "系列" | "Series" => panel.insert("series".into(), text_of(&value_el)),
            "评分" | "評分" | "Rating" => panel.insert("rating".into(), text_of(&value_el)),
            "类别" | "類別" | "Category" => {
                categories = links;
                None
            }
            "演员" | "演員" | "Actor" | "Actors" => {
                actors = links;
                None
            }
            _ => None,
        };
    }
    detail.release_date = panel.get("date").filter(|s| !s.is_empty()).cloned();
    detail.duration_ms = panel.get("duration").and_then(|s| parse_duration_ms(s));
    detail.rating = panel.get("rating").and_then(|s| parse_rating(s));
    detail.director = panel.get("director").filter(|s| !s.is_empty()).cloned();
    detail.maker = panel.get("maker").filter(|s| !s.is_empty()).cloned();
    detail.series = panel.get("series").filter(|s| !s.is_empty()).cloned();
    detail.categories = categories;
    detail.actors = actors;

    // 截图（只取 samples 大图，跳过预告片容器）
    for a in doc.select(&sel("a.tile-item")) {
        let Some(href) = attr(&a, "href") else {
            continue;
        };
        if !href.contains("/samples/") {
            continue;
        }
        let Some(file_name) = href.rsplit('/').next().filter(|s| !s.is_empty()) else {
            continue;
        };
        detail.screenshots.push(ScrapedImage {
            file_name: file_name.to_owned(),
            source_url: Some(href),
        });
        if detail.screenshots.len() >= MAX_SCREENSHOTS {
            break;
        }
    }

    // 预告片：匿名时是 /login；登录后是 <video data-url="...">。
    let mut preview_endpoint = None;
    if let Some(el) = doc.select(&sel("video[data-url]")).next() {
        preview_endpoint = attr(&el, "data-url").filter(|url| is_playable_url(url));
    }
    if let Some(el) = doc.select(&sel("a.preview-video-container")).next() {
        if let Some(href) = attr(&el, "href") {
            if !href.contains("/login") && is_playable_url(&href) {
                let kind = if href.contains(".m3u8") {
                    TrailerKind::Hls
                } else {
                    TrailerKind::Direct
                };
                detail.trailer = Some(TrailerSource { url: href, kind });
            }
        }
    }
    if detail.trailer.is_none() {
        if let Some(el) = doc.select(&sel("video source[src]")).next() {
            if let Some(src) = attr(&el, "src").filter(|src| is_playable_url(src)) {
                let kind = if src.contains(".m3u8") {
                    TrailerKind::Hls
                } else {
                    TrailerKind::Direct
                };
                detail.trailer = Some(TrailerSource { url: src, kind });
            }
        }
    }

    let csrf = doc
        .select(&sel("meta[name=csrf-token]"))
        .next()
        .and_then(|el| attr(&el, "content"));

    ParsedDetail {
        detail,
        preview_endpoint,
        csrf,
    }
}

/// 搜索列表页：`.movie-list .item > a.box`。
fn parse_search_html(html: &str) -> Vec<RemoteHit> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for a in doc.select(&sel(".movie-list .item > a.box")) {
        let Some(href) = attr(&a, "href") else {
            continue;
        };
        let Some(uid) = href.strip_prefix("/v/") else {
            continue;
        };
        let code = a
            .select(&sel(".video-title strong"))
            .next()
            .map(|el| text_of(&el))
            .unwrap_or_default();
        let title = a
            .select(&sel(".video-title"))
            .next()
            .map(|el| {
                let full = text_of(&el);
                full.strip_prefix(&code)
                    .unwrap_or(&full)
                    .trim()
                    .to_owned()
            })
            .unwrap_or_default();
        let cover_url = a.select(&sel(".cover img")).next().and_then(|el| attr(&el, "src"));
        let release_date = a.select(&sel(".meta")).next().map(|el| text_of(&el));
        out.push(RemoteHit {
            uid: uid.to_owned(),
            code,
            title,
            cover_url,
            release_date,
        });
    }
    out
}

/// 短评审片：`dt.review-item`（排除 `.more`）。
fn parse_reviews_html(html: &str) -> Vec<ScrapedComment> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    for item in doc.select(&sel("dt.review-item")) {
        let classes = item.value().classes().collect::<Vec<_>>();
        if classes.contains(&"more") {
            continue;
        }
        let content = item
            .select(&sel(".content"))
            .next()
            .map(|el| text_of(&el))
            .unwrap_or_default();
        if content.is_empty() {
            continue;
        }
        let likes = item
            .select(&sel(".likes-count"))
            .next()
            .and_then(|el| text_of(&el).parse::<u32>().ok())
            .unwrap_or(0);
        let posted_at = item.select(&sel("span.time")).next().map(|el| text_of(&el));
        let (score, _) = star_count(item);
        let author = review_author(item);
        out.push(ScrapedComment {
            author,
            score,
            content,
            likes,
            posted_at,
        });
    }
    out.sort_by(|a, b| b.likes.cmp(&a.likes));
    out.truncate(MAX_COMMENTS);
    out
}

/// 作者的文本节点直接挂在 `.review-title` 下（与点赞按钮、日期同级）。
fn review_author(item: ElementRef<'_>) -> Option<String> {
    let title = item.select(&sel(".review-title")).next()?;
    for child in title.children() {
        if let scraper::node::Node::Text(text) = child.value() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// 星级：返回（有效星数, 灰星数）。评分区间 1–5。
fn star_count(item: ElementRef<'_>) -> (Option<i32>, usize) {
    let stars: Vec<bool> = item
        .select(&sel(".score-stars i.icon-star"))
        .map(|el| el.value().classes().any(|c| c == "gray"))
        .collect();
    if stars.is_empty() {
        return (None, 0);
    }
    let total = stars.len();
    let gray = stars.iter().filter(|g| **g).count();
    (Some((total - gray) as i32), gray)
}

fn parse_preview_json(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_login_html(html: &str) -> LoginState {
    let doc = Html::parse_document(html);
    // 顶部导航出现 /login 入口即未登录；日志页自身也会带登录表单。
    let has_login_entry = doc.select(&sel("a[href=\"/login\"]")).next().is_some();
    let has_profile_entry = doc.select(&sel("a[href=\"/users/profile\"]")).next().is_some();
    if has_login_entry {
        return LoginState::default();
    }
    if !has_profile_entry {
        return LoginState::default();
    }
    let username = doc
        .select(&sel("a[href=\"/users/profile\"]"))
        .next()
        .map(|el| text_of(&el))
        .filter(|t| !t.is_empty() && t != "查看账户状态" && t != "查看賬戶狀態");
    LoginState {
        logged_in: true,
        username,
    }
}

/// `150 分钟` / `34 分鍾` / `2 小时 5 分钟` → 毫秒。
fn parse_duration_ms(text: &str) -> Option<i64> {
    let mut hours: i64 = 0;
    let mut minutes: i64 = 0;
    let mut pending = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            pending.push(ch);
            continue;
        }
        if pending.is_empty() {
            continue;
        }
        let n: i64 = pending.parse().ok()?;
        if ch == '时' || ch == '時' || ch == 'h' || ch == 'H' {
            hours = n;
            pending.clear();
        } else if ch == '分' || ch == 'm' || ch == 'M' {
            minutes = n;
            pending.clear();
        }
        // 其它字符（空格、逗号）只是分隔，数字留给下一个单位
    }
    if !pending.is_empty() {
        minutes = pending.parse().ok()?;
    }
    let total = hours * 60 + minutes;
    if total > 0 {
        Some(total * 60_000)
    } else {
        None
    }
}

/// `4.43分, 由2265人评价` → 4.43。
fn parse_rating(text: &str) -> Option<f64> {
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || (ch == '.' && !current.is_empty()) {
            current.push(ch);
        } else if !current.is_empty() {
            break;
        }
    }
    current.parse::<f64>().ok()
}

/// 播放列表里第一条非注释 URI。
fn first_uri(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

fn uris(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DETAIL: &str = include_str!("../tests/fixtures/javdb_detail.html");
    const REVIEWS: &str = include_str!("../tests/fixtures/javdb_reviews.html");
    const SEARCH: &str = include_str!("../tests/fixtures/javdb_search.html");

    #[test]
    fn detail_parses_identity_and_panel() {
        let parsed = parse_detail_html(DETAIL, "ZY5eq");
        let d = parsed.detail;
        assert_eq!(d.uid, "ZY5eq");
        assert_eq!(d.code, "SSIS-001");
        assert!(d.title.contains("三上悠亞"));
        assert_eq!(d.release_date.as_deref(), Some("2021-02-19"));
        assert_eq!(d.duration_ms, Some(150 * 60_000));
        assert_eq!(d.director.as_deref(), Some("苺原"));
        assert_eq!(d.maker.as_deref(), Some("S1 NO.1 STYLE"));
        assert!(d.series.as_deref().unwrap_or("").contains("禁欲"));
        assert_eq!(d.rating, Some(4.43));
        assert_eq!(d.categories, vec!["戲劇", "美乳", "白天出軌"]);
        assert_eq!(d.actors, vec!["葵つかさ", "乙白さやか", "平田司"]);
        assert_eq!(
            d.cover_url.as_deref(),
            Some("https://c0.jdbstatic.com/covers/zy/ZY5eq.jpg")
        );
    }

    #[test]
    fn detail_parses_screenshots_and_rejects_login_trailer() {
        let parsed = parse_detail_html(DETAIL, "ZY5eq");
        assert_eq!(parsed.detail.screenshots.len(), 2);
        assert_eq!(
            parsed.detail.screenshots[0].file_name,
            "ZY5eq_l_0.jpg"
        );
        // 样本是匿名页面：预告片容器指向 /login，必须视为没有预告片。
        assert!(parsed.detail.trailer.is_none());
    }

    #[test]
    fn reviews_are_sorted_by_likes_and_capped() {
        let comments = parse_reviews_html(REVIEWS);
        assert_eq!(comments.len(), 3);
        assert_eq!(comments[0].likes, 215);
        assert_eq!(comments[0].posted_at.as_deref(), Some("2021-02-27"));
        assert_eq!(comments[0].score, Some(4));
        assert_eq!(comments[0].author.as_deref(), Some("mo***a"));
        assert!(comments[0].content.contains("葵司"));
        assert!(comments.iter().all(|c| !c.content.contains("成为VIP")));
    }

    #[test]
    fn search_prefers_exact_code() {
        let hits = parse_search_html(SEARCH);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].uid, "ZY5eq");
        assert_eq!(hits[0].code, "SSIS-001");
        assert_eq!(hits[0].release_date.as_deref(), Some("2021-02-19"));
        assert_eq!(hits[0].title, "一ヶ月間の禁欲の果てに");
    }

    #[test]
    fn duration_and_rating_tolerate_traditional_chinese() {
        assert_eq!(parse_duration_ms("34 分鍾"), Some(34 * 60_000));
        assert_eq!(parse_duration_ms("2 小时 5 分钟"), Some(125 * 60_000));
        assert_eq!(parse_duration_ms("暂无"), None);
        assert_eq!(parse_rating("4.5分, 由7人評價"), Some(4.5));
        assert_eq!(parse_rating(""), None);
    }

    #[test]
    fn preview_json_exposes_stream_url() {
        assert_eq!(
            parse_preview_json(r#"{"url":"https://x/y.m3u8"}"#).as_deref(),
            Some("https://x/y.m3u8")
        );
        assert_eq!(parse_preview_json("请先登录"), None);
    }

    #[test]
    fn relative_segments_resolve_against_playlist() {
        assert_eq!(
            resolve_url("https://cdn/a/b/index.m3u8", "seg1.ts"),
            "https://cdn/a/b/seg1.ts"
        );
        assert_eq!(
            resolve_url("https://cdn/a/index.m3u8", "https://o/x.ts"),
            "https://o/x.ts"
        );
    }

    #[test]
    fn fragment_href_is_not_taken_as_trailer() {
        // 登录后按钮仍是页内锚点 #preview-video，真正的取流端点在 <video data-url>。
        let html = r##"<html><head><meta name="csrf-token" content="tk"></head><body>
            <a class="preview-video-container" href="#preview-video">预览</a>
            <video data-url="/v/abc/preview"></video>
        </body></html>"##;
        let parsed = parse_detail_html(html, "abc");
        assert!(parsed.detail.trailer.is_none(), "锚点不能当成视频地址");
        assert_eq!(parsed.preview_endpoint.as_deref(), Some("/v/abc/preview"));
        assert_eq!(parsed.csrf.as_deref(), Some("tk"));
    }
}
