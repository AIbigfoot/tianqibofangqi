//! L3：迅雷影音在线字幕源（XMP 字幕库）。
//!
//! 接入方式与迅雷影音 / Emby 插件一致：无鉴权，按名称搜索返回字幕清单，
//! 字幕文件直链可直接 GET。本模块只负责**搜索 + 下载文本**，
//! 解析交给 L2 [`cinema_domain::subtitle::parse_subtitle_text`]，落库交给 L1/L4。

use std::collections::HashSet;
use std::io::Read;
use std::time::Duration;

use cinema_domain::error::{PortError, Result};
use serde_json::Value;

const SEARCH_URL: &str = "https://api-shoulei-ssl.xunlei.com/oracle/subtitle?name=";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// 字幕文本上限：正常字幕几十 KB，超限视为异常响应。
const MAX_TEXT_BYTES: u64 = 8 * 1024 * 1024;

/// 一条在线字幕候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineSubtitle {
    /// 站点给出的文件名（含扩展名）。
    pub name: String,
    /// 字幕文件直链。
    pub url: String,
    /// 归一化扩展名：`srt|ass|ssa|vtt`。
    pub ext: String,
    /// 语言标注（站点未标注时为「未标注」）。
    pub lang: String,
    /// 来源说明（站点 `extra_name`，如「网友上传」）。
    pub source: String,
    /// 站点记录的影片时长（毫秒），可用于提示匹配度。
    pub duration_ms: Option<i64>,
}

/// 迅雷 XMP 字幕库客户端。
pub struct XunleiSubtitleClient {
    agent: ureq::Agent,
}

impl Default for XunleiSubtitleClient {
    fn default() -> Self {
        Self::new()
    }
}

impl XunleiSubtitleClient {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .user_agent(USER_AGENT)
            .build();
        Self { agent }
    }

    /// 按番号 / 片名搜索字幕；空名称直接返回空清单（不发请求）。
    pub fn search(&self, name: &str) -> Result<Vec<OnlineSubtitle>> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{SEARCH_URL}{}", encode_query(name));
        let resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| PortError::Backend(format!("在线字幕搜索失败：{e}")))?;
        let mut text = String::new();
        resp.into_reader()
            .take(MAX_TEXT_BYTES)
            .read_to_string(&mut text)
            .map_err(|e| PortError::Backend(format!("读取在线字幕搜索结果失败：{e}")))?;
        Ok(parse_search_json(&text))
    }

    /// 下载字幕文件并转为 UTF-8 文本（站点存在 GBK 字幕）。
    pub fn fetch_text(&self, url: &str) -> Result<String> {
        let resp = self
            .agent
            .get(url)
            .call()
            .map_err(|e| PortError::Backend(format!("在线字幕下载失败：{e}")))?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(MAX_TEXT_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|e| PortError::Backend(format!("读取在线字幕文件失败：{e}")))?;
        if bytes.is_empty() {
            return Err(PortError::Backend("在线字幕文件为空".into()));
        }
        Ok(decode_text(&bytes))
    }
}

/// 解析搜索结果 JSON：只保留可下载的字幕文件，按 url 去重，最多 30 条。
fn parse_search_json(text: &str) -> Vec<OnlineSubtitle> {
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let Some(items) = root.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(url) = item
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        else {
            continue;
        };
        let ext = item
            .get("ext")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "srt" | "ass" | "ssa" | "vtt") {
            continue;
        }
        if !seen.insert(url.to_owned()) {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("字幕")
            .to_owned();
        let lang = item
            .get("languages")
            .and_then(Value::as_array)
            .map(|list| {
                let parts: Vec<&str> = list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.is_empty() {
                    "未标注".to_owned()
                } else {
                    parts.join(" / ")
                }
            })
            .unwrap_or_else(|| "未标注".to_owned());
        let source = item
            .get("extra_name")
            .and_then(Value::as_str)
            .map(|s| {
                s.trim_matches(|c| c == '（' || c == '）' || c == '(' || c == ')')
                    .trim()
                    .to_owned()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "迅雷字幕库".to_owned());
        let duration_ms = item
            .get("duration")
            .and_then(Value::as_i64)
            .filter(|v| *v > 0);
        out.push(OnlineSubtitle {
            name,
            url: url.to_owned(),
            ext,
            lang,
            source,
            duration_ms,
        });
        if out.len() >= 30 {
            break;
        }
    }
    out
}

/// 字节 → UTF-8 文本：优先按 UTF-8，失败回退 GB18030（兼容 GBK 字幕）。
pub fn decode_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => {
            let (text, _, _) = encoding_rs::GB18030.decode(bytes);
            text.into_owned()
        }
    }
}

/// 查询参数编码：只保留 RFC 3986 unreserved 字符，其余按 UTF-8 百分号编码。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_json_keeps_downloadable_subtitles() {
        let json = r#"{"code":0,"data":[
            {"url":"https://x/a.srt","ext":"srt","name":"IPX-540.srt","languages":["简体中文"],"extra_name":"（网友上传）","duration":9169754},
            {"url":"https://x/a.srt","ext":"srt","name":"重复","languages":[],"extra_name":"","duration":0},
            {"url":"https://x/b.zip","ext":"zip","name":"b.zip","languages":[],"extra_name":"","duration":0},
            {"url":"javascript:void(0)","ext":"ass","name":"bad","languages":[],"extra_name":"","duration":0}
        ]}"#;
        let list = parse_search_json(json);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].lang, "简体中文");
        assert_eq!(list[0].source, "网友上传");
        assert_eq!(list[0].duration_ms, Some(9169754));
    }

    #[test]
    fn gbk_bytes_fall_back_to_gb18030() {
        // “你好” 的 GBK 编码
        let text = decode_text(&[0xC4, 0xE3, 0xBA, 0xC3]);
        assert_eq!(text, "你好");
    }
}
