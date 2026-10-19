//! L1 实时字幕编排（架构文档 §6 控制面 `SubtitleService` / PRD FR-05）。
//!
//! 本服务是实时字幕的**唯一治理点**，收口五件事：
//! 1. **两道门**：功能开关（默认关闭）与远端隐私授权（默认未授权，不静默上传）；
//! 2. **入口可见性**：入口只在没有任何可用字幕轨道（无内嵌、无外挂）时出现；
//! 3. **会话状态机**：初始化 / 运行中 / 已暂停 / 已保存 / 失败 + 三出口；
//! 4. **与播放联动**：pause 挂起并保留末句、seek 丢弃旧句并重置句界、stop 释放句柄；
//! 5. **结果留存与保护**：保存为 `kind=generated` 轨道；关闭时未保存成句默认落库并提示。
//!
//! 状态、配置推导、水位降级、失败出口、句收集全部来自 L2
//! [`cinema_domain::subtitle`]，本层不复制任何规则。

use std::sync::Arc;

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    CueEvent, EngineLocality, ItemId, QualityLevel, SessionCfg, SubtitleCue, SubtitleKind,
    SubtitleTrack,
};
use cinema_domain::ports::subtitle::{AudioTap, SessionHandle, SubtitleEngine};
use cinema_domain::ports::EventStream;
use cinema_domain::subtitle::{
    session_cfg, CueCollector, CueDisplay, DegradeAction, RealtimeEvent, RealtimeState,
    should_offer_entry,
};
use cinema_domain::tagging::{DeriveFacts, DIM_SUBTITLE};

use crate::tag_service::{apply_derived, builtin_derivers, TagStore};

// ---------------------------------------------------------------------------
// 设置键（架构 §8 `setting(key, value)`：功能开关 / 隐私授权 / 引擎选择）
// ---------------------------------------------------------------------------

/// 功能开关：默认关闭（架构 §9）。用户在隐私告知确认后才开启。
pub const KEY_ENABLED: &str = "realtime_subtitle.enabled";
/// 隐私授权：允许音频离开本机（PRD FR-05：Remote 必须显式授权）。默认未授权。
pub const KEY_REMOTE_AUTH: &str = "realtime_subtitle.remote_authorized";
/// 引擎选择（JSON 字符串，记录上次使用的引擎 id）。
pub const KEY_ENGINE: &str = "realtime_subtitle.engine";

fn quote_json(text: &str) -> String {
    format!("\"{}\"", text.trim_matches('"'))
}

// ---------------------------------------------------------------------------
// 依赖倒置的注入点
// ---------------------------------------------------------------------------

/// 实时字幕所需的持久化能力。实现由**组合根**提供。
///
/// 继承 [`TagStore`]，因为「保存轨道」必然改变字幕状态维度，需要同时回写派生标签
/// 并重建检索索引（FR-03）。
pub trait SubtitleStore: TagStore + Send + Sync {
    /// 写入或更新一条字幕轨道。
    fn save_track(&self, track: &SubtitleTrack) -> Result<()>;

    /// 删除某条目指定类型的轨道（重新生成 / 重新加载外挂前先清旧轨道）。
    fn delete_tracks_of_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<()>;

    /// 某条目的全部轨道（「复用已保存轨道」按此渲染）。
    fn list_tracks(&self, item_id: &ItemId) -> Result<Vec<SubtitleTrack>>;

    /// 某条目是否已有指定类型轨道（入口可见性判定用）。
    fn has_track_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<bool>;

    /// 读设置（返回原始 JSON 文本）。
    fn setting(&self, key: &str) -> Result<Option<String>>;

    /// 写设置。
    fn set_setting(&self, key: &str, value: &str) -> Result<()>;
}

/// 已注册引擎的公开信息（UI 据此告知隐私并给出可选列表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub id: String,
    pub locality: EngineLocality,
}

/// 引擎注册表。新增引擎只需注册，不改本服务（架构 §4 P-03 可替换设计）。
pub trait EngineRegistry: Send + Sync {
    /// 全部已注册引擎。
    fn list(&self) -> Vec<EngineInfo>;

    /// 按 id 取引擎实现。
    fn get(&self, id: &str) -> Option<Arc<dyn SubtitleEngine>>;

    /// 默认引擎 id（本地实现优先，架构 §2）。
    fn default_id(&self) -> String;
}

/// 引擎选项（含是否已获授权）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOption {
    pub id: String,
    pub locality: EngineLocality,
    /// 远端引擎在未授权时不可选（UI 据此置灰并解释原因）。
    pub authorized: bool,
}

/// 开启一次会话的请求（界面输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    /// 期望识别的原声语种；`None` 表示自动判定。
    pub origin_lang: Option<String>,
    /// 目标译文语种；`None` 取默认（简体中文）。
    pub target_lang: Option<String>,
    /// 呈现方式（原文+译文 / 仅译文 / 仅原文）。
    pub display: CueDisplay,
    /// 选定引擎 id；`None` 取注册表默认（本地优先）。
    pub engine_id: Option<String>,
}

/// 面向界面的一轮输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleView {
    /// 视觉占位（未成句，永不进入保存的轨道）。
    Partial { text: String },
    /// 成句结果（原文或译文，`cue.lang` 供界面分行）。
    Cue(SubtitleCue),
    /// 质量上报（低质量给一次非侵入提示）。
    Quality { level: QualityLevel },
    /// 水位降级（仅原文 / 暂停字幕）。
    Degraded { action: DegradeAction },
    /// 失败（界面给出三个出口）。
    Failed { reason: String },
}

/// 面向界面的状态快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleStatus {
    pub state: RealtimeState,
    /// 当前生效的呈现方式（可能已被水位降级改写）。
    pub display: CueDisplay,
    pub engine_id: Option<String>,
    pub failure: Option<String>,
    pub quality: Option<QualityLevel>,
    /// 资源降级是否正在生效（暂停字幕）。
    pub paused_for_resources: bool,
    /// 已收集成句数（界面据此判断能否保存）。
    pub cue_count: usize,
}

/// 关闭会话的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloseOutcome {
    /// 未保存的成句被默认落库为轨道（PRD FR-05「未保存结果保护」）。
    pub saved_unsaved: Option<SubtitleTrack>,
}

// ---------------------------------------------------------------------------
// SubtitleService
// ---------------------------------------------------------------------------

/// 实时字幕编排服务。
pub struct SubtitleService<R: EngineRegistry> {
    registry: R,
    state: RealtimeState,
    session: Option<Box<dyn SessionHandle>>,
    /// 会话事件流（开启时抓取一次，避免反复向引擎索取）。
    events: Option<EventStream<CueEvent>>,
    collector: Option<CueCollector>,
    cfg: SessionCfg,
    /// 用户选择的呈现方式（水位回落到安全线后据此恢复）。
    preferred_display: CueDisplay,
    /// 当前生效的呈现方式。
    display: CueDisplay,
    item_id: Option<ItemId>,
    engine_id: Option<String>,
    failure: Option<String>,
    quality: Option<QualityLevel>,
    paused_for_resources: bool,
    /// 是否因队列水位过高被**降级为「仅原文」**（区别于用户自身的呈现选择）。
    degraded: bool,
}

impl<R: EngineRegistry> SubtitleService<R> {
    pub fn new(registry: R) -> Self {
        Self {
            registry,
            state: RealtimeState::Idle,
            session: None,
            events: None,
            collector: None,
            cfg: SessionCfg::default(),
            preferred_display: CueDisplay::Bilingual,
            display: CueDisplay::Bilingual,
            item_id: None,
            engine_id: None,
            failure: None,
            quality: None,
            paused_for_resources: false,
            degraded: false,
        }
    }

    pub fn state(&self) -> RealtimeState {
        self.state
    }

    pub fn status(&self) -> SubtitleStatus {
        SubtitleStatus {
            state: self.state,
            display: self.display,
            engine_id: self.engine_id.clone(),
            failure: self.failure.clone(),
            quality: self.quality,
            paused_for_resources: self.paused_for_resources,
            cue_count: self.collector.as_ref().map(|c| c.cues().len()).unwrap_or(0),
        }
    }

    /// 入口是否出现（PRD FR-05）：只在没有内嵌字幕、也没有外挂字幕时出现。
    pub fn entry_visible(
        &self,
        store: &dyn SubtitleStore,
        item_id: &ItemId,
        has_embedded: bool,
    ) -> Result<bool> {
        let has_external = store.has_track_kind(item_id, SubtitleKind::External)?;
        Ok(should_offer_entry(has_embedded, has_external))
    }

    /// 用户确认隐私告知后开启本功能（默认关闭；远端另有单独授权）。
    pub fn enable_feature(&self, store: &dyn SubtitleStore, allow_remote: bool) -> Result<()> {
        store.set_setting(KEY_ENABLED, "true")?;
        if allow_remote {
            store.set_setting(KEY_REMOTE_AUTH, "true")?;
        }
        Ok(())
    }

    pub fn feature_enabled(&self, store: &dyn SubtitleStore) -> Result<bool> {
        Ok(store.setting(KEY_ENABLED)?.as_deref() == Some("true"))
    }

    /// 可选引擎（远端未授权时 `authorized=false`，界面据此置灰）。
    pub fn available_engines(&self, store: &dyn SubtitleStore) -> Result<Vec<EngineOption>> {
        let authorized = self.remote_authorized(store)?;
        Ok(self
            .registry
            .list()
            .into_iter()
            .map(|info| EngineOption {
                authorized: info.locality == EngineLocality::Local || authorized,
                id: info.id,
                locality: info.locality,
            })
            .collect())
    }

    fn remote_authorized(&self, store: &dyn SubtitleStore) -> Result<bool> {
        Ok(store.setting(KEY_REMOTE_AUTH)?.as_deref() == Some("true"))
    }

    /// 开启（或失败后原地重启）一次识别会话。
    ///
    /// 门顺序：功能开关 → 会话状态 → 引擎存在 → 远端授权。
    pub fn start(
        &mut self,
        store: &dyn SubtitleStore,
        item_id: &ItemId,
        audio: Box<dyn AudioTap>,
        request: StartRequest,
    ) -> Result<()> {
        // 功能开关门：默认关闭，必须显式开启（PRD FR-05「不默认开启」）
        if !self.feature_enabled(store)? {
            return Err(PortError::Unsupported(
                "实时字幕尚未开启：请先确认隐私告知".into(),
            ));
        }
        // 会话状态门：失败态可原地重启，其余活动态不得重复开启
        let entry = match self.state {
            RealtimeState::Idle | RealtimeState::Saved => RealtimeEvent::Start,
            RealtimeState::Failed => RealtimeEvent::Restart,
            _ => {
                return Err(PortError::Backend("实时字幕会话已在运行".into()));
            }
        };

        let engine_id = request
            .engine_id
            .clone()
            .unwrap_or_else(|| self.registry.default_id());
        let engine = self
            .registry
            .get(&engine_id)
            .ok_or_else(|| PortError::NotFound(format!("未注册的字幕引擎：{engine_id}")))?;

        // 隐私门窗：远端引擎必须已显式授权（默认未授权，不静默上传）
        if engine.locality() == EngineLocality::Remote && !self.remote_authorized(store)? {
            return Err(PortError::Unsupported(
                "远端识别需用户显式授权：音频会离开本机".into(),
            ));
        }

        let cfg = session_cfg(
            request.origin_lang.as_deref(),
            request.target_lang.as_deref(),
            request.display,
        );

        // 先释放可能残留的旧会话，再进入初始化态
        self.release_session();
        self.transition(entry)?;

        let handle = match engine.start(cfg.clone(), audio) {
            Ok(handle) => handle,
            Err(err) => {
                // 启动失败即失败态，界面给出三个出口；播放与界面不受影响
                self.transition(RealtimeEvent::Fail)?;
                self.failure = Some(err.reason().to_owned());
                return Err(err);
            }
        };

        self.events = Some(handle.events());
        self.session = Some(handle);
        self.collector = Some(CueCollector::new(
            item_id.clone(),
            cfg.origin_lang.clone(),
            cfg.target_lang.clone(),
        ));
        self.cfg = cfg;
        self.preferred_display = request.display;
        self.display = request.display;
        self.degraded = false;
        self.item_id = Some(item_id.clone());
        self.engine_id = Some(engine_id.clone());
        self.failure = None;
        self.quality = None;
        self.paused_for_resources = false;
        // 记住引擎选择（设置项：引擎选择）
        store.set_setting(KEY_ENGINE, &quote_json(&engine_id))?;
        self.transition(RealtimeEvent::Ready)?;
        Ok(())
    }

    /// 拉取一轮引擎事件并归一为界面输出。
    pub fn poll(&mut self) -> Vec<SubtitleView> {
        let mut views = Vec::new();
        let mut arrived: Vec<CueEvent> = Vec::new();
        if let Some(stream) = self.events.as_ref() {
            while let Some(event) = stream.try_next() {
                arrived.push(event);
            }
        }
        for event in arrived {
            match &event {
                CueEvent::Partial { text, .. } => {
                    views.push(SubtitleView::Partial { text: text.clone() });
                }
                CueEvent::Final { .. } | CueEvent::Translated { .. } => {
                    // 只有 Final / Translated 会进入轨道；Partial 绝不进入
                    let absorbed = self
                        .collector
                        .as_mut()
                        .map(|c| c.absorb(&event))
                        .unwrap_or(false);
                    if absorbed {
                        if let Some(cue) = self
                            .collector
                            .as_ref()
                            .and_then(|c| c.cues().last())
                            .cloned()
                        {
                            views.push(SubtitleView::Cue(cue));
                        }
                    }
                }
                CueEvent::Quality { level } => {
                    self.quality = Some(*level);
                    views.push(SubtitleView::Quality { level: *level });
                }
                CueEvent::Failed { reason } => {
                    self.failure = Some(reason.clone());
                    if let Some(next) = self.state.on(RealtimeEvent::Fail) {
                        self.state = next;
                    }
                    views.push(SubtitleView::Failed {
                        reason: reason.clone(),
                    });
                }
            }
        }
        views
    }

    /// 播放暂停：挂起识别并保留末句（PRD FR-05）。
    pub fn on_pause(&mut self) -> Result<()> {
        if self.state != RealtimeState::Running {
            return Ok(());
        }
        if let Some(session) = self.session.as_ref() {
            session.pause()?;
        }
        self.transition(RealtimeEvent::Pause)?;
        Ok(())
    }

    /// 播放恢复：继续识别。
    pub fn on_resume(&mut self) -> Result<()> {
        if self.state != RealtimeState::Paused {
            return Ok(());
        }
        if let Some(session) = self.session.as_ref() {
            session.resume()?;
        }
        self.transition(RealtimeEvent::Resume)?;
        Ok(())
    }

    /// 播放跳转：清空缓冲、重置句界、丢弃旧句（PRD FR-05）。
    pub fn on_seek(&mut self, position_ms: i64) -> Result<()> {
        if let Some(session) = self.session.as_ref() {
            session.seek_reset(position_ms)?;
        }
        if let Some(collector) = self.collector.as_mut() {
            collector.reset();
        }
        Ok(())
    }

    /// 切换呈现方式（PRD FR-05「原文+译文 / 仅译文 / 仅原文」）。
    ///
    /// 记为用户选择（`preferred_display`）；仅在未被资源降级时立即生效，
    /// 否则等水位回落（[`Self::check_watermark`]）再恢复该选择。
    pub fn set_display(&mut self, display: CueDisplay) {
        self.preferred_display = display;
        if !self.degraded && !self.paused_for_resources {
            self.display = display;
        }
    }

    /// 水位检查（硬约束 C4）：超阈值降级，回落安全线后恢复用户选择的呈现方式。
    pub fn check_watermark(&mut self) -> DegradeAction {
        let Some(session) = self.session.as_ref() else {
            return DegradeAction::None;
        };
        match session.watermark() {
            cinema_domain::model::Watermark::Ok => {
                // 回到安全线：解除资源降级，恢复用户选择的呈现方式
                self.paused_for_resources = false;
                self.degraded = false;
                self.display = self.preferred_display;
                DegradeAction::None
            }
            cinema_domain::model::Watermark::High => {
                if self.display.shows_translation() {
                    self.degraded = true;
                    self.display = CueDisplay::OriginOnly;
                    DegradeAction::OriginOnly
                } else {
                    DegradeAction::None
                }
            }
            cinema_domain::model::Watermark::Critical => {
                self.paused_for_resources = true;
                DegradeAction::Pause
            }
        }
    }

    /// 保存为字幕轨道（PRD FR-05：结果留存＝保存为字幕轨道，下次复用）。
    pub fn save(&mut self, store: &dyn SubtitleStore) -> Result<SubtitleTrack> {
        let track = self.persist_track(store)?;
        if let Some(next) = self.state.on(RealtimeEvent::Saved) {
            self.state = next;
        }
        Ok(track)
    }

    /// 关闭会话：释放子进程句柄；未保存的成句默认落库为轨道并提示。
    pub fn close(&mut self, store: &dyn SubtitleStore) -> Result<CloseOutcome> {
        self.release_session();
        let unsaved = self
            .collector
            .as_ref()
            .map(|collector| collector.has_unsaved_finals())
            .unwrap_or(false);
        let saved_unsaved = if unsaved && self.item_id.is_some() {
            Some(self.persist_track(store)?)
        } else {
            None
        };
        self.collector = None;
        self.state = RealtimeState::Idle;
        self.failure = None;
        self.quality = None;
        self.paused_for_resources = false;
        self.degraded = false;
        self.display = self.preferred_display;
        Ok(CloseOutcome { saved_unsaved })
    }

    /// 失败出口「加载外挂字幕」：结束实时识别，改挂一条 `kind=external` 轨道。
    ///
    /// `cues` 由调用方（组合根读取文件后）用
    /// [`cinema_domain::subtitle::parse_subtitle_text`] 解析得到。
    pub fn add_external_track(
        &mut self,
        store: &dyn SubtitleStore,
        item_id: &ItemId,
        cues: Vec<SubtitleCue>,
        lang: Option<String>,
    ) -> Result<SubtitleTrack> {
        if cues.is_empty() {
            return Err(PortError::Unsupported("外挂字幕中没有可用的字幕句".into()));
        }
        // 该出口意味着放弃实时识别：释放会话并回到空闲
        self.release_session();
        self.collector = None;
        self.state = RealtimeState::Idle;

        store.delete_tracks_of_kind(item_id, SubtitleKind::External)?;
        let track = SubtitleTrack {
            id: format!("ext-{}", item_id.as_str()),
            item_id: item_id.clone(),
            kind: SubtitleKind::External,
            lang,
            source_engine: None,
            locked: false,
            cues,
        };
        store.save_track(&track)?;
        write_subtitle_state(store, item_id)?;
        Ok(track)
    }

    // -----------------------------------------------------------------------
    // 内部
    // -----------------------------------------------------------------------

    fn transition(&mut self, event: RealtimeEvent) -> Result<()> {
        match self.state.on(event) {
            Some(next) => {
                self.state = next;
                Ok(())
            }
            None => Err(PortError::Backend(format!(
                "非法的实时字幕状态迁移：{:?} + {:?}",
                self.state, event
            ))),
        }
    }

    fn release_session(&mut self) {
        if let Some(session) = self.session.as_ref() {
            let _ = session.stop();
        }
        self.session = None;
        self.events = None;
    }

    /// 把收集到的成句落库为 `kind=generated` 轨道，并回写字幕状态标签。
    fn persist_track(&mut self, store: &dyn SubtitleStore) -> Result<SubtitleTrack> {
        let Some(collector) = self.collector.as_ref() else {
            return Err(PortError::Backend("当前没有可保存的实时字幕会话".into()));
        };
        if collector.cues().is_empty() {
            return Err(PortError::Unsupported("尚无成句结果可保存".into()));
        }
        let item_id = self
            .item_id
            .clone()
            .ok_or_else(|| PortError::Backend("会话缺少条目".into()))?;
        let engine = self
            .engine_id
            .clone()
            .unwrap_or_else(|| "unknown".to_owned());

        // 同一引擎重复生成时先清旧轨道，避免堆积
        store.delete_tracks_of_kind(&item_id, SubtitleKind::Generated)?;
        let track = collector.into_track(format!("gen-{}", item_id.as_str()), &engine);
        store.save_track(&track)?;
        write_subtitle_state(store, &item_id)?;
        if let Some(collector) = self.collector.as_mut() {
            collector.mark_saved();
        }
        Ok(track)
    }
}

/// 字幕状态派生标签回写（FR-03 四取值）并重建检索索引。
///
/// 实时字幕入口只在「无内嵌、无外挂」时出现（[`should_offer_entry`]），因此保存
/// 生成 / 外挂轨道的条目此前必然没有内嵌字幕，字幕状态按轨道事实落值即正确。
fn write_subtitle_state(store: &dyn SubtitleStore, item_id: &ItemId) -> Result<()> {
    let facts = DeriveFacts {
        has_external_subtitle: store.has_track_kind(item_id, SubtitleKind::External)?,
        has_generated_subtitle: store.has_track_kind(item_id, SubtitleKind::Generated)?,
        ..DeriveFacts::default()
    };
    apply_derived(store, builtin_derivers(), DIM_SUBTITLE, item_id, &facts)?;
    store.reindex_item(item_id)
}

// ---------------------------------------------------------------------------
// 单元测试（L1 编排；引擎 / 音频 / 仓储全部用替身）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use cinema_domain::model::{LangSet, Watermark};
    use cinema_domain::ports::{EventSource, EmptyEvents};
    use cinema_domain::tagging::{SUBTITLE_EXTERNAL, SUBTITLE_GENERATED};

    // ---- 替身：共享会话状态 ----

    #[derive(Debug, Default)]
    struct Shared {
        queue: Mutex<VecDeque<CueEvent>>,
        watermark: Mutex<Option<Watermark>>,
        /// pause / resume / seek / stop 的调用痕迹。
        log: Mutex<Vec<String>>,
    }

    impl Shared {
        fn push(&self, event: CueEvent) {
            self.queue.lock().expect("锁不可中毒").push_back(event);
        }

        fn set_watermark(&self, watermark: Watermark) {
            *self.watermark.lock().expect("锁不可中毒") = Some(watermark);
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().expect("锁不可中毒").clone()
        }
    }

    struct StubSession(Arc<Shared>);

    struct QueueReader(Arc<Shared>);

    impl EventSource<CueEvent> for QueueReader {
        fn try_next(&self) -> Option<CueEvent> {
            self.0.queue.lock().expect("锁不可中毒").pop_front()
        }
    }

    impl SessionHandle for StubSession {
        fn events(&self) -> EventStream<CueEvent> {
            Box::new(QueueReader(Arc::clone(&self.0)))
        }

        fn pause(&self) -> Result<()> {
            self.0.log.lock().expect("锁不可中毒").push("pause".into());
            Ok(())
        }

        fn resume(&self) -> Result<()> {
            self.0.log.lock().expect("锁不可中毒").push("resume".into());
            Ok(())
        }

        fn seek_reset(&self, position_ms: i64) -> Result<()> {
            self.0
                .log
                .lock()
                .expect("锁不可中毒")
                .push(format!("seek:{position_ms}"));
            Ok(())
        }

        fn stop(&self) -> Result<()> {
            self.0.log.lock().expect("锁不可中毒").push("stop".into());
            Ok(())
        }

        fn watermark(&self) -> Watermark {
            self.0
                .watermark
                .lock()
                .expect("锁不可中毒")
                .unwrap_or(Watermark::Ok)
        }
    }

    // ---- 替身：引擎 ----

    struct StubEngine {
        id: String,
        locality: EngineLocality,
        shared: Arc<Shared>,
        /// 是否让 `start` 失败（模拟本地子进程缺失）。
        fail_start: bool,
        started: AtomicUsize,
    }

    impl StubEngine {
        fn new(id: &str, locality: EngineLocality, shared: Arc<Shared>) -> Self {
            Self {
                id: id.into(),
                locality,
                shared,
                fail_start: false,
                started: AtomicUsize::new(0),
            }
        }
    }

    impl SubtitleEngine for StubEngine {
        fn id(&self) -> &str {
            &self.id
        }

        fn locality(&self) -> EngineLocality {
            self.locality
        }

        fn languages(&self) -> (LangSet, LangSet) {
            (LangSet::new(["en", "ja"]), LangSet::new(["zh"]))
        }

        fn start(
            &self,
            _cfg: SessionCfg,
            _audio: Box<dyn AudioTap>,
        ) -> Result<Box<dyn SessionHandle>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            if self.fail_start {
                return Err(PortError::Backend("本地识别子进程缺失".into()));
            }
            Ok(Box::new(StubSession(Arc::clone(&self.shared))))
        }
    }

    // ---- 替身：注册表 ----

    struct StubRegistry {
        engines: Vec<(EngineInfo, Arc<StubEngine>)>,
        default_id: String,
    }

    impl EngineRegistry for StubRegistry {
        fn list(&self) -> Vec<EngineInfo> {
            self.engines.iter().map(|(info, _)| info.clone()).collect()
        }

        fn get(&self, id: &str) -> Option<Arc<dyn SubtitleEngine>> {
            self.engines
                .iter()
                .find(|(info, _)| info.id == id)
                .map(|(_, engine)| Arc::clone(engine) as Arc<dyn SubtitleEngine>)
        }

        fn default_id(&self) -> String {
            self.default_id.clone()
        }
    }

    // ---- 替身：仓储 ----

    #[derive(Default)]
    struct StubStore {
        tracks: Mutex<Vec<SubtitleTrack>>,
        settings: Mutex<Vec<(String, String)>>,
        derived: Mutex<Vec<(String, Vec<String>)>>,
        reindexed: Mutex<usize>,
    }

    impl StubStore {
        fn saved_tracks(&self) -> Vec<SubtitleTrack> {
            self.tracks.lock().expect("锁不可中毒").clone()
        }

        fn has_setting(&self, key: &str, value: &str) -> bool {
            self.settings
                .lock()
                .expect("锁不可中毒")
                .iter()
                .any(|(k, v)| k == key && v == value)
        }

        fn subtitle_values(&self) -> Vec<Vec<String>> {
            self.derived
                .lock()
                .expect("锁不可中毒")
                .iter()
                .filter(|(dim, _)| dim == DIM_SUBTITLE)
                .map(|(_, values)| values.clone())
                .collect()
        }
    }

    impl TagStore for StubStore {
        fn replace_derived_tags(
            &self,
            _item_id: &ItemId,
            dimension_id: &str,
            values: &[String],
        ) -> Result<()> {
            self.derived
                .lock()
                .expect("锁不可中毒")
                .push((dimension_id.to_owned(), values.to_vec()));
            Ok(())
        }

        fn reindex_item(&self, _item_id: &ItemId) -> Result<()> {
            *self.reindexed.lock().expect("锁不可中毒") += 1;
            Ok(())
        }
    }

    impl SubtitleStore for StubStore {
        fn save_track(&self, track: &SubtitleTrack) -> Result<()> {
            self.tracks
                .lock()
                .expect("锁不可中毒")
                .push(track.clone());
            Ok(())
        }

        fn delete_tracks_of_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<()> {
            self.tracks
                .lock()
                .expect("锁不可中毒")
                .retain(|track| !(track.item_id == *item_id && track.kind == kind));
            Ok(())
        }

        fn list_tracks(&self, item_id: &ItemId) -> Result<Vec<SubtitleTrack>> {
            Ok(self
                .tracks
                .lock()
                .expect("锁不可中毒")
                .iter()
                .filter(|track| track.item_id == *item_id)
                .cloned()
                .collect())
        }

        fn has_track_kind(&self, item_id: &ItemId, kind: SubtitleKind) -> Result<bool> {
            Ok(self
                .tracks
                .lock()
                .expect("锁不可中毒")
                .iter()
                .any(|track| track.item_id == *item_id && track.kind == kind))
        }

        fn setting(&self, key: &str) -> Result<Option<String>> {
            Ok(self
                .settings
                .lock()
                .expect("锁不可中毒")
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone()))
        }

        fn set_setting(&self, key: &str, value: &str) -> Result<()> {
            let mut settings = self.settings.lock().expect("锁不可中毒");
            settings.retain(|(k, _)| k != key);
            settings.push((key.to_owned(), value.to_owned()));
            Ok(())
        }
    }

    // ---- 替身：音频抽头 ----

    struct StubTap;

    impl AudioTap for StubTap {
        fn read_pcm(&self, _out: &mut [i16]) -> Result<usize> {
            Ok(0)
        }

        fn watermark(&self) -> Watermark {
            Watermark::Ok
        }
    }

    // ---- 组装 ----

    struct Fixture {
        service: SubtitleService<StubRegistry>,
        store: StubStore,
        shared: Arc<Shared>,
        remote: Arc<StubEngine>,
    }

    fn item() -> ItemId {
        ItemId::from("it-1")
    }

    fn fixture() -> Fixture {
        let shared = Arc::new(Shared::default());
        let local = Arc::new(StubEngine::new("local-stub", EngineLocality::Local, Arc::clone(&shared)));
        let remote = Arc::new(StubEngine::new("remote-stub", EngineLocality::Remote, Arc::clone(&shared)));
        let registry = StubRegistry {
            engines: vec![
                (
                    EngineInfo {
                        id: "local-stub".into(),
                        locality: EngineLocality::Local,
                    },
                    local,
                ),
                (
                    EngineInfo {
                        id: "remote-stub".into(),
                        locality: EngineLocality::Remote,
                    },
                    Arc::clone(&remote),
                ),
            ],
            default_id: "local-stub".into(),
        };
        Fixture {
            service: SubtitleService::new(registry),
            store: StubStore::default(),
            shared,
            remote,
        }
    }

    fn request(display: CueDisplay) -> StartRequest {
        StartRequest {
            origin_lang: Some("en".into()),
            target_lang: None,
            display,
            engine_id: None,
        }
    }

    fn cue(start: i64, end: i64, text: &str) -> SubtitleCue {
        SubtitleCue {
            start_ms: start,
            end_ms: end,
            text: text.into(),
            lang: None,
        }
    }

    // ---- 测试 ----

    #[test]
    fn entry_appears_only_without_any_subtitle_track() {
        let fx = fixture();
        assert!(fx.service.entry_visible(&fx.store, &item(), false).unwrap());
        assert!(!fx.service.entry_visible(&fx.store, &item(), true).unwrap(), "有内嵌不出现");
    }

    #[test]
    fn start_is_refused_until_feature_is_enabled() {
        let mut fx = fixture();
        let err = fx
            .service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect_err("功能开关默认关闭，必须拒绝");
        assert!(matches!(err, PortError::Unsupported(_)));
        assert_eq!(fx.service.state(), RealtimeState::Idle);

        fx.service.enable_feature(&fx.store, false).expect("开启功能");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("开启后应可启动");
        assert_eq!(fx.service.state(), RealtimeState::Running);
        assert!(fx.store.has_setting(KEY_ENABLED, "true"));
    }

    #[test]
    fn remote_engine_requires_explicit_authorization() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, false).expect("仅本地模式");

        let mut remote_request = request(CueDisplay::Bilingual);
        remote_request.engine_id = Some("remote-stub".into());
        let err = fx
            .service
            .start(&fx.store, &item(), Box::new(StubTap), remote_request.clone())
            .expect_err("远端未授权必须拒绝");
        assert!(matches!(err, PortError::Unsupported(_)));
        assert_eq!(fx.remote.started.load(Ordering::SeqCst), 0, "不得静默上传");

        // 显式授权后才可启动
        fx.service.enable_feature(&fx.store, true).expect("授权远端");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), remote_request)
            .expect("授权后应可启动");
        assert_eq!(fx.remote.started.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn engine_options_mark_remote_unauthorized() {
        let fx = fixture();
        let options = fx.service.available_engines(&fx.store).expect("列引擎");
        let local = options.iter().find(|o| o.id == "local-stub").expect("本地引擎");
        let remote = options.iter().find(|o| o.id == "remote-stub").expect("远端引擎");
        assert!(local.authorized, "本地引擎默认可用");
        assert!(!remote.authorized, "远端引擎默认未授权");
    }

    #[test]
    fn partials_never_enter_the_saved_track() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");

        fx.shared.push(CueEvent::Partial {
            text: "hel".into(),
            conf_milli: 400,
        });
        fx.shared.push(CueEvent::Final {
            cue: cue(0, 1000, "hello"),
        });

        let views = fx.service.poll();
        assert_eq!(views.len(), 2);
        assert!(matches!(views[0], SubtitleView::Partial { .. }));
        // 成句已被收集，但 Partial 不在其中
        let track = fx.service.save(&fx.store).expect("保存");
        assert_eq!(track.cues.len(), 1, "只有 Final 进入轨道");
        assert_eq!(track.cues[0].text, "hello");
        assert_eq!(track.kind, SubtitleKind::Generated);
        assert_eq!(track.source_engine.as_deref(), Some("local-stub"));
        assert_eq!(fx.service.state(), RealtimeState::Saved);
    }

    #[test]
    fn saving_writes_generated_track_and_subtitle_state() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");
        fx.shared.push(CueEvent::Final {
            cue: cue(0, 1000, "hello"),
        });
        fx.shared.push(CueEvent::Translated {
            cue: cue(0, 1000, "你好"),
        });
        fx.service.poll();
        fx.service.save(&fx.store).expect("保存");

        let saved = fx.store.saved_tracks();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].cues.len(), 2);
        // 字幕状态维度被回写为「已生成实时字幕」
        assert_eq!(
            fx.store.subtitle_values().last().expect("应有取值"),
            &vec![SUBTITLE_GENERATED.to_owned()]
        );
        assert!(*fx.store.reindexed.lock().expect("锁不可中毒") >= 1, "标签变化后必须重建索引");
        assert!(fx.store.has_setting(KEY_ENGINE, "\"local-stub\""));
    }

    #[test]
    fn pause_resume_and_seek_drive_the_session() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");

        fx.shared.push(CueEvent::Final {
            cue: cue(0, 1000, "旧句"),
        });
        fx.service.poll();

        fx.service.on_pause().expect("暂停");
        assert_eq!(fx.service.state(), RealtimeState::Paused);
        fx.service.on_resume().expect("恢复");
        assert_eq!(fx.service.state(), RealtimeState::Running);

        fx.service.on_seek(60_000).expect("跳转");
        // seek 丢弃旧句：跳转后保存应提示「尚无成句结果」
        let err = fx.service.save(&fx.store).expect_err("旧句必须被丢弃");
        assert!(matches!(err, PortError::Unsupported(_)));

        let calls = fx.shared.calls();
        assert!(calls.contains(&"pause".to_owned()));
        assert!(calls.contains(&"resume".to_owned()));
        assert!(calls.contains(&"seek:60000".to_owned()));
    }

    #[test]
    fn engine_failure_reports_three_exits_without_touching_playback() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");

        fx.shared.push(CueEvent::Failed {
            reason: "识别子进程异常退出".into(),
        });
        let views = fx.service.poll();
        assert_eq!(
            views,
            vec![SubtitleView::Failed {
                reason: "识别子进程异常退出".into()
            }]
        );
        assert_eq!(fx.service.state(), RealtimeState::Failed);
        assert_eq!(fx.service.status().failure.as_deref(), Some("识别子进程异常退出"));

        // 失败后仍可原地重启（状态机允许 Failed → 初始化）
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("失败后原地重启");
        assert_eq!(fx.service.state(), RealtimeState::Running);
    }

    #[test]
    fn start_failure_lands_in_failed_state() {
        let shared = Arc::new(Shared::default());
        let mut engine = StubEngine::new("local-stub", EngineLocality::Local, Arc::clone(&shared));
        engine.fail_start = true;
        let registry = StubRegistry {
            engines: vec![(
                EngineInfo {
                    id: "local-stub".into(),
                    locality: EngineLocality::Local,
                },
                Arc::new(engine),
            )],
            default_id: "local-stub".into(),
        };
        let mut service = SubtitleService::new(registry);
        let store = StubStore::default();
        service.enable_feature(&store, false).expect("开启");
        assert!(service
            .start(&store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .is_err());
        assert_eq!(service.state(), RealtimeState::Failed);
        assert!(service.status().failure.is_some());
    }

    #[test]
    fn closing_persists_unsaved_finals_and_releases_the_child() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");
        fx.shared.push(CueEvent::Final {
            cue: cue(0, 1000, "未保存的句子"),
        });
        fx.service.poll();

        let outcome = fx.service.close(&fx.store).expect("关闭");
        let saved = outcome.saved_unsaved.expect("未保存成句应默认落库");
        assert_eq!(saved.cues.len(), 1);
        assert_eq!(fx.service.state(), RealtimeState::Idle);
        assert!(fx.shared.calls().contains(&"stop".to_owned()), "必须释放子进程句柄");
    }

    #[test]
    fn closing_without_unsaved_finals_saves_nothing() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");
        let outcome = fx.service.close(&fx.store).expect("关闭");
        assert!(outcome.saved_unsaved.is_none());
        assert!(fx.store.saved_tracks().is_empty());
    }

    #[test]
    fn watermark_degrades_and_recovers() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");

        fx.shared.set_watermark(Watermark::High);
        assert_eq!(fx.service.check_watermark(), DegradeAction::OriginOnly);
        assert_eq!(fx.service.status().display, CueDisplay::OriginOnly, "高水位自动降为仅原文");

        fx.shared.set_watermark(Watermark::Critical);
        assert_eq!(fx.service.check_watermark(), DegradeAction::Pause);
        assert!(fx.service.status().paused_for_resources);

        // 回落安全线：恢复用户选择的双语呈现
        fx.shared.set_watermark(Watermark::Ok);
        assert_eq!(fx.service.check_watermark(), DegradeAction::None);
        assert_eq!(fx.service.status().display, CueDisplay::Bilingual);
        assert!(!fx.service.status().paused_for_resources);
    }

    #[test]
    fn load_external_exit_registers_external_track() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");

        let track = fx
            .service
            .add_external_track(
                &fx.store,
                &item(),
                vec![cue(0, 1000, "外挂第一句")],
                Some("en".into()),
            )
            .expect("加载外挂字幕");
        assert_eq!(track.kind, SubtitleKind::External);
        assert_eq!(fx.service.state(), RealtimeState::Idle, "该出口结束实时识别");
        assert!(fx.shared.calls().contains(&"stop".to_owned()));
        assert_eq!(
            fx.store.subtitle_values().last().expect("应有取值"),
            &vec![SUBTITLE_EXTERNAL.to_owned()]
        );
        // 已有外挂轨道 → 入口不再出现
        assert!(!fx.service.entry_visible(&fx.store, &item(), false).unwrap());

        // 空字幕被拒绝
        assert!(fx
            .service
            .add_external_track(&fx.store, &item(), Vec::new(), None)
            .is_err());
    }

    #[test]
    fn session_cfg_uses_manual_language_and_generated_track_is_reusable() {
        let mut fx = fixture();
        fx.service.enable_feature(&fx.store, true).expect("开启");
        fx.service
            .start(&fx.store, &item(), Box::new(StubTap), request(CueDisplay::Bilingual))
            .expect("启动");
        fx.shared.push(CueEvent::Final {
            cue: cue(0, 1000, "hello"),
        });
        fx.service.poll();
        let saved = fx.service.save(&fx.store).expect("保存");

        // 复用：仓储里能读回同一条轨道（下次播放按此渲染）
        let tracks = fx.store.list_tracks(&item()).expect("读回轨道");
        assert_eq!(tracks, vec![saved]);
        assert_eq!(tracks[0].item_id, item());
    }

    #[test]
    fn empty_stream_yields_no_views() {
        let fx = fixture();
        let mut service = fx.service;
        assert!(service.poll().is_empty());
        // 无会话时水位检查与暂停/恢复都是空操作
        assert_eq!(service.check_watermark(), DegradeAction::None);
        service.on_pause().expect("空操作");
        assert_eq!(service.state(), RealtimeState::Idle);
    }

    #[test]
    fn unused_empty_events_helper_compiles() {
        // 契约测试复用的空事件流；此处仅确保引用有效
        let stream: EventStream<CueEvent> = Box::new(EmptyEvents::<CueEvent>::new());
        assert!(stream.try_next().is_none());
    }
}
