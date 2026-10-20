//! 跨层共享的数据模型。
//!
//! 字段命名与架构文档 §8「数据模型与迁移」的表结构一一对应，目的是让
//! L3/L4 的实现可以直接映射，不需要再翻译一层概念。

use crate::error::{PortError, Result};

/// 媒体源类型（P-01）。`CloudDrive` 在架构文档中标注为「后续」，此处仅占位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    LocalFolder,
    LanPath,
    DirectUrl,
    CloudDrive,
    /// 在线站点源：`location` 是站点上的**列表页 / 搜索页 URL**，
    /// 由提供者联网枚举出该页上的条目（标题、详情页地址、封面），
    /// 播放时再按详情页实时解析流地址。
    OnlineSite,
}

// ---------------------------------------------------------------------------
// 标识
// ---------------------------------------------------------------------------

macro_rules! id_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(v: String) -> Self {
                Self(v)
            }
        }

        impl From<&str> for $name {
            fn from(v: &str) -> Self {
                Self(v.to_owned())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(SourceId, "媒体源标识（`media_source.id`）。");
id_newtype!(ItemId, "媒体条目标识（`media_item.id`）。");
id_newtype!(TagId, "标签标识（`tag.id`）。");

// ---------------------------------------------------------------------------
// 源与条目
// ---------------------------------------------------------------------------

/// 媒体源状态（`media_source.state`）。某源失败只置该源为 `ScanFailed`，
/// 不影响其他源与其他条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Idle,
    Scanning,
    ScanFailed,
}

/// 媒体源（`media_source`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaSource {
    pub id: SourceId,
    pub kind: SourceKind,
    pub name: String,
    /// 本地目录路径 / 网络路径 / 直链 URL。
    pub location: String,
    /// JSON 文本，排除规则（`media_source.exclude_rules`）。
    pub exclude_rules: Option<String>,
    pub state: SourceState,
    pub last_scan_at: Option<i64>,
}

/// 识别状态（`media_item.identify_state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifyState {
    Identified,
    Pending,
}

/// 元数据状态（`media_item.meta_state`）。元数据源全部不可用时，条目仍可入库，
/// 落为 `PendingFill`（PRD FR-01/FR-02「元数据待补全」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaState {
    Complete,
    PendingFill,
}

/// 媒体条目（`media_item`）。字段为文档表结构的子集，按需增列不算破坏契约。
#[derive(Debug, Clone, PartialEq)]
pub struct MediaItem {
    pub id: ItemId,
    pub source_id: SourceId,
    /// 稳定键：源 + 规范化相对路径 / 规范化 URL，不随元数据变化（保证幂等合并）。
    pub stable_key: String,
    pub rel_path: Option<String>,
    pub url: Option<String>,
    pub title: String,
    pub year: Option<i32>,
    pub duration_ms: Option<i64>,
    pub rating: Option<f32>,
    pub overview: Option<String>,
    pub poster_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub mtime: Option<i64>,
    pub identify_state: IdentifyState,
    pub meta_state: MetaState,
    /// JSON 文本，用户锁定字段（`media_item.field_locks`）。
    pub field_locks: Option<String>,
    /// 入库时间（`media_item.created_at`）。PRD FR-02「最近添加」排序使用。
    pub created_at: Option<i64>,
}

// ---------------------------------------------------------------------------
// 发现（P-01 discover 的输出单元）
// ---------------------------------------------------------------------------

/// 发现流程提交的候选项。只做「发现」，不做解析与入库——解析与入库分别由
/// L1 解析层与 L4 UPSERT 负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredItem {
    /// 稳定键，由提供者按「源 + 规范化相对路径 / URL」计算。
    pub stable_key: String,
    pub rel_path: Option<String>,
    pub url: Option<String>,
    /// 文件名等原始文本，供解析层生成 [`ParseHint`]。
    pub title_hint: Option<String>,
    /// 海报引用（`media_item.poster_id`）：本地目录源不用它（海报由
    /// 元数据源补齐），在线站点源用它登记**已缓存到海报目录下的文件名**。
    pub poster_ref: Option<String>,
}

/// 一次 `discover` 的统计回执。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiscoverStat {
    /// 新发现条目数。
    pub discovered: u64,
    /// 命中稳定键并合并的条目数（幂等）。
    pub merged: u64,
    /// 不可读 / 主动避开（符号链接、目录联结）而被跳过的目录与条目数。
    ///
    /// 它不是错误：整块硬盘上一个坏目录不该让这次扫描判为失败，但也不能悄悄吞掉。
    pub skipped: u64,
    /// 是否因取消信号提前结束；已提交条目保留。
    pub cancelled: bool,
}

/// 扫描阶段（进度上报用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScanStage {
    /// 枚举目录树、提交候选项。
    #[default]
    Discovering,
    /// 逐条探测流事实（时长 / 画质 / 字幕）。
    Probing,
}

/// 一次扫描的进度快照（[`crate::ports::media_source::DiscoverSink::progress`] 的输出单元）。
///
/// 该结构是「给界面看的」：只放可展示的计数与文本，不放任何内部句柄。
/// 各计数都是**累计值**，界面直接覆盖显示即可，无需自行求和。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanProgress {
    pub stage: ScanStage,
    /// 已处理的目录数（`Discovering` 阶段增长）。
    pub dirs_scanned: u64,
    /// 已读到的文件条目数（含非视频文件）。
    pub files_seen: u64,
    /// 已提交并落库的候选条目数。
    pub discovered: u64,
    /// 不可读 / 被跳过（如符号链接）的目录与条目数。
    pub skipped: u64,
    /// 探测阶段：当前第几条（从 1 起）。
    pub probe_current: u64,
    /// 探测阶段：本次待探测的总条数。
    pub probe_total: u64,
    /// 当前正在处理的路径（探测阶段为条目标题）。
    pub current: Option<String>,
    /// 是否已收到取消请求。
    pub cancelled: bool,
}

// ---------------------------------------------------------------------------
// 播放引用与探测事实
// ---------------------------------------------------------------------------

/// `open` 的输出：可直接交给播放内核的引用。在线源必要时附带请求头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayableRef {
    /// 本地路径或在线可播放地址。
    pub location: String,
    /// 需要时附带的请求头（如 Referer / User-Agent）。
    pub headers: Vec<(String, String)>,
}

impl PlayableRef {
    pub fn local(path: impl Into<String>) -> Self {
        Self {
            location: path.into(),
            headers: Vec::new(),
        }
    }
}

/// 音轨信息（内嵌音轨清单的元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackInfo {
    pub index: i64,
    pub lang: Option<String>,
    pub codec: Option<String>,
}

/// 内嵌字幕轨道信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedSubtitle {
    pub index: i64,
    pub lang: Option<String>,
    pub title: Option<String>,
}

/// `probe` 的输出：时长 / 分辨率 / 音轨 / 内嵌字幕清单。
///
/// 该结果是「系统自动维度」（画质、时长区间、字幕状态）的**唯一计算点**，
/// 计算一次即可缓存，避免每次筛选重算。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamFacts {
    pub duration_ms: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub audio_tracks: Vec<AudioTrackInfo>,
    pub embedded_subtitles: Vec<EmbeddedSubtitle>,
}

/// 观看进度（`playback_state`）。按**条目**维度持久化，与屏位无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    pub position_ms: i64,
    pub duration_ms: Option<i64>,
}

// ---------------------------------------------------------------------------
// 元数据（P-02）
// ---------------------------------------------------------------------------

/// 解析输入提示。由文件名或原始标题生成，作为 `candidates` 的输入。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseHint {
    /// 原始文件名或标题文本。
    pub raw_name: String,
    /// 从文件名中解析出的年份（若有）。
    pub year: Option<i32>,
}

/// 提供者内部引用，用于后续 `fetch` 精确定位一条记录。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProviderRef {
    pub provider: String,
    pub id: String,
}

/// 候选元数据条目（`candidates` 的元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataCandidate {
    pub provider_ref: ProviderRef,
    pub title: String,
    pub year: Option<i32>,
    /// 匹配置信度，供 L1 做多提供者排序。
    pub score: u8,
}

/// 抓取到的元数据（`fetch` 的输出）：含封面候选与简介。
///
/// 该结构是 [`crate::rules::FieldLock`] 的过滤对象——用户已锁定的字段在写入前
/// 会被置空，从而「不覆盖用户数据」。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata {
    pub title: Option<String>,
    pub year: Option<i32>,
    pub rating: Option<f32>,
    pub overview: Option<String>,
    /// 封面候选 URL。
    pub poster_url: Option<String>,
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// 字幕（P-03）
// ---------------------------------------------------------------------------

/// 引擎所在位置：Local | Remote。UI 必须读取该字段向用户告知隐私（PRD FR-05）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineLocality {
    Local,
    Remote,
}

/// 语种集合。P-03 `languages()` 返回「支持的原声集 / 目标语集」两元组。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LangSet(pub Vec<String>);

impl LangSet {
    pub fn new(items: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(items.into_iter().map(Into::into).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, lang: &str) -> bool {
        self.0.iter().any(|l| l == lang)
    }
}

/// 一行字幕。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleCue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub lang: Option<String>,
}

/// 字幕轨道类型：外挂 / 内嵌 / 实时生成。三类走同一渲染通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleKind {
    External,
    Embedded,
    Generated,
}

/// 统一字幕产物（PRD 核心对象）。无论来自本地还是云端引擎，
/// 结果都归一到该对象，因此播放器与「保存为字幕轨道」对引擎来源无感。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
    pub id: String,
    pub item_id: ItemId,
    pub kind: SubtitleKind,
    pub lang: Option<String>,
    /// 生成该轨道的引擎 id（外挂/内嵌时为 `None`）。
    pub source_engine: Option<String>,
    /// 用户是否已锁定该轨道。
    pub locked: bool,
    pub cues: Vec<SubtitleCue>,
}

/// 会话配置（`start` 的输入）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCfg {
    /// 期望识别的原声语种；`None` 表示由引擎自动判定。
    pub origin_lang: Option<String>,
    /// 目标译文语种；同语种时翻译层跳过。
    pub target_lang: Option<String>,
    /// 是否开启翻译；关闭时只输出原文。
    pub translate: bool,
}

/// 队列水位。L1 依据水位触发降级（「仅原文」或暂停字幕），不得无界缓冲（硬约束 C4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Watermark {
    Ok,
    High,
    Critical,
}

/// 识别质量等级（`CueEvent::Quality`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityLevel {
    Ok,
    Degraded,
    Critical,
}

/// 引擎输出的事件流元素：`Partial(原文) → Final(原文) → Translated(译文)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CueEvent {
    /// 视觉占位，不进入保存的轨道。
    Partial { text: String, conf_milli: u32 },
    /// 句界驱动的最终结果。
    Final { cue: SubtitleCue },
    /// 译文。
    Translated { cue: SubtitleCue },
    Quality { level: QualityLevel },
    Failed { reason: String },
}

// ---------------------------------------------------------------------------
// 播放（P-04）
// ---------------------------------------------------------------------------

/// 音频抽取模式。`Pcm16kMono` 为实时字幕所需格式；`Off` 表示零开销。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapMode {
    Off,
    Pcm16kMono,
}

/// 音轨 / 字幕轨道选择。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSelection {
    Audio(i64),
    Subtitle(i64),
    SubtitleOff,
}

/// 字幕样式（全局统一样式）。
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStyle {
    pub font_size: u32,
    /// CSS 颜色文本。
    pub color: String,
}

impl Default for SubtitleStyle {
    fn default() -> Self {
        Self {
            font_size: 28,
            color: "#ffffff".to_owned(),
        }
    }
}

/// 轨道快照（`PlayerEvent::TracksChanged` 携带）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracksSnapshot {
    pub audio: Vec<AudioTrackInfo>,
    pub subtitles: Vec<EmbeddedSubtitle>,
}

/// 下发给播放内核的指令。
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCmd {
    Play,
    Pause,
    Seek { position_ms: i64 },
    SetRate(f64),
    SetVolume(f64),
    SelectTrack(TrackSelection),
    SetSubtitleStyle(SubtitleStyle),
}

/// 播放内核上报的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerEvent {
    Position { position_ms: i64 },
    TracksChanged(TracksSnapshot),
    Eof,
    DecodeError { message: String },
    Buffering { active: bool },
}

// ---------------------------------------------------------------------------
// 便捷构造
// ---------------------------------------------------------------------------

impl MediaItem {
    /// 元数据全部不可用时，条目仍可入库（PRD FR-01/FR-02）。
    pub fn pending_fill(id: impl Into<ItemId>, source_id: impl Into<SourceId>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source_id: source_id.into(),
            stable_key: String::new(),
            rel_path: None,
            url: None,
            title: title.into(),
            year: None,
            duration_ms: None,
            rating: None,
            overview: None,
            poster_id: None,
            size_bytes: None,
            mtime: None,
            identify_state: IdentifyState::Pending,
            meta_state: MetaState::PendingFill,
            field_locks: None,
            created_at: None,
        }
    }
}

impl TryFrom<&str> for SourceKind {
    type Error = PortError;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "local_folder" => Ok(SourceKind::LocalFolder),
            "lan_path" => Ok(SourceKind::LanPath),
            "direct_url" => Ok(SourceKind::DirectUrl),
            "cloud_drive" => Ok(SourceKind::CloudDrive),
            "online_site" => Ok(SourceKind::OnlineSite),
            other => Err(PortError::Unsupported(format!("未知源类型: {other}"))),
        }
    }
}
