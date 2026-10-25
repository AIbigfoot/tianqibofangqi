#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 组合根（M1）：把 L3 适配器与 L4 基础设施注入 L1 服务，并向 L0 暴露命令。
//!
//! 三件事在这里收口：
//! 1. 装配：`MediaSourceProvider` / `MpvProber` / `PlayerCore` / 仓储；
//! 2. 窗口：主窗口句柄 + 影片画面宿主子窗口（`wid` 的来源）；
//! 3. 编排调用：`ImportService` / `ProgressService` / `PlaybackService`。
//!
//! 业务规则一律不在本文件重新定义 —— 判定门槛来自 L2，流程来自 L1。

mod host_window;
mod player;
mod store;
mod subtitle;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use cinema_adapters::enrichment::{self, Enrichment};
use cinema_adapters::javdb::JavdbProvider;
use cinema_adapters::llm::{local_highlight, summarize_highlights, HighlightInput, LlmConfig};
use cinema_adapters::websearch::search_snippets;
use cinema_adapters::media_source::{
    normalize_local_root, normalized_lan_root, DirectUrlProvider, FileListProvider, LanPathProvider,
    LocalFolderProvider,
};
use cinema_adapters::mpv::MpvProber;
use cinema_adapters::thumbs;
use cinema_adapters::online_site::{
    DownloadRegistry, DownloadState, OnlineDetail, OnlineSiteProvider,
};
use cinema_adapters::player::MpvPlayerCore;
use cinema_app::decode_quota::DecodeQuota;
use cinema_app::import_service::{ImportService, ImportStore, ScanControl, ScanReporter};
use cinema_app::metadata_service::{MetadataService, MetadataStore};
use cinema_app::playback_service::{PlaybackService, PreviewRequest};
use cinema_app::portability_service::PortabilityService;
use cinema_app::progress_service::ProgressService;
use cinema_app::search_service::{SearchQuery, SearchService, SearchStore};
use cinema_domain::error::{PortError, Result as PortResult};
use cinema_domain::model::{
    IdentifyState, ItemId, MediaItem, MediaSource, MetaState, PlayableRef, PlayerCmd, PlayerEvent,
    Progress, ScrapedComment, ScrapedImage, ScrapeState, ScanProgress, ScanStage, SourceId,
    SourceKind, SourceState, StreamFacts, TrackSelection,
};
use cinema_domain::ports::javdb::{JavdbClient, JavdbConfig};
use cinema_domain::ports::media_source::MediaSourceProvider;
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::tag_dimension::TagDimensionDef;
use cinema_domain::rules::{
    preview_allowed, ExitPolicy, Layout, ResumePolicy, SortMode, PREVIEW_SEGMENT_MS,
    PREVIEW_TRIGGER_MS,
};
use cinema_domain::tagging::{
    year_label_from, DIM_ACTOR, DIM_CATEGORY, DIM_DIRECTOR, DIM_MAKER, DIM_SERIES, DIM_YEAR,
};
use cinema_infra::db::Database;
use cinema_infra::now_ms;
use cinema_infra::repo::{
    CachedHighlight, CommentedItem, MediaItemRepo, MediaSourceRepo, MetadataRepo, SourceRootRepo,
};
use cinema_infra::subtitle_repo::SettingRepo;
use cinema_infra::tag_repo::{DimensionRepo, ItemTagRepo, SearchIndexRepo, TagRepo};

use host_window::HostWindow;
use player::{FactoryHandle, MpvPlayerFactory};
use store::DbStore;

/// 画面下方留给 webview 控制条的高度（与前端 CSS 一致）。
const CONTROL_BAR_PX: i32 = 76;

/// 同时播放上限在 `setting` 表中的键（PRD FR-06「路数上限」：默认 4，可调）。
const KEY_MAX_SCREENS: &str = "multi_screen.max_screens";

/// 并发发声开关在 `setting` 表中的键（默认关闭，硬约束 C6：新增能力默认关闭）。
const KEY_CONCURRENT_AUDIO: &str = "multi_screen.concurrent_audio";

/// 元数据数据源地址（`setting` 表）：站点域名会变，必须可配置。
const KEY_JAVDB_BASE_URL: &str = "javdb.base_url";
/// 元数据数据源登录 Cookie（`setting` 表）：评论与预告片需要登录态。
const KEY_JAVDB_COOKIE: &str = "javdb.cookie";
/// 默认站点：`javdb580.com` 早已失效，实测可用域名是 `javdb575.com`。
const DEFAULT_JAVDB_BASE_URL: &str = "https://javdb575.com";

/// 推荐看点所用大模型（OpenAI 兼容接口）的配置键。
const KEY_LLM_BASE_URL: &str = "llm.base_url";
const KEY_LLM_API_KEY: &str = "llm.api_key";
const KEY_LLM_MODEL: &str = "llm.model";
const KEY_LLM_PROVIDER: &str = "llm.provider";
/// 默认接 DeepSeek 的 OpenAI 兼容端点：国内可直连，接口协议通用。
const DEFAULT_LLM_BASE_URL: &str = "https://api.deepseek.com/v1";
const DEFAULT_LLM_MODEL: &str = "deepseek-chat";
/// 预置服务商：用户只需选服务商 + 填密钥，地址与模型由这里固化。
const DEEPSEEK_MODELS: [&str; 2] = ["deepseek-chat", "deepseek-reasoner"];
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_MODELS: [&str; 2] = ["gpt-4o-mini", "gpt-4o"];
/// 推荐横排默认条数。
const RECOMMEND_LIMIT: usize = 10;

/// 画面宿主窗口：多屏按屏位各一个，另加一个悬停预览窗口（PRD FR-06）。
#[derive(Default)]
struct Hosts {
    screens: std::collections::BTreeMap<u32, HostWindow>,
    preview: Option<HostWindow>,
}

/// 一个屏位的播放状态（多屏时每格一份；单路播放即只有 0 号屏位）。
#[derive(Clone)]
struct ScreenSlot {
    /// 该格正在播放的条目。进度按**条目维度**持久化，与屏位无关（架构 §7 图 4）。
    item_id: ItemId,
    /// 界面标题（错误卡与格位标签使用）。
    title: String,
    /// 打开播放时探测得到的流事实（含内嵌字幕清单），供实时字幕入口判定。
    facts: Option<StreamFacts>,
    /// 该格故障信息：解码 / 加载失败时显示错误卡，其余格继续（PRD FR-06 异常表）。
    fault: Option<String>,
    /// 该格是否正在缓冲 / 重连（在线地址网络中断，不中断其他格）。
    buffering: bool,
    /// 在线源（边下边播）的本地缓存文件；本地源为 `None`。
    ///
    /// 有值时心跳需要按它查下载状态：下载仍在进行时，mpv 读到「已下载到的位置」会
    /// 报 EOF，这在语义上不是「看完了」，必须补一次跳转续看；下载失败才报故障。
    stream: Option<PathBuf>,
    /// 上一次重试装载的时间（节流用，避免心跳每 400ms 都发一次 `loadfile`）。
    stream_retry_at: Option<std::time::Instant>,
    /// 上一次「下载前沿 EOF」时观察到的文件字节数。
    ///
    /// 用来防止「在下载前沿来回空转」：只有文件确实长大了才补一次续看，否则同一段
    /// 结尾会被反复重播（看上去像卡死在结尾循环）。
    stream_bytes: u64,
}

/// 应用状态：装配结果 + 播放期可变状态。
struct AppState {
    /// 主窗口句柄。
    parent: i64,
    /// 应用句柄：画面窗口的点击要经它转发回前端。
    app: AppHandle,
    db: Arc<Mutex<Database>>,
    store: Arc<DbStore>,
    factory: Arc<MpvPlayerFactory>,
    playback: Mutex<PlaybackService<FactoryHandle>>,
    /// libmpv 不可用时为 `None`：入库照常，只是暂时没有时长 → 无预览、无续播。
    prober: Option<Arc<MpvProber>>,
    hosts: Mutex<Hosts>,
    /// 屏位 → 该格的条目与状态（预览从不写入这里）。
    screens: Mutex<std::collections::BTreeMap<u32, ScreenSlot>>,
    /// 实时字幕运行时（服务 + 注册表）。
    subtitles: Mutex<subtitle::SubtitleRuntime>,
    /// 在线源「边下边播」的下载登记表（跨命令存活：重复点播放不重复下载）。
    downloads: Arc<DownloadRegistry>,
    /// 正在后台抓列表页封面的在线源（源 id → 开始时间），避免同一源被反复抓取。
    covers_inflight: Arc<Mutex<std::collections::HashMap<String, std::time::Instant>>>,
    /// 联网视频下载任务（条目 id → 进度快照），跨命令存活供界面查询与续看。
    online_downloads: Arc<Mutex<std::collections::HashMap<String, OnlineDownloadView>>>,
    /// 当前扫描的取消位。界面点「取消」置位，扫描就地收尾（见 `cancel_scan`）。
    scan_cancel: Arc<AtomicBool>,
    /// 元数据数据源客户端（HTTP 抓取与解析）。
    javdb: Arc<JavdbProvider>,
    /// 正在刮削的条目 id：自动刮削与手动重刮可能并发，同一时间只允许一条在途。
    scraping: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 单路播放时画面底部额外留出的物理像素（字幕菜单 / 弹层打开时让出空间，
    /// 否则原生画面窗口会盖住这些 HTML 控件，点击落到画面上表现为「控件没反应」）。
    player_extra_reserve: AtomicI32,
}

// ---------------------------------------------------------------------------
// 视图对象
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceView {
    id: String,
    name: String,
    location: String,
    /// `local_folder` | `lan_path` | `direct_url` | `online_site`。
    kind: String,
    state: String,
    /// 该源当前入库的条目数（「库管理」据此展示清除的影响面）。
    item_count: u64,
    /// 最近一次扫描失败的原因（PRD FR-01 异常表）。无失败时为 `None`。
    last_error: Option<String>,
    /// 合并进来的附加目录（只登记，不移动文件）。
    extra_roots: Vec<String>,
}

/// 源类型的界面标识（前端据此只对在线源展示「下载视频」）。
fn source_kind_label(kind: SourceKind) -> &'static str {
    match kind {
        SourceKind::LocalFolder => "local_folder",
        SourceKind::LanPath => "lan_path",
        SourceKind::DirectUrl => "direct_url",
        SourceKind::CloudDrive => "cloud_drive",
        SourceKind::OnlineSite => "online_site",
    }
}

/// 一条联网视频下载任务（进度事件与查询命令共用）。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct OnlineDownloadView {
    item_id: String,
    title: String,
    /// `running` | `done` | `failed` | `skipped`（磁盘上已有同名视频）。
    state: String,
    /// 0–100；已完成时恒为 100。
    percent: f64,
    /// 落盘路径（已完成时给出）。
    dest: Option<String>,
    /// 失败原因。
    error: Option<String>,
}

/// 导出 / 导入的回执（PRD FR-07）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PortabilityReportView {
    sources: usize,
    items: usize,
    /// 导入后按本机路径判定不可访问、被标记「缺失」的条目数。
    items_missing: usize,
    user_tags: usize,
    collections: usize,
    settings: usize,
    subtitle_tracks: usize,
    /// 导入时按进度事实重算观看状态的条目数（导出恒为 0）。
    restored_progress: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemView {
    id: String,
    title: String,
    duration_ms: Option<i64>,
    position_ms: i64,
    /// 有值时表示「继续观看 N%」。
    resume_percent: Option<u32>,
    watched: bool,
    missing: bool,
    /// 有真实封面可展示（在线站点源抓到的封面；本地源一律为 false，由前端生成占位图）。
    has_poster: bool,
}

/// 元数据数据源设置回执（`javdb_settings` / `save_javdb_settings`）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct JavdbSettingsView {
    base_url: String,
    has_cookie: bool,
    logged_in: bool,
    username: Option<String>,
    /// 站点或探测过程给出的可读提示（Cookie 失效 / 站点不可达 / 登录成功）。
    message: Option<String>,
}

/// 一次刮削的回执。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScrapeResultView {
    item_id: String,
    state: String,
    comments: usize,
    screenshots: usize,
    trailer_saved: bool,
    /// 失败原因（成功时为 `None`）。
    message: Option<String>,
}

/// 批量刮削的进度事件（`scrape-progress`）载荷。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ScrapeProgressView {
    item_id: String,
    done: usize,
    total: usize,
}

/// 详情页里的一条短评。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CommentView {
    author: Option<String>,
    score: Option<i32>,
    content: String,
    likes: u32,
    posted_at: Option<String>,
}

/// 条目详情（海报单击进入）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemDetailView {
    id: String,
    title: String,
    code: Option<String>,
    release_date: Option<String>,
    duration_ms: Option<i64>,
    rating: Option<f32>,
    detail_url: Option<String>,
    has_poster: bool,
    /// 截图文件名（前端再调 `item_screenshot` 取 data URL）。
    screenshots: Vec<String>,
    comments: Vec<CommentView>,
    categories: Vec<String>,
    series: Vec<String>,
    maker: Vec<String>,
    director: Vec<String>,
    actors: Vec<String>,
    has_trailer: bool,
    trailer_url: Option<String>,
    /// 该条目属于在线站点源（界面据此显示「下载视频」）。
    online: bool,
    /// 视频文件已经在下载目录里：详情页不再显示下载入口。
    downloaded: bool,
    /// `pending` | `running` | `done` | `failed` | `no_code`。
    scrape_state: String,
    scrape_error: Option<String>,
}

/// 大模型配置回执（界面只关心是否已配置，不回显密钥）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LlmSettingsView {
    /// `deepseek` | `openai` | `custom`。
    provider: String,
    base_url: String,
    model: String,
    has_key: bool,
}

/// 推荐横排的一张卡片：影片信息 + 一句核心看点。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecommendItemView {
    item_id: String,
    title: String,
    code: Option<String>,
    rating: Option<f32>,
    has_poster: bool,
    /// 由高赞短评总结出的核心看点。
    highlight: String,
    /// 用来展示「多少人认同」的最高赞数。
    likes: u32,
    /// 参与总结的高赞短评条数（界面标注来源用）。
    comments: usize,
    /// 除短评外参与总结的资料条数（简介 / 标签 / 联网结果）。
    sources: usize,
    /// `llm` | `local`。
    source: String,
}

/// 一次推荐的结果。`llm=false` 表示本次走的是本地兜底提炼。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RecommendView {
    items: Vec<RecommendItemView>,
    llm: bool,
    /// 本次是否真的发起了大模型请求（都命中缓存时为 false）。
    requested: bool,
    message: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportReportView {
    discovered: u64,
    stored: u64,
    probed: u64,
    /// 不可读 / 主动避开而跳过的目录与条目数。
    skipped: u64,
    /// 是否被用户取消（取消不算失败，已入库条目保留）。
    cancelled: bool,
}

/// 扫描进度事件（`scan-progress`）的载荷。
///
/// 时间与总数由 L1/L3 计算，本层只做搬运与序列化：界面拿到的就是「扫描到哪了」。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ScanProgressView {
    /// 正在扫描的源 id（一次扫描只对应一个源）。
    source_id: String,
    /// `discovering` | `probing`。
    stage: String,
    dirs_scanned: u64,
    files_seen: u64,
    discovered: u64,
    skipped: u64,
    probe_current: u64,
    probe_total: u64,
    /// 当前正在处理的路径（探测阶段为条目标题）。
    current: Option<String>,
    cancelled: bool,
}

impl ScanProgressView {
    fn new(source_id: &str, progress: &ScanProgress) -> Self {
        Self {
            source_id: source_id.to_owned(),
            stage: match progress.stage {
                ScanStage::Discovering => "discovering",
                ScanStage::Probing => "probing",
            }
            .into(),
            dirs_scanned: progress.dirs_scanned,
            files_seen: progress.files_seen,
            discovered: progress.discovered,
            skipped: progress.skipped,
            probe_current: progress.probe_current,
            probe_total: progress.probe_total,
            current: progress.current.clone(),
            cancelled: progress.cancelled,
        }
    }
}

/// 把入库编排上报的进度转成前端事件。
struct EmitScanReporter {
    app: AppHandle,
    source_id: String,
}

impl ScanReporter for EmitScanReporter {
    fn report(&self, progress: &ScanProgress) {
        let _ = self
            .app
            .emit("scan-progress", ScanProgressView::new(&self.source_id, progress));
    }
}

/// 检索区提交的查询：文本 + 维度筛选 + 排序。
///
/// `filters` 是「维度 id → 选中取值」，组合语义（同维度 OR、跨维度 AND）由 L2 决定，
/// 本层只做搬运。
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchQueryView {
    #[serde(default)]
    text: String,
    #[serde(default)]
    filters: std::collections::BTreeMap<String, Vec<String>>,
    /// 排序标记（`title` / `year` / ...），未知取值由 L2 回落到「最近添加」。
    #[serde(default)]
    sort: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DimensionView {
    id: String,
    label: String,
    /// `single` | `multi`。
    selector: String,
    /// `metadata` | `derived` | `user`。
    source: String,
    order: u32,
    editable: bool,
    lockable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TagUsageView {
    id: String,
    name: String,
    /// 该取值被多少条目使用（检索区按此排序，超出部分折叠）。
    count: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResumeOfferView {
    position_ms: i64,
    percent: u32,
    duration_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlaybackStatusView {
    playing: bool,
    ended: bool,
    position_ms: i64,
    duration_ms: Option<i64>,
    /// 在线源（边下边播）：影片还在下载 / 尚未装载成功，界面提示「正在准备」。
    buffering: bool,
    /// 该格的故障信息（取流失败等）。界面弹一次提示，避免「无明确响应」。
    fault: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PreviewResultView {
    started: bool,
    reason: Option<String>,
}

/// 预告片播放进度（详情页画面下方的进度条用）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrailerStatusView {
    /// 预览实例是否仍在（停止后为 false）。
    active: bool,
    position_ms: i64,
    duration_ms: Option<i64>,
}

/// 一路多屏窗口的状态（PRD FR-06「每格能力与规则」）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiSlotView {
    slot: u32,
    item_id: String,
    title: String,
    playing: bool,
    position_ms: i64,
    duration_ms: Option<i64>,
    ended: bool,
    /// 是否为本格发声窗口（单声道输出：同一时刻只有一路发声）。
    speaker: bool,
    /// 该格是否正在缓冲 / 重连。
    buffering: bool,
    /// 该格故障信息；有值时界面显示错误卡与「重试 / 更换影片」。
    fault: Option<String>,
}

/// 多屏整体状态。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiScreenView {
    slots: Vec<MultiSlotView>,
    speaker: Option<u32>,
    /// 布局标记（`1x2` / `2x1` / `2x2` / `1plus3` / `3x3`）。
    layout: String,
    layout_label: String,
    /// 同时播放上限（默认 4，可在设置中调整）。
    limit: usize,
    /// 并发发声是否开启（默认关闭）：开启后多路同时出声，
    /// 每格右上角「发声切换」退化为单条命令的作用对象。
    concurrent_audio: bool,
    /// 是否处于多屏播放（≥ 2 路）。
    multi_screen: bool,
    /// 多屏播放期间是否提供悬停预览（恒 false，供界面标注原因）。
    preview_available: bool,
}

/// 界面可选的布局与上限（PRD FR-06「布局」表 / 「路数上限」）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MultiLayoutView {
    tag: String,
    label: String,
    slots: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UiConstants {
    preview_trigger_ms: u64,
    preview_segment_ms: i64,
    control_bar_px: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryStats {
    sources: usize,
    items: u64,
    /// libmpv 是否可用：决定预览与续播能否工作。
    decoder_ready: bool,
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 由目录路径派生稳定的源 id，避免同一目录重复添加产生第二行。
fn source_id_for(path: &str) -> SourceId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    SourceId(format!("src-local-{hash:016x}"))
}

fn dir_label(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.to_owned())
}

/// 默认元数据目录（`%LOCALAPPDATA%\Cinema`）：数据库、封面、截图与预告片。
fn default_data_dir() -> PathBuf {
    std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("Cinema")
}

/// 默认视频下载目录（默认元数据目录下的 `streams`，与旧版本落盘位置一致）。
///
/// 视频文件与视频信息分开存放：这里只放联网播放 / 下载的视频。
fn default_download_dir() -> PathBuf {
    default_data_dir().join("streams")
}

/// 目录字符串归一化（去首尾空白，空串视为未设置）。
fn normalize_dir(raw: &str) -> Option<String> {
    let dir = raw.trim();
    (!dir.is_empty()).then(|| dir.to_owned())
}

/// 判断两个目录是否指向同一位置（Windows 下大小写与斜杠写法不同也算同一个）。
fn same_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// 环境变量指定的元数据目录（`CINEMA_DATA_DIR`），优先级最高。
fn env_data_dir() -> Option<String> {
    std::env::var("CINEMA_DATA_DIR")
        .ok()
        .and_then(|dir| normalize_dir(&dir))
}

/// 环境变量指定的视频下载目录（`CINEMA_DOWNLOAD_DIR`），优先级最高。
fn env_download_dir() -> Option<String> {
    std::env::var("CINEMA_DOWNLOAD_DIR")
        .ok()
        .and_then(|dir| normalize_dir(&dir))
}

/// 设置文件路径：固定放在默认目录下，不随数据目录迁移。
///
/// 否则换了目录就再也读不到自己选过哪里。数据目录搬家后，旧目录里只会留下这份很小的设置文件。
fn storage_config_path() -> PathBuf {
    default_data_dir().join("storage.json")
}

/// 设置文件版本：旧版本只记 `dataDir`，升级后按其目标补一次自动迁移。
const STORAGE_CONFIG_VERSION: u32 = 2;

/// `storage.json` 的内容：元数据目录与视频下载目录分开记录。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageConfig {
    /// 配置版本。
    #[serde(default)]
    version: u32,
    /// 元数据目录（数据库、封面、截图、预告片）。兼容旧版本字段 `dataDir`。
    #[serde(default, alias = "dataDir")]
    metadata_dir: Option<String>,
    /// 联网视频的默认下载目录。
    #[serde(default)]
    download_dir: Option<String>,
    /// 尚未完成的迁移来源：启动时把该目录的元数据搬到目标目录，成功后清空。
    #[serde(default)]
    migrate_from: Option<String>,
}

fn read_storage_config() -> StorageConfig {
    std::fs::read_to_string(storage_config_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_storage_config(config: &StorageConfig) -> std::result::Result<(), String> {
    let path = storage_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, raw).map_err(|e| e.to_string())
}

/// 本次运行实际生效的目录：启动时解析一次并固定，改设置后靠重启生效。
///
/// 固定下来还避免「已保存但尚未重启」时海报目录已经指向新位置、数据库却还在旧位置。
static ACTIVE_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static ACTIVE_DOWNLOAD_DIR: OnceLock<PathBuf> = OnceLock::new();

fn ensure_dir(path: &Path, label: &str) -> std::result::Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{label}不可用：{e}"))
}

/// 落盘前探测目标目录可写（只读盘 / 权限不足当场报错，而不是重启后启动失败）。
fn write_probe(path: &Path) -> std::result::Result<(), String> {
    let probe = path.join(".cinema-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| format!("目标目录不可写：{e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// 按「环境变量 > 设置文件 > 默认」解析元数据目录（不做迁移）。
///
/// 设置文件里记录的目录若不可用（盘符拔掉、权限丢失），回退默认目录而不是启动失败。
fn resolve_data_dir() -> std::result::Result<PathBuf, String> {
    if let Some(dir) = env_data_dir() {
        let path = PathBuf::from(dir);
        ensure_dir(&path, "CINEMA_DATA_DIR 指向的目录")?;
        return Ok(path);
    }
    if let Some(dir) = read_storage_config()
        .metadata_dir
        .and_then(|dir| normalize_dir(&dir))
    {
        let path = PathBuf::from(&dir);
        if ensure_dir(&path, "设置的数据目录").is_ok() {
            return Ok(path);
        }
        eprintln!("[存储] 设置的数据目录不可用，回退默认目录：{dir}");
    }
    let path = default_data_dir();
    ensure_dir(&path, "默认数据目录")?;
    Ok(path)
}

/// 按「环境变量 > 设置文件 > 默认」解析视频下载目录。
fn resolve_download_dir() -> std::result::Result<PathBuf, String> {
    if let Some(dir) = env_download_dir() {
        let path = PathBuf::from(dir);
        ensure_dir(&path, "CINEMA_DOWNLOAD_DIR 指向的目录")?;
        return Ok(path);
    }
    if let Some(dir) = read_storage_config()
        .download_dir
        .and_then(|dir| normalize_dir(&dir))
    {
        let path = PathBuf::from(&dir);
        if ensure_dir(&path, "设置的下载目录").is_ok() {
            return Ok(path);
        }
        eprintln!("[存储] 设置的下载目录不可用，回退默认目录：{dir}");
    }
    let path = default_download_dir();
    ensure_dir(&path, "默认下载目录")?;
    Ok(path)
}

/// 生效的元数据目录：启动时固定，未初始化时才回落到动态解析。
fn data_dir() -> std::result::Result<PathBuf, String> {
    match ACTIVE_DATA_DIR.get() {
        Some(dir) => Ok(dir.clone()),
        None => resolve_data_dir(),
    }
}

/// 生效的视频下载目录：启动时固定，未初始化时才回落到动态解析。
fn download_dir() -> std::result::Result<PathBuf, String> {
    match ACTIVE_DOWNLOAD_DIR.get() {
        Some(dir) => Ok(dir.clone()),
        None => resolve_download_dir(),
    }
}

/// 递归复制目录内容（覆盖同名文件，保留目标目录里的其它文件）。
fn copy_dir_all(from: &Path, to: &Path) -> std::result::Result<(), String> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
    for entry in std::fs::read_dir(from).map_err(|e| format!("读取 {} 失败：{e}", from.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| format!("复制 {} 失败：{e}", src.display()))?;
        }
    }
    Ok(())
}

/// 迁移元数据（数据库 + 封面 + 截图 + 预告片），**不迁移视频文件**。
fn migrate_metadata(from: &Path, to: &Path) -> std::result::Result<(), String> {
    if same_path(from, to) {
        return Ok(());
    }
    std::fs::create_dir_all(to).map_err(|e| format!("创建 {} 失败：{e}", to.display()))?;
    // WAL 一并复制（强制退出时最新改动可能还在 WAL 里）；-shm 由 SQLite 自动重建
    for name in ["cinema.db", "cinema.db-wal"] {
        let src = from.join(name);
        if src.is_file() {
            std::fs::copy(&src, to.join(name))
                .map_err(|e| format!("复制 {} 失败：{e}", src.display()))?;
        }
    }
    for sub in ["posters", "shots", "trailers"] {
        copy_dir_all(&from.join(sub), &to.join(sub))?;
    }
    Ok(())
}

/// 启动时解析目录并完成待迁移的元数据，返回本次生效的元数据目录。
///
/// 迁移失败时继续使用旧目录：宁可这次不搬家，也不让用户看到一个空库。
fn init_storage() -> std::result::Result<PathBuf, String> {
    let env_meta = env_data_dir();
    let config = read_storage_config();
    let mut next = config.clone();

    // 目标元数据目录：环境变量优先，其余按设置文件 / 默认
    let target = match &env_meta {
        Some(dir) => PathBuf::from(dir),
        None => config
            .metadata_dir
            .clone()
            .and_then(|dir| normalize_dir(&dir))
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir),
    };

    // 迁移来源：记录的一次性迁移；旧版本设置把默认目录当作来源补一次自动迁移
    let mut migrate_from = if env_meta.is_some() {
        None
    } else {
        config
            .migrate_from
            .clone()
            .and_then(|dir| normalize_dir(&dir))
    };
    if migrate_from.is_none() && env_meta.is_none() && config.version < STORAGE_CONFIG_VERSION {
        let legacy_source = default_data_dir();
        if config.metadata_dir.is_some()
            && !target.join("cinema.db").exists()
            && legacy_source.join("cinema.db").exists()
        {
            migrate_from = Some(legacy_source.display().to_string());
        }
    }

    let mut active = target.clone();
    if let Err(err) = ensure_dir(&target, "元数据目录") {
        // 目标目录不可用（盘未接、权限丢失）：继续用旧目录，绝不让播放器打不开
        eprintln!("[存储] 目标元数据目录不可用，本次回退：{err}");
        let fallback = migrate_from
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(default_data_dir);
        ensure_dir(&fallback, "默认数据目录")?;
        active = fallback;
        // 保留 next.migrate_from，下次启动继续尝试
    } else if let Some(from) = migrate_from {
        let from_path = PathBuf::from(&from);
        if same_path(&from_path, &target) {
            next.migrate_from = None;
        } else {
            match migrate_metadata(&from_path, &target) {
                Ok(()) => {
                    next.migrate_from = None;
                    println!(
                        "[存储] 元数据已迁移：{} → {}",
                        from_path.display(),
                        target.display()
                    );
                }
                Err(err) => {
                    eprintln!("[存储] 元数据迁移失败，本次继续使用原目录：{err}");
                    active = from_path;
                }
            }
        }
    }
    next.version = STORAGE_CONFIG_VERSION;
    if let Err(err) = write_storage_config(&next) {
        eprintln!("[存储] 写设置文件失败：{err}");
    }

    let download = resolve_download_dir()?;
    let _ = ACTIVE_DATA_DIR.set(active.clone());
    let _ = ACTIVE_DOWNLOAD_DIR.set(download.clone());
    println!(
        "[存储] 元数据目录 = {}；下载目录 = {}",
        active.display(),
        download.display()
    );
    Ok(active)
}

/// 存储位置设置回执（`storage_settings` / `save_storage_settings`）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StorageSettingsView {
    /// 本次运行实际使用的元数据目录（改设置后要重启才会变）。
    data_dir: String,
    /// 默认元数据目录（「恢复默认」会切回这里）。
    default_dir: String,
    /// 已保存但尚未生效的元数据目录。
    pending_dir: Option<String>,
    /// 本次运行实际使用的视频下载目录。
    download_dir: String,
    /// 默认视频下载目录。
    default_download_dir: String,
    /// 已保存但尚未生效的视频下载目录。
    pending_download_dir: Option<String>,
    /// 环境变量覆盖值；有值时对应区域禁止更改。
    env_dir: Option<String>,
    env_download_dir: Option<String>,
    /// 设置文件位置，便于备份与排查。
    config_path: String,
    /// 上次启动未完成的迁移来源（迁移失败时仍有值）。
    migrate_from: Option<String>,
}

fn storage_view() -> StorageSettingsView {
    let config = read_storage_config();
    let active = data_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let active_download = download_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    // 待生效目标：设置文件里的值，没设置就是默认目录
    let target = config
        .metadata_dir
        .clone()
        .and_then(|dir| normalize_dir(&dir))
        .unwrap_or_else(|| default_data_dir().display().to_string());
    let download_target = config
        .download_dir
        .clone()
        .and_then(|dir| normalize_dir(&dir))
        .unwrap_or_else(|| default_download_dir().display().to_string());
    StorageSettingsView {
        data_dir: active.clone(),
        default_dir: default_data_dir().display().to_string(),
        pending_dir: (!same_path(Path::new(&target), Path::new(&active))).then_some(target),
        download_dir: active_download.clone(),
        default_download_dir: default_download_dir().display().to_string(),
        pending_download_dir: (!same_path(
            Path::new(&download_target),
            Path::new(&active_download),
        ))
        .then_some(download_target),
        env_dir: env_data_dir(),
        env_download_dir: env_download_dir(),
        config_path: storage_config_path().display().to_string(),
        migrate_from: config.migrate_from,
    }
}

/// 读取当前存储位置。界面打开设置时调用一次。
#[tauri::command]
fn storage_settings() -> StorageSettingsView {
    storage_view()
}

/// 保存存储位置。`None` 表示该项不修改；恢复默认由前端显式传入默认目录。
///
/// 元数据目录切到空目录时登记一次性迁移（重启后在打开数据库之前完成复制），
/// 切到已有 `cinema.db` 的目录则直接使用那个库，不覆盖。
#[tauri::command]
fn save_storage_settings(
    metadata_dir: Option<String>,
    download_dir: Option<String>,
) -> std::result::Result<StorageSettingsView, String> {
    let mut config = read_storage_config();
    config.version = STORAGE_CONFIG_VERSION;

    if let Some(raw) = metadata_dir {
        let dir = normalize_dir(&raw).ok_or("元数据目录不能为空")?;
        let path = PathBuf::from(&dir);
        if !path.is_absolute() {
            return Err("请选择绝对路径".into());
        }
        if let Some(env) = env_data_dir() {
            if env != dir {
                return Err(format!(
                    "当前由环境变量 CINEMA_DATA_DIR 指定元数据目录（{env}），请先移除该变量再更改"
                ));
            }
        }
        ensure_dir(&path, "元数据目录")?;
        write_probe(&path)?;
        let active = data_dir()?;
        if !same_path(&path, &active) {
            // 目标已有库：直接切过去；空目录：重启时自动迁移当前库
            config.migrate_from = if path.join("cinema.db").exists() {
                None
            } else {
                Some(active.display().to_string())
            };
        } else {
            config.migrate_from = None;
        }
        config.metadata_dir = (!same_path(&path, &default_data_dir())).then(|| dir);
    }

    if let Some(raw) = download_dir {
        let dir = normalize_dir(&raw).ok_or("下载目录不能为空")?;
        let path = PathBuf::from(&dir);
        if !path.is_absolute() {
            return Err("请选择绝对路径".into());
        }
        if let Some(env) = env_download_dir() {
            if env != dir {
                return Err(format!(
                    "当前由环境变量 CINEMA_DOWNLOAD_DIR 指定下载目录（{env}），请先移除该变量再更改"
                ));
            }
        }
        ensure_dir(&path, "下载目录")?;
        write_probe(&path)?;
        config.download_dir = (!same_path(&path, &default_download_dir())).then(|| dir);
    }

    write_storage_config(&config)?;
    Ok(storage_view())
}

/// 重启应用，让新的存储位置生效（元数据迁移在下次启动、打开数据库之前完成）。
#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

/// 海报缓存目录（与 `DbStore` 的门面一致：`<数据目录>/posters`），不存在则创建。
///
/// 在线站点源会把 yt-dlp 抓到的封面按确定性文件名落在这里；封面是**按需懒抓**的
/// （见 [`item_cover`]），所以这里只负责目录存在。
fn poster_dir() -> std::result::Result<PathBuf, String> {
    let dir = data_dir()?.join("posters");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 在线影片下载 / 缓存目录（默认 `<默认元数据目录>/streams`，可在「存储」里单独改）。
///
/// 视频文件与视频信息分离：这里只放联网播放 / 下载的视频，元数据（数据库、封面、
/// 截图、预告片）仍在数据目录。在线源采用「边下边播」：yt-dlp 后台把影片写到这里
/// （`online-<hash>.mp4`），播放内核打开这个**正在增长**的文件（见 L3 `online_site.rs`）。
fn streams_dir() -> std::result::Result<PathBuf, String> {
    let dir = download_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

/// 在 PATH 中查找可执行文件（Windows 下额外补 `.exe`）。
fn on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if cfg!(windows) && candidate.extension().is_none() {
            let with_exe = candidate.with_extension("exe");
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

/// 定位 yt-dlp：优先主程序同目录（运行时依赖约定：子进程可执行文件与动态库同目录），
/// 否则回落到 PATH。缺失时返回可读提示，由命令层直接呈现给用户。
fn ytdlp_path() -> std::result::Result<PathBuf, String> {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
    {
        for name in ["yt-dlp.exe", "yt-dlp"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    on_path("yt-dlp").ok_or_else(|| {
        "未找到 yt-dlp：请把 yt-dlp.exe 放到主程序同目录（或加入系统 PATH）后重试".to_string()
    })
}

/// 一次联网导入最多展开的页数上限：防误填页码，把一次导入拖成几小时。
const MAX_IMPORT_PAGES: u32 = 300;

/// 去掉 URL 里的分页参数 `page`，用于派生源身份与源地址。
///
/// 同一搜索页的第 1 页与第 3 页是同一批内容的不同页，不应各登记成一个源；
/// 因此源 id 与源地址都基于去掉 `page` 的地址，其它查询参数与锚点原样保留。
fn strip_page_param(url: &str) -> String {
    let (head, frag) = match url.find('#') {
        Some(i) => (&url[..i], &url[i..]),
        None => (url, ""),
    };
    let Some(q) = head.find('?') else {
        return url.to_owned();
    };
    let (path, query) = head.split_at(q);
    let kept: Vec<&str> = query[1..]
        .split('&')
        .filter(|part| !part.is_empty() && *part != "page" && !part.starts_with("page="))
        .collect();
    if kept.is_empty() {
        format!("{path}{frag}")
    } else {
        format!("{path}?{}{frag}", kept.join("&"))
    }
}

/// 生成某一页的地址：已有 `page=` 就替换取值，没有就追加，保留其它查询参数与锚点。
fn page_url(base: &str, page: u32) -> String {
    let (head, frag) = match base.find('#') {
        Some(i) => (&base[..i], &base[i..]),
        None => (base, ""),
    };
    let (path, query) = match head.find('?') {
        Some(i) => head.split_at(i),
        None => (head, ""),
    };
    let mut parts: Vec<String> = if query.is_empty() {
        Vec::new()
    } else {
        query[1..]
            .split('&')
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut replaced = false;
    for part in parts.iter_mut() {
        if part == "page" || part.starts_with("page=") {
            *part = format!("page={page}");
            replaced = true;
        }
    }
    if !replaced {
        parts.push(format!("page={page}"));
    }
    format!("{path}?{}{frag}", parts.join("&"))
}

/// 由列表页 URL 派生源显示名：取主机名（如 `hanime1.me`），失败则回落到原 URL。
fn site_label(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .trim();
    if host.is_empty() {
        url.to_owned()
    } else {
        host.to_owned()
    }
}

/// 极简 base64 编码（标准表 + `=` 补齐）。
///
/// 只为把封面字节作为 data URL 交给前端 `<img>`，不值得为单一用途引入新依赖。
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from).unwrap_or(0);
        let b2 = chunk.get(2).copied().map(u32::from).unwrap_or(0);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 按源类型挑选提供者。新增源类型＝在此加一个分支，域层与流程不动。
fn provider_for(
    src: &MediaSource,
    prober: Option<&Arc<MpvProber>>,
    state_downloads: &Arc<DownloadRegistry>,
) -> PortResult<Box<dyn MediaSourceProvider>> {
    match src.kind {
        SourceKind::LocalFolder => {
            // 提供者的根目录必须与源 location 一致：open 用它把相对路径拼回绝对路径
            let root = PathBuf::from(&src.location);
            Ok(match prober {
                Some(p) => Box::new(LocalFolderProvider::with_prober(&root, Arc::clone(p))),
                None => Box::new(LocalFolderProvider::new(&root)),
            })
        }
        SourceKind::DirectUrl => Ok(Box::new(DirectUrlProvider)),
        // 在线站点源（M7 联网导入）：只为「列表页 URL」配一个 yt-dlp 子进程提供者；
        // yt-dlp 缺失属运行时依赖问题，在此转成可读提示。
        //
        // 播放走「边下边播」：`open` 只算缓存路径并起一个**分离**下载（毫秒级返回），
        // 因为同步命令跑在主线程上——任何前台 yt-dlp 调用都会冻结界面。
        SourceKind::OnlineSite => {
            let ytdlp = ytdlp_path().map_err(PortError::Backend)?;
            let covers = poster_dir().map_err(PortError::Backend)?;
            let streams = streams_dir().map_err(PortError::Backend)?;
            Ok(Box::new(OnlineSiteProvider::new(
                ytdlp,
                covers,
                streams,
                Arc::clone(state_downloads),
            )))
        }
        // 局域网路径源（M5 增量）：只在此登记一行，域层与流程不动
        SourceKind::LanPath => Ok(match prober {
            Some(p) => Box::new(LanPathProvider::with_prober(&src.location, Arc::clone(p))),
            None => Box::new(LanPathProvider::new(&src.location)),
        }),
        other => Err(PortError::Unsupported(format!("尚未实现该源类型：{other:?}"))),
    }
}

fn load_item(state: &AppState, item_id: &str) -> std::result::Result<(MediaItem, MediaSource), String> {
    let db = state
        .db
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    let item = MediaItemRepo::get(db.conn(), &ItemId::from(item_id))
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("条目不存在：{item_id}"))?;
    let src = MediaSourceRepo::get(db.conn(), &item.source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("媒体源不存在：{}", item.source_id))?;
    Ok((item, src))
}

// ---------------------------------------------------------------------------
// 元数据刮削：配置读取、状态标签与落盘路径
// ---------------------------------------------------------------------------

/// 从设置表读取数据源配置。站点地址为空时回落到内置镜像（`javdb580.com` 已失效）。
fn javdb_config_from(db: &Database) -> JavdbConfig {
    let base_url = SettingRepo::get(db.conn(), KEY_JAVDB_BASE_URL)
        .ok()
        .flatten()
        .map(|raw| normalize_base_url(&raw))
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_JAVDB_BASE_URL.to_owned());
    let cookie = SettingRepo::get(db.conn(), KEY_JAVDB_COOKIE)
        .ok()
        .flatten()
        .unwrap_or_default();
    JavdbConfig {
        base_url,
        cookie: cookie.trim().to_owned(),
    }
}

/// 规范化用户填写的站点地址：补 `https://`、去掉尾部斜杠与路径外的空白。
fn normalize_base_url(raw: &str) -> String {
    let mut url = raw.trim().trim_end_matches('/').to_owned();
    if url.is_empty() {
        return String::new();
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{url}");
    }
    url
}

// ---------------------------------------------------------------------------
// 推荐看点（P-07）：高赞短评 → 大模型总结 → 主机模式横排
// ---------------------------------------------------------------------------

/// 从设置表读取大模型配置。未填写的项回落到默认值；密钥为空即视为未配置。
fn llm_config_from(db: &Database) -> LlmConfig {
    let read = |key: &str| {
        SettingRepo::get(db.conn(), key)
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    let base = normalize_base_url(&read(KEY_LLM_BASE_URL));
    let model = read(KEY_LLM_MODEL).trim().to_owned();
    LlmConfig {
        base_url: if base.is_empty() {
            DEFAULT_LLM_BASE_URL.to_owned()
        } else {
            base
        },
        api_key: read(KEY_LLM_API_KEY).trim().to_owned(),
        model: if model.is_empty() {
            DEFAULT_LLM_MODEL.to_owned()
        } else {
            model
        },
    }
}

fn llm_settings_view(config: &LlmConfig, provider: &str) -> LlmSettingsView {
    LlmSettingsView {
        provider: provider.to_owned(),
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        has_key: !config.api_key.is_empty(),
    }
}

/// 识别当前服务商：优先用已保存的选择；没有记录时按接口地址推断。
fn llm_provider_from(db: &Database, config: &LlmConfig) -> &'static str {
    let stored = SettingRepo::get(db.conn(), KEY_LLM_PROVIDER)
        .ok()
        .flatten()
        .unwrap_or_default();
    match stored.trim() {
        "deepseek" => "deepseek",
        "openai" => "openai",
        "custom" => "custom",
        _ => {
            if config.base_url.contains("deepseek") {
                "deepseek"
            } else if config.base_url.contains("openai") {
                "openai"
            } else {
                "custom"
            }
        }
    }
}

/// 读取大模型配置（不回显密钥，只回报是否已配置）。
#[tauri::command]
fn llm_settings(state: State<'_, AppState>) -> std::result::Result<LlmSettingsView, String> {
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let config = llm_config_from(&db);
    let provider = llm_provider_from(&db, &config);
    Ok(llm_settings_view(&config, provider))
}

/// 保存大模型配置。密钥留空表示保留已保存的密钥（与 Cookie 的处理方式一致）。
///
/// 预置服务商（DeepSeek / OpenAI）的接口地址与可选模型由后端固化，界面只让用户选模型；
/// 「自定义」才使用界面上填的地址与模型名。
#[tauri::command]
fn save_llm_settings(
    state: State<'_, AppState>,
    provider: String,
    base_url: String,
    api_key: String,
    model: String,
) -> std::result::Result<LlmSettingsView, String> {
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let provider = match provider.trim() {
        "openai" => "openai",
        "custom" => "custom",
        _ => "deepseek",
    };
    let model = |fallback: &str| {
        let raw = model.trim();
        if raw.is_empty() {
            fallback.to_owned()
        } else {
            raw.to_owned()
        }
    };
    let (base, model) = match provider {
        "openai" => (OPENAI_BASE_URL.to_owned(), model("gpt-4o-mini")),
        "custom" => {
            let base = normalize_base_url(&base_url);
            let base = if base.is_empty() {
                DEFAULT_LLM_BASE_URL.to_owned()
            } else {
                base
            };
            (base, model(DEFAULT_LLM_MODEL))
        }
        _ => (DEFAULT_LLM_BASE_URL.to_owned(), model(DEFAULT_LLM_MODEL)),
    };
    // 预置服务商只接受列表内的模型名，避免手填错字导致每次都调用失败
    let allowed: &[&str] = match provider {
        "openai" => &OPENAI_MODELS,
        "custom" => &[],
        _ => &DEEPSEEK_MODELS,
    };
    let model = if allowed.is_empty() || allowed.contains(&model.as_str()) {
        model
    } else {
        allowed.first().copied().unwrap_or(DEFAULT_LLM_MODEL).to_owned()
    };
    let key = api_key.trim();
    if !key.is_empty() {
        SettingRepo::set(db.conn(), KEY_LLM_API_KEY, key).map_err(|e| e.to_string())?;
    }
    SettingRepo::set(db.conn(), KEY_LLM_PROVIDER, provider).map_err(|e| e.to_string())?;
    SettingRepo::set(db.conn(), KEY_LLM_BASE_URL, &base).map_err(|e| e.to_string())?;
    SettingRepo::set(db.conn(), KEY_LLM_MODEL, &model).map_err(|e| e.to_string())?;
    let config = llm_config_from(&db);
    Ok(llm_settings_view(&config, provider))
}

/// 推荐横排的一条候选：影片 + 该片的短评 + 可能已有的缓存。
struct RecommendPick {
    item: CommentedItem,
    comments: Vec<ScrapedComment>,
    cached: Option<CachedHighlight>,
}

/// 推荐看点：随机挑若干部带短评的影片，把高赞评论总结成一句核心看点。
///
/// 「换一批」优先取还没生成过的条目，确保每批都有新内容；生成过的直接读缓存，
/// 不再请求大模型。大模型不可用时退回本地提炼，不让界面空着。
#[tauri::command]
async fn recommend_highlights(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> std::result::Result<RecommendView, String> {
    let limit = limit.unwrap_or(RECOMMEND_LIMIT).clamp(1, 30);
    let (mut picks, config) = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let config = llm_config_from(&db);
        let candidates = MetadataRepo::commented_items(db.conn()).map_err(|e| e.to_string())?;
        if candidates.is_empty() {
            return Ok(RecommendView {
                items: Vec::new(),
                llm: config.ready(),
                requested: false,
                message: Some(
                    "库内还没有带短评的影片：先在「元数据」里补齐全库，再来看看推荐".into(),
                ),
            });
        }
        let cached = MetadataRepo::cached_highlights(db.conn()).map_err(|e| e.to_string())?;
        // 评论条数变了说明重新刮削过，旧看点已过期，按未生成处理
        let mut fresh: Vec<CommentedItem> = Vec::new();
        let mut known: Vec<(CommentedItem, CachedHighlight)> = Vec::new();
        for item in candidates {
            match cached.get(&item.id.0) {
                // 配置了大模型后，旧的「本地提炼」缓存视为未生成，让下次刷新改用大模型
                Some(highlight)
                    if highlight.comment_count == item.comment_count
                        && !(config.ready() && highlight.source == "local") =>
                {
                    known.push((item, highlight.clone()));
                }
                _ => fresh.push(item),
            }
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5eed);
        shuffle_in_place(&mut fresh, seed);
        shuffle_in_place(&mut known, seed ^ 0x9e37_79b9);

        let take_fresh = fresh.len().min(limit);
        let mut picks: Vec<RecommendPick> = Vec::new();
        for item in fresh.into_iter().take(take_fresh) {
            let comments = MetadataRepo::comments(db.conn(), &item.id).unwrap_or_default();
            picks.push(RecommendPick {
                item,
                comments,
                cached: None,
            });
        }
        for (item, highlight) in known.into_iter().take(limit - take_fresh) {
            let comments = MetadataRepo::comments(db.conn(), &item.id).unwrap_or_default();
            picks.push(RecommendPick {
                item,
                comments,
                cached: Some(highlight),
            });
        }
        (picks, config)
    };

    let mut message = None;
    let mut llm_used = config.ready();
    let mut requested = false;
    if picks.iter().any(|pick| pick.cached.is_none()) {
        // 先把要总结的条目整理成「标题 + 番号 + 高赞评论」，在阻塞线程里一次性请求
        let pending: Vec<usize> = picks
            .iter()
            .enumerate()
            .filter(|(_, pick)| pick.cached.is_none())
            .map(|(idx, _)| idx)
            .collect();
        let payload: Vec<HighlightInput> = pending
            .iter()
            .map(|&idx| {
                let pick = &picks[idx];
                HighlightInput {
                    title: pick.item.title.clone(),
                    code: pick.item.code.clone(),
                    overview: pick.item.overview.clone(),
                    tags: pick.item.tags.clone(),
                    comments: pick.comments.clone(),
                    web: Vec::new(),
                }
            })
            .collect();

        let mut generated: Vec<Option<String>> = vec![None; pending.len()];
        let mut extra_counts: Vec<usize> = vec![0; pending.len()];
        if config.ready() {
            let cfg = config.clone();
            requested = true;
            let result = tauri::async_runtime::spawn_blocking(move || {
                let mut extra_counts: Vec<usize> = Vec::with_capacity(payload.len());
                let enriched: Vec<HighlightInput> = payload
                    .into_iter()
                    .map(|mut input| {
                        // 联网检索：按番号精确过滤，避免把相邻番号的资料喂给模型
                        let query = recommend_search_query(&input.code, &input.title);
                        input.web = if query.is_empty() {
                            Vec::new()
                        } else {
                            search_snippets(&query, input.code.as_deref()).unwrap_or_default()
                        };
                        // 简介、标签与联网结果都算「短评之外的资料」
                        let extra = input.web.len()
                            + usize::from(!input.overview.trim().is_empty())
                            + input.tags.split('、').filter(|tag| !tag.trim().is_empty()).count();
                        extra_counts.push(extra);
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        input
                    })
                    .collect();
                (summarize_highlights(&cfg, &enriched), extra_counts)
            })
            .await;
            match result {
                Ok((Ok(list), counts)) => {
                    generated = list;
                    extra_counts = counts;
                }
                Ok((Err(err), _)) => {
                    message = Some(format!("大模型总结失败，已改用本地提炼：{}", err.reason()));
                }
                Err(err) => message = Some(format!("总结任务异常，已改用本地提炼：{err}")),
            }
        } else {
            llm_used = false;
            message = Some(
                "未配置大模型：当前按高赞评论本地提炼；可在「元数据」里选择服务商并填写 API Key"
                    .into(),
            );
        }

        {
            let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
            for (slot, &idx) in pending.iter().enumerate() {
                let text = generated
                    .get(slot)
                    .and_then(|text| text.clone())
                    .unwrap_or_else(|| fallback_highlight(&picks[idx].comments));
                let source = if generated.get(slot).and_then(|t| t.as_ref()).is_some() {
                    "llm"
                } else {
                    "local"
                };
                let extra_count = if source == "llm" {
                    extra_counts.get(slot).copied().unwrap_or(0)
                } else {
                    0
                };
                let _ = MetadataRepo::save_highlight(
                    db.conn(),
                    &picks[idx].item.id,
                    &text,
                    picks[idx].item.comment_count,
                    extra_count as i64,
                    source,
                    if source == "llm" {
                        Some(config.model.as_str())
                    } else {
                        None
                    },
                );
                picks[idx].cached = Some(CachedHighlight {
                    text,
                    comment_count: picks[idx].item.comment_count,
                    source: source.to_owned(),
                    extra_count: extra_count as i64,
                });
            }
        }
    }

    let mut items: Vec<RecommendItemView> = picks
        .into_iter()
        .filter_map(|pick| {
            let highlight = pick.cached?;
            let text = if highlight.text.trim().is_empty() {
                fallback_highlight(&pick.comments)
            } else {
                highlight.text
            };
            Some(RecommendItemView {
                item_id: pick.item.id.0.clone(),
                title: pick.item.title.clone(),
                code: pick.item.code.clone(),
                rating: pick.item.rating,
                has_poster: pick.item.has_poster,
                highlight: text,
                likes: pick.comments.first().map(|c| c.likes).unwrap_or(0),
                comments: pick.comments.len().min(10),
                sources: highlight.extra_count.max(0) as usize,
                source: highlight.source,
            })
        })
        .collect();
    let order_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1);
    shuffle_in_place(&mut items, order_seed);

    Ok(RecommendView {
        items,
        llm: llm_used,
        requested,
        message,
    })
}

/// 联网检索用的关键词：番号优先（能精确命中同一部片），再补片名与「影评 看点」。
fn recommend_search_query(code: &Option<String>, title: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(code) = code.as_deref().map(str::trim).filter(|code| !code.is_empty()) {
        parts.push(code.to_owned());
    }
    let title = title.trim();
    if !title.is_empty() && !parts.iter().any(|part| part == title) {
        parts.push(title.to_owned());
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.push("影评 看点".to_owned());
    parts.join(" ")
}

/// 本地兜底：评论太短或没有可用句子时，至少给一句原文摘录，避免出现空卡片。
fn fallback_highlight(comments: &[ScrapedComment]) -> String {
    let local = local_highlight(comments);
    if !local.is_empty() {
        return local;
    }
    comments
        .first()
        .map(|comment| comment.content.trim().chars().take(48).collect::<String>())
        .unwrap_or_default()
}

/// 无第三方依赖的洗牌（xorshift）：推荐只需要「每次顺序不同」，不需要密码学随机。
fn shuffle_in_place<T>(slice: &mut [T], seed: u64) {
    let mut state = seed | 1;
    for i in (1..slice.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = (state % (i as u64 + 1)) as usize;
        slice.swap(i, j);
    }
}

fn scrape_state_label(state: ScrapeState) -> &'static str {
    match state {
        ScrapeState::Pending => "pending",
        ScrapeState::Running => "running",
        ScrapeState::Done => "done",
        ScrapeState::Failed => "failed",
        ScrapeState::NoCode => "no_code",
    }
}

/// 本地预告片路径：条目登记了文件名且文件确实存在。
fn trailer_path(item: &MediaItem) -> Option<PathBuf> {
    let name = item.trailer_file.as_deref()?;
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let path = data_dir().ok()?.join("trailers").join(name);
    if path.is_file() {
        Some(path)
    } else {
        None
    }
}

/// 把图片字节包成 data URL（截图与封面共用）。
fn image_data_url(bytes: &[u8]) -> String {
    let mime = if bytes.starts_with(&[0xFF, 0xD8]) {
        "image/jpeg"
    } else if bytes.starts_with(b"\x89PNG") {
        "image/png"
    } else if bytes.starts_with(b"RIFF") {
        "image/webp"
    } else {
        "image/jpeg"
    };
    format!("data:{mime};base64,{}", base64_encode(bytes))
}

/// 在途去重：返回 `false` 表示这条正在刮削（自动与手动可能同时到达）。
fn begin_scrape(inflight: &std::sync::Mutex<std::collections::HashSet<String>>, id: &str) -> bool {
    match inflight.lock() {
        Ok(mut set) => set.insert(id.to_owned()),
        Err(_) => false,
    }
}

fn end_scrape(inflight: &std::sync::Mutex<std::collections::HashSet<String>>, id: &str) {
    if let Ok(mut set) = inflight.lock() {
        set.remove(id);
    }
}

/// 扫描结束后自动刮削该源里「待刮削 / 上次失败」的条目（后台线程，失败静默）。
fn spawn_auto_scrape(state: &AppState, source_id: SourceId) {
    let config = match state.db.lock() {
        Ok(db) => javdb_config_from(&db),
        Err(_) => return,
    };
    let store = Arc::clone(&state.store);
    let client = Arc::clone(&state.javdb);
    let inflight = Arc::clone(&state.scraping);
    let app = state.app.clone();
    std::thread::spawn(move || {
        let Ok(ids) = store.pending_ids(Some(&source_id)) else {
            return;
        };
        let total = ids.len();
        for (idx, id) in ids.iter().enumerate() {
            if !begin_scrape(&inflight, &id.0) {
                continue;
            }
            let outcome = MetadataService::new(client.as_ref(), store.as_ref())
                .scrape_one(id, &config, false);
            end_scrape(&inflight, &id.0);
            // 单条失败不影响同批其余条目：状态已写入库，界面刷新时可见
            let _ = outcome;
            let _ = app.emit(
                "scrape-progress",
                ScrapeProgressView {
                    item_id: id.0.clone(),
                    done: idx + 1,
                    total,
                },
            );
        }
    });
}

fn stop_preview_inner(state: &AppState) {
    if let Ok(mut svc) = state.playback.lock() {
        svc.stop_preview();
    }
    state.factory.set_target(None);
    state.factory.forget_preview();
    if let Ok(hosts) = state.hosts.lock() {
        if let Some(host) = &hosts.preview {
            host.set_click_through(false);
            host.hide();
        }
    }
}

// ---------------------------------------------------------------------------
// 多屏（FR-06）辅助：屏位状态、进度落库、格位生命周期、轮询
// ---------------------------------------------------------------------------

/// 已开启的屏位（升序）。以 L1 治理点为准（额度 / 焦点 / 窗口集合都在那里）。
pub(crate) fn open_slots(state: &AppState) -> Vec<u32> {
    state
        .playback
        .lock()
        .map(|svc| svc.screens())
        .unwrap_or_default()
}

/// 当前「操作目标」屏位：发声窗口优先，否则最小屏位。无窗口时 `None`。
///
/// 多屏时单条命令（暂停 / 跳转 / 字幕）需要一个作用对象；界面点击某格会显式传屏位。
pub(crate) fn current_slot(state: &AppState) -> Option<u32> {
    let slots = open_slots(state);
    if slots.is_empty() {
        return None;
    }
    if let Ok(svc) = state.playback.lock() {
        if let Some(speaker) = svc.speaker() {
            if slots.contains(&speaker) {
                return Some(speaker);
            }
        }
    }
    slots.first().copied()
}

/// 某屏位正在播放的条目。
pub(crate) fn slot_item(state: &AppState, slot: u32) -> Option<ItemId> {
    state
        .screens
        .lock()
        .ok()
        .and_then(|map| map.get(&slot).map(|entry| entry.item_id.clone()))
}

/// 某屏位的流事实（打开播放时探测得到），供实时字幕入口可见性判定。
pub(crate) fn slot_facts(state: &AppState, slot: u32) -> Option<StreamFacts> {
    state
        .screens
        .lock()
        .ok()
        .and_then(|map| map.get(&slot).and_then(|entry| entry.facts.clone()))
}

/// 某屏位在线源（边下边播）的本地缓存文件；本地源为 `None`。
pub(crate) fn slot_stream(state: &AppState, slot: u32) -> Option<PathBuf> {
    state
        .screens
        .lock()
        .ok()
        .and_then(|map| map.get(&slot).and_then(|entry| entry.stream.clone()))
}

fn set_slot_fault(state: &AppState, slot: u32, fault: Option<String>) {
    if let Ok(mut screens) = state.screens.lock() {
        if let Some(entry) = screens.get_mut(&slot) {
            entry.fault = fault;
        }
    }
}

fn set_slot_buffering(state: &AppState, slot: u32, active: bool) {
    if let Ok(mut screens) = state.screens.lock() {
        if let Some(entry) = screens.get_mut(&slot) {
            entry.buffering = active;
        }
    }
}

/// 某格进度落库（按条目维度）。关闭 / 退出前调用。
fn finish_slot_progress(state: &AppState, slot: u32) {
    let Some(item_id) = slot_item(state, slot) else {
        return;
    };
    let Some(core) = state.factory.screen(slot) else {
        return;
    };
    let position = core.position_ms().unwrap_or(0);
    let duration = core.duration_ms();
    let _ = ProgressService::finish(state.store.as_ref(), &item_id, position, duration);
}

/// 关闭某格：发声窗口先行字幕收尾 → 落库进度 → 释放实例与画面窗口 → 同步状态。
fn close_slot(state: &AppState, slot: u32) {
    // 实时字幕的音频抽头绑定在该格实例上：关闭该格即关闭它的字幕会话。
    // 并发发声模式下每格各有一份会话（键为屏位）；否则会话只在「发声窗口」的主上下文。
    let (was_speaker, concurrent) = state
        .playback
        .lock()
        .map(|svc| (svc.speaker() == Some(slot), svc.concurrent_audio()))
        .unwrap_or((false, false));
    if let Ok(mut runtime) = state.subtitles.lock() {
        // 该格的并发上下文（若有）一并收尾
        runtime.close_slot(state.store.as_ref(), slot);
        if !concurrent && was_speaker {
            let _ = runtime.main().service.close(state.store.as_ref());
            runtime.main().target_lang = None;
        }
    }

    finish_slot_progress(state, slot);

    // 在线源：该格关闭即收尾它的下载子进程（硬约束 C5 的「不留孤儿进程」）
    if let Some(stream) = slot_stream(state, slot) {
        state.downloads.stop(&stream);
    }

    if let Ok(mut svc) = state.playback.lock() {
        svc.close_screen(slot);
    }
    state.factory.forget_screen(slot);
    if let Ok(mut hosts) = state.hosts.lock() {
        hosts.screens.remove(&slot);
    }
    if let Ok(mut screens) = state.screens.lock() {
        screens.remove(&slot);
    }
}

/// 关闭全部窗口并收尾实时字幕（单路播放 / 全部停止共用）。
fn close_all_screens(state: &AppState) {
    if let Ok(mut runtime) = state.subtitles.lock() {
        // 主上下文与全部并发格上下文一并收尾
        runtime.close_all(state.store.as_ref());
    }
    for slot in open_slots(state) {
        close_slot(state, slot);
    }
    // 兜底：任何未登记到屏位的下载（例如中途失败重开）也一并收尾
    state.downloads.stop_all();
}

/// 在线格（边下边播）的心跳收尾：让「播放内核位置」跟上「文件下载前沿」。
///
/// 需要纠正的两种情形：
/// 1. **装载失败**：文件还没长出来时 `loadfile` 会失败，mpv 停在空闲态。此时按
///    节流补发一次装载，让下载一开始就接上（用户点播放时文件通常只有几 KB）。
/// 2. **下载前沿 EOF**：mpv 读到「已经下载到的位置」就报 EOF，语义上不是看完。
///    只有文件确实又长大了（字节数增加）才补一次续看，避免同一段结尾被反复重播。
///
/// 下载失败时：文件已有内容就继续放已下载的部分；文件是空的才报故障卡。
fn nudge_online_slot(state: &AppState, slot: u32, stream: &Path, core: &Arc<MpvPlayerCore>) {
    let size = std::fs::metadata(stream).map(|meta| meta.len()).unwrap_or(0);
    match state.downloads.state(stream) {
        DownloadState::Running => {
            set_slot_fault(state, slot, None);
            if core.at_end() {
                let grew = state
                    .screens
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&slot).map(|entry| size > entry.stream_bytes))
                    .unwrap_or(false);
                if grew {
                    mark_slot_stream_bytes(state, slot, size);
                    let back = core
                        .position_ms()
                        .unwrap_or(0)
                        .saturating_sub(500)
                        .max(0);
                    if let Ok(svc) = state.playback.lock() {
                        svc.screen_command(slot, PlayerCmd::Seek { position_ms: back });
                        svc.screen_command(slot, PlayerCmd::Play);
                    }
                }
                return;
            }
            // 空闲（未装载成功）：节流重发 loadfile
            if core.position_ms().is_none() && core.duration_ms().is_none() {
                let due = state
                    .screens
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&slot).and_then(|entry| entry.stream_retry_at))
                    .map(|at| at.elapsed() >= std::time::Duration::from_millis(1500))
                    .unwrap_or(true);
                if due {
                    mark_slot_stream_retry(state, slot);
                    let ref_ = PlayableRef {
                        location: stream.to_string_lossy().into_owned(),
                        headers: Vec::new(),
                    };
                    let _ = core.load(ref_, None);
                }
            }
        }
        DownloadState::Failed(message) => {
            if size == 0 {
                set_slot_fault(state, slot, Some(message));
            }
        }
        DownloadState::Finished | DownloadState::Unknown => {}
    }
}

/// 记下本次重试装载的时间（心跳节流用）。
fn mark_slot_stream_retry(state: &AppState, slot: u32) {
    if let Ok(mut screens) = state.screens.lock() {
        if let Some(entry) = screens.get_mut(&slot) {
            entry.stream_retry_at = Some(std::time::Instant::now());
        }
    }
}

/// 记下「下载前沿 EOF」时观察到的文件字节数。
fn mark_slot_stream_bytes(state: &AppState, slot: u32, bytes: u64) {
    if let Ok(mut screens) = state.screens.lock() {
        if let Some(entry) = screens.get_mut(&slot) {
            entry.stream_bytes = bytes;
        }
    }
}

/// 该格是否应被视为「已播完」。
///
/// 在线源在下载仍在进行时，mpv 报的 EOF 只是「读到了已下载的位置」，不是看完，
/// 因此不能据此收起播放页。
fn slot_ended(state: &AppState, slot: u32, core: &Arc<MpvPlayerCore>) -> bool {
    if !core.at_end() {
        return false;
    }
    match slot_stream(state, slot) {
        Some(stream) => state.downloads.state(&stream) != DownloadState::Running,
        None => true,
    }
}

/// 心跳轮询：按格拉取事件（故障 / 缓冲互不影响），并把每格进度按条目维度落库。
fn poll_screens(state: &AppState) {
    for slot in open_slots(state) {
        let Some(core) = state.factory.screen(slot) else {
            continue;
        };
        let events = core.events();
        while let Some(event) = events.try_next() {
            match event {
                PlayerEvent::DecodeError { message } => {
                    // 在线源装载失败是预期中的（文件尚未长出来）：由 nudge 补发装载，
                    // 不要立刻判故障，否则用户刚点播放就看到一惊一乍的错误卡
                    if slot_stream(state, slot).is_none() {
                        set_slot_fault(state, slot, Some(message));
                    }
                }
                PlayerEvent::Buffering { active } => {
                    set_slot_buffering(state, slot, active);
                }
                _ => {}
            }
        }

        // 在线源（边下边播）：让播放位置跟上下载前沿
        if let Some(stream) = slot_stream(state, slot) {
            nudge_online_slot(state, slot, &stream, &core);
        }

        if let Some(item_id) = slot_item(state, slot) {
            let position = core.position_ms().unwrap_or(0);
            let duration = core.duration_ms();
            let _ = ProgressService::report(state.store.as_ref(), &item_id, position, duration);
        }
    }
}

/// 组装多屏整体状态视图。
fn multi_view(state: &AppState) -> MultiScreenView {
    let (open, speaker, layout, limit, multi, concurrent) = match state.playback.lock() {
        Ok(svc) => (
            svc.screens(),
            svc.speaker(),
            svc.layout(),
            svc.quota().limit(),
            svc.multi_screen_active(),
            svc.concurrent_audio(),
        ),
        Err(_) => (
            Vec::new(),
            None,
            Layout::default(),
            DecodeQuota::DEFAULT_LIMIT,
            false,
            false,
        ),
    };

    let entries: std::collections::BTreeMap<u32, ScreenSlot> = state
        .screens
        .lock()
        .map(|map| map.iter().map(|(k, v)| (*k, v.clone())).collect())
        .unwrap_or_default();

    let mut slots = Vec::with_capacity(open.len());
    for slot in open {
        let Some(entry) = entries.get(&slot) else {
            continue;
        };
        let (playing, position_ms, duration_ms, ended) = match state.factory.screen(slot) {
            Some(core) => (
                true,
                core.position_ms().unwrap_or(0),
                core.duration_ms(),
                slot_ended(state, slot, &core),
            ),
            None => (false, 0, None, false),
        };
        slots.push(MultiSlotView {
            slot,
            item_id: entry.item_id.as_str().to_string(),
            title: entry.title.clone(),
            playing,
            position_ms,
            duration_ms,
            ended,
            speaker: speaker == Some(slot),
            buffering: entry.buffering,
            fault: entry.fault.clone(),
        });
    }

    MultiScreenView {
        slots,
        speaker,
        layout: layout.tag().to_string(),
        layout_label: layout.label().to_string(),
        limit,
        concurrent_audio: concurrent,
        multi_screen: multi,
        preview_available: preview_allowed(multi),
    }
}

// ---------------------------------------------------------------------------
// 入库（FR-01）
// ---------------------------------------------------------------------------

/// 主机模式进入前的窗口状态（只保存主窗口一份）：用于退出时精确还原。
struct ConsoleWindowBackup {
    position: (i32, i32),
    size: (u32, u32),
    maximized: bool,
    decorated: bool,
}

static CONSOLE_WINDOW_BACKUP: Mutex<Option<ConsoleWindowBackup>> = Mutex::new(None);

/// 主机模式：整窗切成无边框全屏（不改变分辨率），退出时还原进入前的窗口状态。
/// 备份只写一次：重复进入不会把「全屏后的窗口」当成原始状态。
#[tauri::command]
fn set_console_fullscreen(
    window: tauri::WebviewWindow,
    enabled: bool,
) -> std::result::Result<(), String> {
    if enabled {
        {
            let mut backup = CONSOLE_WINDOW_BACKUP
                .lock()
                .map_err(|_| "窗口状态锁不可用".to_string())?;
            if backup.is_none() {
                let position = window.outer_position().map_err(|e| e.to_string())?;
                // 存客户区尺寸：`set_size` 的语义是内尺寸，还原时再加回边框才不会越还原越大
                let size = window.inner_size().map_err(|e| e.to_string())?;
                *backup = Some(ConsoleWindowBackup {
                    position: (position.x, position.y),
                    size: (size.width, size.height),
                    maximized: window.is_maximized().unwrap_or(false),
                    decorated: window.is_decorated().unwrap_or(true),
                });
            }
        }
        // 先去掉标题栏再全屏：Windows 下即无边框全屏，不切换显示器分辨率
        window.set_decorations(false).map_err(|e| e.to_string())?;
        window.set_fullscreen(true).map_err(|e| e.to_string())?;
    } else {
        window.set_fullscreen(false).map_err(|e| e.to_string())?;
        let backup = CONSOLE_WINDOW_BACKUP
            .lock()
            .map_err(|_| "窗口状态锁不可用".to_string())?
            .take();
        if let Some(backup) = backup {
            window
                .set_decorations(backup.decorated)
                .map_err(|e| e.to_string())?;
            if backup.maximized {
                window.maximize().map_err(|e| e.to_string())?;
            } else {
                window
                    .set_size(tauri::PhysicalSize::new(backup.size.0, backup.size.1))
                    .map_err(|e| e.to_string())?;
                window
                    .set_position(tauri::PhysicalPosition::new(backup.position.0, backup.position.1))
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn add_local_source(state: State<'_, AppState>, path: String) -> std::result::Result<SourceView, String> {
    // 裸盘符（`D:`）是相对路径，存的、扫的都会变成「进程当前目录」而不是盘根：
    // 入库前归一为 `D:\`，让用户看见的、扫描用的、播放拼回的始终是同一个路径
    let path = normalize_local_root(&path);
    if path.is_empty() {
        return Err("请填写本地目录路径".into());
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("目录不存在：{path}"));
    }

    let src = MediaSource {
        id: source_id_for(&path),
        kind: SourceKind::LocalFolder,
        name: dir_label(&path),
        location: path,
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    };

    let db = state
        .db
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    MediaSourceRepo::upsert(db.conn(), &src).map_err(|e| e.to_string())?;
    Ok(SourceView {
        id: src.id.as_str().to_string(),
        name: src.name,
        location: src.location,
        kind: source_kind_label(src.kind).to_owned(),
        state: "idle".into(),
        // 刚登记的源还没扫描，条目数一定是 0（扫描完成后前端会重新拉清单）
        item_count: 0,
        last_error: None,
        extra_roots: Vec::new(),
    })
}

#[tauri::command]
fn list_sources(state: State<'_, AppState>) -> std::result::Result<Vec<SourceView>, String> {
    let db = state
        .db
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    let sources = MediaSourceRepo::list(db.conn()).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(sources.len());
    for s in sources {
        // 失败原因单独读取（不在 MediaSource 模型里）：只在「扫描失败」时才有意义
        let last_error = MediaSourceRepo::last_error(db.conn(), &s.id).map_err(|e| e.to_string())?;
        out.push(SourceView {
            id: s.id.as_str().to_string(),
            name: s.name,
            location: s.location,
            kind: source_kind_label(s.kind).to_owned(),
            state: match s.state {
                SourceState::Idle => "idle",
                SourceState::Scanning => "scanning",
                SourceState::ScanFailed => "scan_failed",
            }
            .into(),
            item_count: MediaItemRepo::count_by_source(db.conn(), &s.id)
                .map_err(|e| e.to_string())?,
            last_error,
            extra_roots: SourceRootRepo::list(db.conn(), &s.id).map_err(|e| e.to_string())?,
        });
    }
    Ok(out)
}

/// 扫描入库一个源。
///
/// **必须 async + `spawn_blocking`**：扫描是纯磁盘 IO（逐目录 `read_dir` + 逐文件
/// 探测时长），耗时随目录树规模线性增长。Tauri 的同步命令跑在主线程上，一条
/// 「整块硬盘」的源会把界面冻死到扫描结束——这与 PRD「扫描期间可见进度」相冲突。
///
/// 进度经 `scan-progress` 事件推给前端；用户可随时用 `cancel_scan` 中止。
#[tauri::command]
async fn scan_source(
    state: State<'_, AppState>,
    id: String,
) -> std::result::Result<ImportReportView, String> {
    let src = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::get(db.conn(), &SourceId::from(id.clone()))
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("媒体源不存在：{id}"))?
    };

    // 每次扫描从干净的取消位开始：上一次的取消不应影响这一次
    let cancel = Arc::clone(&state.scan_cancel);
    cancel.store(false, Ordering::SeqCst);

    // 合并目录：主根之外登记进来的附加根。只登记不移动文件，条目记绝对路径
    let extra_roots = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        SourceRootRepo::list(db.conn(), &src.id).map_err(|e| e.to_string())?
    };

    let store = Arc::clone(&state.store);
    let prober = state.prober.clone();
    let downloads = Arc::clone(&state.downloads);
    let reporter: Arc<dyn ScanReporter> = Arc::new(EmitScanReporter {
        app: state.app.clone(),
        source_id: src.id.as_str().to_string(),
    });

    // 开跑先报一次「0 进度」：大目录树要几秒才会读到第一个子目录，
    // 界面需要立刻知道「已经开始了」，而不是盯着按钮猜
    reporter.report(&ScanProgress::default());

    let scanned_source = src.id.clone();
    let report = tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&src, prober.as_ref(), &downloads)
            .map_err(|e| e.reason().to_string())?;
        let control = ScanControl {
            reporter: Some(Arc::clone(&reporter)),
            cancel: Some(Arc::clone(&cancel)),
        };
        let report = ImportService::import_with(store.as_ref(), provider.as_ref(), &src, &control)
            .map_err(|e| e.reason().to_string())?;
        let mut discovered = report.stat.discovered;
        let mut stored = report.stored;
        let mut probed = report.probed;
        let mut skipped = report.stat.skipped;
        let mut cancelled = report.cancelled;

        for root in &extra_roots {
            if cancelled {
                break;
            }
            let root_path = PathBuf::from(root);
            let merged: Box<dyn MediaSourceProvider> = match prober.as_ref() {
                Some(p) => Box::new(LocalFolderProvider::with_prober_absolute(
                    &root_path,
                    Arc::clone(p),
                )),
                None => Box::new(LocalFolderProvider::new_absolute(&root_path)),
            };
            // 单个附加目录坏了不扩大为整次扫描失败：跳过它，其余根继续
            if let Ok(part) =
                ImportService::import_with(store.as_ref(), merged.as_ref(), &src, &control)
            {
                discovered += part.stat.discovered;
                stored += part.stored;
                probed += part.probed;
                skipped += part.stat.skipped;
                cancelled = part.cancelled;
            } else {
                skipped += 1;
            }
        }

        Ok::<ImportReportView, String>(ImportReportView {
            discovered,
            stored,
            probed,
            skipped,
            cancelled,
        })
    })
    .await
    .map_err(|e| format!("扫描任务异常中止：{e}"))?;

    // 扫描成功（未被取消）后自动刮削该源的待刮条目：新入库的条目默认 `pending`
    if let Ok(view) = &report {
        if !view.cancelled {
            spawn_auto_scrape(&state, scanned_source);
        }
    }
    report
}

/// 请求中止当前扫描。已入库的条目保留，源不会被置为「扫描失败」。
#[tauri::command]
fn cancel_scan(state: State<'_, AppState>) {
    state.scan_cancel.store(true, Ordering::SeqCst);
}

/// 批量清除入库记录（海报墙「清除所选」）。
///
/// **只删库内的条目记录，绝不删除磁盘上的视频文件**；进度、标签关联、字幕登记
/// 等随条目级联清除，搜索索引同批清理（见 `MediaItemRepo::remove_many`）。
/// 返回实际清除的条数。
///
/// **必须 async + `spawn_blocking`**：一次「全选清除」可能涉及上万条记录，
/// 同步命令跑在主线程会把界面卡住（与 `scan_source` 同一条理由）。
#[tauri::command]
async fn remove_items(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> std::result::Result<usize, String> {
    let items: Vec<ItemId> = ids.into_iter().map(ItemId::from).collect();
    if items.is_empty() {
        return Ok(0);
    }
    let db = Arc::clone(&state.db);
    tauri::async_runtime::spawn_blocking(move || {
        let db = db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaItemRepo::remove_many(db.conn(), &items).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("清除任务异常中止：{e}"))?
}

/// 清除某个媒体源及其全部入库记录（「库管理」的整源清除）。
///
/// 与 [`remove_items`] 同一条边界：**只删库内记录，不动磁盘文件**。
/// 源被一并移除，底部源条不再出现；需要时再点「添加目录」重新入库即可。
#[tauri::command]
async fn clear_source(state: State<'_, AppState>, id: String) -> std::result::Result<usize, String> {
    let db = Arc::clone(&state.db);
    let source_id = SourceId::from(id);
    tauri::async_runtime::spawn_blocking(move || {
        let db = db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        if MediaSourceRepo::get(db.conn(), &source_id)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!("媒体源不存在：{}", source_id.as_str()));
        }
        MediaSourceRepo::remove(db.conn(), &source_id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("清除任务异常中止：{e}"))?
}

/// 把一个目录合并进已有媒体源：只登记为附加根，**不移动、不复制磁盘文件**。
///
/// 扫描时主根与附加根都会遍历；附加根下的条目记录绝对路径，因此播放不依赖主根。
#[tauri::command]
fn add_source_root(
    state: State<'_, AppState>,
    id: String,
    path: String,
) -> std::result::Result<Vec<String>, String> {
    let path = normalize_local_root(&path);
    if path.is_empty() {
        return Err("请填写要合并的目录".into());
    }
    if !std::path::Path::new(&path).is_dir() {
        return Err(format!("目录不存在：{path}"));
    }
    let source_id = SourceId::from(id);
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let src = MediaSourceRepo::get(db.conn(), &source_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("媒体源不存在：{}", source_id.as_str()))?;

    // 主根与附加根互相包含时，同一批文件会以两种稳定键各入一次库：直接拒绝
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let main = norm(&src.location);
    let extra = norm(&path);
    if main == extra || main.starts_with(&format!("{extra}\\")) || extra.starts_with(&format!("{main}\\")) {
        return Err("该目录与主目录相互包含，无需合并".into());
    }

    SourceRootRepo::add(db.conn(), &source_id, &path).map_err(|e| e.to_string())?;
    SourceRootRepo::list(db.conn(), &source_id).map_err(|e| e.to_string())
}

/// 解除一个附加根的登记，并清除该目录下已入库的条目（只删库内记录，不动硬盘文件）。
#[tauri::command]
async fn remove_source_root(
    state: State<'_, AppState>,
    id: String,
    path: String,
) -> std::result::Result<usize, String> {
    let db = Arc::clone(&state.db);
    let source_id = SourceId::from(id);
    tauri::async_runtime::spawn_blocking(move || {
        let db = db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let ids = SourceRootRepo::item_ids_under(db.conn(), &source_id, &path)
            .map_err(|e| e.to_string())?;
        let items: Vec<ItemId> = ids.iter().map(|id| ItemId::from(id.as_str())).collect();
        let removed = MediaItemRepo::remove_many(db.conn(), &items).map_err(|e| e.to_string())?;
        SourceRootRepo::remove(db.conn(), &source_id, &path).map_err(|e| e.to_string())?;
        Ok(removed)
    })
    .await
    .map_err(|e| format!("解除合并异常中止：{e}"))?
}

/// 把若干视频文件添加到已有媒体源（只登记，不移动文件）。
#[tauri::command]
async fn add_videos_to_source(
    state: State<'_, AppState>,
    id: String,
    paths: Vec<String>,
) -> std::result::Result<ImportReportView, String> {
    let source_id = SourceId::from(id);
    let src = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::get(db.conn(), &source_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("媒体源不存在：{}", source_id.as_str()))?
    };
    let files: Vec<std::path::PathBuf> = paths
        .into_iter()
        .map(|p| std::path::PathBuf::from(p))
        .filter(|p| p.is_file())
        .collect();
    if files.is_empty() {
        return Err("没有可添加的视频文件".into());
    }

    let cancel = Arc::clone(&state.scan_cancel);
    cancel.store(false, Ordering::SeqCst);
    let store = Arc::clone(&state.store);
    let prober = state.prober.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let provider: Box<dyn MediaSourceProvider> = match prober.as_ref() {
            Some(p) => Box::new(FileListProvider::with_prober(files, Arc::clone(p))),
            None => Box::new(FileListProvider::new(files)),
        };
        let control = ScanControl {
            reporter: None,
            cancel: Some(cancel),
        };
        let report = ImportService::import_with(store.as_ref(), provider.as_ref(), &src, &control)
            .map_err(|e| e.reason().to_string())?;
        Ok::<ImportReportView, String>(ImportReportView {
            discovered: report.stat.discovered,
            stored: report.stored,
            probed: report.probed,
            skipped: report.stat.skipped,
            cancelled: report.cancelled,
        })
    })
    .await
    .map_err(|e| format!("添加视频异常中止：{e}"))?
}

/// 全库删除一个标签取值：所有影片上的该标签一并移除，搜索索引同步重建。
#[tauri::command]
fn delete_dimension_tag(
    state: State<'_, AppState>,
    tag_id: String,
) -> std::result::Result<usize, String> {
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let affected = TagRepo::delete_value(db.conn(), &tag_id).map_err(|e| e.to_string())?;
    for id in &affected {
        SearchIndexRepo::reindex_item(db.conn(), &ItemId::from(id.as_str()))
            .map_err(|e| e.to_string())?;
    }
    Ok(affected.len())
}

// ---------------------------------------------------------------------------
// 联网导入（M7）：粘贴列表页 URL → 登记为一个在线站点源并立即扫描一次
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnlineImportView {
    source_id: String,
    /// 由 URL 主机名派生的源显示名（如 `hanime1.me`）。
    source_name: String,
    discovered: u64,
    stored: u64,
    probed: u64,
    /// 本次实际处理的页数（未指定页码范围时为 1）。
    pages: u32,
    /// 导入失败的页码（仅页码范围导入时可能非空；未指定范围时为空）。
    failed_pages: Vec<u32>,
}

/// 联网导入：把粘贴的列表页 URL 登记为在线站点源并扫描。
///
/// 给出 `start_page` / `end_page` 时按页码自动翻页，连续导入这几页，全部条目
/// 登记到**同一个源**（源 id 由去掉 `page` 参数的地址派生）；不给出时只导入地址
/// 这一页，沿用原有行为。
///
/// 同一 URL 重复导入会命中同一源 id并幂等合并条目，
/// 封面以「文件是否存在」判断是否重新抓取，不会重复下载。
///
/// yt-dlp 缺失时**在开子进程之前**就返回可读提示，避免用户只看到晦涩的子进程错误。
#[tauri::command]
fn online_import(
    state: State<'_, AppState>,
    url: String,
    start_page: Option<u32>,
    end_page: Option<u32>,
) -> std::result::Result<OnlineImportView, String> {
    let raw = url.trim().to_string();
    if !raw.starts_with("http://") && !raw.starts_with("https://") {
        return Err("请填写以 http(s) 开头的列表页地址".into());
    }

    // 源身份与源地址都不带分页参数：翻页导入的是同一个源的不同页
    let base = strip_page_param(&raw);
    let pages: Vec<(Option<u32>, String)> = match (start_page, end_page) {
        (None, None) => vec![(None, raw.clone())],
        (start, end) => {
            let start = start.unwrap_or(1).max(1);
            let end = end.unwrap_or(start);
            if end < start {
                return Err("结束页不能小于起始页".into());
            }
            if end - start + 1 > MAX_IMPORT_PAGES {
                return Err(format!("一次最多展开 {MAX_IMPORT_PAGES} 页，请缩小页码范围"));
            }
            (start..=end)
                .map(|page| {
                    // 第 1 页直接用原地址：站点未必接受显式的 page=1
                    let target = if page == 1 {
                        base.clone()
                    } else {
                        page_url(&base, page)
                    };
                    (Some(page), target)
                })
                .collect()
        }
    };

    let ytdlp = ytdlp_path()?;
    let covers = poster_dir()?;

    let src = MediaSource {
        id: source_id_for(&base),
        kind: SourceKind::OnlineSite,
        name: site_label(&base),
        location: base.clone(),
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    };
    {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::upsert(db.conn(), &src).map_err(|e| e.to_string())?;
    }

    let provider = OnlineSiteProvider::new(ytdlp, covers, streams_dir()?, Arc::clone(&state.downloads));
    let mut discovered = 0u64;
    let mut stored = 0u64;
    let mut probed = 0u64;
    let mut succeeded = 0u64;
    let mut failed_pages: Vec<u32> = Vec::new();
    let mut first_error: Option<String> = None;

    for (page_no, target) in &pages {
        // 每页都换成一个同 id、只在 location 上不同的临时源：条目 id 由详情页地址
        // 派生，所以跨页重复的条目会幂等合并，不会因为翻页产生第二份
        let page_src = MediaSource {
            location: target.clone(),
            ..src.clone()
        };
        match ImportService::import(state.store.as_ref(), &provider, &page_src) {
            Ok(report) => {
                discovered += report.stat.discovered;
                stored += report.stored;
                probed += report.probed;
                succeeded += 1;
            }
            Err(err) => {
                if let Some(page) = page_no {
                    failed_pages.push(*page);
                }
                if first_error.is_none() {
                    first_error = Some(err.reason().to_string());
                }
            }
        }
    }

    // 单页导入沿用原行为；多页时最后一页的结局会覆盖源记录，这里统一还原：
    // 地址回到不带分页参数的列表页，状态回到 idle（部分页失败但整体有成功时，
    // 不在源清单上显示「扫描失败」），并清掉失败原因
    let existing = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::get(db.conn(), &src.id).map_err(|e| e.to_string())?
    };
    let restore = MediaSource {
        state: if succeeded > 0 {
            SourceState::Idle
        } else {
            existing.as_ref().map_or(SourceState::Idle, |s| s.state)
        },
        last_scan_at: existing.and_then(|s| s.last_scan_at),
        ..src.clone()
    };
    {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::upsert(db.conn(), &restore).map_err(|e| e.to_string())?;
    }
    if succeeded == 0 {
        return Err(first_error.unwrap_or_else(|| "导入失败".to_string()));
    }
    state
        .store
        .set_source_error(&src.id, None)
        .map_err(|e| e.to_string())?;

    // 导入成功就顺手在后台把每一页的封面各抓一遍：封面来自列表页卡片，
    // 与条目枚举共用同一页抓取，因此此刻抓最省事；用户若只是「导入完就退出」，
    // 封面也不会等到下次浏览才补
    spawn_cover_fetch_pages(
        &state,
        &src,
        pages.iter().map(|(_, target)| target.clone()).collect(),
    );

    // 列表页只给标题与详情页地址：字段、评论与截图在后台逐条补齐，不阻塞本次导入
    spawn_online_scrape(&state, &src.id);

    Ok(OnlineImportView {
        source_id: src.id.as_str().to_string(),
        source_name: src.name,
        discovered,
        stored,
        probed,
        pages: pages.len() as u32,
        failed_pages,
    })
}

/// 取条目的封面字节，编码为 data URL 交给前端 `<img>`。
///
/// **本命令只读本地文件、绝不联网**：在线源的封面由 [`spawn_cover_fetch`] 在后台
/// 从**列表页卡片**下载（见 L3 `fetch_covers`——详情页的 `--write-thumbnail` 拿到的是
/// 视频帧，不是封面 art）。同步命令跑在主线程上，任何等待都会冻结界面，因此这里
/// 命中就返回字节，没命中就返回空串让前端稍后重试（同时顺手挂起一次后台抓取）。
///
/// 抓取失败（站点不可达、yt-dlp 缺失、封面本就没有）一律返回空串——这不是错误。
#[tauri::command]
fn item_cover(state: State<'_, AppState>, item_id: String) -> std::result::Result<String, String> {
    let (item, src) = load_item(&state, &item_id)?;
    let Some(poster) = item.poster_id.as_deref() else {
        return Ok(String::new());
    };
    // 边界校验：poster_id 只应是海报目录下的文件名，不接受任何路径分隔
    if poster.contains('/') || poster.contains('\\') || poster.contains("..") {
        return Ok(String::new());
    }
    let path = poster_dir()?.join(poster);
    if !path.is_file() {
        // 后台按「源」补抓：本次返回空串，前端稍后重试
        spawn_cover_fetch(&state, &src);
        return Ok(String::new());
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(String::new()),
    };
    Ok(format!("data:image/jpeg;base64,{}", base64_encode(&bytes)))
}

/// 在后台为某个在线源抓一遍列表页封面（同一源在途时只抓一次）。任何失败都静默忽略。
///
/// **按源而不是按条目去重**：一次列表页抓取就包含该页**所有**卡片的封面，所以第一张
/// 封面缺失时触发一次就够（其余条目的请求命中在途表直接返回）。保留在途登记而不是
/// 抓完就删，是为了避免「封面本来就没有」的条目被前端反复重试时把列表页反复抓一遍；
/// 超过时限（视为那次抓取没成功）才允许再试。
///
/// 这也是**重启后的自愈路径**：覆盖「封面文件被清掉 / 换机导入」等情况——用户一打开
/// 库、第一张卡片请求封面，就会触发一次补抓。
fn spawn_cover_fetch(state: &AppState, src: &MediaSource) {
    spawn_cover_fetch_pages(state, src, vec![src.location.clone()]);
}

/// 在后台为**一组列表页**各抓一遍封面（同一源在途时只抓一次）。任何失败都静默忽略。
///
/// 多页导入时一页的封面并不全在源地址那一页上，因此这里按页循环；在途去重仍按源，
/// 一次导入的后台任务会覆盖它展开的所有页。
fn spawn_cover_fetch_pages(state: &AppState, src: &MediaSource, pages: Vec<String>) {
    if src.kind != SourceKind::OnlineSite {
        return;
    }
    let pages: Vec<String> = pages
        .into_iter()
        .map(|page| page.trim().to_owned())
        .filter(|page| !page.is_empty())
        .collect();
    if pages.is_empty() {
        return;
    }
    // 条目超过这个时限仍在途，视为那次抓取没成功，允许再试一次
    const INFLIGHT_TTL: std::time::Duration = std::time::Duration::from_secs(300);
    let key = src.id.as_str().to_owned();
    {
        let Ok(mut inflight) = state.covers_inflight.lock() else {
            return;
        };
        let now = std::time::Instant::now();
        inflight.retain(|_, at| now.duration_since(*at) < INFLIGHT_TTL);
        if inflight.contains_key(&key) {
            return;
        }
        inflight.insert(key, now);
    }

    let (Ok(ytdlp), Ok(covers), Ok(streams)) = (ytdlp_path(), poster_dir(), streams_dir()) else {
        return;
    };
    let downloads = Arc::clone(&state.downloads);
    std::thread::spawn(move || {
        // 只抓封面：不会起影片下载（`downloads` 仅为构造提供者所需）
        let provider = OnlineSiteProvider::new(ytdlp, covers, streams, downloads);
        for page in pages {
            provider.fetch_covers(&page);
        }
    });
}

// ---------------------------------------------------------------------------
// 联网条目详情补齐（M7）：列表页只有标题与详情页地址，字段 / 评论 / 截图在后台逐条抓
// ---------------------------------------------------------------------------

/// 联网条目的视频 id（详情页地址里的 `v=NNNNN`），用作截图文件名。
fn online_video_id(url: &str) -> Option<String> {
    let at = url.find("v=")?;
    let digits: String = url[at + 2..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    (!digits.is_empty()).then_some(digits)
}

/// 抓一条联网条目的详情页字段 + 评论 + 一张预览截图并入库。
///
/// 返回 `(评论数, 截图数)`。截图失败只静默跳过（站点不保证帧图可下载），
/// 其余任何错误都会把条目标成 `Failed` 并记录原因，供界面显示与下次重试。
fn scrape_online_item(
    store: &DbStore,
    provider: &OnlineSiteProvider,
    id: &ItemId,
) -> std::result::Result<(usize, usize), String> {
    match scrape_online_item_inner(store, provider, id) {
        Ok(counts) => Ok(counts),
        Err(reason) => {
            let _ = store.set_scrape_state(id, ScrapeState::Failed, Some(&reason));
            Err(reason)
        }
    }
}

fn scrape_online_item_inner(
    store: &DbStore,
    provider: &OnlineSiteProvider,
    id: &ItemId,
) -> std::result::Result<(usize, usize), String> {
    let mut item = store.item(id).map_err(|err| err.reason().to_owned())?;
    let url = item
        .url
        .clone()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("条目 {} 缺少详情页地址", id.0))?;

    let detail = scrape_detail_with_retry(provider, &url, &mut |message| {
        println!("[联网元数据] {url} 抓取失败，准备重试：{message}");
    })
    .map_err(|err| err.reason().to_owned())?;

    if !detail.title.trim().is_empty() {
        item.title = detail.title.trim().to_owned();
    }
    if let Some(overview) = detail.overview.filter(|value| !value.trim().is_empty()) {
        item.overview = Some(overview);
    }
    if let Some(duration_ms) = detail.duration_ms {
        item.duration_ms = Some(duration_ms);
    }
    if let Some(date) = detail.release_date.filter(|value| !value.is_empty()) {
        // 年份从发行日期前 4 位取；解析不出来就保留已有值
        item.year = date
            .get(0..4)
            .and_then(|year| year.parse::<i32>().ok())
            .or(item.year);
        item.release_date = Some(date);
    }
    if let Some(code) = detail.external_code.filter(|value| !value.trim().is_empty()) {
        item.external_code = Some(code);
    }
    item.detail_url = Some(url.clone());
    item.scraped_at = Some(now_ms());
    item.scrape_state = ScrapeState::Done;
    item.scrape_error = None;
    item.identify_state = IdentifyState::Identified;
    item.meta_state = MetaState::Complete;
    store
        .save_scraped(&item)
        .map_err(|err| err.reason().to_owned())?;

    // 类别与标签合并后统一写入「类别」维度（站点把二者混排展示）
    let mut categories: Vec<String> = Vec::new();
    for value in detail.categories.iter().chain(detail.tags.iter()) {
        let text = value.trim();
        if !text.is_empty() && !categories.iter().any(|existing| existing == text) {
            categories.push(text.to_owned());
        }
    }
    if !categories.is_empty() {
        store
            .replace_metadata_tags(id, DIM_CATEGORY, &categories)
            .map_err(|err| err.reason().to_owned())?;
    }
    if let Some(author) = detail
        .author
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        store
            .replace_metadata_tags(id, DIM_MAKER, &[author.to_owned()])
            .map_err(|err| err.reason().to_owned())?;
    }
    // 年份：从发行日期（或已有年份）取 `2015年` 形式的取值
    if let Some(label) = year_label_from(item.release_date.as_deref(), item.year) {
        store
            .replace_metadata_tags(id, DIM_YEAR, &[label])
            .map_err(|err| err.reason().to_owned())?;
    }

    // 截图：每个条目只有一张 `og:image` 视频帧；失败不算整体失败
    let mut screenshots = 0usize;
    if let Some(frame_url) = detail
        .frame_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Some(video_id) = online_video_id(&url) {
            if let Ok(bytes) = provider.fetch_image_bytes(frame_url) {
                let file_name = format!("{video_id}h.jpg");
                let written = store.write_screenshot(id, &file_name, &bytes).is_ok();
                let replaced = store
                    .replace_screenshots(
                        id,
                        &[ScrapedImage {
                            file_name: file_name.clone(),
                            source_url: Some(frame_url.to_owned()),
                        }],
                    )
                    .is_ok();
                if written && replaced {
                    screenshots = 1;
                }
            }
        }
    }

    let comment_count = detail.comments.len();
    store
        .replace_comments(id, &detail.comments)
        .map_err(|err| err.reason().to_owned())?;
    store
        .reindex_item(id)
        .map_err(|err| err.reason().to_owned())?;
    store
        .set_scrape_state(id, ScrapeState::Done, None)
        .map_err(|err| err.reason().to_owned())?;
    Ok((comment_count, screenshots))
}

/// 详情页抓取的重试策略。
///
/// 目标站点的 CDN 在连续请求时会**间歇性掐断 TLS**（`SSL_ERROR_SYSCALL`、
/// `curl: (35)`），这不是代码问题，隔几秒重试通常就能过。只在网络类错误上重试；
/// 解析类（页面 404 / DOM 变了）重试无意义，直接失败。
fn scrape_detail_with_retry(
    provider: &OnlineSiteProvider,
    url: &str,
    log: &mut dyn FnMut(&str),
) -> PortResult<OnlineDetail> {
    const DELAYS_MS: [u64; 3] = [0, 3000, 8000];
    let mut last = None;
    for (attempt, delay) in DELAYS_MS.iter().enumerate() {
        if *delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(*delay));
        }
        match provider.scrape_detail(url) {
            Ok(detail) => return Ok(detail),
            Err(err) => {
                let message = err.reason().to_owned();
                if !is_transient_network_error(&message) {
                    return Err(err);
                }
                log(&message);
                last = Some(err);
                let _ = attempt;
            }
        }
    }
    Err(last.unwrap_or_else(|| PortError::Backend(format!("未能获取详情页：{url}"))))
}

/// 是否为「重试可能成功」的网络类错误（TLS 掐断 / 连接重置 / 超时 / 403）。
fn is_transient_network_error(message: &str) -> bool {
    [
        "SSL",
        "BoringSSL",
        "Connection closed",
        "Connection reset",
        "Connection aborted",
        "timed out",
        "timeout",
        "curl: (35)",
        "curl: (56)",
        "Unable to download webpage",
        "HTTP Error 403",
        "HTTP Error 429",
        "HTTP Error 5",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// 在后台逐条补齐某个在线源的条目（顺序处理，避免把站点反爬触发到极限）。
///
/// 线程内不持有 `State` 引用：先把需要的东西克隆出来，命令才能立刻返回。
fn spawn_online_scrape(state: &AppState, source_id: &SourceId) {
    let (Ok(ytdlp), Ok(covers), Ok(streams)) = (ytdlp_path(), poster_dir(), streams_dir()) else {
        println!("[联网元数据] 缺少 yt-dlp 或缓存目录，已跳过详情补齐");
        return;
    };
    let store = Arc::clone(&state.store);
    let app = state.app.clone();
    let downloads = Arc::clone(&state.downloads);
    let source = source_id.clone();
    std::thread::spawn(move || {
        let provider = OnlineSiteProvider::new(ytdlp, covers, streams, downloads);
        let ids = match store.pending_ids(Some(&source)) {
            Ok(ids) => ids,
            Err(err) => {
                println!("[联网元数据] 读取待抓条目失败：{}", err.reason());
                return;
            }
        };
        let total = ids.len();
        let mut failed = 0usize;
        for (index, id) in ids.iter().enumerate() {
            if scrape_online_item(store.as_ref(), &provider, id).is_err() {
                failed += 1;
            }
            let _ = app.emit(
                "scrape-progress",
                ScrapeProgressView {
                    item_id: id.0.clone(),
                    done: index + 1,
                    total,
                },
            );
            // 条目之间留出间隔：连续请求会被站点的 Cloudflare 判为爬虫（TLS 直接掐断）
            if index + 1 < total {
                std::thread::sleep(std::time::Duration::from_millis(1500));
            }
        }
        let done = total - failed;
        println!("[联网元数据] 完成 {done} 条，失败 {failed} 条");
        let _ = app.emit(
            "online-meta-done",
            serde_json::json!({
                "sourceId": source.as_str(),
                "done": done,
                "failed": failed
            }),
        );
    });
}

// ---------------------------------------------------------------------------
// 联网视频下载（完整落盘 + 进度）
// ---------------------------------------------------------------------------

/// 更新一条下载任务的快照并推给界面。
fn publish_download(
    registry: &Mutex<std::collections::HashMap<String, OnlineDownloadView>>,
    app: &AppHandle,
    view: OnlineDownloadView,
) {
    if let Ok(mut map) = registry.lock() {
        map.insert(view.item_id.clone(), view.clone());
    }
    let _ = app.emit("online-download-progress", view);
}

/// 下载目录里是否已有该视频。
///
/// 文件名里的 id 是 yt-dlp 自己算出来的：generic 提取器把详情页地址的尾段整段当 id，
/// 实际落盘名称形如 `标题 [watch？v=143645-1].mp4`（`--windows-filenames` 还会把
/// `?` 写成全角 `？`）。因此不能只找 `[143645]` —— 那样永远匹配不上，
/// 每次点下载都会重新下一遍。
fn existing_download(dir: &Path, video_id: &str) -> Option<PathBuf> {
    if video_id.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // 未完成的 `.part` / `.ytdl` 不算已下载，交给 `--continue` 续传
        if name.ends_with(".part") || name.ends_with(".ytdl") {
            continue;
        }
        if !matches_video_id(&name, video_id) {
            continue;
        }
        // 只有几 KB 的残片不能当「已下载」，否则用户永远补不全
        if entry.metadata().map(|meta| meta.len()).unwrap_or(0) < 1024 * 1024 {
            continue;
        }
        return Some(path);
    }
    None
}

/// 文件名里是否含该视频 id：兼容 `[143645]`、`v=143645`、`v？143645`（全角问号）
/// 三种写法，并要求 id 后面不是数字，避免 `14365` 误命中 `143650`。
fn matches_video_id(name: &str, video_id: &str) -> bool {
    if name.contains(&format!("[{video_id}]")) {
        return true;
    }
    for sep in ["v=", "v？"] {
        let mut rest = name;
        while let Some(at) = rest.find(sep) {
            let after = &rest[at + sep.len()..];
            if let Some(tail) = after.strip_prefix(video_id) {
                if tail.chars().next().map_or(true, |c| !c.is_ascii_digit()) {
                    return true;
                }
            }
            rest = after;
        }
    }
    false
}

/// 跑完一条下载（在后台线程里调用），把终态写回任务表。
fn run_download(
    provider: &OnlineSiteProvider,
    registry: &Mutex<std::collections::HashMap<String, OnlineDownloadView>>,
    app: &AppHandle,
    item_id: &str,
    title: &str,
    url: &str,
    dir: &Path,
) {
    let mut base = OnlineDownloadView {
        item_id: item_id.to_owned(),
        title: title.to_owned(),
        state: "running".into(),
        percent: 0.0,
        dest: None,
        error: None,
    };

    // 已经下过（文件在下载目录里）就不再重复跑一遍
    if let Some(path) = existing_download(dir, &online_video_id(url).unwrap_or_default()) {
        base.state = "done".into();
        base.percent = 100.0;
        base.dest = Some(path.to_string_lossy().into_owned());
        publish_download(registry, app, base);
        return;
    }

    let mut last_percent = -1.0f64;
    let result = provider.download_to_dir(url, dir, |percent| {
        // 整数百分比变化才发事件：yt-dlp 每秒几十行，全发会把 webview 打满
        if percent.floor() > last_percent {
            last_percent = percent.floor();
            let mut view = base.clone();
            view.percent = percent;
            publish_download(registry, app, view);
        }
    });
    match result {
        Ok(path) => {
            base.state = "done".into();
            base.percent = 100.0;
            base.dest = Some(path.to_string_lossy().into_owned());
            println!("[联网下载] 完成：{}", base.dest.as_deref().unwrap_or(""));
        }
        Err(err) => {
            base.state = "failed".into();
            base.error = Some(err.reason().to_owned());
            println!("[联网下载] 失败：{}", err.reason());
        }
    }
    publish_download(registry, app, base);
}

/// 起一个后台线程顺序下载一批视频（站点反爬，绝不并发）。
fn spawn_online_downloads(
    state: &AppState,
    jobs: Vec<(String, String, String)>,
) -> std::result::Result<usize, String> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let dir = download_dir()?;
    let (Ok(ytdlp), Ok(covers), Ok(streams)) = (ytdlp_path(), poster_dir(), streams_dir()) else {
        return Err("未找到 yt-dlp 或缓存目录不可用".to_string());
    };
    let registry = Arc::clone(&state.online_downloads);
    let app = state.app.clone();
    let downloads = Arc::clone(&state.downloads);

    // 下载目录里已有完整文件的条目直接标完成：绝不重新下一遍
    let mut jobs = jobs;
    jobs.retain(|(id, title, url)| {
        let video_id = online_video_id(url).unwrap_or_default();
        let Some(path) = existing_download(&dir, &video_id) else {
            return true;
        };
        publish_download(
            registry.as_ref(),
            &app,
            OnlineDownloadView {
                item_id: id.clone(),
                title: title.clone(),
                state: "done".into(),
                percent: 100.0,
                dest: Some(path.to_string_lossy().into_owned()),
                error: None,
            },
        );
        false
    });
    if jobs.is_empty() {
        return Ok(0);
    }
    let count = jobs.len();
    // 先登记为「排队中」，界面立刻能看到整批任务
    for (id, title, _) in &jobs {
        publish_download(
            registry.as_ref(),
            &app,
            OnlineDownloadView {
                item_id: id.clone(),
                title: title.clone(),
                state: "running".into(),
                percent: 0.0,
                dest: None,
                error: None,
            },
        );
    }
    std::thread::spawn(move || {
        let provider = OnlineSiteProvider::new(ytdlp, covers, streams, downloads);
        for (index, (id, title, url)) in jobs.iter().enumerate() {
            run_download(&provider, registry.as_ref(), &app, id, title, url, &dir);
            if index + 1 < jobs.len() {
                std::thread::sleep(std::time::Duration::from_millis(1200));
            }
        }
        let _ = app.emit("online-download-done", serde_json::json!({ "count": count }));
    });
    Ok(count)
}

/// 下载单条联网视频到「视频下载目录」。
#[tauri::command]
fn start_online_download(
    state: State<'_, AppState>,
    item_id: String,
) -> std::result::Result<OnlineDownloadView, String> {
    if let Ok(map) = state.online_downloads.lock() {
        if let Some(view) = map.get(&item_id) {
            // 下载中：直接回任务快照
            if view.state == "running" {
                return Ok(view.clone());
            }
            // 已完成：只有文件还在才算数（用户删了文件就该允许重新下）
            if view.state == "done" {
                let alive = view
                    .dest
                    .as_deref()
                    .map(|dest| Path::new(dest).is_file())
                    .unwrap_or(false);
                if alive {
                    return Ok(view.clone());
                }
            }
        }
    }
    let (item, src) = load_item(&state, &item_id)?;
    if src.kind != SourceKind::OnlineSite {
        return Err("只有联网导入的条目支持下载视频".to_string());
    }
    let url = item
        .url
        .clone()
        .ok_or_else(|| "该条目缺少详情页地址".to_string())?;

    // 下载目录里已有完整文件：标完成返回，不再启动 yt-dlp
    let dir = download_dir()?;
    if let Some(path) = existing_download(&dir, &online_video_id(&url).unwrap_or_default()) {
        let view = OnlineDownloadView {
            item_id: item.id.0.clone(),
            title: item.title.clone(),
            state: "done".into(),
            percent: 100.0,
            dest: Some(path.to_string_lossy().into_owned()),
            error: None,
        };
        publish_download(state.online_downloads.as_ref(), &state.app, view.clone());
        return Ok(view);
    }

    spawn_online_downloads(&state, vec![(item.id.0.clone(), item.title.clone(), url)])?;
    Ok(OnlineDownloadView {
        item_id: item.id.0.clone(),
        title: item.title,
        state: "running".into(),
        percent: 0.0,
        dest: None,
        error: None,
    })
}

/// 下载某个在线源的全部条目（跳过已下载过的），返回入队条数。
#[tauri::command]
fn download_online_source(
    state: State<'_, AppState>,
    source_id: String,
) -> std::result::Result<usize, String> {
    let scope = SourceId::from(source_id);
    let jobs = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let src = MediaSourceRepo::get(db.conn(), &scope)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("媒体源不存在：{}", scope))?;
        if src.kind != SourceKind::OnlineSite {
            return Err("只有联网导入的媒体源支持下载视频".to_string());
        }
        MediaItemRepo::list(db.conn(), Some(&scope))
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter_map(|item| {
                item.url
                    .clone()
                    .map(|url| (item.id.0.clone(), item.title.clone(), url))
            })
            .collect::<Vec<_>>()
    };
    spawn_online_downloads(&state, jobs)
}

/// 当前全部下载任务（界面打开时拉一次，之后靠事件增量更新）。
#[tauri::command]
fn online_download_status(state: State<'_, AppState>) -> Vec<OnlineDownloadView> {
    state
        .online_downloads
        .lock()
        .map(|map| map.values().cloned().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 数据可迁移（FR-07）：导出库到目录 / 从目录导入库
// ---------------------------------------------------------------------------

#[tauri::command]
fn export_library(
    state: State<'_, AppState>,
    dest: String,
) -> std::result::Result<PortabilityReportView, String> {
    let dest = dest.trim().to_string();
    if dest.is_empty() {
        return Err("请填写导出目录".into());
    }
    let outcome = PortabilityService::export(state.store.as_ref(), &dest)
        .map_err(|e| e.reason().to_string())?;
    Ok(PortabilityReportView {
        sources: outcome.sources,
        items: outcome.items,
        items_missing: 0,
        user_tags: outcome.user_tags,
        collections: outcome.collections,
        settings: outcome.settings,
        subtitle_tracks: outcome.subtitle_tracks,
        restored_progress: 0,
    })
}

#[tauri::command]
fn import_library(
    state: State<'_, AppState>,
    src: String,
) -> std::result::Result<PortabilityReportView, String> {
    let src = src.trim().to_string();
    if src.is_empty() {
        return Err("请填写导出包目录".into());
    }
    let outcome = PortabilityService::import(state.store.as_ref(), &src)
        .map_err(|e| e.reason().to_string())?;
    Ok(PortabilityReportView {
        sources: outcome.sources,
        items: outcome.items,
        items_missing: outcome.items_missing,
        user_tags: outcome.user_tags,
        collections: outcome.collections,
        settings: outcome.settings,
        subtitle_tracks: outcome.subtitle_tracks,
        restored_progress: outcome.restored_progress.len(),
    })
}

// ---------------------------------------------------------------------------
// 海报墙与检索（FR-02 / FR-03）
// ---------------------------------------------------------------------------

/// 海报墙与检索结果共用同一个查询入口：空查询就是「全部条目」。
///
/// 组合筛选与排序全部由 L1 [`SearchService`] 走 L2 规则完成，
/// 本函数只负责把结果翻译成界面视图。
#[tauri::command]
fn search_items(
    state: State<'_, AppState>,
    query: SearchQueryView,
) -> std::result::Result<Vec<ItemView>, String> {
    let hits = SearchService::search(
        state.store.as_ref(),
        &SearchQuery {
            text: query.text,
            filters: query.filters.into_iter().collect(),
            sort: SortMode::from_tag(&query.sort),
        },
    )
    .map_err(|e| e.reason().to_string())?;

    // 缺失标记一次取全量，避免逐条目回查数据库
    let missing: std::collections::BTreeSet<String> = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaItemRepo::missing_ids(db.conn())
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|id| id.0)
            .collect()
    };

    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let item = hit.item;
        let progress = state
            .store
            .progress_state(&item.id)
            .map_err(|e| e.reason().to_string())?;
        // 续播文案的百分比同样出自 L2 规则，不在界面层另算一遍
        let offer = ResumePolicy::resume_offer(
            Some(Progress {
                position_ms: progress.position_ms,
                duration_ms: progress.duration_ms,
            }),
            item.duration_ms,
        );
        // 「登记了封面」即视为有封面：在线条目的封面文件是**按需懒抓**的，导入时
        // 还不存在；界面据此调 `item_cover` 取图，取不到就保留占位图（不会破图）
        let has_poster = item.poster_id.is_some();
        out.push(ItemView {
            id: item.id.as_str().to_string(),
            title: item.title,
            duration_ms: item.duration_ms,
            position_ms: progress.position_ms,
            resume_percent: offer.map(|o| o.percent),
            watched: progress.watched,
            missing: missing.contains(item.id.as_str()),
            has_poster,
        });
    }
    Ok(out)
}

/// 已登记的标签维度（按配置顺序），供检索区渲染维度面板。
#[tauri::command]
fn list_dimensions(state: State<'_, AppState>) -> std::result::Result<Vec<DimensionView>, String> {
    let dims = SearchService::dimensions(state.store.as_ref())
        .map_err(|e| e.reason().to_string())?;
    Ok(dims
        .into_iter()
        .map(|d| DimensionView {
            id: d.id,
            label: d.label,
            selector: match d.selector {
                cinema_domain::ports::tag_dimension::Selector::Single => "single",
                cinema_domain::ports::tag_dimension::Selector::Multi => "multi",
            }
            .into(),
            source: match d.source {
                cinema_domain::ports::tag_dimension::TagSource::Metadata => "metadata",
                cinema_domain::ports::tag_dimension::TagSource::Derived => "derived",
                cinema_domain::ports::tag_dimension::TagSource::User => "user",
            }
            .into(),
            order: d.order,
            editable: d.editable,
            lockable: d.lockable,
        })
        .collect())
}

/// 某维度下的取值及使用频次（频次降序）。
#[tauri::command]
fn list_dimension_tags(
    state: State<'_, AppState>,
    dimension_id: String,
) -> std::result::Result<Vec<TagUsageView>, String> {
    let usage = SearchService::dimension_tags(state.store.as_ref(), &dimension_id)
        .map_err(|e| e.reason().to_string())?;
    Ok(usage
        .into_iter()
        .map(|u| TagUsageView {
            id: u.tag_id,
            name: u.name,
            count: u.count,
        })
        .collect())
}

#[tauri::command]
fn resume_offer(
    state: State<'_, AppState>,
    item_id: String,
) -> std::result::Result<Option<ResumeOfferView>, String> {
    let (item, _src) = load_item(&state, &item_id)?;
    let offer = ProgressService::resume_point(state.store.as_ref(), &item.id, item.duration_ms)
        .map_err(|e| e.reason().to_string())?;
    Ok(offer.map(|o| ResumeOfferView {
        position_ms: o.position_ms,
        percent: o.percent,
        duration_ms: item.duration_ms,
    }))
}

// ---------------------------------------------------------------------------
// 播放（FR-04）：单路播放 = 只有 0 号屏位
// ---------------------------------------------------------------------------

/// 在某屏位亮出画面宿主并把它交给工厂作为渲染目标（`wid` 来源）。
///
/// `fill` 为真时画面铺满主窗口、底部留出控制条（单路播放）；否则按给定矩形摆放（多屏每格）。
fn place_screen_host(state: &AppState, slot: u32, fill: bool, rect: Option<(i32, i32, i32, i32)>) -> std::result::Result<(), String> {
    let mut hosts = state.hosts.lock().map_err(|_| "宿主窗口锁不可用".to_string())?;
    if !hosts.screens.contains_key(&slot) {
        let title = if slot == 0 && !fill {
            format!("CinemaMulti{slot}")
        } else if slot == 0 {
            "CinemaPlay".to_string()
        } else {
            format!("CinemaMulti{slot}")
        };
        let host = HostWindow::create(state.parent, &title).map_err(|e| e.reason().to_string())?;
        // 画面盖住页面，点击只能在原生窗口里截获后转发：单路由前端切换暂停，多屏由前端聚焦该格
        let app = state.app.clone();
        if slot == 0 && fill {
            // 方向键在原生画面窗口里截获后转发给页面：画面压住页面时焦点可能在它身上，
            // 不转发的话按键会被原生窗口吞掉（现场表现为上/下键变成快进快退）。
            let app_key = state.app.clone();
            host.set_on_key(move |vk| {
                let _ = app_key.emit("player-key", vk);
            });
            host.set_on_click(move || {
                // 点击画面后把键盘焦点还给页面，后续按键全部走前端那套处理
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.set_focus();
                }
                let _ = app.emit("player-click", ());
            });
            // 控制条自动隐藏后，指针落在原生画面上也要能唤醒：移动事件在 Rust 侧节流转发。
            // 画面窗口在指针下方被撑开时系统会补发「位置没变」的 WM_MOUSEMOVE，
            // 这类事件必须丢弃，否则控制条刚收起就被自己撑开的画面叫回来（画面会来回闪）。
            let app_move = state.app.clone();
            let last_move = std::sync::Arc::new(std::sync::Mutex::new((
                None::<(i32, i32)>,
                std::time::Instant::now() - std::time::Duration::from_millis(500),
            )));
            host.set_on_move(move |x, y| {
                let now = std::time::Instant::now();
                if let Ok(mut slot) = last_move.lock() {
                    let (last_pos, last_at) = &mut *slot;
                    if *last_pos == Some((x, y)) {
                        return;
                    }
                    *last_pos = Some((x, y));
                    if now.duration_since(*last_at) < std::time::Duration::from_millis(120) {
                        return;
                    }
                    *last_at = now;
                }
                let _ = app_move.emit("player-move", ());
            });
        } else {
            host.set_on_click(move || {
                let _ = app.emit("multi-click", slot);
            });
        }
        hosts.screens.insert(slot, host);
    }
    if let Some(host) = hosts.screens.get(&slot) {
        match rect {
            Some((x, y, w, h)) => host.place(x, y, w, h),
            None => host.fill_parent(state.parent, CONTROL_BAR_PX),
        }
        state.factory.set_target(Some(host.wid()));
    }
    Ok(())
}

#[tauri::command]
fn open_player(
    state: State<'_, AppState>,
    item_id: String,
    start_ms: Option<i64>,
) -> std::result::Result<PlaybackStatusView, String> {
    println!("[播放] open_player {item_id}");
    // 单路播放只允许 0 号屏位：先退出多屏，避免重复占用
    close_all_screens(&state);

    let (item, src) = load_item(&state, &item_id)?;
    let provider = provider_for(&src, state.prober.as_ref(), &state.downloads)
        .map_err(|e| e.reason().to_string())?;
    let ref_ = provider.open(&item).map_err(|e| e.reason().to_string())?;

    // 打开播放前探测流事实（含内嵌字幕清单），供实时字幕入口可见性判定。
    // 探测失败（无 prober / 文件不可读）一律当「无内嵌字幕」处理，绝不让打开播放失败。
    let facts = if state.prober.is_some() {
        provider.probe(&ref_).ok()
    } else {
        None
    };

    // 在线源（边下边播）的本地缓存文件：心跳要按它查下载状态（放完 / 还在下载）
    let stream = online_stream_path(&src, &ref_);

    // 先亮出画面宿主，再创建实例：mpv 需要拿到 `wid`
    // 新开播放一律回到「仅留控制条」的布局，避免沿用上一场字幕面板让出的空间
    state.player_extra_reserve.store(0, Ordering::Relaxed);
    place_screen_host(&state, 0, true, None)?;

    let start_at = start_ms.map(|ms| Progress {
        position_ms: ms,
        duration_ms: item.duration_ms,
    });

    let opened = {
        let mut svc = state
            .playback
            .lock()
            .map_err(|_| "播放服务锁不可用".to_string())?;
        svc.open_screen(0, ref_, start_at)
    };

    if let Err(err) = opened {
        state.factory.set_target(None);
        state.factory.forget_screen(0);
        if let Ok(hosts) = state.hosts.lock() {
            if let Some(host) = hosts.screens.get(&0) {
                host.hide();
            }
        }
        return Err(err.reason().to_string());
    }

    // 打开成功才登记屏位状态（探测失败时 facts 为空，视为无内嵌字幕）
    if let Ok(mut screens) = state.screens.lock() {
        screens.insert(
            0,
            ScreenSlot {
                item_id: item.id.clone(),
                title: item.title.clone(),
                facts,
                fault: None,
                buffering: false,
                stream,
                stream_retry_at: None,
                stream_bytes: 0,
            },
        );
    }

    Ok(PlaybackStatusView {
        playing: true,
        ended: false,
        position_ms: start_at.map(|p| p.position_ms).unwrap_or(0),
        duration_ms: item.duration_ms,
        buffering: false,
        fault: None,
    })
}

/// 在线源的本地缓存文件路径（本地源为 `None`）。
///
/// 判定依据是**源类型 + 引用是本地文件**：在线源的 `open` 恒返回本地缓存路径，
/// 本地源的引用也是路径，但源类型不是 `OnlineSite`，不会误判。
fn online_stream_path(src: &MediaSource, ref_: &PlayableRef) -> Option<PathBuf> {
    if src.kind != SourceKind::OnlineSite {
        return None;
    }
    let path = PathBuf::from(&ref_.location);
    // 仅当缓存目录下的文件才登记（例如 stream 尚未落盘时就是「下载中」的状态）
    match streams_dir() {
        Ok(dir) => path.starts_with(&dir).then_some(path),
        Err(_) => None,
    }
}

/// 播放页心跳：返回当前屏位进度，并顺带按 L2 门槛落库**所有**屏位的进度。
#[tauri::command]
fn tick(state: State<'_, AppState>) -> std::result::Result<PlaybackStatusView, String> {
    poll_screens(&state);

    let mut status = PlaybackStatusView {
        playing: false,
        ended: false,
        position_ms: 0,
        duration_ms: None,
        buffering: false,
        fault: None,
    };

    if let Some(slot) = current_slot(&state) {
        if let Some(core) = state.factory.screen(slot) {
            status.playing = true;
            status.ended = slot_ended(&state, slot, &core);
            status.position_ms = core.position_ms().unwrap_or(0);
            status.duration_ms = core.duration_ms();
            // 在线源：装载成功前提示「正在准备」；下载失败时把原因交给界面弹一次
            status.buffering = status.duration_ms.is_none()
                && state
                    .screens
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&slot).map(|entry| entry.stream.is_some()))
                    .unwrap_or(false);
            status.fault = state
                .screens
                .lock()
                .ok()
                .and_then(|map| map.get(&slot).and_then(|entry| entry.fault.clone()));
        }
    }

    Ok(status)
}

#[tauri::command]
fn set_paused(state: State<'_, AppState>, paused: bool) {
    if let Some(slot) = current_slot(&state) {
        if let Ok(svc) = state.playback.lock() {
            svc.screen_command(
                slot,
                if paused {
                    PlayerCmd::Pause
                } else {
                    PlayerCmd::Play
                },
            );
        }
        // 与实时字幕会话联动：暂停挂起识别并保留末句，恢复则继续；忽略其错误
        if let Ok(mut runtime) = state.subtitles.lock() {
            let ctx = runtime.main();
            let _ = if paused {
                ctx.service.on_pause()
            } else {
                ctx.service.on_resume()
            };
        }
    }
}

#[tauri::command]
fn seek(state: State<'_, AppState>, position_ms: i64) {
    println!("[播放] seek -> {position_ms} ms");
    if let Some(slot) = current_slot(&state) {
        // 在线源（边下边播）的缓存文件还在增长：mpv 在装载时就把当时的文件大小
        // 记死了，往后 seek 会被判为「超出文件末尾」而静默失败——表现就是快进
        // 没反应。这里改成「按目标位置重新装载文件」，让 demuxer 按**当前**大小
        // 重新解析；下载已完成时普通 seek 即可，不必重载。
        let streaming = slot_stream(&state, slot)
            .filter(|path| state.downloads.state(path) == DownloadState::Running);
        match (streaming, state.factory.screen(slot)) {
            (Some(path), Some(core)) => {
                let ref_ = PlayableRef::local(path.to_string_lossy().into_owned());
                let duration_ms = core.duration_ms();
                let _ = core.load(
                    ref_,
                    Some(Progress {
                        position_ms,
                        duration_ms,
                    }),
                );
            }
            _ => {
                if let Ok(svc) = state.playback.lock() {
                    svc.screen_command(slot, PlayerCmd::Seek { position_ms });
                }
            }
        }
        // 跳转后清空实时字幕缓冲、重置句界、丢弃旧句；忽略其错误
        if let Ok(mut runtime) = state.subtitles.lock() {
            let _ = runtime.main().service.on_seek(position_ms);
        }
    }
}

/// 设置音量（0–100），返回真正生效的音量。
#[tauri::command]
fn set_volume(state: State<'_, AppState>, volume: f64) -> Option<f64> {
    let volume = volume.clamp(0.0, 100.0);
    println!("[播放] set_volume -> {volume}");
    if let Some(slot) = current_slot(&state) {
        if let Ok(svc) = state.playback.lock() {
            svc.screen_command(slot, PlayerCmd::SetVolume(volume));
        }
    }
    // 预览实例也同步：详情页里播预告片时上下键同样要能调音量
    if let Some(core) = state.factory.preview() {
        core.command(PlayerCmd::SetVolume(volume));
    }
    // 音量提示用 mpv 自己的 OSD：原生画面窗口压在 WebView 之上，页面层的提示看不见
    let osd = volume_osd_text(volume);
    if let Some(slot) = current_slot(&state) {
        if let Some(core) = state.factory.screen(slot) {
            core.show_osd(&osd, 1200);
        }
    }
    if let Some(core) = state.factory.preview() {
        core.show_osd(&osd, 1200);
    }
    Some(volume)
}

/// 音量提示文案：10 格进度条便于一眼看出大小，纯 ASCII 避免字体缺字形。
fn volume_osd_text(volume: f64) -> String {
    let pct = volume.round().clamp(0.0, 100.0) as usize;
    let filled = pct / 10;
    format!("音量 {pct}%  [{}{}]", "#".repeat(filled), "-".repeat(10 - filled))
}

/// 读取当前音量（0–100）。播放内核读不到时返回 `None`，界面沿用本地记忆值。
#[tauri::command]
fn player_volume(state: State<'_, AppState>) -> Option<f64> {
    if let Some(slot) = current_slot(&state) {
        if let Some(core) = state.factory.screen(slot) {
            if let Some(volume) = core.volume() {
                return Some(volume);
            }
        }
    }
    state.factory.preview().and_then(|core| core.volume())
}

#[tauri::command]
fn close_player(state: State<'_, AppState>) -> std::result::Result<(), String> {
    state.player_extra_reserve.store(0, Ordering::Relaxed);
    close_all_screens(&state);
    Ok(())
}

/// 单路播放时把画面底部再留出 `extra` 个物理像素。
///
/// 原生画面窗口恒压在 WebView 之上，字幕菜单 / 字幕弹层这些 HTML 控件若落在画面
/// 区域内就会被盖住（点击反而落到画面上触发暂停）。打开面板时把画面向上收，
/// 面板关闭后传 0 还原。控制条自动隐藏时传负值，让画面铺到窗口底部。
#[tauri::command]
fn set_player_reserve(state: State<'_, AppState>, extra: i32) {
    // 正值 = 面板让位；负值最多收掉整条控制条（画面铺满）
    let extra = extra.max(-CONTROL_BAR_PX);
    state.player_extra_reserve.store(extra, Ordering::Relaxed);
    if let Ok(hosts) = state.hosts.lock() {
        if let Some(host) = hosts.screens.get(&0) {
            host.fill_parent(state.parent, (CONTROL_BAR_PX + extra).max(0));
        }
    }
}

/// 当前画面的显示宽高比（宽 / 高）；供前端把字幕定位到**画面内**底部。
///
/// 宿主窗口与视频比例不一致时 mpv 会居中留黑边，字幕若贴在窗口底部就会落到黑边上。
#[tauri::command]
fn player_video_aspect(state: State<'_, AppState>, slot: Option<u32>) -> Option<f64> {
    state
        .factory
        .screen(slot.unwrap_or(0))
        .and_then(|core| core.video_aspect())
}

// ---------------------------------------------------------------------------
// 多屏（FR-06）：布局、上限、每格操作与退出
// ---------------------------------------------------------------------------

/// 界面可选的布局（PRD FR-06「布局」表）。
#[tauri::command]
fn multi_layouts() -> Vec<MultiLayoutView> {
    Layout::ALL
        .iter()
        .map(|layout| MultiLayoutView {
            tag: layout.tag().to_string(),
            label: layout.label().to_string(),
            slots: layout.slots(),
        })
        .collect()
}

/// 多屏整体状态（前端按固定节奏轮询）。
#[tauri::command]
fn multi_status(state: State<'_, AppState>) -> std::result::Result<MultiScreenView, String> {
    poll_screens(&state);
    Ok(multi_view(&state))
}

/// 切换布局。格位数不足以容纳已开启窗口时拒绝，由用户先移除窗口。
#[tauri::command]
fn multi_set_layout(
    state: State<'_, AppState>,
    layout: String,
) -> std::result::Result<MultiScreenView, String> {
    let layout = Layout::from_tag(&layout);
    state
        .playback
        .lock()
        .map_err(|_| "播放服务锁不可用".to_string())?
        .set_layout(layout)
        .map_err(|e| e.reason().to_string())?;
    Ok(multi_view(&state))
}

/// 调整同时播放上限（默认 4，可调）。上限降低不影响已开启的窗口。
#[tauri::command]
fn multi_set_limit(
    state: State<'_, AppState>,
    limit: u32,
) -> std::result::Result<MultiScreenView, String> {
    let limit = limit.max(1) as usize;
    if let Ok(mut svc) = state.playback.lock() {
        svc.set_limit(limit);
    }
    // 持久化到设置，下次启动沿用（架构 §9「每个新功能在 setting 中有开关」）
    {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        SettingRepo::set(db.conn(), KEY_MAX_SCREENS, &limit.to_string())
            .map_err(|e| e.to_string())?;
    }
    Ok(multi_view(&state))
}

/// 开关并发发声（默认关闭，硬约束 C6）：开启后多路同时出声，关闭即回落单路发声。
///
/// 即时生效：开启时全部已开窗口立刻出声；关闭时只保留当前发声窗口。
/// 字幕会话随模式切换收尾：单路模式只有主上下文，并发模式才按格各持一份，
/// 因此切换时把旧模式的会话全部收掉（避免识别子进程成为孤儿）。
#[tauri::command]
fn multi_set_concurrent_audio(
    state: State<'_, AppState>,
    enabled: bool,
) -> std::result::Result<MultiScreenView, String> {
    if let Ok(mut svc) = state.playback.lock() {
        svc.set_concurrent_audio(enabled);
    }
    if let Ok(mut runtime) = state.subtitles.lock() {
        if enabled {
            // 切到并发发声：原主上下文（未并发时的唯一会话）随模式切换收尾
            let _ = runtime.main().service.close(state.store.as_ref());
            runtime.main().target_lang = None;
        } else {
            // 切回单路发声：各格并发上下文收尾，会话回到主上下文
            for slot in runtime.slot_keys() {
                runtime.close_slot(state.store.as_ref(), slot);
            }
        }
    }
    // 持久化到设置，下次启动沿用（架构 §9「每个新功能在 setting 中有开关」）
    {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        SettingRepo::set(
            db.conn(),
            KEY_CONCURRENT_AUDIO,
            if enabled { "true" } else { "false" },
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(multi_view(&state))
}

/// 在某格打开影片（已有窗口的屏位视为「重试 / 更换影片」，先关闭再重开）。
#[tauri::command]
fn multi_open(
    state: State<'_, AppState>,
    slot: u32,
    item_id: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    start_ms: Option<i64>,
) -> std::result::Result<MultiScreenView, String> {
    if open_slots(&state).contains(&slot) {
        close_slot(&state, slot);
    }

    let (item, src) = load_item(&state, &item_id)?;
    let provider = provider_for(&src, state.prober.as_ref(), &state.downloads)
        .map_err(|e| e.reason().to_string())?;
    let ref_ = provider.open(&item).map_err(|e| e.reason().to_string())?;
    let facts = if state.prober.is_some() {
        provider.probe(&ref_).ok()
    } else {
        None
    };
    let stream = online_stream_path(&src, &ref_);
    let start_at = start_ms.map(|ms| Progress {
        position_ms: ms,
        duration_ms: item.duration_ms,
    });

    place_screen_host(&state, slot, false, Some((x, y, width, height)))?;

    let opened = state
        .playback
        .lock()
        .map_err(|_| "播放服务锁不可用".to_string())?
        .open_screen(slot, ref_, start_at);

    if let Err(err) = opened {
        state.factory.set_target(None);
        state.factory.forget_screen(slot);
        if let Ok(hosts) = state.hosts.lock() {
            if let Some(host) = hosts.screens.get(&slot) {
                host.hide();
            }
        }
        return Err(err.reason().to_string());
    }

    if let Ok(mut screens) = state.screens.lock() {
        screens.insert(
            slot,
            ScreenSlot {
                item_id: item.id.clone(),
                title: item.title.clone(),
                facts,
                fault: None,
                buffering: false,
                stream,
                stream_retry_at: None,
                stream_bytes: 0,
            },
        );
    }

    Ok(multi_view(&state))
}

/// 重摆某格画面矩形（布局变化 / 窗口尺寸变化时由前端上报）。
#[tauri::command]
fn multi_place(
    state: State<'_, AppState>,
    slot: u32,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> std::result::Result<(), String> {
    let hosts = state.hosts.lock().map_err(|_| "宿主窗口锁不可用".to_string())?;
    if let Some(host) = hosts.screens.get(&slot) {
        host.place(x, y, width, height);
    }
    Ok(())
}

/// 关闭某格（其余格继续；关闭发声窗口时焦点按屏位顺序移交）。
#[tauri::command]
fn multi_remove(
    state: State<'_, AppState>,
    slot: u32,
) -> std::result::Result<MultiScreenView, String> {
    close_slot(&state, slot);
    Ok(multi_view(&state))
}

/// 切换发声窗口（单声道输出：同一时刻只有一路发声）。切换即时生效。
#[tauri::command]
fn multi_set_speaker(
    state: State<'_, AppState>,
    slot: u32,
) -> std::result::Result<MultiScreenView, String> {
    // 实时字幕的音频抽头绑定在旧发声窗口的主上下文上：切换前先收尾会话
    // （并发发声模式下每格各有独立会话，不受发声窗切换影响）
    let concurrent = state
        .playback
        .lock()
        .map(|svc| svc.concurrent_audio())
        .unwrap_or(false);
    if !concurrent {
        if let Ok(mut runtime) = state.subtitles.lock() {
            let _ = runtime.main().service.close(state.store.as_ref());
            runtime.main().target_lang = None;
        }
    }
    state
        .playback
        .lock()
        .map_err(|_| "播放服务锁不可用".to_string())?
        .set_speaker(slot)
        .map_err(|e| e.reason().to_string())?;
    Ok(multi_view(&state))
}

/// 播放 / 暂停某格（每格独立，不影响其他格）。
#[tauri::command]
fn multi_set_paused(state: State<'_, AppState>, slot: u32, paused: bool) {
    if let Ok(svc) = state.playback.lock() {
        svc.screen_command(
            slot,
            if paused {
                PlayerCmd::Pause
            } else {
                PlayerCmd::Play
            },
        );
    }
    // 实时字幕联动：并发发声模式下每格有自己的会话，操作哪格就联动哪格；
    // 否则会话只在发声窗口的主上下文，仅当操作的是发声窗口时才联动
    let (is_speaker, concurrent) = state
        .playback
        .lock()
        .map(|svc| (svc.speaker() == Some(slot), svc.concurrent_audio()))
        .unwrap_or((false, false));
    if concurrent || is_speaker {
        if let Ok(mut runtime) = state.subtitles.lock() {
            let ctx = if concurrent {
                runtime.slot(slot)
            } else {
                runtime.main()
            };
            let _ = if paused {
                ctx.service.on_pause()
            } else {
                ctx.service.on_resume()
            };
        }
    }
}

/// 跳转某格进度。
#[tauri::command]
fn multi_seek(state: State<'_, AppState>, slot: u32, position_ms: i64) {
    if let Ok(svc) = state.playback.lock() {
        svc.screen_command(slot, PlayerCmd::Seek { position_ms });
    }
    let (is_speaker, concurrent) = state
        .playback
        .lock()
        .map(|svc| (svc.speaker() == Some(slot), svc.concurrent_audio()))
        .unwrap_or((false, false));
    if concurrent || is_speaker {
        if let Ok(mut runtime) = state.subtitles.lock() {
            let ctx = if concurrent {
                runtime.slot(slot)
            } else {
                runtime.main()
            };
            let _ = ctx.service.on_seek(position_ms);
        }
    }
}

/// 设置某格倍速。
#[tauri::command]
fn multi_set_rate(state: State<'_, AppState>, slot: u32, rate: f64) {
    if let Ok(svc) = state.playback.lock() {
        svc.screen_command(slot, PlayerCmd::SetRate(rate));
    }
}

/// 选择某格音轨 / 字幕轨道。
#[tauri::command]
fn multi_select_track(
    state: State<'_, AppState>,
    slot: u32,
    kind: String,
    index: i64,
) -> std::result::Result<(), String> {
    let selection = match kind.as_str() {
        "audio" => TrackSelection::Audio(index),
        "subtitle" => TrackSelection::Subtitle(index),
        "subtitle_off" => TrackSelection::SubtitleOff,
        other => return Err(format!("未知的轨道类型：{other}")),
    };
    if let Ok(svc) = state.playback.lock() {
        svc.screen_command(slot, PlayerCmd::SelectTrack(selection));
    }
    Ok(())
}

/// 隐藏全部画面窗口（多屏期间「浏览库」用：原生画面让位给海报墙，音频继续）。
#[tauri::command]
fn multi_hide_all(state: State<'_, AppState>) {
    if let Ok(hosts) = state.hosts.lock() {
        for host in hosts.screens.values() {
            host.hide();
        }
    }
}

/// 隐藏单格画面窗口：某格故障时让位给错误卡（其余格画面不受影响）。
#[tauri::command]
fn multi_hide_slot(state: State<'_, AppState>, slot: u32) {
    if let Ok(hosts) = state.hosts.lock() {
        if let Some(host) = hosts.screens.get(&slot) {
            host.hide();
        }
    }
}

/// 单条音轨的展示视图（索引与文案均取自打开播放时探测到的流事实）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioTrackView {
    /// mpv 轨道索引，前端直接作为 `aid` 回传（与 `multi_select_track` 对齐）。
    index: i64,
    label: String,
}

/// 某格音轨清单：标签取「语言 → 编码 → 音轨 序号」逐级降级，不发明文档未定义的规则。
#[tauri::command]
fn multi_audio_tracks(state: State<'_, AppState>, slot: u32) -> Vec<AudioTrackView> {
    let Some(facts) = slot_facts(&state, slot) else {
        return Vec::new();
    };
    facts
        .audio_tracks
        .iter()
        .map(|track| AudioTrackView {
            index: track.index,
            label: track
                .lang
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| track.codec.clone().filter(|s| !s.trim().is_empty()))
                .unwrap_or_else(|| format!("音轨 {}", track.index)),
        })
        .collect()
}

/// 退出多屏（PRD FR-06「退出」）：必须由用户明确二选一，不设默认。
#[tauri::command]
fn multi_exit(
    state: State<'_, AppState>,
    policy: String,
) -> std::result::Result<MultiScreenView, String> {
    let policy = ExitPolicy::from_tag(&policy).ok_or_else(|| "退出方式必须明确选择".to_string())?;
    match policy {
        ExitPolicy::KeepProgressAndReturn => {
            // 保留发声窗口的进度并回到单路，关闭其余窗口
            let keep = current_slot(&state);
            // 回到单路即回到主上下文：保留格的并发字幕会话也必须收尾，
            // 否则识别子进程会随着会话进入无人认领的空转状态。
            if let (Some(keep), Ok(mut runtime)) = (keep, state.subtitles.lock()) {
                runtime.close_slot(state.store.as_ref(), keep);
            }
            for slot in open_slots(&state) {
                if Some(slot) != keep {
                    close_slot(&state, slot);
                }
            }
        }
        ExitPolicy::StopAll => close_all_screens(&state),
    }
    Ok(multi_view(&state))
}

// ---------------------------------------------------------------------------
// 悬停预览（FR-02）
// ---------------------------------------------------------------------------

/// 悬停预览与预告片共用：放置预览宿主并启动一路预览实例。
fn run_preview(
    state: &AppState,
    ref_: PlayableRef,
    has_highlight: bool,
    rect: (i32, i32, i32, i32),
) -> std::result::Result<PreviewResultView, String> {
    let (x, y, width, height) = rect;
    {
        let mut hosts = state
            .hosts
            .lock()
            .map_err(|_| "宿主窗口锁不可用".to_string())?;
        if hosts.preview.is_none() {
            let host =
                HostWindow::create(state.parent, "CinemaPreview").map_err(|e| e.reason().to_string())?;
            // 点击预览画面 = 直接进入播放页（画面压在页面上，点击只能在此截获）
            let app = state.app.clone();
            host.set_on_click(move || {
                let _ = app.emit("preview-click", ());
            });
            hosts.preview = Some(host);
        }
        if let Some(host) = &hosts.preview {
            host.place(x, y, width, height);
            state.factory.set_target(Some(host.wid()));
        }
    }

    let started = {
        let mut svc = state
            .playback
            .lock()
            .map_err(|_| "播放服务锁不可用".to_string())?;
        svc.start_preview(PreviewRequest { ref_, has_highlight })
    };

    match started {
        Ok(()) => Ok(PreviewResultView {
            started: true,
            reason: None,
        }),
        Err(rejection) => {
            stop_preview_inner(state);
            Ok(PreviewResultView {
                started: false,
                reason: Some(rejection.message().to_string()),
            })
        }
    }
}

#[tauri::command]
fn start_preview(
    state: State<'_, AppState>,
    item_id: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> std::result::Result<PreviewResultView, String> {
    let enabled = state
        .playback
        .lock()
        .map_err(|_| "播放服务锁不可用".to_string())?
        .preview_enabled();
    if !enabled {
        return Ok(PreviewResultView {
            started: false,
            reason: Some("已关闭悬停自动预览".into()),
        });
    }

    let (item, src) = load_item(&state, &item_id)?;
    // 本地预告片优先：抓取时已落盘的预告片比「精华片段」更贴近需求，
    // 也与详情页的「播放预告片」保持同一份素材。
    let ref_ = match trailer_path(&item) {
        Some(path) => PlayableRef::local(path.to_string_lossy().to_string()),
        None => {
            // 在线源（边下边播）不提供悬停预览：预览要顺带把整部影片下载下来，
            // 代价远大于收益，也会挤占正在播放的下载。
            if src.kind == SourceKind::OnlineSite {
                return Ok(PreviewResultView {
                    started: false,
                    reason: Some("在线条目暂不提供悬停预览".into()),
                });
            }
            let provider = provider_for(&src, state.prober.as_ref(), &state.downloads)
                .map_err(|e| e.reason().to_string())?;
            provider.open(&item).map_err(|e| e.reason().to_string())?
        }
    };

    // 「是否有精华片段」由时长是否已探明决定；起始位置判定在 PRD 中标为 TBD，
    // 因此 M1 一律从片头开始（与 P-04 预览起点恒为 None 一致）。
    let has_highlight = item.duration_ms.is_some();
    run_preview(&state, ref_, has_highlight, (x, y, width, height))
}

#[tauri::command]
fn stop_preview(state: State<'_, AppState>) {
    stop_preview_inner(&state);
}

/// 悬停预览的声音开关：预览实例默认静音，打开后即时解除。
#[tauri::command]
fn set_preview_sound(state: State<'_, AppState>, enabled: bool) {
    if let Ok(mut svc) = state.playback.lock() {
        svc.set_preview_muted(!enabled);
    }
}

#[tauri::command]
fn set_preview_enabled(state: State<'_, AppState>, enabled: bool) {
    if let Ok(mut svc) = state.playback.lock() {
        svc.set_preview_enabled(enabled);
    }
    if !enabled {
        stop_preview_inner(&state);
    }
}

#[tauri::command]
fn ui_constants() -> UiConstants {
    UiConstants {
        preview_trigger_ms: PREVIEW_TRIGGER_MS,
        preview_segment_ms: PREVIEW_SEGMENT_MS,
        control_bar_px: CONTROL_BAR_PX,
    }
}

#[tauri::command]
fn library_stats(state: State<'_, AppState>) -> std::result::Result<LibraryStats, String> {
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    Ok(LibraryStats {
        sources: MediaSourceRepo::list(db.conn())
            .map_err(|e| e.to_string())?
            .len(),
        items: MediaItemRepo::count(db.conn()).map_err(|e| e.to_string())?,
        decoder_ready: state.prober.is_some(),
    })
}

// ---------------------------------------------------------------------------
// 元数据刮削（P-06）：设置 / 单条刮削 / 批量补齐 / 详情 / 截图 / 预告片
// ---------------------------------------------------------------------------

/// 读取数据源设置并探测登录态（Cookie 为空时只回报「未登录」）。
///
/// 探测要访问站点，因此走 `spawn_blocking`：同步命令跑在主线程上会卡住界面。
#[tauri::command]
async fn javdb_settings(
    state: State<'_, AppState>,
) -> std::result::Result<JavdbSettingsView, String> {
    let config = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        javdb_config_from(&db)
    };
    let client = Arc::clone(&state.javdb);
    tauri::async_runtime::spawn_blocking(move || {
        let login = client.login_state(&config);
        let (logged_in, username, message) = match &login {
            Ok(state) => (
                state.logged_in,
                state.username.clone(),
                if state.logged_in {
                    Some("Cookie 有效，评论与预告片可用".to_string())
                } else if config.has_cookie() {
                    Some("Cookie 已失效，请重新复制".to_string())
                } else {
                    Some("未填写 Cookie：可刮削标题/封面/截图/标签，评论与预告片需登录".to_string())
                },
            ),
            Err(err) => (false, None, Some(err.reason().to_owned())),
        };
        let has_cookie = config.has_cookie();
        JavdbSettingsView {
            base_url: config.base_url,
            has_cookie,
            logged_in,
            username,
            message,
        }
    })
    .await
    .map_err(|e| format!("读取数据源设置失败：{e}"))
}

/// 保存数据源设置并立即验证 Cookie。
#[tauri::command]
async fn save_javdb_settings(
    state: State<'_, AppState>,
    base_url: String,
    cookie: String,
) -> std::result::Result<JavdbSettingsView, String> {
    let base = normalize_base_url(&base_url);
    if base.is_empty() {
        return Err("请填写数据源站点地址".into());
    }
    // Cookie 头不能含换行：粘贴时常带折行，这里统一清掉
    let cookie = cookie.replace(['\r', '\n'], "").trim().to_owned();
    // 留空表示「保留已保存的 Cookie」：界面不回显 Cookie，避免误把旧值覆盖掉
    let cookie = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let effective = if cookie.is_empty() {
            SettingRepo::get(db.conn(), KEY_JAVDB_COOKIE)
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            cookie
        };
        SettingRepo::set(db.conn(), KEY_JAVDB_BASE_URL, &base).map_err(|e| e.to_string())?;
        SettingRepo::set(db.conn(), KEY_JAVDB_COOKIE, &effective).map_err(|e| e.to_string())?;
        effective
    };

    let config = JavdbConfig {
        base_url: base.clone(),
        cookie,
    };
    let client = Arc::clone(&state.javdb);
    tauri::async_runtime::spawn_blocking(move || {
        let login = client.login_state(&config);
        let (logged_in, username, message) = match &login {
            Ok(state) => (
                state.logged_in,
                state.username.clone(),
                if state.logged_in {
                    Some("Cookie 有效，评论与预告片可用".to_string())
                } else if config.has_cookie() {
                    Some("Cookie 已保存，但站点未识别到登录态，请检查是否复制完整".to_string())
                } else {
                    Some("已保存（未填写 Cookie）".to_string())
                },
            ),
            Err(err) => (false, None, Some(err.reason().to_owned())),
        };
        let has_cookie = config.has_cookie();
        JavdbSettingsView {
            base_url: config.base_url,
            has_cookie,
            logged_in,
            username,
            message,
        }
    })
    .await
    .map_err(|e| format!("保存数据源设置失败：{e}"))
}

/// 刮削单个条目。`force = false` 时已完成的条目直接跳过。
///
/// 网络抓取与图片下载都走 `spawn_blocking`；同一时间只允许一条在途（自动与手动去重）。
#[tauri::command]
async fn scrape_item(
    state: State<'_, AppState>,
    item_id: String,
    force: Option<bool>,
) -> std::result::Result<ScrapeResultView, String> {
    let target_id = ItemId::from(item_id.clone());
    // 在线站点源的详情页字段由站点自己给出，不走 javdb 的番号搜索
    let online = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        match MediaItemRepo::get(db.conn(), &target_id).map_err(|e| e.to_string())? {
            Some(item) if item.url.is_some() => {
                let src = MediaSourceRepo::get(db.conn(), &item.source_id)
                    .map_err(|e| e.to_string())?;
                matches!(src.map(|value| value.kind), Some(SourceKind::OnlineSite))
            }
            _ => false,
        }
    };
    if online {
        let store = Arc::clone(&state.store);
        let downloads = Arc::clone(&state.downloads);
        let inflight = Arc::clone(&state.scraping);
        let id = target_id;
        return tauri::async_runtime::spawn_blocking(move || {
            if !begin_scrape(&inflight, &id.0) {
                return Err("该条目正在刮削中".to_string());
            }
            let built = (ytdlp_path(), poster_dir(), streams_dir());
            let outcome = match built {
                (Ok(ytdlp), Ok(covers), Ok(streams)) => {
                    let provider = OnlineSiteProvider::new(ytdlp, covers, streams, downloads);
                    scrape_online_item(store.as_ref(), &provider, &id)
                }
                _ => Err("未找到 yt-dlp 或缓存目录不可用".to_string()),
            };
            end_scrape(&inflight, &id.0);
            Ok(match outcome {
                Ok((comments, screenshots)) => ScrapeResultView {
                    item_id: id.0.clone(),
                    state: scrape_state_label(ScrapeState::Done).to_owned(),
                    comments,
                    screenshots,
                    trailer_saved: false,
                    message: None,
                },
                Err(reason) => {
                    // 构造提供者之前就失败时也要把状态落到界面上
                    let _ = store.set_scrape_state(&id, ScrapeState::Failed, Some(&reason));
                    ScrapeResultView {
                        item_id: id.0.clone(),
                        state: scrape_state_label(ScrapeState::Failed).to_owned(),
                        comments: 0,
                        screenshots: 0,
                        trailer_saved: false,
                        message: Some(reason),
                    }
                }
            })
        })
        .await
        .map_err(|e| format!("刮削任务异常中止：{e}"))?;
    }

    let config = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        javdb_config_from(&db)
    };
    let store = Arc::clone(&state.store);
    let client = Arc::clone(&state.javdb);
    let inflight = Arc::clone(&state.scraping);
    let force = force.unwrap_or(true);
    tauri::async_runtime::spawn_blocking(move || {
        let id = ItemId::from(item_id);
        if !begin_scrape(&inflight, &id.0) {
            return Err("该条目正在刮削中".to_string());
        }
        let outcome = MetadataService::new(client.as_ref(), store.as_ref())
            .scrape_one(&id, &config, force);
        end_scrape(&inflight, &id.0);
        Ok(match outcome {
            Ok(done) => ScrapeResultView {
                item_id: done.item_id,
                state: scrape_state_label(done.state).to_owned(),
                comments: done.comments,
                screenshots: done.screenshots,
                trailer_saved: done.trailer_saved,
                message: None,
            },
            Err(err) => ScrapeResultView {
                item_id: id.0,
                state: scrape_state_label(ScrapeState::Failed).to_owned(),
                comments: 0,
                screenshots: 0,
                trailer_saved: false,
                message: Some(err.reason().to_owned()),
            },
        })
    })
    .await
    .map_err(|e| format!("刮削任务异常中止：{e}"))?
}

/// 外部元数据补全结果：实际使用的源与本次真正写入的字段。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnrichResultView {
    /// `javlibrary` | `hanime`；没找到任何数据时为空。
    source: String,
    applied: Vec<String>,
    message: Option<String>,
}

fn union_values(base: &[String], extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.to_vec();
    for value in extra {
        let text = value.trim();
        if !text.is_empty() && !out.iter().any(|existing| existing == text) {
            out.push(text.to_owned());
        }
    }
    out
}

/// 把一个维度的补全值并入已有标签；没有新值时不写库。
fn apply_dimension(
    store: &DbStore,
    item_id: &ItemId,
    dimension: &str,
    label: &str,
    base: &[String],
    extra: &[String],
    applied: &mut Vec<String>,
) -> std::result::Result<(), String> {
    if extra.is_empty() {
        return Ok(());
    }
    let merged = union_values(base, extra);
    if merged.len() == base.len() {
        return Ok(());
    }
    store
        .replace_metadata_tags(item_id, dimension, &merged)
        .map_err(|err| err.reason().to_owned())?;
    applied.push(label.to_owned());
    Ok(())
}

/// 用 JavLibrary / hanime.tv 补全条目资料：只填空字段，已有值一律不覆盖。
///
/// `source`：`auto`（默认，先 JavLibrary 后 hanime.tv）/ `javlibrary` / `hanime`。
/// 两个源都不可用或没匹配到时不改库，把原因放在 `message` 里返回。
#[tauri::command]
async fn enrich_item(
    state: State<'_, AppState>,
    item_id: String,
    source: Option<String>,
) -> std::result::Result<EnrichResultView, String> {
    let mode = match source.as_deref().map(str::trim).unwrap_or("auto") {
        "javlibrary" => "javlibrary",
        "hanime" => "hanime",
        _ => "auto",
    };
    let id = ItemId::from(item_id);
    let (item, existing_shots, tags) = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let item = MediaItemRepo::get(db.conn(), &id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("条目不存在：{}", id.0))?;
        let shots = MetadataRepo::screenshots(db.conn(), &id).map_err(|e| e.to_string())?;
        let tags = ItemTagRepo::of_item(db.conn(), &id).map_err(|e| e.to_string())?;
        (item, shots, tags)
    };
    let store = Arc::clone(&state.store);

    tauri::async_runtime::spawn_blocking(move || {
        let title = item.title.clone();
        let code = item.external_code.clone().unwrap_or_default();
        let mut notes: Vec<String> = Vec::new();
        let mut found: Option<(&'static str, Enrichment)> = None;

        if mode != "hanime" {
            if code.trim().is_empty() {
                notes.push("条目没有番号，JavLibrary 无法匹配".to_owned());
            } else {
                match enrichment::from_javlibrary(&code) {
                    Ok(Some(value)) => found = Some(("javlibrary", value)),
                    Ok(None) => notes.push("JavLibrary 未找到该番号".to_owned()),
                    Err(err) => notes.push(format!("JavLibrary 查询失败：{}", err.reason())),
                }
            }
        }
        if found.is_none() && mode != "javlibrary" {
            match enrichment::from_hanime(&title) {
                Ok(Some(value)) => found = Some(("hanime", value)),
                Ok(None) => notes.push("hanime.tv 未匹配到该作品".to_owned()),
                Err(err) => notes.push(format!("hanime.tv 查询失败：{}", err.reason())),
            }
        }
        let Some((source_name, enrichment)) = found else {
            return Ok(EnrichResultView {
                source: String::new(),
                applied: Vec::new(),
                message: Some(notes.join("；")),
            });
        };

        let mut item = item;
        let mut applied: Vec<String> = Vec::new();
        let empty = |value: Option<&str>| value.map(|text| text.trim().is_empty()).unwrap_or(true);

        if empty(item.overview.as_deref()) {
            if let Some(value) = enrichment
                .overview
                .clone()
                .filter(|text| !text.trim().is_empty())
            {
                item.overview = Some(value);
                applied.push("简介".to_owned());
            }
        }
        if empty(item.release_date.as_deref()) {
            if let Some(value) = enrichment.release_date.clone() {
                item.year = value
                    .get(0..4)
                    .and_then(|year| year.parse::<i32>().ok())
                    .or(item.year);
                item.release_date = Some(value);
                applied.push("发行日期".to_owned());
            }
        }
        if item.duration_ms.is_none() {
            if let Some(value) = enrichment.duration_ms {
                item.duration_ms = Some(value);
                applied.push("时长".to_owned());
            }
        }
        if item.rating.is_none() {
            if let Some(value) = enrichment.rating {
                item.rating = Some(value);
                applied.push("评分".to_owned());
            }
        }
        if empty(item.external_code.as_deref()) {
            if let Some(value) = enrichment.external_code.clone() {
                item.external_code = Some(value);
                applied.push("番号".to_owned());
            }
        }
        if empty(item.detail_url.as_deref()) {
            if let Some(value) = enrichment.detail_url.clone() {
                item.detail_url = Some(value);
                applied.push("详情链接".to_owned());
            }
        }
        if empty(Some(item.title.as_str())) || item.title.trim() == "未命名" {
            if let Some(value) = enrichment.title.clone() {
                item.title = value;
                applied.push("标题".to_owned());
            }
        }

        // 剧照：外部源提供样品图时最多取 3 张；已有截图保留，按地址去重
        let mut shots = existing_shots;
        let mut added_shots = 0usize;
        for (index, url) in enrichment
            .screenshots
            .iter()
            .take(enrichment::MAX_SCREENSHOTS)
            .enumerate()
        {
            if shots
                .iter()
                .any(|shot| shot.source_url.as_deref() == Some(url.as_str()))
            {
                continue;
            }
            let Ok(bytes) = enrichment::fetch_bytes(url) else {
                continue;
            };
            let file_name = format!("{}-enrich{}.jpg", item.id.0, index + 1);
            if store.write_screenshot(&item.id, &file_name, &bytes).is_ok() {
                shots.push(ScrapedImage {
                    file_name,
                    source_url: Some(url.clone()),
                });
                added_shots += 1;
            }
        }
        if added_shots > 0 {
            store
                .replace_screenshots(&item.id, &shots)
                .map_err(|err| err.reason().to_owned())?;
            applied.push(format!("截图 {added_shots} 张"));
        }

        let base = |dimension: &str| tags.get(dimension).cloned().unwrap_or_default();
        apply_dimension(
            &store,
            &item.id,
            DIM_CATEGORY,
            "类别",
            &base(DIM_CATEGORY),
            &enrichment.categories,
            &mut applied,
        )?;
        apply_dimension(
            &store,
            &item.id,
            DIM_SERIES,
            "系列",
            &base(DIM_SERIES),
            &enrichment.series,
            &mut applied,
        )?;
        apply_dimension(
            &store,
            &item.id,
            DIM_MAKER,
            "制作商",
            &base(DIM_MAKER),
            &enrichment.maker,
            &mut applied,
        )?;
        apply_dimension(
            &store,
            &item.id,
            DIM_DIRECTOR,
            "导演",
            &base(DIM_DIRECTOR),
            &enrichment.director,
            &mut applied,
        )?;
        apply_dimension(
            &store,
            &item.id,
            DIM_ACTOR,
            "演员",
            &base(DIM_ACTOR),
            &enrichment.actors,
            &mut applied,
        )?;
        let year_values: Vec<String> =
            year_label_from(item.release_date.as_deref(), item.year)
                .into_iter()
                .collect();
        apply_dimension(
            &store,
            &item.id,
            DIM_YEAR,
            "年份",
            &base(DIM_YEAR),
            &year_values,
            &mut applied,
        )?;

        if applied.is_empty() {
            return Ok(EnrichResultView {
                source: source_name.to_owned(),
                applied,
                message: Some(if notes.is_empty() {
                    "没有可补充的新字段".to_owned()
                } else {
                    notes.join("；")
                }),
            });
        }

        item.scraped_at = Some(now_ms());
        item.meta_state = MetaState::Complete;
        store
            .save_scraped(&item)
            .map_err(|err| err.reason().to_owned())?;
        store
            .reindex_item(&item.id)
            .map_err(|err| err.reason().to_owned())?;
        Ok(EnrichResultView {
            source: source_name.to_owned(),
            applied,
            message: None,
        })
    })
    .await
    .map_err(|e| format!("补全任务异常中止：{e}"))?
}

/// 批量补齐的回执：处理数量 + 仍未补齐的缺口，界面据此如实提示。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScrapeBatchView {
    /// 本轮实际处理（成功或失败都算处理过）的条目数。
    processed: usize,
    /// 本轮计划处理的条目数。
    total: usize,
    /// 本轮从视频抽帧补过截图的影片数。
    shots_filled: usize,
    /// 处理完后仍没有短评的已刮削条目数。
    comments_missing: usize,
    /// 数据源是否已登录：短评只有登录后才能抓。
    logged_in: bool,
}

/// 批量补齐：刮削 `pending` / `failed` 的条目（可按源限定），逐条上报进度。
///
/// - 数据源已登录时，顺手重试「已完成但没有任何短评」的条目；
/// - 每条处理完都会检查截图，不足 10 张就从本地视频抽帧补满。
#[tauri::command]
async fn scrape_missing(
    state: State<'_, AppState>,
    source_id: Option<String>,
) -> std::result::Result<ScrapeBatchView, String> {
    let (config, sources) = {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        (
            javdb_config_from(&db),
            MediaSourceRepo::list(db.conn()).map_err(|e| e.to_string())?,
        )
    };
    let has_cookie = config.has_cookie();
    let online_provider = match (ytdlp_path(), poster_dir(), streams_dir()) {
        (Ok(ytdlp), Ok(covers), Ok(streams)) => Some(Arc::new(OnlineSiteProvider::new(
            ytdlp,
            covers,
            streams,
            Arc::clone(&state.downloads),
        ))),
        _ => None,
    };
    let store = Arc::clone(&state.store);
    let client = Arc::clone(&state.javdb);
    let inflight = Arc::clone(&state.scraping);
    let app = state.app.clone();
    let scope = source_id.map(SourceId::from);
    tauri::async_runtime::spawn_blocking(move || {
        // 在线站点源的条目没有番号，番号搜索对它们永远走不通；
        // 批量补齐时改走详情页路径（与导入后的自动补齐一致）
        let online_sources: std::collections::HashSet<SourceId> = sources
            .iter()
            .filter(|src| src.kind == SourceKind::OnlineSite)
            .map(|src| src.id.clone())
            .collect();

        let mut ids = store
            .pending_ids(scope.as_ref())
            .map_err(|e| e.reason().to_owned())?;
        // 已完成的条目不会再进 pending：手动补齐时单独把「缺短评」的挑出来重试。
        // 在线源详情页本身带短评（不需要 Cookie）；javdb 源则只有登录后才可能抓到。
        let mut seen: std::collections::HashSet<String> =
            ids.iter().map(|id| id.0.clone()).collect();
        for id in store
            .ids_done_without_comments(scope.as_ref())
            .unwrap_or_default()
        {
            if seen.contains(&id.0) {
                continue;
            }
            let online = store
                .item(&id)
                .map(|item| online_sources.contains(&item.source_id))
                .unwrap_or(false);
            if online || has_cookie {
                seen.insert(id.0.clone());
                ids.push(id);
            }
        }

        let total = ids.len();
        // 先报一次总数：进度条立刻知道总量，不必等第一条处理完才有数字
        let _ = app.emit(
            "scrape-progress",
            ScrapeProgressView {
                item_id: String::new(),
                done: 0,
                total,
            },
        );
        let mut done = 0usize;
        let mut shots_filled = 0usize;
        for (idx, id) in ids.iter().enumerate() {
            if begin_scrape(&inflight, &id.0) {
                let item = store.item(id).ok();
                let online = item
                    .as_ref()
                    .map(|value| online_sources.contains(&value.source_id))
                    .unwrap_or(false);
                // 已完成的条目要重抓短评：必须 force，否则 scrape_one 会直接跳过
                let force = item
                    .as_ref()
                    .map(|value| value.scrape_state == ScrapeState::Done)
                    .unwrap_or(false);
                match (online, online_provider.as_ref()) {
                    (true, Some(provider)) => {
                        let _ = scrape_online_item(store.as_ref(), provider.as_ref(), id);
                    }
                    _ => {
                        let _ = MetadataService::new(client.as_ref(), store.as_ref())
                            .scrape_one(id, &config, force);
                    }
                }
                // 截图补足：先读库里已有的，不足 10 张就从本地视频抽帧
                if let Some(item) = item.as_ref() {
                    if let Some(source) = sources.iter().find(|src| src.id == item.source_id) {
                        if let Ok(shots) = store.screenshots(&item.id) {
                            let (_, added) =
                                top_up_screenshots(store.as_ref(), item, source, shots);
                            if added > 0 {
                                shots_filled += 1;
                            }
                        }
                    }
                }
                end_scrape(&inflight, &id.0);
                done += 1;
            }
            let _ = app.emit(
                "scrape-progress",
                ScrapeProgressView {
                    item_id: id.0.clone(),
                    done: idx + 1,
                    total,
                },
            );
        }
        let comments_missing = store
            .ids_done_without_comments(scope.as_ref())
            .map(|list| list.len())
            .unwrap_or(0);
        Ok::<ScrapeBatchView, String>(ScrapeBatchView {
            processed: done,
            total,
            shots_filled,
            comments_missing,
            logged_in: has_cookie,
        })
    })
    .await
    .map_err(|e| format!("批量刮削异常中止：{e}"))?
}

/// 条目详情：元数据 + 截图 + 十条高赞短评 + 五类标签。
#[tauri::command]
fn item_detail(
    state: State<'_, AppState>,
    item_id: String,
) -> std::result::Result<ItemDetailView, String> {
    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let id = ItemId::from(item_id);
    let item = MediaItemRepo::get(db.conn(), &id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("条目不存在：{}", id.0))?;
    let shots = MetadataRepo::screenshots(db.conn(), &id).map_err(|e| e.to_string())?;
    let comments = MetadataRepo::comments(db.conn(), &id).map_err(|e| e.to_string())?;
    let tags = ItemTagRepo::of_item(db.conn(), &id).map_err(|e| e.to_string())?;
    let group = |dimension: &str| tags.get(dimension).cloned().unwrap_or_default();
    // 在线站点源才有「下载视频」能力（本地源的文件已经在硬盘上）
    let online = MediaSourceRepo::get(db.conn(), &item.source_id)
        .ok()
        .flatten()
        .map(|src| src.kind == SourceKind::OnlineSite)
        .unwrap_or(false);
    // 已有完整视频文件（≥1MB）时不再提供下载入口，避免重复下载
    let downloaded = item
        .detail_url
        .as_deref()
        .and_then(online_video_id)
        .and_then(|video_id| {
            download_dir()
                .ok()
                .and_then(|dir| existing_download(&dir, &video_id))
        })
        .is_some();

    Ok(ItemDetailView {
        id: item.id.0.clone(),
        title: item.title.clone(),
        code: item.external_code.clone(),
        release_date: item.release_date.clone(),
        duration_ms: item.duration_ms,
        rating: item.rating,
        detail_url: item.detail_url.clone(),
        has_poster: item.poster_id.is_some(),
        screenshots: shots.into_iter().map(|shot| shot.file_name).collect(),
        comments: comments
            .into_iter()
            .map(|comment| CommentView {
                author: comment.author,
                score: comment.score,
                content: comment.content,
                likes: comment.likes,
                posted_at: comment.posted_at,
            })
            .collect(),
        categories: group(DIM_CATEGORY),
        series: group(DIM_SERIES),
        maker: group(DIM_MAKER),
        director: group(DIM_DIRECTOR),
        actors: group(DIM_ACTOR),
        has_trailer: trailer_path(&item).is_some() || item.trailer_url.is_some(),
        trailer_url: item.trailer_url.clone(),
        online,
        downloaded,
        scrape_state: scrape_state_label(item.scrape_state).to_owned(),
        scrape_error: item.scrape_error.clone(),
    })
}

/// 读一张刮削截图（`<数据目录>/shots/<file_name>`），返回 data URL。
#[tauri::command]
fn item_screenshot(
    state: State<'_, AppState>,
    item_id: String,
    file_name: String,
) -> std::result::Result<String, String> {
    let _ = (state, item_id);
    // 边界校验：文件名来自库内登记，不接受任何路径分隔
    if file_name.contains('/') || file_name.contains('\\') || file_name.contains("..") {
        return Ok(String::new());
    }
    let path = data_dir()?.join("shots").join(&file_name);
    if !path.is_file() {
        return Ok(String::new());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(image_data_url(&bytes))
}

/// 详情页截图的目标张数：不足时从视频里按随机时间抽帧补满。
const DETAIL_SHOTS_TARGET: usize = 10;

/// 详情页截图补足：不足 [`DETAIL_SHOTS_TARGET`] 张时，从视频文件按随机时间抽帧，
/// 按时间升序追加。没有可读的视频文件（在线源尚未下载完）时原样返回。
#[tauri::command]
async fn fill_screenshots(
    state: State<'_, AppState>,
    item_id: String,
) -> std::result::Result<Vec<String>, String> {
    let (item, source) = load_item(&state, &item_id)?;
    let store = Arc::clone(&state.store);
    let shots = store
        .screenshots(&item.id)
        .map_err(|err| err.reason().to_owned())?;
    tauri::async_runtime::spawn_blocking(move || {
        let (all, _) = top_up_screenshots(store.as_ref(), &item, &source, shots);
        Ok(all.into_iter().map(|shot| shot.file_name).collect())
    })
    .await
    .map_err(|e| format!("补足截图任务异常中止：{e}"))?
}

/// 截图补足（详情页与批量维护共用）：不足 10 张时按随机时间从视频抽帧。
///
/// 返回补完后的截图列表与本轮新增张数；没有视频文件 / 读不到时长时原样返回。
fn top_up_screenshots(
    store: &DbStore,
    item: &MediaItem,
    source: &MediaSource,
    shots: Vec<ScrapedImage>,
) -> (Vec<ScrapedImage>, usize) {
    if shots.len() >= DETAIL_SHOTS_TARGET {
        return (shots, 0);
    }
    let Some(video) = item_video_file(item, source) else {
        return (shots, 0);
    };
    let Some(duration_ms) = item.duration_ms.filter(|ms| *ms > 0).or_else(|| {
        MpvProber::new()
            .ok()
            .and_then(|prober| prober.probe(video.to_string_lossy().as_ref()).ok())
            .and_then(|facts| facts.duration_ms)
    }) else {
        return (shots, 0);
    };
    let need = DETAIL_SHOTS_TARGET - shots.len();
    let times = thumbs::random_times(duration_ms as f64 / 1000.0, need);
    if times.is_empty() {
        return (shots, 0);
    }

    let mut all = shots;
    let mut added = 0usize;
    for (index, secs) in times.iter().enumerate() {
        // 每帧独占一个临时目录：mpv 的 image 输出文件名固定，共用会互相覆盖
        let work = std::env::temp_dir().join(format!(
            "cinema-frame-{}-{}-{}",
            std::process::id(),
            index,
            item.id.0
        ));
        let _ = std::fs::remove_dir_all(&work);
        let bytes = thumbs::extract_frame(&video, *secs, &work)
            .ok()
            .and_then(|path| std::fs::read(path).ok());
        let _ = std::fs::remove_dir_all(&work);
        let Some(bytes) = bytes else {
            continue;
        };
        let file_name = format!("{}-t{:08}.jpg", item.id.0, (*secs * 1000.0).round() as i64);
        if store.write_screenshot(&item.id, &file_name, &bytes).is_ok() {
            all.push(ScrapedImage {
                file_name,
                source_url: None,
            });
            added += 1;
        }
    }
    if added > 0 {
        let _ = store.replace_screenshots(&item.id, &all);
    }
    (all, added)
}

/// 该条目真正可读的视频文件：本地源按相对路径还原，在线源只认已下载完成的文件。
fn item_video_file(item: &MediaItem, source: &MediaSource) -> Option<PathBuf> {
    if source.kind == SourceKind::OnlineSite {
        let url = item
            .detail_url
            .as_deref()
            .or(item.url.as_deref())
            .map(str::trim)
            .filter(|url| !url.is_empty())?;
        let video_id = online_video_id(url)?;
        return download_dir()
            .ok()
            .and_then(|dir| existing_download(&dir, &video_id))
            .filter(|path| path.is_file());
    }

    let rel = item.rel_path.as_deref()?;
    let rel_path = PathBuf::from(rel);
    let path = if rel_path.is_absolute() {
        rel_path
    } else if source.kind == SourceKind::LanPath {
        PathBuf::from(normalized_lan_root(&source.location)).join(rel_path)
    } else {
        PathBuf::from(&source.location).join(rel_path)
    };
    path.is_file().then_some(path)
}

/// 从详情页地址取出 javdb 的 uid（`/v/<uid>`），用于播放前重新解析预告片。
fn javdb_uid(detail_url: &str) -> Option<String> {
    let idx = detail_url.rfind("/v/")?;
    let uid = detail_url[idx + 3..]
        .split(['?', '#', '/'])
        .next()
        .unwrap_or_default();
    if uid.is_empty() {
        None
    } else {
        Some(uid.to_string())
    }
}

/// 播放条目预告片（详情页按钮与悬停预览共用同一路预览宿主）。
///
/// 手动点播**不受**「悬停预览开关」限制：临时打开开关，播完后恢复原值。
/// 站点给的 HLS 地址带签名且约 30 分钟过期，直接复用库里存的地址会「点了没反应」，
/// 因此播放前先按 uid 重新解析一次；解析失败再退回库内地址。
#[tauri::command]
async fn play_trailer(
    state: State<'_, AppState>,
    item_id: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> std::result::Result<PreviewResultView, String> {
    let (item, _src) = load_item(&state, &item_id)?;
    let config = {
        let db = state
            .db
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        javdb_config_from(&db)
    };
    let mut headers = Vec::new();
    if let Some(detail) = item.detail_url.as_ref() {
        headers.push(("Referer".to_string(), detail.clone()));
    }
    if config.has_cookie() {
        headers.push(("Cookie".to_string(), config.cookie.clone()));
    }

    let ref_ = match trailer_path(&item) {
        Some(path) => PlayableRef::local(path.to_string_lossy().to_string()),
        None => {
            let client = state.javdb.clone();
            let cfg = config.clone();
            let uid = item.detail_url.as_deref().and_then(javdb_uid);
            let fresh = match uid {
                Some(uid) => tauri::async_runtime::spawn_blocking(move || {
                    client.detail(&cfg, &uid).ok().and_then(|d| d.trailer)
                })
                .await
                .map_err(|e| e.to_string())?
                .map(|t| t.url),
                None => None,
            };
            match fresh.or_else(|| item.trailer_url.clone()) {
                Some(url) => PlayableRef {
                    location: url,
                    headers,
                },
                None => {
                    return Ok(PreviewResultView {
                        started: false,
                        reason: Some("该条目还没有预告片，请先在详情页点「从数据源更新」".into()),
                    })
                }
            }
        }
    };

    let was_enabled = state
        .playback
        .lock()
        .map(|svc| svc.preview_enabled())
        .unwrap_or(true);
    if !was_enabled {
        if let Ok(mut svc) = state.playback.lock() {
            svc.set_preview_enabled(true);
        }
    }
    let result = run_preview(&state, ref_, true, (x, y, width, height));
    if let Ok(view) = &result {
        if view.started {
            // 手动预告片：画面可点击穿透到 HTML 层（点画面即关闭），并解除静音
            if let Ok(hosts) = state.hosts.lock() {
                if let Some(host) = &hosts.preview {
                    host.set_click_through(true);
                }
            }
            if let Ok(mut svc) = state.playback.lock() {
                svc.unmute_preview();
            }
        }
    }
    if !was_enabled {
        if let Ok(mut svc) = state.playback.lock() {
            svc.set_preview_enabled(false);
        }
    }
    result
}

/// 预告片心跳：读取预览实例的位置与时长（标题栏与进度条据此推进）。
#[tauri::command]
fn trailer_status(state: State<'_, AppState>) -> TrailerStatusView {
    match state.factory.preview() {
        Some(core) => TrailerStatusView {
            active: true,
            position_ms: core.position_ms().unwrap_or(0),
            duration_ms: core.duration_ms(),
        },
        None => TrailerStatusView {
            active: false,
            position_ms: 0,
            duration_ms: None,
        },
    }
}

/// 预告片跳转：预览实例与正式播放是不同实例，需单独下发命令。
#[tauri::command]
fn trailer_seek(state: State<'_, AppState>, position_ms: i64) {
    if let Some(core) = state.factory.preview() {
        core.command(PlayerCmd::Seek { position_ms });
    }
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let window = app
                .get_webview_window("main")
                .ok_or("主窗口 main 未创建")?;
            let parent = window.hwnd().map_err(|e| e.to_string())?.0 as i64;

            // 存储目录：先解析并完成待迁移的元数据，再打开数据库（避免复制正在使用的库）
            let data_dir = init_storage()?;
            let db = Database::open(data_dir.join("cinema.db")).map_err(|e| e.to_string())?;
            let version = db.migrate().map_err(|e| e.to_string())?;

            // 播种内置标签维度：幂等，已存在的 id 不覆盖（用户改过的配置优先）
            DimensionRepo::seed(db.conn(), &TagDimensionDef::builtin_defaults())
                .map_err(|e| e.to_string())?;
            // 历史库补写年份标签：有年份 / 发行日期但还没有「年份」取值的条目
            match store::backfill_year_tags(&db) {
                Ok(0) => {}
                Ok(count) => println!("[标签] 已补齐 {count} 个条目的年份标签"),
                Err(err) => eprintln!("[标签] 年份标签补齐失败：{err}"),
            }

            println!("[M2] 数据库 = {}（schema v{version}）", data_dir.display());

            let db = Arc::new(Mutex::new(db));

            // 探测实例：libmpv 不可用时降级，入库照常进行
            let prober = match MpvProber::new() {
                Ok(prober) => {
                    println!("[M1] libmpv 就绪，时长探测可用");
                    Some(Arc::new(prober))
                }
                Err(err) => {
                    eprintln!("[M1] libmpv 不可用，降级：无时长 → 无预览 / 无续播（{err}）");
                    None
                }
            };

            let factory = Arc::new(MpvPlayerFactory::new());
            let mut playback = PlaybackService::new(FactoryHandle(Arc::clone(&factory)));
            // 同时播放上限沿用上次设置（键 `multi_screen.max_screens`）；无设置则用默认 4 路
            if let Some(limit) = SettingRepo::get(&db.lock().map_err(|_| "数据库锁不可用".to_string())?.conn(), KEY_MAX_SCREENS)
                .map_err(|e| e.to_string())?
                .and_then(|raw| raw.trim().parse::<usize>().ok())
            {
                playback.set_limit(limit.max(1));
            }
            // 并发发声开关沿用上次设置（键 `multi_screen.concurrent_audio`）；无设置则为默认关闭
            let concurrent_audio = SettingRepo::get(
                &db.lock().map_err(|_| "数据库锁不可用".to_string())?.conn(),
                KEY_CONCURRENT_AUDIO,
            )
            .map_err(|e| e.to_string())?
            .map(|raw| raw.trim() == "true")
            .unwrap_or(false);
            playback.set_concurrent_audio(concurrent_audio);

            app.manage(AppState {
                parent,
                app: app.handle().clone(),
                store: Arc::new(DbStore::shared(
                    Arc::clone(&db),
                    Some(data_dir.join("posters")),
                )),
                db,
                factory,
                playback: Mutex::new(playback),
                prober,
                hosts: Mutex::new(Hosts::default()),
                screens: Mutex::new(std::collections::BTreeMap::new()),
                subtitles: Mutex::new(subtitle::SubtitleRuntime::new()),
                downloads: Arc::new(DownloadRegistry::new()),
                covers_inflight: Arc::new(Mutex::new(std::collections::HashMap::new())),
                online_downloads: Arc::new(Mutex::new(std::collections::HashMap::new())),
                // 与本次会话的扫描一一对应；`scan_source` 每次开跑都会先清零
                scan_cancel: Arc::new(AtomicBool::new(false)),
                javdb: Arc::new(JavdbProvider::new()),
                scraping: Arc::new(Mutex::new(std::collections::HashSet::new())),
                player_extra_reserve: AtomicI32::new(0),
            });

            // 【启动自愈】上次退出时仍在 `running` 的刮削已不可能完成，复位为 `pending`，
            // 否则这些条目永远不会再被「补齐元数据」选中。
            {
                let state = app.state::<AppState>();
                let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
                let _ = MediaItemRepo::reset_running_scrapes(db.conn());
            }

            // 【启动自愈】在线源的封面是「懒抓」的：启动时检查一遍，
            // 只要某个在线源存在「已登记封面但本地文件缺失」的条目，就后台补抓一次。
            {
                let state = app.state::<AppState>();
                let sources = {
                    let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
                    MediaSourceRepo::list(db.conn()).map_err(|e| e.to_string())?
                };
                for s in &sources {
                    let missing_covers = {
                        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
                        let items =
                            MediaItemRepo::list(db.conn(), Some(&s.id)).map_err(|e| e.to_string())?;
                        let covers = poster_dir()?;
                        items
                            .iter()
                            .filter(|it| {
                                it.poster_id
                                    .as_deref()
                                    .map(|p| !covers.join(p).is_file())
                                    .unwrap_or(false)
                            })
                            .count()
                    };
                    if missing_covers > 0 {
                        spawn_cover_fetch(&state, s);
                    }
                }
            }

            // 【启动自愈】上一次运行若在扫描中途退出（关窗 / 崩溃），源会永远停在
            // `scanning`，界面一直转圈——而进度浮层只属于本会话。扫描不是可跨会话续跑
            // 的任务，因此启动时把遗留的 `scanning` 复位为 `idle`。
            {
                let state = app.state::<AppState>();
                let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
                let sources = MediaSourceRepo::list(db.conn()).map_err(|e| e.to_string())?;
                for s in sources.iter().filter(|s| s.state == SourceState::Scanning) {
                    MediaSourceRepo::set_scan_result(
                        db.conn(),
                        &s.id,
                        SourceState::Idle,
                        cinema_infra::now_ms(),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }

            // 窗口尺寸变化：单路播放时画面重新贴合（控制条高度始终留出）；
            // 多屏每格的矩形由前端按布局重新上报（`multi_place`）。
            let handle = app.handle().clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event {
                    let state = handle.state::<AppState>();
                    let parent = state.parent;
                    let single = state
                        .playback
                        .lock()
                        .map(|svc| svc.screen_count() <= 1)
                        .unwrap_or(true);
                    if single {
                        let extra = state.player_extra_reserve.load(Ordering::Relaxed);
                        if let Ok(hosts) = state.hosts.lock() {
                            if let Some(host) = hosts.screens.get(&0) {
                                host.fill_parent(parent, (CONTROL_BAR_PX + extra).max(0));
                            }
                        }
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_local_source,
            list_sources,
            scan_source,
            cancel_scan,
            online_import,
            item_cover,
            export_library,
            import_library,
            search_items,
            list_dimensions,
            list_dimension_tags,
            resume_offer,
            open_player,
            tick,
            set_paused,
            seek,
            set_volume,
            player_volume,
            close_player,
            set_player_reserve,
            player_video_aspect,
            remove_items,
            clear_source,
            add_source_root,
            remove_source_root,
            add_videos_to_source,
            delete_dimension_tag,
            start_preview,
            stop_preview,
            set_preview_sound,
            set_preview_enabled,
            set_console_fullscreen,
            ui_constants,
            library_stats,
            storage_settings,
            save_storage_settings,
            restart_app,
            javdb_settings,
            save_javdb_settings,
            llm_settings,
            save_llm_settings,
            recommend_highlights,
            enrich_item,
            scrape_item,
            scrape_missing,
            start_online_download,
            download_online_source,
            online_download_status,
            item_detail,
            item_screenshot,
            fill_screenshots,
            play_trailer,
            trailer_status,
            trailer_seek,
            multi_layouts,
            multi_status,
            multi_set_layout,
            multi_set_limit,
            multi_open,
            multi_place,
            multi_remove,
            multi_set_speaker,
            multi_set_paused,
            multi_seek,
            multi_set_rate,
            multi_select_track,
            multi_hide_all,
            multi_hide_slot,
            multi_audio_tracks,
            multi_exit,
            multi_set_concurrent_audio,
            subtitle::subtitle_info,
            subtitle::subtitle_set_enabled,
            subtitle::subtitle_start,
            subtitle::subtitle_set_display,
            subtitle::subtitle_poll,
            subtitle::subtitle_save,
            subtitle::subtitle_close,
            subtitle::subtitle_add_external,
            subtitle::subtitle_online_search,
            subtitle::subtitle_online_load,
            subtitle::subtitle_tracks,
            subtitle::subtitle_entry_visible
        ])
        .build(tauri::generate_context!())
        .expect("启动桌面客户端失败")
        .run(|handle, event| {
            // 退出时收尾「边下边播」的下载子进程：Windows 下父进程退出不会带走子进程，
            // 不显式终止就会留下孤儿 yt-dlp（硬约束 C5）
            if let tauri::RunEvent::Exit = event {
                let state = handle.state::<AppState>();
                state.downloads.stop_all();
            }
        });
}


