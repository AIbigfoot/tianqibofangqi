//! 标签派生规则与维度注册（PRD FR-03「标签体系」）。
//!
//! 分工（对应硬约束 C6）：
//! - **维度取值方式**声明在 [`crate::ports::tag_dimension::TagDimensionDef`]，属配置；
//! - **派生维度的取值规则**以「注册的派生器」声明（[`DeriverRegistry`]），属配置；
//! - **检索组合逻辑**只有一份（[`crate::rules::matches_filters`]），作用于全部维度。
//!
//! 因此新增一个维度只需增加一条定义（+ 必要时一个派生器），检索代码一行不动。
//!
//! 计算时机（架构文档 §5）：派生维度的**唯一计算点**是 `probe()`——写入画质 /
//! 时长区间 / 字幕状态；「来源」「观看状态」分别在入库与进度变化时归集。

use crate::model::{SourceKind, StreamFacts};
use crate::ports::tag_dimension::TagDimensionDef;

// ---------------------------------------------------------------------------
// 维度 id 常量（与 tag_dimension.id 一一对应）
// ---------------------------------------------------------------------------

pub const DIM_GENRE: &str = "genre";
pub const DIM_REGION: &str = "region";
pub const DIM_YEAR: &str = "year";
pub const DIM_DURATION: &str = "duration";
pub const DIM_QUALITY: &str = "quality";
pub const DIM_SOURCE: &str = "source";
pub const DIM_WATCH: &str = "watch";
pub const DIM_SUBTITLE: &str = "subtitle";
pub const DIM_CUSTOM: &str = "custom";

/// JavDB 刮削维度：类别 / 系列 / 片商 / 导演 / 演员。
///
/// 这些取值完全由元数据源提供，复用通用标签体系，因此检索、筛选与导出无需改动。
pub const DIM_CATEGORY: &str = "category";
pub const DIM_SERIES: &str = "series";
pub const DIM_MAKER: &str = "maker";
pub const DIM_DIRECTOR: &str = "director";
pub const DIM_ACTOR: &str = "actor";

// ---------------------------------------------------------------------------
// 派生规则标识（tag_dimension.derive_rule）
// ---------------------------------------------------------------------------

pub const RULE_DURATION_BUCKET: &str = "duration_bucket";
pub const RULE_RESOLUTION_TIER: &str = "resolution_tier";
pub const RULE_SOURCE_SCOPE: &str = "source_scope";
pub const RULE_WATCH_STATE: &str = "watch_state";
pub const RULE_SUBTITLE_STATE: &str = "subtitle_state";

// ---------------------------------------------------------------------------
// 取值（PRD FR-03 维度取值示例）
// ---------------------------------------------------------------------------

/// 时长区间阈值：短片（< 40 分钟）、正片（40–150 分钟）、长片（> 150 分钟）。
pub const SHORT_MAX_MS: i64 = 40 * 60 * 1000;
pub const FEATURE_MAX_MS: i64 = 150 * 60 * 1000;
pub const DURATION_SHORT: &str = "短片";
pub const DURATION_FEATURE: &str = "正片";
pub const DURATION_LONG: &str = "长片";

/// 画质与规格。`HDR` / `高码率` 需要 `probe` 给出 HDR 标记与码率，
/// 当前 [`StreamFacts`] 尚未提供这两项事实，因此不在这里编造。
pub const QUALITY_4K: &str = "4K";
pub const QUALITY_1080P: &str = "1080P";
pub const QUALITY_720P: &str = "720P";

/// 来源。
pub const SOURCE_LOCAL: &str = "本地";
pub const SOURCE_NETWORK: &str = "网络";

/// 观看状态。
pub const WATCH_UNSEEN: &str = "未看";
pub const WATCH_WATCHING: &str = "在看";
pub const WATCH_WATCHED: &str = "已看";

/// 字幕状态（PRD FR-03 的四个取值）。
///
/// `有内嵌字幕` 由 `probe` 的 [`StreamFacts`] 判定；`有外挂字幕` / `已生成实时字幕`
/// 由字幕轨道事实判定（M3 补齐），因此字幕状态是「流事实 + 轨道事实」的共同结果。
pub const SUBTITLE_EMBEDDED: &str = "有内嵌字幕";
pub const SUBTITLE_EXTERNAL: &str = "有外挂字幕";
pub const SUBTITLE_GENERATED: &str = "已生成实时字幕";
pub const SUBTITLE_NONE: &str = "无字幕";

// ---------------------------------------------------------------------------
// 纯取值函数
// ---------------------------------------------------------------------------

/// 时长区间归属。
pub fn duration_bucket(duration_ms: i64) -> &'static str {
    if duration_ms < SHORT_MAX_MS {
        DURATION_SHORT
    } else if duration_ms <= FEATURE_MAX_MS {
        DURATION_FEATURE
    } else {
        DURATION_LONG
    }
}

/// 画质归属。分辨率未知时不给出取值（不猜）。
pub fn resolution_tier(facts: &StreamFacts) -> Option<&'static str> {
    let width = facts.width?;
    let height = facts.height?;
    // 4K：至少 3840×2160；1080P：至少 1920×1080；720P：至少 1280×720
    if width >= 3840 || height >= 2160 {
        Some(QUALITY_4K)
    } else if width >= 1920 || height >= 1080 {
        Some(QUALITY_1080P)
    } else if width >= 1280 || height >= 720 {
        Some(QUALITY_720P)
    } else {
        None
    }
}

/// 字幕状态（仅流事实）：探测到内嵌字幕轨道即为「有内嵌字幕」，否则「无字幕」。
///
/// 需要把轨道事实（外挂 / 实时生成）一并计入时用 [`subtitle_state_with_tracks`]。
pub fn subtitle_state(facts: &StreamFacts) -> &'static str {
    subtitle_state_with_tracks(facts, false, false)
}

/// 字幕状态（流事实 + 轨道事实）。
///
/// 取值优先级与轨道优先级（PRD §11）一致：外挂 > 内嵌 > 实时生成 > 无。
/// 任一档命中即给出该档取值，保证「字幕状态」恰好一个取值。
pub fn subtitle_state_with_tracks(
    facts: &StreamFacts,
    has_external_track: bool,
    has_generated_track: bool,
) -> &'static str {
    if has_external_track {
        SUBTITLE_EXTERNAL
    } else if !facts.embedded_subtitles.is_empty() {
        SUBTITLE_EMBEDDED
    } else if has_generated_track {
        SUBTITLE_GENERATED
    } else {
        SUBTITLE_NONE
    }
}

/// 来源归属：本地目录 / 局域网 → 本地；直链 / 云盘 / 在线站点 → 网络。
pub fn source_scope(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::LocalFolder | SourceKind::LanPath => SOURCE_LOCAL,
        SourceKind::DirectUrl | SourceKind::CloudDrive | SourceKind::OnlineSite => SOURCE_NETWORK,
    }
}

/// 观看状态：达 95%（[`crate::rules::ResumePolicy::is_watched`]）→ 已看；
/// 有有效进度 → 在看；否则未看。
pub fn watch_state(watched: bool, has_progress: bool) -> &'static str {
    if watched {
        WATCH_WATCHED
    } else if has_progress {
        WATCH_WATCHING
    } else {
        WATCH_UNSEEN
    }
}

/// 年份维度的取值标签，如 `2015年`。
///
/// 只接受 1900–2100 的四位年份：超出范围的值视为脏数据，宁可不给取值也不猜。
pub fn year_label(year: i32) -> Option<String> {
    (1900..=2100).contains(&year).then(|| format!("{year}年"))
}

/// 从发行日期（`YYYY-MM-DD`）或年份数字取年份标签；日期优先。
///
/// 日期只取前四位，不解析月日——年份维度只关心年份本身。
pub fn year_label_from(release_date: Option<&str>, year: Option<i32>) -> Option<String> {
    let parsed = release_date
        .map(str::trim)
        .and_then(|date| date.get(0..4))
        .and_then(|prefix| prefix.parse::<i32>().ok());
    year_label(parsed.or(year)?)
}

// ---------------------------------------------------------------------------
// 标签清单（供检索区渲染）
// ---------------------------------------------------------------------------

/// 某维度下的一个取值及使用频次。PRD FR-03 异常表：标签过多时按使用频次排序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagUsage {
    pub tag_id: String,
    pub name: String,
    /// 该取值当前被多少条目使用。频次为 0 的取值（刚创建、尚未挂到条目）也保留。
    pub count: u64,
}

// ---------------------------------------------------------------------------
// 派生器注册表
// ---------------------------------------------------------------------------

/// 派生器输入：只包含**事实**，不含 IO。派生维度只依赖这些事实。
#[derive(Debug, Clone, Copy, Default)]
pub struct DeriveFacts<'a> {
    /// `probe` 得到的事实；未探测时为空。
    pub stream: Option<&'a StreamFacts>,
    /// 条目所属源的类型与名称。
    pub source_kind: Option<SourceKind>,
    pub source_name: Option<&'a str>,
    /// 进度事实。
    pub watched: bool,
    pub has_progress: bool,
    /// 条目是否已挂外挂字幕轨道（`subtitle_track.kind = external`）。
    pub has_external_subtitle: bool,
    /// 条目是否已有实时生成字幕轨道（`subtitle_track.kind = generated`）。
    pub has_generated_subtitle: bool,
}

/// 派生器：把事实映射为该维度的取值集合。
pub type DeriveFn = fn(&DeriveFacts<'_>) -> Vec<String>;

/// 派生器注册表。`derive_rule` 即键；未注册的规则返回 `None`（该维度取值为空）。
pub trait DeriverRegistry: Send + Sync {
    fn get(&self, rule: &str) -> Option<DeriveFn>;
}

/// 内置派生器：覆盖 FR-03 的五个「系统自动」维度。
pub struct BuiltinDerivers;

fn derive_duration(facts: &DeriveFacts<'_>) -> Vec<String> {
    facts
        .stream
        .and_then(|s| s.duration_ms)
        .map(|ms| vec![duration_bucket(ms).to_owned()])
        .unwrap_or_default()
}

fn derive_resolution(facts: &DeriveFacts<'_>) -> Vec<String> {
    facts
        .stream
        .and_then(resolution_tier)
        .map(|tier| vec![tier.to_owned()])
        .unwrap_or_default()
}

fn derive_subtitle(facts: &DeriveFacts<'_>) -> Vec<String> {
    // 字幕状态 = 流事实（内嵌）+ 轨道事实（外挂 / 实时生成）。
    // 有流事实时用完整判定；无流事实但已知轨道事实时，仍按轨道事实给出取值
    // （此时可能漏判「内嵌」，但绝不漏掉外挂 / 已生成这两种已确知的事实）。
    match (facts.stream, facts.has_external_subtitle || facts.has_generated_subtitle) {
        (Some(stream), _) => vec![subtitle_state_with_tracks(
            stream,
            facts.has_external_subtitle,
            facts.has_generated_subtitle,
        )
        .to_owned()],
        (None, true) => vec![subtitle_state_with_tracks(
            &StreamFacts::default(),
            facts.has_external_subtitle,
            facts.has_generated_subtitle,
        )
        .to_owned()],
        (None, false) => Vec::new(),
    }
}

fn derive_source(facts: &DeriveFacts<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(kind) = facts.source_kind {
        out.push(source_scope(kind).to_owned());
    }
    if let Some(name) = facts.source_name.filter(|n| !n.trim().is_empty()) {
        out.push(name.to_owned());
    }
    out
}

fn derive_watch(facts: &DeriveFacts<'_>) -> Vec<String> {
    vec![watch_state(facts.watched, facts.has_progress).to_owned()]
}

impl DeriverRegistry for BuiltinDerivers {
    fn get(&self, rule: &str) -> Option<DeriveFn> {
        match rule {
            RULE_DURATION_BUCKET => Some(derive_duration),
            RULE_RESOLUTION_TIER => Some(derive_resolution),
            RULE_SOURCE_SCOPE => Some(derive_source),
            RULE_WATCH_STATE => Some(derive_watch),
            RULE_SUBTITLE_STATE => Some(derive_subtitle),
            _ => None,
        }
    }
}

/// 按维度定义求值：非 `derived` 维度、或规则未注册时返回空集合。
pub fn derive_values(
    def: &TagDimensionDef,
    registry: &dyn DeriverRegistry,
    facts: &DeriveFacts<'_>,
) -> Vec<String> {
    let Some(rule) = def.derive_rule.as_deref() else {
        return Vec::new();
    };
    match registry.get(rule) {
        Some(derive) => derive(facts),
        None => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// 单元测试（L2 领域单测）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EmbeddedSubtitle, StreamFacts};

    fn facts(width: Option<u32>, height: Option<u32>) -> StreamFacts {
        StreamFacts {
            width,
            height,
            ..Default::default()
        }
    }

    #[test]
    fn builtin_defaults_cover_fr03_dimensions() {
        let defs = TagDimensionDef::builtin_defaults();
        assert_eq!(defs.len(), 14, "FR-03 九个维度 + JavDB 五个刮削维度");

        let mut ids: Vec<&str> = defs.iter().map(|d| d.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 14, "维度 id 不得重复");

        // 序必须严格升序，保证界面顺序稳定
        for pair in defs.windows(2) {
            assert!(pair[0].order < pair[1].order, "order 必须严格升序");
        }

        // 派生维度必须声明注册的派生器，且不可手动编辑（PRD「系统自动」）
        for def in defs.iter().filter(|d| d.source == crate::ports::tag_dimension::TagSource::Derived) {
            assert!(def.derive_rule.is_some(), "派生维度 {} 必须声明 derive_rule", def.id);
            assert!(!def.editable, "派生维度 {} 不可手动编辑", def.id);
        }
    }

    #[test]
    fn duration_bucket_boundaries_match_prd() {
        assert_eq!(duration_bucket(39 * 60 * 1000), DURATION_SHORT);
        assert_eq!(duration_bucket(SHORT_MAX_MS), DURATION_FEATURE, "40 分钟属于正片");
        assert_eq!(duration_bucket(FEATURE_MAX_MS), DURATION_FEATURE, "150 分钟属于正片");
        assert_eq!(duration_bucket(FEATURE_MAX_MS + 1), DURATION_LONG);
    }

    #[test]
    fn resolution_tier_reads_only_known_resolutions() {
        assert_eq!(resolution_tier(&facts(Some(3840), Some(2160))), Some(QUALITY_4K));
        assert_eq!(resolution_tier(&facts(Some(1920), Some(1080))), Some(QUALITY_1080P));
        assert_eq!(resolution_tier(&facts(Some(1280), Some(720))), Some(QUALITY_720P));
        assert_eq!(resolution_tier(&facts(Some(640), Some(360))), None, "低于 720P 不给取值");
        assert_eq!(resolution_tier(&facts(None, None)), None, "分辨率未知不得猜测");
    }

    #[test]
    fn subtitle_state_depends_on_embedded_tracks() {
        let mut with_sub = facts(Some(1920), Some(1080));
        with_sub.embedded_subtitles.push(EmbeddedSubtitle {
            index: 0,
            lang: Some("chi".into()),
            title: None,
        });
        assert_eq!(subtitle_state(&with_sub), SUBTITLE_EMBEDDED);
        assert_eq!(subtitle_state(&facts(None, None)), SUBTITLE_NONE);
    }

    #[test]
    fn subtitle_state_combines_stream_and_track_facts() {
        let mut with_sub = facts(Some(1920), Some(1080));
        with_sub.embedded_subtitles.push(EmbeddedSubtitle {
            index: 0,
            lang: Some("chi".into()),
            title: None,
        });

        // 优先级：外挂 > 内嵌 > 实时生成 > 无
        assert_eq!(
            subtitle_state_with_tracks(&with_sub, true, true),
            SUBTITLE_EXTERNAL
        );
        assert_eq!(
            subtitle_state_with_tracks(&with_sub, false, true),
            SUBTITLE_EMBEDDED
        );
        assert_eq!(
            subtitle_state_with_tracks(&facts(None, None), false, true),
            SUBTITLE_GENERATED
        );
        assert_eq!(
            subtitle_state_with_tracks(&facts(None, None), false, false),
            SUBTITLE_NONE
        );
    }

    #[test]
    fn derive_subtitle_reads_track_facts_without_stream() {
        let registry = BuiltinDerivers;
        let subtitle_def = TagDimensionDef::builtin_defaults()
            .into_iter()
            .find(|d| d.id == DIM_SUBTITLE)
            .expect("字幕维度存在");

        // 无流事实但已知外挂轨道 → 仍给出「有外挂字幕」
        let external_only = DeriveFacts {
            has_external_subtitle: true,
            ..DeriveFacts::default()
        };
        assert_eq!(
            derive_values(&subtitle_def, &registry, &external_only),
            vec![SUBTITLE_EXTERNAL]
        );

        let generated_only = DeriveFacts {
            has_generated_subtitle: true,
            ..DeriveFacts::default()
        };
        assert_eq!(
            derive_values(&subtitle_def, &registry, &generated_only),
            vec![SUBTITLE_GENERATED]
        );

        // 既无流事实也无轨道事实 → 不产出取值（不猜测）
        assert!(derive_values(&subtitle_def, &registry, &DeriveFacts::default()).is_empty());
    }

    #[test]
    fn builtin_derivers_produce_values_for_probe_facts() {
        let registry = BuiltinDerivers;
        let defs = TagDimensionDef::builtin_defaults();
        let by_id = |id: &str| defs.iter().find(|d| d.id == id).expect("维度存在").clone();

        let mut stream = facts(Some(3840), Some(2160));
        stream.duration_ms = Some(100 * 60 * 1000);
        let input = DeriveFacts {
            stream: Some(&stream),
            source_kind: Some(SourceKind::LocalFolder),
            source_name: Some("我的电影"),
            watched: false,
            has_progress: true,
            ..DeriveFacts::default()
        };

        assert_eq!(derive_values(&by_id(DIM_DURATION), &registry, &input), vec![DURATION_FEATURE]);
        assert_eq!(derive_values(&by_id(DIM_QUALITY), &registry, &input), vec![QUALITY_4K]);
        assert_eq!(derive_values(&by_id(DIM_SUBTITLE), &registry, &input), vec![SUBTITLE_NONE]);
        assert_eq!(
            derive_values(&by_id(DIM_SOURCE), &registry, &input),
            vec![SOURCE_LOCAL.to_owned(), "我的电影".to_owned()]
        );
        assert_eq!(derive_values(&by_id(DIM_WATCH), &registry, &input), vec![WATCH_WATCHING]);
    }

    #[test]
    fn unregistered_rule_yields_no_values() {
        let registry = BuiltinDerivers;
        let mut def = TagDimensionDef::builtin_defaults()[3].clone(); // duration
        def.derive_rule = Some("不存在_的_规则".into());
        assert!(derive_values(&def, &registry, &DeriveFacts::default()).is_empty());

        // 非派生维度（无 derive_rule）同样不产出取值
        let metadata_dim = TagDimensionDef::builtin_defaults()[0].clone();
        assert!(derive_values(&metadata_dim, &registry, &DeriveFacts::default()).is_empty());
    }

    #[test]
    fn watch_state_transitions_follow_watched_flag() {
        assert_eq!(watch_state(true, true), WATCH_WATCHED);
        assert_eq!(watch_state(false, true), WATCH_WATCHING);
        assert_eq!(watch_state(false, false), WATCH_UNSEEN);
    }

    #[test]
    fn year_label_reads_release_date_or_year() {
        assert_eq!(year_label(2015).as_deref(), Some("2015年"));
        assert_eq!(year_label(1800), None, "超出范围的年份不给取值");
        assert_eq!(
            year_label_from(Some("2016-04-01"), Some(2015)).as_deref(),
            Some("2016年"),
            "日期优先于年份数字"
        );
        assert_eq!(
            year_label_from(None, Some(2015)).as_deref(),
            Some("2015年")
        );
        assert_eq!(year_label_from(None, None), None);
        assert_eq!(year_label_from(Some("不详"), Some(2015)).as_deref(), Some("2015年"));
    }
}
