//! P-02 实现：文件名解析提供者，以及空实现。

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{Metadata, MetadataCandidate, ParseHint, ProviderRef};
use cinema_domain::ports::metadata::MetadataProvider;

/// 空实现：提供零候选、且不可被 fetch。
///
/// 用途：模拟「全部元数据源被关闭」——此时入库仍必须成功，条目落为
/// 「元数据待补全」并使用占位海报。
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyMetadataProvider;

impl MetadataProvider for EmptyMetadataProvider {
    fn name(&self) -> &str {
        "empty"
    }

    fn priority(&self) -> u8 {
        u8::MAX
    }

    fn candidates(&self, _hint: &ParseHint) -> Result<Vec<MetadataCandidate>> {
        Ok(Vec::new())
    }

    fn fetch(&self, id: &ProviderRef) -> Result<Metadata> {
        Err(PortError::NotFound(format!("空实现无可用记录: {}", id.id)))
    }
}

/// 文件名解析提供者最小实现：完全离线，从文件名推出标题与年份。
///
/// 它是「元数据源不可用」时的兜底：即使所有在线源都失败，条目依然有可用标题。
#[derive(Debug, Clone, Copy, Default)]
pub struct FileNameMetadataProvider;

impl MetadataProvider for FileNameMetadataProvider {
    fn name(&self) -> &str {
        "filename"
    }

    fn priority(&self) -> u8 {
        // 数值越小越优先；文件名解析属于最低可信来源，排在在线源之后
        100
    }

    fn candidates(&self, hint: &ParseHint) -> Result<Vec<MetadataCandidate>> {
        let parsed = parse_file_name(&hint.raw_name, hint.year);
        Ok(vec![MetadataCandidate {
            provider_ref: ProviderRef {
                provider: self.name().to_owned(),
                id: parsed.title.clone(),
            },
            title: parsed.title,
            year: parsed.year,
            score: 60,
        }])
    }

    fn fetch(&self, id: &ProviderRef) -> Result<Metadata> {
        let parsed = parse_file_name(&id.id, None);
        Ok(Metadata {
            title: Some(parsed.title),
            year: parsed.year,
            ..Default::default()
        })
    }
}

struct ParsedName {
    title: String,
    year: Option<i32>,
}

/// 从文件名解析标题与年份：去扩展名 → 统一分隔符 → 提取四位年份。
/// 具体"如何判定"的算法不在本文档定义范围，M0 只保证可复现。
fn parse_file_name(raw: &str, fallback_year: Option<i32>) -> ParsedName {
    let stem = std::path::Path::new(raw)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);

    let normalized: String = stem
        .chars()
        .map(|c| if c == '.' || c == '_' { ' ' } else { c })
        .collect();

    let lower = normalized.to_ascii_lowercase();
    let year = extract_year(&lower).or(fallback_year);

    let title = match extract_year_span(&lower) {
        Some(start) => normalized[..start].trim().to_owned(),
        None => normalized.trim().to_owned(),
    };
    let title = if title.is_empty() {
        normalized.trim().to_owned()
    } else {
        title
    };

    ParsedName { title, year }
}

/// 返回四位年份在字符串中的起始字节位置（保证是字符边界）。
fn extract_year_span(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.len() < 4 {
        return None;
    }
    for i in 0..=bytes.len() - 4 {
        let window = &bytes[i..i + 4];
        if !window.iter().all(u8::is_ascii_digit) {
            continue;
        }
        // 前一位若是数字，说明是更长的数字串，不是年份
        if i > 0 && bytes[i - 1].is_ascii_digit() {
            continue;
        }
        if let Ok(y) = s[i..i + 4].parse::<i32>() {
            if (1900..=2100).contains(&y) {
                return Some(i);
            }
        }
    }
    None
}

fn extract_year(s: &str) -> Option<i32> {
    extract_year_span(s).and_then(|i| s[i..i + 4].parse::<i32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_title_and_year() {
        let parsed = parse_file_name("Blade.Runner.1982.1080p.mkv", None);
        assert_eq!(parsed.year, Some(1982));
        assert_eq!(parsed.title, "Blade Runner");

        let parsed = parse_file_name("无名影片.mkv", None);
        assert_eq!(parsed.year, None);
        assert_eq!(parsed.title, "无名影片");
    }

    #[test]
    fn falls_back_to_hint_year() {
        let parsed = parse_file_name("Some_Movie.mkv", Some(2001));
        assert_eq!(parsed.year, Some(2001));
        assert_eq!(parsed.title, "Some Movie");
    }
}
