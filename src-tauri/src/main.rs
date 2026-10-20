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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use cinema_adapters::media_source::{
    normalize_local_root, DirectUrlProvider, LanPathProvider, LocalFolderProvider,
};
use cinema_adapters::mpv::MpvProber;
use cinema_adapters::online_site::{DownloadRegistry, DownloadState, OnlineSiteProvider};
use cinema_adapters::player::MpvPlayerCore;
use cinema_app::decode_quota::DecodeQuota;
use cinema_app::import_service::{ImportService, ScanControl, ScanReporter};
use cinema_app::playback_service::{PlaybackService, PreviewRequest};
use cinema_app::portability_service::PortabilityService;
use cinema_app::progress_service::ProgressService;
use cinema_app::search_service::{SearchQuery, SearchService, SearchStore};
use cinema_domain::error::{PortError, Result as PortResult};
use cinema_domain::model::{
    ItemId, MediaItem, MediaSource, PlayableRef, PlayerCmd, PlayerEvent, Progress, ScanProgress,
    ScanStage, SourceId, SourceKind, SourceState, StreamFacts, TrackSelection,
};
use cinema_domain::ports::media_source::MediaSourceProvider;
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::tag_dimension::TagDimensionDef;
use cinema_domain::rules::{
    preview_allowed, ExitPolicy, Layout, ResumePolicy, SortMode, PREVIEW_SEGMENT_MS,
    PREVIEW_TRIGGER_MS,
};
use cinema_infra::db::Database;
use cinema_infra::repo::{MediaItemRepo, MediaSourceRepo};
use cinema_infra::subtitle_repo::SettingRepo;
use cinema_infra::tag_repo::DimensionRepo;

use host_window::HostWindow;
use player::{FactoryHandle, MpvPlayerFactory};
use store::DbStore;

/// 画面下方留给 webview 控制条的高度（与前端 CSS 一致）。
const CONTROL_BAR_PX: i32 = 76;

/// 同时播放上限在 `setting` 表中的键（PRD FR-06「路数上限」：默认 4，可调）。
const KEY_MAX_SCREENS: &str = "multi_screen.max_screens";

/// 并发发声开关在 `setting` 表中的键（默认关闭，硬约束 C6：新增能力默认关闭）。
const KEY_CONCURRENT_AUDIO: &str = "multi_screen.concurrent_audio";

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
    /// 当前扫描的取消位。界面点「取消」置位，扫描就地收尾（见 `cancel_scan`）。
    scan_cancel: Arc<AtomicBool>,
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
    state: String,
    /// 最近一次扫描失败的原因（PRD FR-01 异常表）。无失败时为 `None`。
    last_error: Option<String>,
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
    /// 布局标记（`1x2` / `2x1` / `2x2` / `1plus3`）。
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

fn data_dir() -> std::result::Result<PathBuf, String> {
    if let Ok(dir) = std::env::var("CINEMA_DATA_DIR") {
        if !dir.trim().is_empty() {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
            return Ok(path);
        }
    }
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let path = base.join("Cinema");
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    Ok(path)
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

/// 在线影片缓存目录（`<数据目录>/streams`），不存在则创建。
///
/// 在线源采用「边下边播」：yt-dlp 后台把影片写到这里（`online-<hash>.mp4`），
/// 播放内核打开这个**正在增长**的文件（见 L3 `online_site.rs` 模块注释）。
fn streams_dir() -> std::result::Result<PathBuf, String> {
    let dir = data_dir()?.join("streams");
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

fn stop_preview_inner(state: &AppState) {
    if let Ok(mut svc) = state.playback.lock() {
        svc.stop_preview();
    }
    state.factory.set_target(None);
    state.factory.forget_preview();
    if let Ok(hosts) = state.hosts.lock() {
        if let Some(host) = &hosts.preview {
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
        state: "idle".into(),
        last_error: None,
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
            state: match s.state {
                SourceState::Idle => "idle",
                SourceState::Scanning => "scanning",
                SourceState::ScanFailed => "scan_failed",
            }
            .into(),
            last_error,
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

    tauri::async_runtime::spawn_blocking(move || {
        let provider = provider_for(&src, prober.as_ref(), &downloads)
            .map_err(|e| e.reason().to_string())?;
        let control = ScanControl {
            reporter: Some(reporter),
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
    .map_err(|e| format!("扫描任务异常中止：{e}"))?
}

/// 请求中止当前扫描。已入库的条目保留，源不会被置为「扫描失败」。
#[tauri::command]
fn cancel_scan(state: State<'_, AppState>) {
    state.scan_cancel.store(true, Ordering::SeqCst);
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
}

/// 联网导入：把粘贴的列表页 URL 登记为在线站点源并扫描。
///
/// 同一 URL 重复导入会命中同一源 id（id 由 URL 派生）并幂等合并条目，
/// 封面以「文件是否存在」判断是否重新抓取，不会重复下载。
///
/// yt-dlp 缺失时**在开子进程之前**就返回可读提示，避免用户只看到晦涩的子进程错误。
#[tauri::command]
fn online_import(
    state: State<'_, AppState>,
    url: String,
) -> std::result::Result<OnlineImportView, String> {
    let url = url.trim().to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("请填写以 http(s) 开头的列表页地址".into());
    }
    let ytdlp = ytdlp_path()?;
    let covers = poster_dir()?;

    let src = MediaSource {
        id: source_id_for(&url),
        kind: SourceKind::OnlineSite,
        name: site_label(&url),
        location: url,
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    };
    {
        let db = state.db.lock().map_err(|_| "数据库锁不可用".to_string())?;
        MediaSourceRepo::upsert(db.conn(), &src).map_err(|e| e.to_string())?;
    }

    let provider = OnlineSiteProvider::new(ytdlp, covers, streams_dir()?, Arc::clone(&state.downloads));
    let report = ImportService::import(state.store.as_ref(), &provider, &src)
        .map_err(|e| e.reason().to_string())?;

    // 导入成功就顺手在后台抓一遍封面：封面来自列表页卡片，与条目枚举共用同一页抓取，
    // 因此此刻抓最省事；用户若只是「导入完就退出」，封面也不会等到下次浏览才补
    spawn_cover_fetch(&state, &src);

    Ok(OnlineImportView {
        source_id: src.id.as_str().to_string(),
        source_name: src.name,
        discovered: report.stat.discovered,
        stored: report.stored,
        probed: report.probed,
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
    if src.kind != SourceKind::OnlineSite {
        return;
    }
    let location = src.location.trim().to_owned();
    if location.is_empty() {
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
        OnlineSiteProvider::new(ytdlp, covers, streams, downloads).fetch_covers(&location);
    });
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
            host.set_on_click(move || {
                let _ = app.emit("player-click", ());
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
    if let Some(slot) = current_slot(&state) {
        if let Ok(svc) = state.playback.lock() {
            svc.screen_command(slot, PlayerCmd::Seek { position_ms });
        }
        // 跳转后清空实时字幕缓冲、重置句界、丢弃旧句；忽略其错误
        if let Ok(mut runtime) = state.subtitles.lock() {
            let _ = runtime.main().service.on_seek(position_ms);
        }
    }
}

#[tauri::command]
fn close_player(state: State<'_, AppState>) -> std::result::Result<(), String> {
    close_all_screens(&state);
    Ok(())
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
    // 在线源（边下边播）不提供悬停预览：预览要顺带把整部影片下载下来，代价远大于收益，
    // 也会挤占正在播放的下载。悬停即拒绝，界面按既有方式标注原因。
    if src.kind == SourceKind::OnlineSite {
        return Ok(PreviewResultView {
            started: false,
            reason: Some("在线条目暂不提供悬停预览".into()),
        });
    }
    let provider = provider_for(&src, state.prober.as_ref(), &state.downloads)
        .map_err(|e| e.reason().to_string())?;
    let ref_ = provider.open(&item).map_err(|e| e.reason().to_string())?;

    // 「是否有精华片段」由时长是否已探明决定；起始位置判定在 PRD 中标为 TBD，
    // 因此 M1 一律从片头开始（与 P-04 预览起点恒为 None 一致）。
    let has_highlight = item.duration_ms.is_some();

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
            stop_preview_inner(&state);
            Ok(PreviewResultView {
                started: false,
                reason: Some(rejection.message().to_string()),
            })
        }
    }
}

#[tauri::command]
fn stop_preview(state: State<'_, AppState>) {
    stop_preview_inner(&state);
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

            let data_dir = data_dir()?;
            let db = Database::open(data_dir.join("cinema.db")).map_err(|e| e.to_string())?;
            let version = db.migrate().map_err(|e| e.to_string())?;

            // 播种内置标签维度：幂等，已存在的 id 不覆盖（用户改过的配置优先）
            DimensionRepo::seed(db.conn(), &TagDimensionDef::builtin_defaults())
                .map_err(|e| e.to_string())?;

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
                // 与本次会话的扫描一一对应；`scan_source` 每次开跑都会先清零
                scan_cancel: Arc::new(AtomicBool::new(false)),
            });

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
                        if let Ok(hosts) = state.hosts.lock() {
                            if let Some(host) = hosts.screens.get(&0) {
                                host.fill_parent(parent, CONTROL_BAR_PX);
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
            close_player,
            start_preview,
            stop_preview,
            set_preview_enabled,
            ui_constants,
            library_stats,
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


