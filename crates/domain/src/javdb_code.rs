//! 番号识别（JavDB 刮削的输入）。
//!
//! 纯函数、零 IO：从文件名（或页面上的番号文本）提取候选番号，按可信度排序，
//! 供 L1 依次向数据源搜索。识别失败不是错误——条目照常播放，只标记为待手动指定。

/// 常见噪音前缀：文件名里带站名/压制组，不能当番号。
const NOISE_PREFIXES: &[&str] = &[
    "hhd", "hhd800", "com", "www", "net", "javdb", "javbus", "jav", "mp4", "mkv", "avi", "wmv",
    "hd", "fhd", "uhd", "hevc", "avc", "x264", "x265", "h264", "h265", "aac", "dts", "web",
    "webrip", "bluray", "bdrip", "hdr", "remux", "uncensored", "censored", "leak", "part",
];

/// 数字部分不能是分辨率 / 帧率 / 编码等常见规格。
///
/// 480/540/576/120 刻意<b>不</b>算噪音：三位数番号（IPX-540、CWP-120）远比
/// 不带 `p` 的标清分辨率常见；带 `p` 的分辨率由 `leading_digits` 单独排除。
const NOISE_NUMBERS: &[&str] = &[
    "720", "1080", "2160", "4320", "24", "25", "30", "50", "60", "264", "265",
];

/// 允许出现在番号末尾的版本后缀。
const CODE_SUFFIXES: &[&str] = &["C", "UC", "CH", "HD", "4K"];

/// 从条目相对路径与标题提取候选番号，按可信度降序、去重。
pub fn candidates(rel_path: Option<&str>, title: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(path) = rel_path {
        push_unique(&mut out, scan(&file_stem(path)));
    }
    // 标题里通常已含番号（刮削过或用户手动命名），作为第二来源
    push_unique(&mut out, scan(title));
    out
}

/// 把任意文本里的第一处番号归一为 `ABC-123`；不是番号返回 `None`。
pub fn canonical_code(raw: &str) -> Option<String> {
    scan(raw).into_iter().next()
}

/// 两个番号是否指向同一部片（忽略分隔符大小写）。
pub fn same_code(a: &str, b: &str) -> bool {
    canonical_code(a) == canonical_code(b)
}

fn push_unique(out: &mut Vec<String>, found: Vec<String>) {
    for code in found {
        if !out.contains(&code) {
            out.push(code);
        }
    }
}

fn file_stem(path: &str) -> String {
    let name = path
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or(path);
    match name.rfind('.') {
        Some(at) if at > 0 => name[..at].to_owned(),
        _ => name.to_owned(),
    }
}

/// 扫描文本，返回按可信度排序的候选（已去重）。
fn scan(text: &str) -> Vec<String> {
    let tokens = tokenize(text);
    let mut scored: Vec<(u8, String)> = Vec::new();

    // 日期式番号（1pondo / 10musume 等）：092415_159、121914-760
    for code in date_codes(text) {
        scored.push((0, code));
    }

    for (i, token) in tokens.iter().enumerate() {
        // FC2-PPV-1234567
        if token.eq_ignore_ascii_case("FC2") {
            if let Some(n) = tokens.get(i + 2) {
                let is_ppv = tokens.get(i + 1).map(|t| t.eq_ignore_ascii_case("PPV")) == Some(true);
                if is_ppv && is_digits(n, 5, 7) {
                    scored.push((0, format!("FC2-PPV-{n}")));
                }
            }
        }
        // FC2PPV-1234567（连写）
        if token.eq_ignore_ascii_case("FC2PPV") {
            if let Some(n) = tokens.get(i + 1) {
                if is_digits(n, 5, 7) {
                    scored.push((0, format!("FC2-PPV-{n}")));
                }
            }
        }
        // heyzo_hd_0783 → HEYZO-0783（中间可夹一个噪音 token）
        if token.eq_ignore_ascii_case("HEYZO") {
            for n in tokens.iter().skip(i + 1).take(3) {
                if is_digits(n, 3, 5) {
                    scored.push((0, format!("HEYZO-{n}")));
                    break;
                }
            }
        }

        // ABC-123（分隔符被切成的两个 token）
        if is_alpha_token(token) {
            if let Some(next) = tokens.get(i + 1) {
                if is_digits(next, 2, 6) {
                    if let Some(code) = build_code(token, next) {
                        scored.push((1, code));
                    }
                }
                // ABC-601B / ABC-777ch / ABC-023v：序号后紧跟字母标记，取前导数字
                if let Some(digits) = leading_digits(next) {
                    if let Some(code) = build_code(token, digits) {
                        scored.push((1, code));
                    }
                }
                // MKBD-S94：序号带 1–2 位字母前缀（少见写法，排在后面兜底）
                if let Some((_, digits)) = split_short_prefix(next) {
                    if build_code(token, digits).is_some() {
                        scored.push((
                            3,
                            format!(
                                "{}-{}",
                                token.to_ascii_uppercase(),
                                next.to_ascii_uppercase()
                            ),
                        ));
                    }
                }
            }
        }

        // 单 token：SSIS001
        if let Some((alpha, digits)) = split_alpha_digits(token) {
            if let Some(code) = build_code(alpha, digits) {
                scored.push((2, code));
            }
        }

        // ABC-123-X 版本后缀
        if is_alpha_token(token) {
            if let (Some(mid), Some(suffix)) = (tokens.get(i + 1), tokens.get(i + 2)) {
                if is_digits(mid, 2, 6) && CODE_SUFFIXES.iter().any(|s| s.eq_ignore_ascii_case(suffix)) {
                    if let Some(code) = build_code(token, mid) {
                        scored.push((1, code));
                    }
                }
            }
        }
    }

    scored.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<String> = Vec::new();
    for (_, code) in scored {
        if !out.contains(&code) {
            out.push(code);
        }
    }
    out
}

/// 日期式番号：`MMDDYY_序号` 或 `MMDDYY-序号`（如 092415_159 / 121914-760）。
///
/// 站点上这类条目的番号就是原样字符串，因此保留原分隔符，方便搜索命中后按同号排序。
fn date_codes(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // 日期恰好 6 位；分隔符后面是 2–4 位序号
        if i - start != 6 || i + 1 >= bytes.len() {
            continue;
        }
        let sep = bytes[i];
        if sep != b'_' && sep != b'-' {
            continue;
        }
        let date = &text[start..i];
        if !is_month_day(date) {
            continue;
        }
        let digits_start = i + 1;
        let mut j = digits_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if (2..=4).contains(&(j - digits_start)) {
            out.push(format!("{}{}{}", date, sep as char, &text[digits_start..j]));
        }
        i = j;
    }
    // 同一串里若出现多次，保留首次
    out.dedup();
    out
}

/// `MMDD` 合法性：月份 01–12、日期 01–31（避免把任意 6 位数当番号）。
fn is_month_day(date: &str) -> bool {
    let month = date[0..2].parse::<u32>().ok();
    let day = date[2..4].parse::<u32>().ok();
    matches!(month, Some(m) if (1..=12).contains(&m))
        && matches!(day, Some(d) if (1..=31).contains(&d))
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn is_alpha_token(token: &str) -> bool {
    (2..=6).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_alphabetic())
        && !NOISE_PREFIXES.contains(&token.to_ascii_lowercase().as_str())
}

fn is_digits(token: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&token.len())
        && token.chars().all(|c| c.is_ascii_digit())
        && !is_noise_number(token)
}

fn is_noise_number(token: &str) -> bool {
    if NOISE_NUMBERS.contains(&token.to_ascii_lowercase().as_str()) {
        return true;
    }
    // 1900–2100 视为年份，不是番号序号
    matches!(token.parse::<i32>(), Ok(y) if (1900..=2100).contains(&y))
}

/// `SSIS001` → `("SSIS", "001")`；纯字母或纯数字返回 `None`。
fn split_alpha_digits(token: &str) -> Option<(&str, &str)> {
    let bytes = token.as_bytes();
    let split = bytes.iter().position(|b| b.is_ascii_digit())?;
    if split == 0 || split == bytes.len() {
        return None;
    }
    let (alpha, digits) = token.split_at(split);
    if !alpha.chars().all(|c| c.is_ascii_alphabetic())
        || !digits.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some((alpha, digits))
}

/// `601B` / `777ch` / `023v` → 前导数字；`1080p` 这类分辨率返回 `None`。
fn leading_digits(token: &str) -> Option<&str> {
    let split = token.as_bytes().iter().position(|b| !b.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (digits, rest) = token.split_at(split);
    // 带 p 的是分辨率（480p / 1080p），不是番号序号
    if rest.eq_ignore_ascii_case("p") {
        return None;
    }
    Some(digits)
}

/// `S94` / `S03` → `("S", "94")`；前缀超过 2 位或不是纯数字结尾返回 `None`。
fn split_short_prefix(token: &str) -> Option<(&str, &str)> {
    let split = token.as_bytes().iter().position(|b| b.is_ascii_digit())?;
    if split == 0 || split > 2 {
        return None;
    }
    let (alpha, digits) = token.split_at(split);
    if !alpha.chars().all(|c| c.is_ascii_alphabetic())
        || !digits.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    Some((alpha, digits))
}

fn build_code(alpha: &str, digits: &str) -> Option<String> {
    if !is_alpha_token(alpha) || !is_digits(digits, 2, 6) {
        return None;
    }
    Some(format!("{}-{}", alpha.to_ascii_uppercase(), digits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_names() {
        assert_eq!(canonical_code("SSIS-001.mp4"), Some("SSIS-001".into()));
        assert_eq!(canonical_code("ssis_001 1080p"), Some("SSIS-001".into()));
        assert_eq!(canonical_code("SSIS001"), Some("SSIS-001".into()));
        assert_eq!(canonical_code("[javdb] ABC-123-C.mkv"), Some("ABC-123".into()));
        assert_eq!(
            canonical_code("FC2-PPV-1234567.mp4"),
            Some("FC2-PPV-1234567".into())
        );
    }

    #[test]
    fn ignores_noise_and_years() {
        assert_eq!(canonical_code("Blade.Runner.1982.1080p.mkv"), None);
        assert_eq!(canonical_code("hhd800.com@SSIS-001.mp4"), Some("SSIS-001".into()));
        assert_eq!(canonical_code("movie_2024_2160p"), None);
        assert_eq!(canonical_code("随便起的名字"), None);
    }

    #[test]
    fn candidates_prefer_file_name_and_keep_order() {
        let list = candidates(Some("D:/media/SSIS-001.mp4"), "SSIS-001 标题");
        assert_eq!(list, vec!["SSIS-001".to_string()]);

        let list = candidates(Some("D:/media/无番号.mp4"), "ABC-123");
        assert_eq!(list, vec!["ABC-123".to_string()]);
    }

    #[test]
    fn same_code_ignores_separators() {
        assert!(same_code("SSIS-001", "ssis001"));
        assert!(same_code("SSIS-001", "SSIS-001-C"));
        assert!(!same_code("SSIS-001", "SSIS-002"));
    }

    #[test]
    fn recognizes_suffix_and_attached_markers() {
        assert_eq!(canonical_code("ABP-601B-C"), Some("ABP-601".into()));
        assert_eq!(canonical_code("ABP-601A-C"), Some("ABP-601".into()));
        assert_eq!(canonical_code("DASS-777ch"), Some("DASS-777".into()));
        assert_eq!(canonical_code("CAWD-404C"), Some("CAWD-404".into()));
        assert_eq!(canonical_code("abf-029ch"), Some("ABF-029".into()));
        assert_eq!(canonical_code("start-023v"), Some("START-023".into()));
        assert_eq!(canonical_code("ipx-540-C"), Some("IPX-540".into()));
        assert_eq!(canonical_code("SONE-576-UC"), Some("SONE-576".into()));
        assert_eq!(canonical_code("CWPBD-120"), Some("CWPBD-120".into()));
        assert_eq!(canonical_code("CWP-120"), Some("CWP-120".into()));
        assert_eq!(
            canonical_code("FC2PPV-1204746"),
            Some("FC2-PPV-1204746".into())
        );
        assert_eq!(
            canonical_code("heyzo_hd_0783_full"),
            Some("HEYZO-0783".into())
        );
        assert_eq!(canonical_code("MKBD-S94"), Some("MKBD-S94".into()));
    }

    #[test]
    fn resolution_does_not_become_code() {
        assert_eq!(canonical_code("Some.Movie.1080p.mkv"), None);
        assert_eq!(canonical_code("Some.Movie.480p.mkv"), None);
    }

    #[test]
    fn recognizes_date_style_codes() {
        assert_eq!(
            canonical_code("1pondo-092415_159-FHD"),
            Some("092415_159".into())
        );
        assert_eq!(
            canonical_code("010115_001-1pon-whole1_hd"),
            Some("010115_001".into())
        );
        assert_eq!(canonical_code("121914-760-C"), Some("121914-760".into()));
        assert_eq!(canonical_code("052716-172-C"), Some("052716-172".into()));
        // 非日期（月份非法）不当作番号
        assert_eq!(canonical_code("123456-789"), None);
    }
}
