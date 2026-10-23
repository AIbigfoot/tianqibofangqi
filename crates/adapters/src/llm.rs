//! 在线大模型（OpenAI 兼容 `chat/completions`）：把一部影片的高赞短评
//! 总结成一段「核心看点」，供主机模式的推荐横排使用。
//!
//! 设计取舍：
//! - 只依赖一套通用协议（`/chat/completions` + Bearer Key），DeepSeek / OpenAI /
//!   任意兼容网关都能用，不必为每家服务各写一个适配器；
//! - 输入 = 存储的 10 条高赞短评 + 联网检索到的资料，两者取舍后写成一段完整看点；
//! - 一次请求打包多部影片，减少往返次数（10 条看点原本要 10 次请求）；
//! - 大模型不可用（未配置 / 超时 / 返回不可解析）时不报错中断推荐，
//!   由 [`local_highlight`] 从高赞评论里兜底提炼，界面照常可用。

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::ScrapedComment;

/// 单次请求的超时：10 部影片一起总结，给足时间但不无限等待。
const LLM_TIMEOUT_SECS: u64 = 60;
/// 每部影片送给模型的评论条数：与数据源保存的 10 条高赞短评一致，确保总结覆盖全部评论。
const COMMENTS_PER_ITEM: usize = 10;
/// 看点长度上限（字符）：模型要求 200–300 字，这里留出裁剪余量。
const HIGHLIGHT_MAX_CHARS: usize = 300;
/// 低于这个字数视为不合格，触发一次扩写补足。
const MIN_HIGHLIGHT_CHARS: usize = 200;

/// 大模型连接配置（全部来自本机设置表）。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// OpenAI 兼容的接口基址，例如 `https://api.deepseek.com/v1`。
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl LlmConfig {
    /// 三项齐全才算「已配置」。
    pub fn ready(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.api_key.trim().is_empty()
            && !self.model.trim().is_empty()
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.base_url.trim().trim_end_matches('/')
        )
    }
}

/// 一部影片的总结素材。
#[derive(Debug, Clone)]
pub struct HighlightInput {
    pub title: String,
    pub code: Option<String>,
    /// 数据源写入的剧情简介。
    pub overview: String,
    /// 已抓取的标签（类别 / 系列 / 片商 / 导演 / 演员）。
    pub tags: String,
    /// 库内保存的高赞短评（最多取 10 条）。
    pub comments: Vec<ScrapedComment>,
    /// 联网检索到的资料片段（可能为空）。
    pub web: Vec<String>,
}

impl HighlightInput {
    fn comment_lines(&self) -> Vec<String> {
        self.comments
            .iter()
            .take(COMMENTS_PER_ITEM)
            .filter_map(|comment| {
                let text = sanitize_comment(&comment.content);
                if text.is_empty() {
                    return None;
                }
                Some(format!("（{}赞）{}", comment.likes, text))
            })
            .collect()
    }
}

/// 为多部影片生成看点；返回顺序与入参一致，单项失败为 `None`。
pub fn summarize_highlights(
    config: &LlmConfig,
    entries: &[HighlightInput],
) -> Result<Vec<Option<String>>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let payload: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            json!({
                "n": idx + 1,
                "title": entry.title,
                "code": entry.code,
                "overview": entry.overview,
                "tags": entry.tags,
                "comments": entry.comment_lines(),
                "web": entry.web,
            })
        })
        .collect();

    let system = "你是影视推荐编辑。输入包含每部影片的片名、番号、剧情简介（overview）、标签（tags）、\
10 条高赞短评（comments）与联网资料（web）。请为每部影片写一段 200–300 字的中文核心看点。\
必须写满 4 句、每句 50–70 字：①题材与设定；②表演与角色关系；③制作质感（镜头、布光、美术、节奏）；\
④整体氛围与适合的观众。写作要求：必须真正使用 comments 与 overview / tags 里的具体信息（演员、片商、\
场景、题材、剧情设定），严禁只改写片名或空泛扩写；web 为空时忽略，不得编造简介与短评中没有的具体事实；\
不剧透结局；不写「这部」「本片」「推荐」等空话；禁止提下载、链接、资源或任何获取方式。\
总字数少于 200 字视为不合格，请先扩充再输出。只输出 JSON，不要任何解释：\
{\"items\":[{\"n\":1,\"text\":\"...\"}]}";

    let content = chat(config, system, &serde_json::to_string(&payload).unwrap_or_default())?;
    let parsed = parse_items(&content)
        .ok_or_else(|| PortError::Backend("大模型没有返回可解析的看点 JSON".into()))?;

    let mut out = vec![None; entries.len()];
    for (key, text) in parsed {
        if key >= 1 && key <= out.len() {
            let cleaned = clean_highlight(&text);
            if !cleaned.is_empty() {
                out[key - 1] = Some(cleaned);
            }
        }
    }

    // 模型常把「200–300 字」写成 120–150 字：对不足的条目再做一次扩写，
    // 只补充短评与资料里已有的信息，避免又回到「翻译标题」。
    let shorts: Vec<usize> = out
        .iter()
        .enumerate()
        .filter_map(|(idx, text)| match text {
            Some(text) if text.chars().count() < MIN_HIGHLIGHT_CHARS => Some(idx),
            _ => None,
        })
        .collect();
    if !shorts.is_empty() {
        if let Ok(expanded) = expand_highlights(config, entries, &out, &shorts) {
            for (idx, text) in expanded {
                if idx < out.len() {
                    out[idx] = Some(text);
                }
            }
        }
    }
    Ok(out)
}

/// 扩写不足字数的看点：把原素材再给模型一次，要求补足 220–280 字。
fn expand_highlights(
    config: &LlmConfig,
    entries: &[HighlightInput],
    current: &[Option<String>],
    shorts: &[usize],
) -> Result<Vec<(usize, String)>> {
    let payload: Vec<Value> = shorts
        .iter()
        .map(|&idx| {
            let entry = &entries[idx];
            json!({
                "n": idx + 1,
                "title": entry.title,
                "code": entry.code,
                "overview": entry.overview,
                "tags": entry.tags,
                "comments": entry.comment_lines(),
                "web": entry.web,
                "current": current.get(idx).and_then(|text| text.clone()).unwrap_or_default(),
            })
        })
        .collect();
    let system = "下面每部影片的看点初稿不足 200 字。请利用随附的剧情简介、标签、10 条短评与网络资料，\
把 current 扩写到 220–280 字：保留原有事实，只补充短评与资料里已经出现的具体信息（演员、片商、场景、\
题材、制作细节、观众反馈），严禁编造；不要只改写标题；不剧透结局；禁止提下载、链接、资源或获取方式。\
只输出 JSON，不要任何解释：{\"items\":[{\"n\":1,\"text\":\"...\"}]}";
    let content = chat(config, system, &serde_json::to_string(&payload).unwrap_or_default())?;
    let parsed = parse_items(&content)
        .ok_or_else(|| PortError::Backend("大模型没有返回可解析的扩写 JSON".into()))?;
    let mut out = Vec::new();
    for (key, text) in parsed {
        let cleaned = clean_highlight(&text);
        if !cleaned.is_empty() && key >= 1 {
            out.push((key - 1, cleaned));
        }
    }
    Ok(out)
}

/// 一次 OpenAI 兼容的对话请求，返回 `choices[0].message.content`。
fn chat(config: &LlmConfig, system: &str, user: &str) -> Result<String> {
    let body = json!({
        "model": config.model.trim(),
        "temperature": 0.5,
        // 一次最多总结 10 部影片，每部 200–300 字：输出预算必须按「10 部」给足，
        // 1500 会让模型把每部压到 100 多字来迁就上限。
        "max_tokens": 8000,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]
    });
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(LLM_TIMEOUT_SECS))
        .build();
    let response = agent
        .post(&config.endpoint())
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", config.api_key.trim()))
        .send_string(&body.to_string());

    let response = match response {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp
                .into_string()
                .map(|text| text.chars().take(160).collect::<String>())
                .unwrap_or_default();
            return Err(PortError::Backend(format!(
                "大模型接口返回 HTTP {code}：{detail}"
            )));
        }
        Err(ureq::Error::Transport(err)) => {
            return Err(PortError::Backend(format!("连接大模型失败：{err}")));
        }
    };

    // ureq 未启用 json 特性，这里自行读取文本再解析（响应体通常只有几 KB）
    let mut text = String::new();
    response
        .into_reader()
        .take(256 * 1024)
        .read_to_string(&mut text)
        .map_err(|e| PortError::Backend(format!("读取大模型响应失败：{e}")))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| PortError::Backend(format!("大模型响应不是 JSON：{e}")))?;
    value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(str::to_owned)
        .ok_or_else(|| PortError::Backend("大模型响应缺少 choices[0].message.content".into()))
}

/// 容错解析：模型偶尔会套 ```json 代码块或前后加解释，这里截取第一对花括号。
fn parse_items(content: &str) -> Option<Vec<(usize, String)>> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    if end <= start {
        return None;
    }
    let value: Value = serde_json::from_str(&content[start..=end]).ok()?;
    let items = value.get("items")?.as_array()?;
    let mut out = Vec::new();
    for item in items {
        let key = item.get("n")?.as_u64()? as usize;
        let text = item.get("text")?.as_str()?.to_string();
        out.push((key, text));
    }
    Some(out)
}

/// 去掉引号、换行与结尾的句号，并压到一段的长度上限。
fn clean_highlight(raw: &str) -> String {
    let text = raw
        .trim()
        .trim_matches(|c: char| c == '"' || c == '「' || c == '」' || c == '“' || c == '”')
        .replace(['\r', '\n'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim_end_matches(['。', '.', '；', ';', '，']).trim().to_string();
    truncate_chars(&text, HIGHLIGHT_MAX_CHARS)
}

/// 清洗一条短评：去掉网址、磁力 / 迅雷 / ed2k 链接等与看点无关的内容。
///
/// 短评里常见「资源已私信」「下载 https://…」这类噪声，直接送给模型会污染总结，
/// 因此统一在提取看点前先剥掉链接（含中文直接黏在链接前的写法）。
fn sanitize_comment(raw: &str) -> String {
    const NEEDLES: [&str; 6] = ["http://", "https://", "magnet:", "ed2k://", "thunder://", "ftp://"];
    let mut text = raw.to_string();
    while let Some((pos, after)) = find_link(&text, &NEEDLES) {
        let end = text[after..]
            .char_indices()
            .find(|(_, c)| {
                c.is_whitespace()
                    || matches!(
                        c,
                        '，' | '。' | '；' | '！' | '？' | '、' | '）' | ')' | '】' | ']' | '"' | '\''
                    )
            })
            .map(|(idx, _)| after + idx)
            .unwrap_or(text.len());
        text.replace_range(pos..end, "");
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 按字节做 ASCII 忽略大小写匹配，返回「链接前缀起点 / 前缀之后」的字节下标。
///
/// 比先 `to_lowercase` 再查找更安全：某些字符转小写会改变字节长度，导致下标错位。
fn find_link(text: &str, needles: &[&str]) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    for pos in 0..bytes.len() {
        if !text.is_char_boundary(pos) {
            continue;
        }
        for needle in needles {
            let raw = needle.as_bytes();
            if pos + raw.len() <= bytes.len()
                && bytes[pos..pos + raw.len()].eq_ignore_ascii_case(raw)
            {
                return Some((pos, pos + raw.len()));
            }
        }
    }
    None
}

/// 本地兜底：从高赞评论里挑一句最像「看点」的话。
///
/// 没有大模型时也能出结果；只做抽取式提炼，不编造内容。
pub fn local_highlight(comments: &[ScrapedComment]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comment in comments.iter().take(COMMENTS_PER_ITEM) {
        let text = sanitize_comment(&comment.content);
        if text.chars().count() < 6 {
            continue;
        }
        let sentence = best_sentence(&text);
        if sentence.is_empty() || parts.iter().any(|part| part == &sentence) {
            continue;
        }
        parts.push(sentence);
        // 至少合并两条短评再停：单条评论往往只有半句卖点，撑不起一段看点
        if parts.len() >= 2 && parts.iter().map(|part| part.chars().count()).sum::<usize>() >= 60 {
            break;
        }
    }
    truncate_chars(&parts.join("；"), HIGHLIGHT_MAX_CHARS)
}

/// 把一条评论切成句子，取信息量最大的一句（长度适中、不是纯感叹）。
fn best_sentence(text: &str) -> String {
    let mut best = String::new();
    for raw in text.split(['。', '！', '？', '!', '?', '；', ';', '\n']) {
        let sentence = raw.trim().trim_matches(|c: char| c == '，' || c == ',');
        let len = sentence.chars().count();
        if len < 8 {
            continue;
        }
        if len > best.chars().count() {
            best = sentence.to_string();
            if len >= 20 {
                break;
            }
        }
    }
    best
}

fn truncate_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
