//! L0 组合根：实时字幕（P-03 / PRD FR-05）的装配与命令面。
//!
//! 本文件只做三件事，业务规则一律不在此重新定义（门槛来自 L2、流程来自 L1）：
//! 1. **引擎注册表**（[`SubtitleEngines`]）：本地引擎恒注册且为默认（架构 §2「本地默认」），
//!    远端引擎仅在配置了 `CINEMA_ASR_REMOTE_URL` 时注册，内置脚本引擎仅在
//!    `CINEMA_ASR_INLINE` 置位时注册；
//! 2. **运行时**（[`SubtitleRuntime`]）：主上下文一份 [`SubtitleService`]，
//!    并发发声模式下多屏每格再各持一份（键为屏位），外加组合根
//!    缓存的会话目标语种（界面据此判定译文行）；
//! 3. **命令面**：把 L1 的编排结果翻译成前端可消费的视图，并暴露 `subtitle_*` 命令。

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
use tauri::State;

use cinema_adapters::subtitle::{
    LocalAsrSubtitleEngine, RemoteSubtitleEngine, ScriptedInlineEngine,
};
use cinema_adapters::xunlei_subtitle::{OnlineSubtitle, XunleiSubtitleClient};
use cinema_app::subtitle_service::{
    CloseOutcome, EngineInfo, EngineOption, EngineRegistry, StartRequest, SubtitleService,
    SubtitleStore, SubtitleView, KEY_ENABLED, KEY_REMOTE_AUTH,
};
use cinema_domain::model::{
    EngineLocality, ItemId, MediaItem, QualityLevel, SubtitleCue, SubtitleKind, SubtitleTrack,
    TapMode,
};
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::subtitle::{AudioTap, SubtitleEngine};
use cinema_domain::subtitle::{
    parse_subtitle_text, session_cfg, CueDisplay, DegradeAction, RealtimeState,
};
use cinema_infra::repo::MediaItemRepo;
use cinema_infra::subtitle_repo::SubtitleTrackRepo;

use crate::AppState;

// ---------------------------------------------------------------------------
// 注册表与运行时
// ---------------------------------------------------------------------------

/// 引擎注册表：新增引擎只需在此登记，不改动 L1 服务。
pub struct SubtitleEngines {
    engines: Vec<(EngineInfo, Arc<dyn SubtitleEngine>)>,
    default_id: String,
}

impl EngineRegistry for SubtitleEngines {
    fn list(&self) -> Vec<EngineInfo> {
        self.engines.iter().map(|(info, _)| info.clone()).collect()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn SubtitleEngine>> {
        self.engines
            .iter()
            .find(|(info, _)| info.id == id)
            .map(|(_, engine)| Arc::clone(engine))
    }

    fn default_id(&self) -> String {
        self.default_id.clone()
    }
}

/// 构造注册表：本地引擎恒注册且为默认；远端引擎仅在配置了服务地址时注册；
/// 内置脚本引擎仅在 `CINEMA_ASR_INLINE` 置位时注册（诊断 / 演示用）；
/// 被禁用的空引擎不注册。
pub fn build_registry() -> SubtitleEngines {
    let local = LocalAsrSubtitleEngine::new();
    let default_id = local.id().to_owned();
    let mut engines: Vec<(EngineInfo, Arc<dyn SubtitleEngine>)> = vec![(
        EngineInfo {
            id: default_id.clone(),
            locality: local.locality(),
        },
        Arc::new(local),
    )];

    if let Some(remote) = RemoteSubtitleEngine::new() {
        engines.push((
            EngineInfo {
                id: remote.id().to_owned(),
                locality: remote.locality(),
            },
            Arc::new(remote),
        ));
    }

    // 内置脚本引擎（M5 增量）：只加这一处登记。默认不出现，免得用户看到一个
    // 只会念固定台词的引擎；置位环境变量即可用于演示「新增引擎＝新增实现＋注册」。
    if std::env::var("CINEMA_ASR_INLINE")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        let inline = ScriptedInlineEngine::new();
        engines.push((
            EngineInfo {
                id: inline.id().to_owned(),
                locality: inline.locality(),
            },
            Arc::new(inline),
        ));
    }

    SubtitleEngines {
        engines,
        default_id,
    }
}

/// 单份字幕会话上下文：一个 [`SubtitleService`] 实例 + 组合根缓存的会话目标语种。
pub struct SlotRuntime {
    pub service: SubtitleService<SubtitleEngines>,
    /// 最近一次开启会话的目标译文语种（缺省简体中文）；`is_translation` 判定用。
    pub(crate) target_lang: Option<String>,
}

impl SlotRuntime {
    fn new() -> Self {
        Self {
            service: SubtitleService::new(build_registry()),
            target_lang: None,
        }
    }
}

/// 实时字幕运行时。
///
/// - **主上下文**：单路播放，以及未开启并发发声时的多屏（会话只跟随发声窗口），
///   行为与既有实现逐位一致；
/// - **格上下文**：并发发声模式下多屏每格各持一份会话（键为屏位），
///   因此多路可以同时识别与显示字幕（各格音频抽头绑定在各自实例上）。
pub struct SubtitleRuntime {
    main: SlotRuntime,
    slots: BTreeMap<u32, SlotRuntime>,
}

impl SubtitleRuntime {
    pub fn new() -> Self {
        Self {
            main: SlotRuntime::new(),
            slots: BTreeMap::new(),
        }
    }

    /// 主上下文（单路播放 / 发声窗口；未开启并发发声时的唯一会话）。
    pub fn main(&mut self) -> &mut SlotRuntime {
        &mut self.main
    }

    /// 指定屏位的并发上下文；首次访问时创建。
    pub fn slot(&mut self, slot: u32) -> &mut SlotRuntime {
        self.slots.entry(slot).or_insert_with(SlotRuntime::new)
    }

    /// 已存在的并发格上下文键（切换发声模式时按此收尾）。
    pub fn slot_keys(&self) -> Vec<u32> {
        self.slots.keys().copied().collect()
    }

    /// 收尾并丢弃某格的并发上下文（该格关闭时调用）。
    pub fn close_slot(&mut self, store: &dyn SubtitleStore, slot: u32) {
        if let Some(mut ctx) = self.slots.remove(&slot) {
            let _ = ctx.service.close(store);
        }
    }

    /// 收尾全部会话（退出多屏 / 全部停止）。
    pub fn close_all(&mut self, store: &dyn SubtitleStore) {
        let _ = self.main.service.close(store);
        self.main.target_lang = None;
        for (_, mut ctx) in std::mem::take(&mut self.slots) {
            let _ = ctx.service.close(store);
        }
    }
}

// ---------------------------------------------------------------------------
// 面向界面的视图对象
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleEngineView {
    pub id: String,
    pub locality: String,
    pub authorized: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleInfoView {
    pub enabled: bool,
    pub remote_authorized: bool,
    pub state: String,
    pub display: String,
    pub quality: Option<String>,
    pub failure: Option<String>,
    pub paused_for_resources: bool,
    pub cue_count: i64,
    pub engines: Vec<SubtitleEngineView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleCueView {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub lang: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleTrackView {
    pub id: String,
    pub kind: String,
    pub lang: Option<String>,
    pub cue_count: i64,
    pub cues: Vec<SubtitleCueView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleEventView {
    pub kind: String,
    pub text: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub lang: Option<String>,
    pub is_translation: Option<bool>,
    pub level: Option<String>,
    pub action: Option<String>,
    pub reason: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitlePollView {
    pub state: String,
    pub display: String,
    pub quality: Option<String>,
    pub failure: Option<String>,
    pub paused_for_resources: bool,
    pub cue_count: i64,
    pub events: Vec<SubtitleEventView>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleCloseView {
    pub saved: bool,
    pub track_id: Option<String>,
    pub cue_count: i64,
}

// ---------------------------------------------------------------------------
// 枚举 → 界面字符串（取值必须与 L2 一致）
// ---------------------------------------------------------------------------

/// 会话状态：`idle|initializing|running|paused|saved|failed`。
fn state_tag(state: RealtimeState) -> &'static str {
    match state {
        RealtimeState::Idle => "idle",
        RealtimeState::Initializing => "initializing",
        RealtimeState::Running => "running",
        RealtimeState::Paused => "paused",
        RealtimeState::Saved => "saved",
        RealtimeState::Failed => "failed",
    }
}

/// 引擎所在位置：`local|remote`。
fn locality_tag(locality: EngineLocality) -> &'static str {
    match locality {
        EngineLocality::Local => "local",
        EngineLocality::Remote => "remote",
    }
}

/// 识别质量：`ok|degraded|critical`。
fn quality_tag(level: QualityLevel) -> &'static str {
    match level {
        QualityLevel::Ok => "ok",
        QualityLevel::Degraded => "degraded",
        QualityLevel::Critical => "critical",
    }
}

/// 轨道类型：`external|generated|embedded`。
fn kind_tag(kind: SubtitleKind) -> &'static str {
    match kind {
        SubtitleKind::External => "external",
        SubtitleKind::Generated => "generated",
        SubtitleKind::Embedded => "embedded",
    }
}

/// 水位降级动作：`originOnly|pause`；`None` 不产生事件。
fn degrade_tag(action: DegradeAction) -> Option<&'static str> {
    match action {
        DegradeAction::None => None,
        DegradeAction::OriginOnly => Some("originOnly"),
        DegradeAction::Pause => Some("pause"),
    }
}

/// 界面用的呈现方式标记（camelCase，与前端常量一致）。
///
/// L2 的 [`CueDisplay::tag`] 用 snake_case，这里是**界面契约**的取值，
/// 因此单独映射，避免前端跟着库内部命名走。
fn display_tag(display: CueDisplay) -> &'static str {
    match display {
        CueDisplay::Bilingual => "bilingual",
        CueDisplay::TranslationOnly => "translationOnly",
        CueDisplay::OriginOnly => "originOnly",
    }
}

/// 解析界面传入的呈现方式标记：接受界面用的 camelCase，也兼容 L2 的 snake_case
/// 标记；无法解析时**报错**（绝不默认成双语）。
fn parse_display(raw: &str) -> std::result::Result<CueDisplay, String> {
    match raw {
        "bilingual" => Ok(CueDisplay::Bilingual),
        "translationOnly" | "translation_only" => Ok(CueDisplay::TranslationOnly),
        "originOnly" | "origin_only" => Ok(CueDisplay::OriginOnly),
        other => Err(format!("未知的字幕呈现方式：{other}")),
    }
}

// ---------------------------------------------------------------------------
// 视图构造
// ---------------------------------------------------------------------------

fn empty_event(kind: &'static str) -> SubtitleEventView {
    SubtitleEventView {
        kind: kind.to_owned(),
        text: None,
        start_ms: None,
        end_ms: None,
        lang: None,
        is_translation: None,
        level: None,
        action: None,
        reason: None,
    }
}

fn engine_view(option: EngineOption) -> SubtitleEngineView {
    SubtitleEngineView {
        id: option.id,
        locality: locality_tag(option.locality).to_owned(),
        authorized: option.authorized,
    }
}

fn cue_view(cue: SubtitleCue) -> SubtitleCueView {
    SubtitleCueView {
        start_ms: cue.start_ms,
        end_ms: cue.end_ms,
        text: cue.text,
        lang: cue.lang,
    }
}

fn track_view(track: SubtitleTrack) -> SubtitleTrackView {
    let cue_count = track.cues.len() as i64;
    SubtitleTrackView {
        id: track.id,
        kind: kind_tag(track.kind).to_owned(),
        lang: track.lang,
        cue_count,
        cues: track.cues.into_iter().map(cue_view).collect(),
    }
}

/// L1 一轮输出 → 界面事件。`is_translation` 由 `cue.lang == 当前 target_lang` 判定。
fn event_view(view: SubtitleView, target_lang: Option<&str>) -> SubtitleEventView {
    match view {
        SubtitleView::Partial { text } => SubtitleEventView {
            text: Some(text),
            ..empty_event("partial")
        },
        SubtitleView::Cue(cue) => {
            let is_translation =
                matches!((target_lang, cue.lang.as_deref()), (Some(t), Some(l)) if l == t);
            SubtitleEventView {
                start_ms: Some(cue.start_ms),
                end_ms: Some(cue.end_ms),
                text: Some(cue.text),
                lang: cue.lang,
                is_translation: Some(is_translation),
                ..empty_event("cue")
            }
        }
        SubtitleView::Quality { level } => SubtitleEventView {
            level: Some(quality_tag(level).to_owned()),
            ..empty_event("quality")
        },
        SubtitleView::Degraded { action } => SubtitleEventView {
            action: degrade_tag(action).map(str::to_owned),
            ..empty_event("degraded")
        },
        SubtitleView::Failed { reason } => SubtitleEventView {
            reason: Some(reason),
            ..empty_event("failed")
        },
    }
}

fn info_view(
    runtime: &SlotRuntime,
    store: &dyn SubtitleStore,
) -> std::result::Result<SubtitleInfoView, String> {
    let enabled = runtime
        .service
        .feature_enabled(store)
        .map_err(|e| e.reason().to_string())?;
    let remote_authorized = store
        .setting(KEY_REMOTE_AUTH)
        .map_err(|e| e.reason().to_string())?
        .as_deref()
        == Some("true");
    let engines = runtime
        .service
        .available_engines(store)
        .map_err(|e| e.reason().to_string())?
        .into_iter()
        .map(engine_view)
        .collect();
    let status = runtime.service.status();
    Ok(SubtitleInfoView {
        enabled,
        remote_authorized,
        state: state_tag(status.state).to_owned(),
        display: display_tag(status.display).to_owned(),
        quality: status.quality.map(|l| quality_tag(l).to_owned()),
        failure: status.failure,
        paused_for_resources: status.paused_for_resources,
        cue_count: status.cue_count as i64,
        engines,
    })
}

// ---------------------------------------------------------------------------
// Tauri 命令（前端按 camelCase 消费；参数名用 Rust snake_case 由 Tauri v2 映射）
// ---------------------------------------------------------------------------

/// 某格（或当前作用对象）正在播放的条目 id；实时字幕只在正式播放时可用。
///
/// `slot` 为 `Some` 时按该格取值（并发发声模式下每格独立）；为 `None` 时取组合根
/// 认定的当前作用对象（单路播放 / 未开启并发发声时的发声窗口）。
fn item_id_for(state: &AppState, slot: Option<u32>) -> std::result::Result<ItemId, String> {
    let slot = match slot {
        Some(slot) => slot,
        None => crate::current_slot(state).ok_or_else(|| "当前没有正在播放的条目".to_string())?,
    };
    crate::slot_item(state, slot).ok_or_else(|| "当前没有正在播放的条目".to_string())
}

/// 取该格的音频抽头（实时识别只接正式播放实例）；拿不到即视为不可用。
fn audio_for(
    state: &AppState,
    slot: Option<u32>,
) -> std::result::Result<(u32, Box<dyn AudioTap>), String> {
    let slot = match slot {
        Some(slot) => slot,
        None => crate::current_slot(state).ok_or_else(|| "实时字幕只在正式播放时可用".to_string())?,
    };
    let audio = state
        .factory
        .screen(slot)
        .and_then(|core| core.audio_tap(TapMode::Pcm16kMono))
        .ok_or_else(|| "实时字幕只在正式播放时可用（当前无法抽取音频）".to_string())?;
    Ok((slot, audio))
}

#[tauri::command]
pub fn subtitle_info(
    state: State<'_, AppState>,
    slot: Option<u32>,
) -> std::result::Result<SubtitleInfoView, String> {
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    info_view(ctx, state.store.as_ref())
}

#[tauri::command]
pub fn subtitle_set_enabled(
    state: State<'_, AppState>,
    enabled: bool,
    allow_remote: bool,
    slot: Option<u32>,
) -> std::result::Result<SubtitleInfoView, String> {
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let store = state.store.as_ref();
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    if enabled {
        ctx
            .service
            .enable_feature(store, allow_remote)
            .map_err(|e| e.reason().to_string())?;
    } else {
        // 关闭：功能开关回落默认（隐私授权单独保留，由远端门窗把关）
        store
            .set_setting(KEY_ENABLED, "false")
            .map_err(|e| e.reason().to_string())?;
    }
    info_view(ctx, store)
}

#[tauri::command]
pub fn subtitle_start(
    state: State<'_, AppState>,
    origin_lang: Option<String>,
    target_lang: Option<String>,
    display: String,
    engine_id: Option<String>,
    slot: Option<u32>,
) -> std::result::Result<(), String> {
    let display = parse_display(&display)?;

    let item_id = item_id_for(&state, slot)
        .map_err(|_| "实时字幕只在正式播放时可用".to_string())?;
    // 经 PlayerCore 端口取该格的音频抽头；拿不到即视为不可用。
    // `audio_for` 会把 `None` 兜底到当前发声窗口并返回实际屏位，但**不能**用它改写
    // 外层 `slot`：上下文归属仍按「有 slot ＝ 格上下文、无 slot ＝ 主上下文」判定。
    let (_tap_slot, audio) = audio_for(&state, slot)?;

    // 目标语种取与 L2 一致的口径（缺省简体中文），供译文行判定
    let cfg = session_cfg(origin_lang.as_deref(), target_lang.as_deref(), display);

    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    ctx.target_lang = cfg.target_lang.clone();

    let request = StartRequest {
        origin_lang,
        target_lang: cfg.target_lang,
        display,
        engine_id,
    };
    ctx
        .service
        .start(state.store.as_ref(), &item_id, audio, request)
        .map_err(|e| e.reason().to_string())
}

#[tauri::command]
pub fn subtitle_set_display(
    state: State<'_, AppState>,
    display: String,
    slot: Option<u32>,
) -> std::result::Result<(), String> {
    let display = parse_display(&display)?;
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    ctx.service.set_display(display);
    Ok(())
}

#[tauri::command]
pub fn subtitle_poll(
    state: State<'_, AppState>,
    slot: Option<u32>,
) -> std::result::Result<SubtitlePollView, String> {
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };

    // 先查水位（降级动作由 check_watermark 返回），再拉取引擎事件
    let degrade = ctx.service.check_watermark();
    let views = ctx.service.poll();
    let target_lang = ctx.target_lang.clone();

    let mut events = Vec::new();
    if let Some(action) = degrade_tag(degrade) {
        events.push(SubtitleEventView {
            action: Some(action.to_owned()),
            ..empty_event("degraded")
        });
    }
    events.extend(
        views
            .into_iter()
            .map(|view| event_view(view, target_lang.as_deref())),
    );

    let status = ctx.service.status();
    Ok(SubtitlePollView {
        state: state_tag(status.state).to_owned(),
        display: display_tag(status.display).to_owned(),
        quality: status.quality.map(|l| quality_tag(l).to_owned()),
        failure: status.failure,
        paused_for_resources: status.paused_for_resources,
        cue_count: status.cue_count as i64,
        events,
    })
}

#[tauri::command]
pub fn subtitle_save(
    state: State<'_, AppState>,
    slot: Option<u32>,
) -> std::result::Result<SubtitleTrackView, String> {
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    let track = ctx
        .service
        .save(state.store.as_ref())
        .map_err(|e| e.reason().to_string())?;
    Ok(track_view(track))
}

#[tauri::command]
pub fn subtitle_close(
    state: State<'_, AppState>,
    slot: Option<u32>,
) -> std::result::Result<SubtitleCloseView, String> {
    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    let outcome = ctx
        .service
        .close(state.store.as_ref())
        .map_err(|e| e.reason().to_string())?;
    Ok(close_view(outcome))
}

fn close_view(outcome: CloseOutcome) -> SubtitleCloseView {
    match outcome.saved_unsaved {
        Some(track) => SubtitleCloseView {
            saved: true,
            track_id: Some(track.id),
            cue_count: track.cues.len() as i64,
        },
        None => SubtitleCloseView {
            saved: false,
            track_id: None,
            cue_count: 0,
        },
    }
}

#[tauri::command]
pub fn subtitle_add_external(
    state: State<'_, AppState>,
    path: String,
    slot: Option<u32>,
) -> std::result::Result<SubtitleTrackView, String> {
    let item_id = item_id_for(&state, slot)?;

    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("读取外挂字幕文件失败：{path}（{e}）"))?;
    let cues = parse_subtitle_text(&text);
    if cues.is_empty() {
        return Err(format!("外挂字幕文件无法解析出任何字幕句：{path}"));
    }

    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    let track = ctx
        .service
        .add_external_track(state.store.as_ref(), &item_id, cues, None)
        .map_err(|e| e.reason().to_string())?;
    Ok(track_view(track))
}

/// 在线字幕候选（界面列表项）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineSubtitleView {
    pub name: String,
    pub url: String,
    pub ext: String,
    pub lang: String,
    pub source: String,
    pub duration_ms: Option<i64>,
}

fn online_view(sub: OnlineSubtitle) -> OnlineSubtitleView {
    OnlineSubtitleView {
        name: sub.name,
        url: sub.url,
        ext: sub.ext,
        lang: sub.lang,
        source: sub.source,
        duration_ms: sub.duration_ms,
    }
}

/// 搜索关键词候选：番号优先，其次标题与文件名（去重，最多 3 个）。
fn online_search_terms(item: &MediaItem) -> Vec<String> {
    let file_stem = item
        .rel_path
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_stem())
        .and_then(|s| s.to_str());
    let candidates = [
        item.external_code.as_deref(),
        Some(item.title.as_str()),
        file_stem,
        item.title_origin.as_deref(),
    ];
    let mut terms: Vec<String> = Vec::new();
    for candidate in candidates.into_iter().flatten() {
        let term = candidate.trim();
        if term.is_empty() || terms.iter().any(|t| t.eq_ignore_ascii_case(term)) {
            continue;
        }
        terms.push(term.to_owned());
        if terms.len() >= 3 {
            break;
        }
    }
    terms
}

/// 在线字幕搜索：`query` 为空时按条目番号 / 标题 / 文件名依次检索迅雷字幕库。
#[tauri::command]
pub async fn subtitle_online_search(
    state: State<'_, AppState>,
    item_id: Option<String>,
    query: Option<String>,
) -> std::result::Result<Vec<OnlineSubtitleView>, String> {
    let explicit = query
        .map(|q| q.trim().to_owned())
        .filter(|q| !q.is_empty());
    let terms = match explicit {
        Some(q) => vec![q],
        None => {
            let item_id = match item_id {
                Some(id) => ItemId::from(id),
                None => item_id_for(&state, None)?,
            };
            let db = state
                .db
                .lock()
                .map_err(|_| "数据库锁不可用".to_string())?;
            let item = MediaItemRepo::get(db.conn(), &item_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "找不到该条目".to_string())?;
            online_search_terms(&item)
        }
    };
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    tauri::async_runtime::spawn_blocking(move || {
        let client = XunleiSubtitleClient::new();
        let mut merged: Vec<OnlineSubtitle> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut last_error: Option<String> = None;
        for term in terms {
            match client.search(&term) {
                Ok(list) => {
                    for sub in list {
                        if seen.insert(sub.url.clone()) {
                            merged.push(sub);
                        }
                    }
                    if !merged.is_empty() {
                        break;
                    }
                }
                Err(err) => last_error = Some(err.reason().to_owned()),
            }
        }
        // 所有关键词都请求失败时如实上报，避免界面误判为「没有字幕」
        if merged.is_empty() {
            if let Some(reason) = last_error {
                return Err(reason);
            }
        }
        merged.truncate(30);
        Ok::<Vec<OnlineSubtitleView>, String>(merged.into_iter().map(online_view).collect())
    })
    .await
    .map_err(|e| format!("在线字幕搜索异常中止：{e}"))?
}

/// 下载并挂载一条在线字幕：解析为字幕句后保存为外挂轨道（替换旧外挂轨道）。
#[tauri::command]
pub async fn subtitle_online_load(
    state: State<'_, AppState>,
    item_id: Option<String>,
    url: String,
    lang: Option<String>,
    slot: Option<u32>,
) -> std::result::Result<SubtitleTrackView, String> {
    let item_id = match item_id {
        Some(id) => ItemId::from(id),
        None => item_id_for(&state, slot)?,
    };
    let url = url.trim().to_owned();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("在线字幕地址无效".to_string());
    }

    let text = tauri::async_runtime::spawn_blocking(move || {
        XunleiSubtitleClient::new().fetch_text(&url)
    })
    .await
    .map_err(|e| format!("在线字幕下载异常中止：{e}"))?
    .map_err(|e| e.reason().to_owned())?;

    let cues = parse_subtitle_text(&text);
    if cues.is_empty() {
        return Err("该在线字幕无法解析出任何字幕句".to_string());
    }
    let lang = lang
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty() && l != "未标注" && l != "默认");

    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    let track = ctx
        .service
        .add_external_track(state.store.as_ref(), &item_id, cues, lang)
        .map_err(|e| e.reason().to_string())?;
    Ok(track_view(track))
}

#[tauri::command]
pub fn subtitle_tracks(
    state: State<'_, AppState>,
    item_id: String,
) -> std::result::Result<Vec<SubtitleTrackView>, String> {
    let db = state
        .db
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    let tracks = SubtitleTrackRepo::list_by_item(db.conn(), &ItemId::from(item_id))
        .map_err(|e| e.to_string())?;
    Ok(tracks.into_iter().map(track_view).collect())
}

#[tauri::command]
pub fn subtitle_entry_visible(
    state: State<'_, AppState>,
    item_id: String,
    slot: Option<u32>,
) -> std::result::Result<bool, String> {
    // 内嵌字幕事实来自「打开播放」时探测并缓存的流事实；仅当条目匹配且清单非空才算有内嵌。
    // 并发发声模式下按该格判定；否则以组合根认定的当前作用对象（发声窗口）为准。
    let facts_slot = match slot {
        Some(slot) => Some(slot),
        None => crate::current_slot(&state),
    };
    let has_embedded = facts_slot
        .and_then(|slot| {
            let item = crate::slot_item(&state, slot)?;
            if item.as_str() != item_id.as_str() {
                return None;
            }
            crate::slot_facts(&state, slot)
        })
        .map(|facts| !facts.embedded_subtitles.is_empty())
        .unwrap_or(false);

    let mut runtime = state
        .subtitles
        .lock()
        .map_err(|_| "实时字幕状态锁不可用".to_string())?;
    let ctx = match slot {
        Some(slot) => runtime.slot(slot),
        None => runtime.main(),
    };
    ctx
        .service
        .entry_visible(state.store.as_ref(), &ItemId::from(item_id), has_embedded)
        .map_err(|e| e.reason().to_string())
}
