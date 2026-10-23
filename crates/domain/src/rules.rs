//! L2 纯规则：不依赖 IO，回归风险最高的四类语义。
//!
//! 这些规则是架构文档 §9「测试布局」中标注的 L2 领域单测对象：
//! 检索组合语义、续播规则、轨道优先级、音频焦点移交。放在领域层的价值是
//! 任何层级（L1/L3/L4）都必须调用同一份实现，规则不会因换个界面而漂移。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Metadata, Progress, SubtitleKind};

// ---------------------------------------------------------------------------
// 用户锁定字段（P-02「不覆盖用户数据」）
// ---------------------------------------------------------------------------

/// 可被用户锁定、从而拒绝自动写入的字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LockedField {
    /// 标题（PRD FR-02「修改后不再被元数据抓取覆盖」）。
    Title,
    /// 海报（上传后立即生效并锁定）。
    Poster,
    /// 标签（用户锁定后不再被任何自动流程移除）。
    Tags,
}

/// 条目上的锁定字段集合。
///
/// 任何元数据提供者写入前都必须经过 [`FieldLock::filter`]，
/// 被锁定的字段会被置空，从而实现「不覆盖用户数据」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldLock {
    locked: BTreeSet<LockedField>,
}

impl FieldLock {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从持久化的 JSON 字段锁恢复。M0 采用逗号分隔的最小形式，
    /// 具体编码由 L4 决定，不改变本规则语义。
    pub fn from_csv(spec: &str) -> Self {
        let mut lock = Self::new();
        for part in spec.split(',') {
            match part.trim() {
                "title" => lock.lock(LockedField::Title),
                "poster" => lock.lock(LockedField::Poster),
                "tags" => lock.lock(LockedField::Tags),
                _ => {}
            }
        }
        lock
    }

    pub fn lock(&mut self, field: LockedField) {
        self.locked.insert(field);
    }

    pub fn is_locked(&self, field: LockedField) -> bool {
        self.locked.contains(&field)
    }

    /// 过滤即将写入的元数据：锁定字段被跳过（置空 / 清空），其余字段保留。
    pub fn filter(&self, incoming: &mut Metadata) {
        if self.is_locked(LockedField::Title) {
            incoming.title = None;
        }
        if self.is_locked(LockedField::Poster) {
            incoming.poster_url = None;
        }
        if self.is_locked(LockedField::Tags) {
            incoming.tags.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// 音频焦点（PRD §11「音频焦点（多屏）」）
// ---------------------------------------------------------------------------

/// 单声道输出模式下的发声窗口决策。
///
/// 规则：仅一路发声；发声窗口默认是进入多屏时的第一路；用户切换后保持；
/// 发声窗口被关闭时自动移交给当前仍在播放的窗口（按屏位顺序）。
/// **预览实例从不进入本结构**——预览强制静音，不参与焦点决策。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioFocus {
    speaker: Option<u32>,
}

impl AudioFocus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn speaker(&self) -> Option<u32> {
        self.speaker
    }

    /// 进入多屏时指定发声窗口；若尚未指定，则以首个屏位为默认。
    pub fn ensure_speaker(&mut self, slot: u32) -> u32 {
        if let Some(current) = self.speaker {
            current
        } else {
            self.speaker = Some(slot);
            slot
        }
    }

    /// 用户切换发声窗口，返回旧发声者。
    pub fn set_speaker(&mut self, slot: u32) -> Option<u32> {
        let old = self.speaker;
        self.speaker = Some(slot);
        old
    }

    /// 发声窗口关闭：按屏位顺序移交给定候选中的第一个；无候选则清空。
    pub fn on_speaker_closed(&mut self, remaining_in_slot_order: &[u32]) -> Option<u32> {
        let next = remaining_in_slot_order.first().copied();
        self.speaker = next;
        next
    }
}

// ---------------------------------------------------------------------------
// 多屏布局与退出策略（PRD FR-06）
// ---------------------------------------------------------------------------

/// 多屏布局（PRD FR-06「布局」表）。
///
/// 2 × 3 六路不在布局表内（由 3 × 3 九宫格覆盖 9 路需求）；
/// 同时播放上限默认仍为 4 路（由 L1 的解码额度给出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// 左右各一路，适合对比观看（默认）。
    #[default]
    OneByTwo,
    /// 上下各一路，适合宽屏与竖屏混合。
    TwoByOne,
    /// 四宫格。
    TwoByTwo,
    /// 左侧主画面更大，右侧三路小窗。
    OnePlusThree,
    /// 九宫格，最多同时 9 路。
    ThreeByThree,
}

impl Layout {
    /// 本阶段可用布局，顺序与 PRD 表格一致。
    pub const ALL: [Layout; 5] = [
        Layout::OneByTwo,
        Layout::TwoByOne,
        Layout::TwoByTwo,
        Layout::OnePlusThree,
        Layout::ThreeByThree,
    ];

    /// 格位数。多屏路数上限由设备能力决定（L1 的额度），布局只决定格位数。
    pub fn slots(self) -> u32 {
        match self {
            Layout::OneByTwo | Layout::TwoByOne => 2,
            Layout::TwoByTwo | Layout::OnePlusThree => 4,
            Layout::ThreeByThree => 9,
        }
    }

    /// 界面标记，与前端布局按钮的 `data-layout` 一致。
    pub fn tag(self) -> &'static str {
        match self {
            Layout::OneByTwo => "1x2",
            Layout::TwoByOne => "2x1",
            Layout::TwoByTwo => "2x2",
            Layout::OnePlusThree => "1plus3",
            Layout::ThreeByThree => "3x3",
        }
    }

    /// 界面标记 → 布局；未知回落默认（1 × 2），不猜测用户意图。
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "2x1" => Layout::TwoByOne,
            "2x2" => Layout::TwoByTwo,
            "1plus3" => Layout::OnePlusThree,
            "3x3" => Layout::ThreeByThree,
            _ => Layout::OneByTwo,
        }
    }

    /// 布局按钮文案。
    pub fn label(self) -> &'static str {
        match self {
            Layout::OneByTwo => "1 × 2",
            Layout::TwoByOne => "2 × 1",
            Layout::TwoByTwo => "2 × 2",
            Layout::OnePlusThree => "1 + 3",
            Layout::ThreeByThree => "3 × 3",
        }
    }
}

/// 退出多屏时的处理方式（PRD FR-06「退出」）。
///
/// 「保留播放进度并回到单路」时，**发声窗口**的进度被记忆（PRD §11 音频焦点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitPolicy {
    /// 保留播放进度并回到单路。
    KeepProgressAndReturn,
    /// 全部停止。
    StopAll,
}

impl ExitPolicy {
    pub fn tag(self) -> &'static str {
        match self {
            ExitPolicy::KeepProgressAndReturn => "keep_progress",
            ExitPolicy::StopAll => "stop_all",
        }
    }

    /// 界面标记 → 策略；未知返回 `None`（退出必须由用户明确选择，不设默认）。
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "keep_progress" => Some(ExitPolicy::KeepProgressAndReturn),
            "stop_all" => Some(ExitPolicy::StopAll),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ExitPolicy::KeepProgressAndReturn => "保留播放进度并回到单路",
            ExitPolicy::StopAll => "全部停止",
        }
    }
}

/// 多屏播放期间是否允许启动悬停预览（PRD FR-06 / 架构 §7）。
///
/// 口径：多屏播放期间**不提供**海报悬停预览 —— 预览与多屏共享同一份解码额度，
/// 且不得抢占正在播放的窗口。单路播放时是否预览由前端与额度另行决定，不在本规则内。
pub fn preview_allowed(multi_screen_active: bool) -> bool {
    !multi_screen_active
}

// ---------------------------------------------------------------------------
// 字幕轨道优先级（PRD §11，全文唯一权威定义）
// ---------------------------------------------------------------------------

/// 轨道来源，按优先级从高到低排列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrackSource {
    /// 实时生成 —— 最低优先级。
    Generated,
    /// 内嵌字幕轨道。
    Embedded,
    /// 外挂字幕文件。
    External,
    /// 用户手动指定（已锁定）。
    UserLocked,
}

impl TrackSource {
    /// 数值越大优先级越高。
    pub fn priority(self) -> u8 {
        match self {
            TrackSource::Generated => 0,
            TrackSource::Embedded => 1,
            TrackSource::External => 2,
            TrackSource::UserLocked => 3,
        }
    }

    pub fn from_subtitle_kind(kind: SubtitleKind) -> Self {
        match kind {
            SubtitleKind::Generated => TrackSource::Generated,
            SubtitleKind::Embedded => TrackSource::Embedded,
            SubtitleKind::External => TrackSource::External,
        }
    }
}

/// 从候选来源中挑默认轨道：优先级最高者。
/// 同优先级内部排序（语种偏好 / 最近选择）由 L1 负责，不在本函数内实现。
pub fn pick_default_track(candidates: &[TrackSource]) -> Option<TrackSource> {
    candidates.iter().copied().max_by_key(|s| s.priority())
}

// ---------------------------------------------------------------------------
// 检索组合语义（硬约束 C6：同维度 OR、跨维度 AND）
// ---------------------------------------------------------------------------

/// 维度筛选条件：`dimension_id -> 选中的取值`。
pub type FilterSelection = BTreeMap<String, Vec<String>>;

/// 条目的标签取值：`dimension_id -> 该条目具备的取值`。
pub type ItemTags = BTreeMap<String, Vec<String>>;

/// 检索组合：**同维度内 OR、跨维度 AND**。
///
/// 该逻辑只实现一次并作用于全部维度，因此新增维度天然获得正确语义。
/// 空选中的维度不参与约束（表示「不限」）。
pub fn matches_filters(selection: &FilterSelection, item: &ItemTags) -> bool {
    selection.iter().all(|(dimension, chosen)| {
        if chosen.is_empty() {
            return true;
        }
        match item.get(dimension) {
            Some(owned) => owned.iter().any(|v| chosen.contains(v)),
            None => false,
        }
    })
}

// ---------------------------------------------------------------------------
// 续播规则（PRD FR-04「进度记忆与续播」）
// ---------------------------------------------------------------------------

/// 开始记录进度所需的**最短位置**（PRD FR-04：播放超过 30 秒才开始记录）。
///
/// 文档写的是「播放超过 30 秒」，而领域层拿到的是播放位置而非累计播放时长；
/// 不拖动进度时二者等价，这里以**位置**作为判据。
pub const RECORD_MIN_POSITION_MS: i64 = 30_000;

/// 开始记录进度所需的**最短完成比例**（PRD FR-04：进度超过总时长 2%）。
pub const RECORD_MIN_RATIO: f64 = 0.02;

/// 视为「已看」的完成比例（PRD FR-04：进度达到总时长 95%）。
pub const WATCHED_RATIO: f64 = 0.95;

/// 「继续观看」提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeOffer {
    /// 续播起点。
    pub position_ms: i64,
    /// 完成百分比，用于「继续观看 62%」文案。
    pub percent: u32,
}

/// 续播决策：纯函数集合。L1 与 L4 都必须调用同一份实现，规则不因界面而漂移。
pub struct ResumePolicy;

impl ResumePolicy {
    /// 是否应记录该进度（PRD FR-04 第 1 条）。
    ///
    /// 总时长未知时不记录：无法判断 2%，也就无法判断「有效进度」。
    pub fn should_record(position_ms: i64, duration_ms: Option<i64>) -> bool {
        if position_ms <= RECORD_MIN_POSITION_MS {
            return false;
        }
        match duration_ms {
            Some(d) if d > 0 => position_ms as f64 > d as f64 * RECORD_MIN_RATIO,
            _ => false,
        }
    }

    /// 是否视为「已看」（PRD FR-04 第 3 条：达 95% 即视为已看，不再提示续播）。
    pub fn is_watched(position_ms: i64, duration_ms: Option<i64>) -> bool {
        match duration_ms {
            Some(d) if d > 0 => position_ms as f64 >= d as f64 * WATCHED_RATIO,
            _ => false,
        }
    }

    /// 打开条目时是否给出续播提示；返回起点与完成百分比。
    ///
    /// 与 PRD 一致：无进度、进度太靠前、已看，三种情况都不提示。
    pub fn resume_offer(
        progress: Option<Progress>,
        duration_ms: Option<i64>,
    ) -> Option<ResumeOffer> {
        let progress = progress?;
        let duration = duration_ms.or(progress.duration_ms)?;
        if duration <= 0 {
            return None;
        }
        if !Self::should_record(progress.position_ms, Some(duration)) {
            return None;
        }
        if Self::is_watched(progress.position_ms, Some(duration)) {
            return None;
        }
        let percent = (progress.position_ms as f64 / duration as f64 * 100.0).round() as u32;
        Some(ResumeOffer {
            position_ms: progress.position_ms,
            percent: percent.min(100),
        })
    }
}

// ---------------------------------------------------------------------------
// 悬停预览的产品常量（PRD FR-02「精华片段预览规则」）
// ---------------------------------------------------------------------------

/// 悬停停留多久后开始预览（PRD FR-02：300ms）。
pub const PREVIEW_TRIGGER_MS: u64 = 300;

/// 精华片段默认长度（PRD FR-02：默认抽取连续 30 秒）。
///
/// 注意：片段的**起始位置判定在 PRD 中标为 `[TBD]`**。因此 M1 不编造算法，
/// 一律从片头开始 —— 这与 P-04 `load` 的「预览实例起点恒为 `None`（从头播放）」
/// 完全一致。待 PRD 定稿基线后，只需替换本常量的用法，不动调用方。
pub const PREVIEW_SEGMENT_MS: i64 = 30_000;

// ---------------------------------------------------------------------------
// 结果排序（PRD FR-02「检索区」的排序切换）
// ---------------------------------------------------------------------------

/// 排序模式。顺序与 PRD FR-02 列出的一致：最近添加 / 标题 / 年份 / 评分 / 时长 / 最近观看。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// 最近添加（新→旧）。
    #[default]
    RecentAdded,
    /// 标题（字典序升序）。
    Title,
    /// 年份（新→旧）。
    Year,
    /// 评分（高→低）。
    Rating,
    /// 时长（短→长）。
    Duration,
    /// 最近观看（近→远）。
    LastWatched,
    /// 随机打乱（每次查询重新洗牌，由检索服务处理，不走比较器）。
    Random,
}

impl SortMode {
    /// 从界面传入的标记解析；未知取值回落到默认（最近添加），不猜测。
    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "title" => SortMode::Title,
            "year" => SortMode::Year,
            "rating" => SortMode::Rating,
            "duration" => SortMode::Duration,
            "last_watched" => SortMode::LastWatched,
            "random" => SortMode::Random,
            _ => SortMode::RecentAdded,
        }
    }
}

/// 排序键。**数值缺失一律排到末尾**：无法比较的条目不应插到有序区间中间。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SortKey {
    /// 入库时间（`media_item.created_at`）。
    pub added_at: i64,
    pub title: String,
    pub year: Option<i32>,
    pub rating: Option<f32>,
    pub duration_ms: Option<i64>,
    /// 最近一次进度更新时间（`playback_state.updated_at`）；从未播放为 `None`。
    pub last_watched_at: Option<i64>,
}

/// 比较两个排序键。`title` 作为所有模式的稳定兜底，避免同值条目的顺序抖动。
pub fn compare_items(mode: SortMode, a: &SortKey, b: &SortKey) -> Ordering {
    let primary = match mode {
        SortMode::RecentAdded => b.added_at.cmp(&a.added_at),
        SortMode::Title => a.title.cmp(&b.title),
        SortMode::Year => desc_none_last(a.year, b.year),
        SortMode::Rating => desc_none_last(
            a.rating.map(f32::to_bits),
            b.rating.map(f32::to_bits),
        ),
        SortMode::Duration => asc_none_last(a.duration_ms, b.duration_ms),
        SortMode::LastWatched => desc_none_last(a.last_watched_at, b.last_watched_at),
        // 随机模式在检索服务里直接洗牌，比较器只作稳定兜底
        SortMode::Random => Ordering::Equal,
    };
    primary.then_with(|| a.title.cmp(&b.title))
}

/// 大→小，`None` 排末尾。
fn desc_none_last<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// 小→大，`None` 排末尾。
fn asc_none_last<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

// ---------------------------------------------------------------------------
// 导出包版本兼容（PRD FR-07「数据可迁移」）
// ---------------------------------------------------------------------------

/// 导入包版本与本机库版本的关系。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageCompatibility {
    /// 同版本：直接导入。
    Same,
    /// 包版本落后于本机：本机已前进式迁移，可直接导入（新列在旧包里缺省）。
    Older,
    /// 包版本高于本机：**不得静默改写**，必须提示用户先升级本机。
    Newer,
}

/// 判定一份导出包能否导入本机。
///
/// 前进式迁移只允许 `vN → vN+1` 且禁止跳版；因此包版本高于本机时唯一正确的
/// 处理是拒绝并提示，而不是按旧结构静默吞掉未知字段（架构文档 §8 迁移规则）。
pub fn package_compatibility(exported: i64, local: i64) -> PackageCompatibility {
    use std::cmp::Ordering;
    match exported.cmp(&local) {
        Ordering::Equal => PackageCompatibility::Same,
        Ordering::Less => PackageCompatibility::Older,
        Ordering::Greater => PackageCompatibility::Newer,
    }
}

// ---------------------------------------------------------------------------
// 单元测试（L2 领域单测）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_lock_skips_only_locked_fields() {
        let mut lock = FieldLock::new();
        lock.lock(LockedField::Title);

        let mut incoming = Metadata {
            title: Some("抓取到的标题".into()),
            year: Some(1999),
            overview: Some("简介".into()),
            poster_url: Some("http://example/p.jpg".into()),
            tags: vec!["动作".into()],
            ..Default::default()
        };
        lock.filter(&mut incoming);

        assert_eq!(incoming.title, None, "锁定标题后不得被覆盖");
        assert_eq!(incoming.year, Some(1999), "未锁定字段应保留");
        assert_eq!(incoming.poster_url.as_deref(), Some("http://example/p.jpg"));
        assert_eq!(incoming.tags, vec!["动作".to_owned()]);
    }

    #[test]
    fn field_lock_from_csv_roundtrip() {
        let lock = FieldLock::from_csv("title,tags");
        assert!(lock.is_locked(LockedField::Title));
        assert!(lock.is_locked(LockedField::Tags));
        assert!(!lock.is_locked(LockedField::Poster));
    }

    #[test]
    fn audio_focus_transfers_in_slot_order() {
        let mut focus = AudioFocus::new();
        assert_eq!(focus.ensure_speaker(2), 2, "首个进入多屏的窗口成为发声窗口");
        assert_eq!(focus.ensure_speaker(3), 2, "已指定则保持不变");

        assert_eq!(focus.set_speaker(4), Some(2), "用户切换返回旧发声者");
        assert_eq!(focus.on_speaker_closed(&[2, 3]), Some(2), "按屏位顺序移交");
        assert_eq!(focus.on_speaker_closed(&[]), None, "无候选则不再发声");
    }

    #[test]
    fn layout_slots_match_prd_table() {
        assert_eq!(Layout::OneByTwo.slots(), 2);
        assert_eq!(Layout::TwoByOne.slots(), 2);
        assert_eq!(Layout::TwoByTwo.slots(), 4);
        assert_eq!(Layout::OnePlusThree.slots(), 4);
        assert_eq!(Layout::default(), Layout::OneByTwo, "默认布局为 1 × 2");
    }

    #[test]
    fn layout_tag_roundtrip_falls_back_to_default() {
        for layout in Layout::ALL {
            assert_eq!(Layout::from_tag(layout.tag()), layout, "标记必须可往返");
        }
        assert_eq!(Layout::from_tag("2x3"), Layout::OneByTwo, "P1 的 2 × 3 不纳入，回落默认");
        assert_eq!(Layout::from_tag("未知"), Layout::OneByTwo);
    }

    #[test]
    fn exit_policy_tag_roundtrip_requires_explicit_choice() {
        assert_eq!(
            ExitPolicy::from_tag(ExitPolicy::KeepProgressAndReturn.tag()),
            Some(ExitPolicy::KeepProgressAndReturn)
        );
        assert_eq!(
            ExitPolicy::from_tag(ExitPolicy::StopAll.tag()),
            Some(ExitPolicy::StopAll)
        );
        assert_eq!(ExitPolicy::from_tag("未知"), None, "未知标记不得猜测");
    }

    #[test]
    fn preview_is_withheld_while_multi_screen_plays() {
        assert!(preview_allowed(false), "单路播放时不由本规则拒绝");
        assert!(!preview_allowed(true), "多屏播放期间不提供悬停预览");
    }

    #[test]
    fn track_priority_prefers_user_lock_then_external() {
        let all = [
            TrackSource::Generated,
            TrackSource::Embedded,
            TrackSource::External,
            TrackSource::UserLocked,
        ];
        assert_eq!(pick_default_track(&all), Some(TrackSource::UserLocked));

        let no_user = [TrackSource::Generated, TrackSource::Embedded, TrackSource::External];
        assert_eq!(pick_default_track(&no_user), Some(TrackSource::External));
        assert_eq!(pick_default_track(&[]), None);
    }

    #[test]
    fn search_combines_or_within_dimension_and_across_dimensions() {
        let mut selection = FilterSelection::new();
        selection.insert("genre".into(), vec!["动作".into(), "科幻".into()]); // OR
        selection.insert("resolution".into(), vec!["4K".into()]); // AND

        let mut item = ItemTags::new();
        item.insert("genre".into(), vec!["科幻".into()]);
        item.insert("resolution".into(), vec!["4K".into()]);
        assert!(matches_filters(&selection, &item), "同维度命中其一即可");

        item.insert("resolution".into(), vec!["1080P".into()]);
        assert!(!matches_filters(&selection, &item), "跨维度必须同时满足");

        let mut unlimited = FilterSelection::new();
        unlimited.insert("genre".into(), vec![]);
        assert!(matches_filters(&unlimited, &ItemTags::new()), "空选中表示不限");
    }

    #[test]
    fn resume_starts_recording_only_past_thirty_seconds_and_two_percent() {
        assert!(
            !ResumePolicy::should_record(20_000, Some(600_000)),
            "未满 30 秒不得开始记录"
        );
        assert!(
            ResumePolicy::should_record(31_000, Some(600_000)),
            "满 30 秒且超过 2% 应开始记录"
        );
        assert!(
            !ResumePolicy::should_record(31_000, None),
            "总时长未知时无法判断 2%，不得记录"
        );
    }

    #[test]
    fn resume_offer_is_suppressed_when_early_or_watched() {
        let early = Progress {
            position_ms: 20_000,
            duration_ms: Some(600_000),
        };
        assert!(ResumePolicy::resume_offer(Some(early), Some(600_000)).is_none());

        let mid = Progress {
            position_ms: 360_000,
            duration_ms: Some(600_000),
        };
        let offer = ResumePolicy::resume_offer(Some(mid), Some(600_000)).expect("应提示续播");
        assert_eq!(offer.position_ms, 360_000);
        assert_eq!(offer.percent, 60, "360s / 600s = 60%");

        // 570s / 600s = 95%，视为已看
        assert!(ResumePolicy::is_watched(570_000, Some(600_000)));
        assert!(
            ResumePolicy::resume_offer(
                Some(Progress {
                    position_ms: 570_000,
                    duration_ms: Some(600_000)
                }),
                Some(600_000)
            )
            .is_none(),
            "已看不提示续播"
        );

        assert!(ResumePolicy::resume_offer(None, Some(600_000)).is_none(), "无进度不提示");
    }

    fn key(title: &str, added: i64) -> SortKey {
        SortKey {
            added_at: added,
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn recent_added_sorts_newest_first() {
        let old = key("旧片", 100);
        let new = key("新片", 900);
        assert_eq!(compare_items(SortMode::RecentAdded, &new, &old), Ordering::Less);
    }

    #[test]
    fn title_sorts_ascending_and_breaks_ties_stably() {
        let a = key("A 片", 1);
        let b = key("B 片", 2);
        assert_eq!(compare_items(SortMode::Title, &a, &b), Ordering::Less);
        // 同一标题下由 title 自身比较，结果确定（不随输入顺序抖动）
        let same_a = key("同名", 1);
        let same_b = key("同名", 2);
        assert_eq!(compare_items(SortMode::Title, &same_a, &same_b), Ordering::Equal);
    }

    #[test]
    fn missing_numeric_values_sort_last() {
        let mut with_year = key("有年份", 1);
        with_year.year = Some(2000);
        let no_year = key("无年份", 2);
        assert_eq!(
            compare_items(SortMode::Year, &with_year, &no_year),
            Ordering::Less,
            "缺年份的条目必须排在有值的后面"
        );

        let mut rated = key("有评分", 1);
        rated.rating = Some(9.2);
        let unrated = key("无评分", 2);
        assert_eq!(compare_items(SortMode::Rating, &rated, &unrated), Ordering::Less);

        let mut timed = key("有时长", 1);
        timed.duration_ms = Some(60_000);
        let untimed = key("无时长", 2);
        assert_eq!(
            compare_items(SortMode::Duration, &timed, &untimed),
            Ordering::Less,
            "时长升序下，有时长的仍应在无时长之前"
        );
    }

    #[test]
    fn year_rating_desc_and_duration_asc() {
        let mut a = key("A", 1);
        a.year = Some(1999);
        a.rating = Some(7.0);
        a.duration_ms = Some(7_200_000);
        let mut b = key("B", 2);
        b.year = Some(2024);
        b.rating = Some(9.0);
        b.duration_ms = Some(5_400_000);

        assert_eq!(compare_items(SortMode::Year, &b, &a), Ordering::Less, "年份新→旧");
        assert_eq!(compare_items(SortMode::Rating, &b, &a), Ordering::Less, "评分高→低");
        assert_eq!(compare_items(SortMode::Duration, &b, &a), Ordering::Less, "时长短→长");
    }

    #[test]
    fn last_watched_sorts_recent_first_with_never_watched_last() {
        let mut seen = key("看过", 1);
        seen.last_watched_at = Some(1_000);
        let never = key("没看过", 2);
        assert_eq!(compare_items(SortMode::LastWatched, &seen, &never), Ordering::Less);
    }

    #[test]
    fn package_compatibility_rejects_newer_package() {
        assert_eq!(package_compatibility(2, 2), PackageCompatibility::Same);
        assert_eq!(package_compatibility(1, 2), PackageCompatibility::Older);
        assert_eq!(
            package_compatibility(3, 2),
            PackageCompatibility::Newer,
            "包版本高于本机时不得静默改写，必须提示"
        );
    }

    #[test]
    fn sort_mode_tag_parsing_falls_back_to_default() {
        assert_eq!(SortMode::from_tag("title"), SortMode::Title);
        assert_eq!(SortMode::from_tag("last_watched"), SortMode::LastWatched);
        assert_eq!(SortMode::from_tag("未知"), SortMode::RecentAdded);
    }
}
