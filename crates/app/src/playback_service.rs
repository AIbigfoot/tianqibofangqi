//! 多屏与悬停预览的统一治理点（架构文档 §7）。
//!
//! 本服务是**唯一**允许创建 `PlayerCore` 实例的地方，因此三件事都只能在此收口：
//! 1. 并发解码额度（多屏与预览共用一份）；
//! 2. 音频焦点（L2 [`AudioFocus`] 决策后经 `set_muted` 下发）；
//! 3. 预览实例的隔离（常驻静音、不注册焦点、不写进度、单路并发、最低优先级）。

use std::result::Result as StdResult;

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{PlayableRef, PlayerCmd, Progress};
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::rules::{preview_allowed, AudioFocus, Layout};

use crate::decode_quota::DecodeQuota;

/// 实例类型：正式播放（按屏位）vs 悬停预览。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceKind {
    /// 多屏 / 单路正式播放窗口，每格一个实例；`slot` 为屏位。
    Screen { slot: u32 },
    /// 悬停预览：低优先级、常驻静音、不写进度。
    Preview,
}

/// 过载提示文案（PRD FR-06 异常表：提示性能受限，建议减小布局或关闭实时字幕；
/// **不自动关闭用户已开启的窗口**）。
pub const OVERLOAD_ADVICE: &str = "性能受限：建议减小布局或关闭实时字幕";

/// 预览被拒绝的**可见降级原因**，与 PRD FR-02 / FR-06 的降级态一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRejection {
    /// 设置中已整体关闭「悬停自动预览」：悬停只显示信息层。
    Disabled,
    /// 该条目无可用精华片段（或片段无法载入）：显示「无预览片段」，不重试。
    NoHighlight,
    /// 多屏播放已达同时播放上限：预览被拒绝，不抢占正在播放的窗口。
    QuotaExceeded,
    /// 多屏播放期间不提供悬停预览（PRD FR-06：多屏时浏览海报墙只显示信息层并标注原因）。
    MultiScreen,
}

impl PreviewRejection {
    /// 浮层标注文案。
    pub fn message(self) -> &'static str {
        match self {
            PreviewRejection::Disabled => "已关闭悬停自动预览",
            PreviewRejection::NoHighlight => "无预览片段",
            PreviewRejection::QuotaExceeded => "多屏额度不足",
            PreviewRejection::MultiScreen => "多屏播放期间不提供悬停预览",
        }
    }
}

/// 过载治理结果（PRD FR-06 异常表 / §11「资源优先级」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverloadOutcome {
    /// 已终止一路预览（价值最低的负载，最先被牺牲）。
    PreviewTerminated,
    /// 无预览可终止：只提示「建议减小布局或关闭实时字幕」，不关闭用户已开启的窗口。
    AdviseDegrade,
}

/// 创建播放实例的工厂。
///
/// 这是 L1 内部的注入点，用于把 L2 的 trait 与 L3/L4 的具体内核解耦；
/// 组合根（桌面客户端）负责提供实现。
pub trait PlayerFactory: Send + Sync {
    fn create(&self, kind: InstanceKind) -> Result<Box<dyn PlayerCore>>;
}

/// 一次悬停预览请求。
#[derive(Debug, Clone)]
pub struct PreviewRequest {
    pub ref_: PlayableRef,
    /// 该条目是否具备可用精华片段。
    ///
    /// 「如何判定精华片段」在 PRD 中标为 TBD，因此本服务只接受结论，
    /// 不在治理层编造算法。
    pub has_highlight: bool,
}

/// 多屏 + 预览治理服务。
pub struct PlaybackService<F: PlayerFactory> {
    factory: F,
    quota: DecodeQuota,
    focus: AudioFocus,
    /// 当前布局：决定可用格位数（上限仍由 [`DecodeQuota`] 决定）。
    layout: Layout,
    /// 正式播放窗口，按屏位保存。预览实例从不进入该集合。
    screens: Vec<(u32, Box<dyn PlayerCore>)>,
    preview: Option<Box<dyn PlayerCore>>,
    preview_enabled: bool,
    /// 并发发声开关（默认关闭，硬约束 C6）：开启后不再压制非发声窗口的音轨。
    ///
    /// 关闭时完全保持「单路发声」既有语义（[`AudioFocus`] 决策 + `set_muted` 下发）。
    concurrent_audio: bool,
}

impl<F: PlayerFactory> PlaybackService<F> {
    /// 默认配置：4 路额度、1 × 2 布局、悬停预览开启。
    pub fn new(factory: F) -> Self {
        Self::with_quota(factory, DecodeQuota::with_default_limit())
    }

    pub fn with_quota(factory: F, quota: DecodeQuota) -> Self {
        Self {
            factory,
            quota,
            focus: AudioFocus::new(),
            layout: Layout::default(),
            screens: Vec::new(),
            preview: None,
            // PRD FR-02「触发」：默认开启，设置中可整体关闭
            preview_enabled: true,
            // 并发发声默认关闭：未开启时与既有「单路发声」行为逐位一致
            concurrent_audio: false,
        }
    }

    pub fn quota(&self) -> &DecodeQuota {
        &self.quota
    }

    /// 调整同时播放上限（PRD FR-06「路数上限」：默认 4，用户可在设置中调整）。
    ///
    /// 上限降低不影响已开启的窗口（不擅自关闭用户已开的窗口）。
    pub fn set_limit(&mut self, limit: usize) {
        self.quota.set_limit(limit);
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// 切换布局。格位数不足以容纳当前已开启的窗口时**拒绝**，
    /// 由用户先移除多余窗口 —— 系统不擅自关闭用户已开启的窗口。
    pub fn set_layout(&mut self, layout: Layout) -> Result<()> {
        let open = self.screens.len() as u32;
        if layout.slots() < open {
            return Err(PortError::Unsupported(format!(
                "当前 {open} 路窗口超出「{}」的 {} 个格位，请先移除多余窗口",
                layout.label(),
                layout.slots()
            )));
        }
        self.layout = layout;
        Ok(())
    }

    pub fn preview_enabled(&self) -> bool {
        self.preview_enabled
    }

    /// 关闭悬停预览时，正在播放的预览必须立即释放（悬停只显示信息层）。
    pub fn set_preview_enabled(&mut self, enabled: bool) {
        self.preview_enabled = enabled;
        if !enabled {
            self.stop_preview();
        }
    }

    /// 当前发声窗口（预览不参与该决策）。
    ///
    /// 并发发声开启时该窗口仅作「单条命令的作用对象」与回落目标，不再代表唯一出声者。
    pub fn speaker(&self) -> Option<u32> {
        self.focus.speaker()
    }

    /// 并发发声是否开启（默认关闭）。
    pub fn concurrent_audio(&self) -> bool {
        self.concurrent_audio
    }

    /// 开关并发发声：
    /// - 开启：全部已开窗口立即出声（切换发声窗不再压制其它格）；
    /// - 关闭：回落既有单路发声，只保留当前发声窗口出声，其余静音（仍继续解码）。
    pub fn set_concurrent_audio(&mut self, enabled: bool) {
        self.concurrent_audio = enabled;
        let speaker = self.focus.speaker();
        for (slot, player) in &self.screens {
            player.set_muted(!enabled && speaker != Some(*slot));
        }
    }

    /// 某格是否应静音：并发发声开启时一律出声；否则只有发声窗口出声。
    fn muted_for(&self, slot: u32) -> bool {
        !self.concurrent_audio && self.focus.speaker() != Some(slot)
    }

    pub fn screen_count(&self) -> usize {
        self.screens.len()
    }

    /// 已开启的屏位（升序），供组合根与界面同步格位状态。
    pub fn screens(&self) -> Vec<u32> {
        let mut slots: Vec<u32> = self.screens.iter().map(|(s, _)| *s).collect();
        slots.sort_unstable();
        slots
    }

    /// 是否处于多屏播放（≥ 2 路）。多屏期间不提供悬停预览（PRD FR-06）。
    pub fn multi_screen_active(&self) -> bool {
        self.screens.len() >= 2
    }

    pub fn preview_active(&self) -> bool {
        self.preview.is_some()
    }

    /// 推入一路正式播放。
    ///
    /// 拒绝条件（都提示且**不擅自关闭用户已开的窗口**）：
    /// - 屏位超出当前布局的格位数；
    /// - 该屏位已有窗口；
    /// - 已达同时播放上限。
    pub fn open_screen(
        &mut self,
        slot: u32,
        ref_: PlayableRef,
        start_at: Option<Progress>,
    ) -> Result<()> {
        if slot >= self.layout.slots() {
            return Err(PortError::Unsupported(format!(
                "「{}」只有 {} 个格位，屏位 {slot} 不可用",
                self.layout.label(),
                self.layout.slots()
            )));
        }
        if self.screens.iter().any(|(s, _)| *s == slot) {
            return Err(PortError::Unsupported(format!("屏位 {slot} 已有窗口")));
        }
        if !self.quota.try_acquire() {
            return Err(PortError::QuotaExceeded(format!(
                "已达同时播放上限（当前 {} 路），请关闭一路窗口或在设置中调整上限",
                self.quota.limit()
            )));
        }
        let player = match self.factory.create(InstanceKind::Screen { slot }) {
            Ok(player) => player,
            Err(err) => {
                self.quota.release();
                return Err(err);
            }
        };
        if let Err(err) = player.load(ref_, start_at) {
            self.quota.release();
            return Err(err);
        }
        // 音频焦点：首个进入多屏的窗口默认发声，其余静音（仍继续解码）；
        // 并发发声开启时全部出声（焦点仍记录，供回落与「作用对象」使用）
        self.focus.ensure_speaker(slot);
        player.set_muted(self.muted_for(slot));
        self.screens.push((slot, player));
        Ok(())
    }

    /// 切换发声窗口（PRD FR-06：点击某格右上角切换；**即时生效，无过渡停顿**）。
    pub fn set_speaker(&mut self, slot: u32) -> Result<()> {
        if !self.screens.iter().any(|(s, _)| *s == slot) {
            return Err(PortError::NotFound(format!("屏位 {slot} 没有正在播放的窗口")));
        }
        let old = self.focus.set_speaker(slot);
        // 并发发声时切换发声窗只改「作用对象」，不压制其它格（它们本来就在出声）
        if !self.concurrent_audio {
            if let Some(old) = old {
                if old != slot {
                    if let Some((_, player)) = self.screens.iter().find(|(s, _)| *s == old) {
                        player.set_muted(true);
                    }
                }
            }
        }
        if let Some((_, player)) = self.screens.iter().find(|(s, _)| *s == slot) {
            player.set_muted(false);
        }
        Ok(())
    }

    /// 向某一路下发播放指令（每格独立：操作某格不影响其他格）。
    pub fn screen_command(&self, slot: u32, cmd: PlayerCmd) -> bool {
        match self.screens.iter().find(|(s, _)| *s == slot) {
            Some((_, player)) => {
                player.command(cmd);
                true
            }
            None => false,
        }
    }

    /// 关闭一路正式播放窗口，并按屏位顺序移交音频焦点。
    pub fn close_screen(&mut self, slot: u32) {
        let Some(pos) = self.screens.iter().position(|(s, _)| *s == slot) else {
            return;
        };
        let (_, player) = self.screens.remove(pos);
        drop(player);
        self.quota.release();

        if self.focus.speaker() == Some(slot) {
            let mut remaining: Vec<u32> = self.screens.iter().map(|(s, _)| *s).collect();
            remaining.sort_unstable();
            if let Some(next_slot) = self.focus.on_speaker_closed(&remaining) {
                if let Some((_, next)) = self.screens.iter().find(|(s, _)| *s == next_slot) {
                    next.set_muted(false);
                }
            }
        }
    }

    /// 退出多屏时保留发声窗口（回到单路），关闭其余窗口并把焦点固定到该窗口。
    /// 返回被关闭的屏位（供组合根清理其画面宿主窗口与进度）。
    pub fn keep_only(&mut self, slot: u32) -> Vec<u32> {
        if !self.screens.iter().any(|(s, _)| *s == slot) {
            return Vec::new();
        }
        let others: Vec<u32> = self
            .screens
            .iter()
            .map(|(s, _)| *s)
            .filter(|s| *s != slot)
            .collect();
        for s in others.iter() {
            self.close_screen(*s);
        }
        self.focus.set_speaker(slot);
        if let Some((_, player)) = self.screens.iter().find(|(s, _)| *s == slot) {
            player.set_muted(false);
        }
        others
    }

    /// 退出多屏时全部停止。返回被关闭的屏位。
    pub fn close_all(&mut self) -> Vec<u32> {
        let slots: Vec<u32> = self.screens.iter().map(|(s, _)| *s).collect();
        for s in slots.iter() {
            self.close_screen(*s);
        }
        self.screens.clear();
        self.focus = AudioFocus::new();
        slots
    }

    /// 启动悬停预览。
    ///
    /// 语义对齐 PRD FR-02 / FR-06 / §11「预览实例与进度隔离」：
    /// - 多屏播放期间不提供预览（[`preview_allowed`] 判定）；
    /// - 单路并发：启动前先终止上一路（移到另一张海报先终止再从头播放）；
    /// - 常驻静音：`set_muted(true)`，不注册音频焦点；
    /// - 不写进度：本方法不接收也不回传任何进度，`load` 的起点恒为 `None`；
    /// - 额度共享：与多屏共用同一份额度，不足即拒绝。
    pub fn start_preview(&mut self, req: PreviewRequest) -> StdResult<(), PreviewRejection> {
        if !self.preview_enabled {
            return Err(PreviewRejection::Disabled);
        }
        if !preview_allowed(self.multi_screen_active()) {
            return Err(PreviewRejection::MultiScreen);
        }
        if !req.has_highlight {
            return Err(PreviewRejection::NoHighlight);
        }

        // 单路并发：切换海报即终止上一路并释放其额度
        self.stop_preview();

        if !self.quota.try_acquire() {
            return Err(PreviewRejection::QuotaExceeded);
        }
        let player = match self.factory.create(InstanceKind::Preview) {
            Ok(player) => player,
            Err(_) => {
                self.quota.release();
                return Err(PreviewRejection::QuotaExceeded);
            }
        };

        // 预览强制静音，且不进入音频焦点决策
        player.set_muted(true);

        // 起点恒为 None：同一海报重复悬停仍从头播放，不记忆上次位置
        if player.load(req.ref_, None).is_err() {
            self.quota.release();
            return Err(PreviewRejection::NoHighlight);
        }

        self.preview = Some(player);
        Ok(())
    }

    /// 停止预览并释放额度（移开鼠标 / 片段播完 / 切换海报时调用）。
    pub fn stop_preview(&mut self) {
        if self.preview.take().is_some() {
            self.quota.release();
        }
    }

    /// 过载治理（PRD FR-06 异常表 / §11「资源优先级」）：
    /// 先终止预览，其次才考虑提示降低多屏画质；**不**关闭用户已开启的窗口。
    pub fn overload(&mut self) -> OverloadOutcome {
        let had_preview = self.preview.is_some();
        self.stop_preview();
        if had_preview {
            OverloadOutcome::PreviewTerminated
        } else {
            OverloadOutcome::AdviseDegrade
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use cinema_domain::model::{PlayerCmd, PlayerEvent, TapMode};
    use cinema_domain::ports::subtitle::AudioTap;
    use cinema_domain::ports::EventStream;

    /// 可观测的测试播放实例：记录静音态，便于验证预览隔离。
    #[derive(Debug, Default)]
    struct Recorder {
        muted: AtomicBool,
    }

    /// 把共享的 [`Recorder`] 包装成本地类型，从而可为其实现领域端口。
    struct RecorderPlayer(Arc<Recorder>);

    impl PlayerCore for RecorderPlayer {
        fn load(&self, _ref_: PlayableRef, _start_at: Option<Progress>) -> Result<()> {
            Ok(())
        }
        fn command(&self, _cmd: PlayerCmd) {}
        fn events(&self) -> EventStream<PlayerEvent> {
            Box::new(cinema_domain::ports::EmptyEvents::<PlayerEvent>::new())
        }
        fn audio_tap(&self, _mode: TapMode) -> Option<Box<dyn AudioTap>> {
            None
        }
        fn set_muted(&self, muted: bool) {
            self.0.muted.store(muted, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct TestFactory {
        created: Mutex<Vec<Arc<Recorder>>>,
    }

    impl TestFactory {
        fn last(&self) -> Arc<Recorder> {
            self.created
                .lock()
                .expect("锁不可中毒")
                .last()
                .cloned()
                .expect("应已创建实例")
        }
    }

    impl PlayerFactory for TestFactory {
        fn create(&self, _kind: InstanceKind) -> Result<Box<dyn PlayerCore>> {
            let recorder = Arc::new(Recorder::default());
            self.created
                .lock()
                .expect("锁不可中毒")
                .push(Arc::clone(&recorder));
            Ok(Box::new(RecorderPlayer(recorder)))
        }
    }

    fn service(limit: usize) -> (PlaybackService<TestFactoryHandle>, Arc<TestFactory>) {
        let factory = Arc::new(TestFactory::default());
        let svc = PlaybackService::with_quota(
            TestFactoryHandle(Arc::clone(&factory)),
            DecodeQuota::new(limit),
        );
        (svc, factory)
    }

    /// 让 `PlaybackService` 持有 `Arc<TestFactory>` 的同时，测试仍能检查其状态。
    struct TestFactoryHandle(Arc<TestFactory>);

    impl PlayerFactory for TestFactoryHandle {
        fn create(&self, kind: InstanceKind) -> Result<Box<dyn PlayerCore>> {
            self.0.create(kind)
        }
    }

    fn sample() -> PreviewRequest {
        PreviewRequest {
            ref_: PlayableRef::local("highlight://it-1"),
            has_highlight: true,
        }
    }

    #[test]
    fn preview_is_always_muted_and_uses_shared_quota() {
        let (mut svc, factory) = service(4);
        svc.start_preview(sample()).expect("预览应可启动");

        assert!(svc.preview_active());
        assert!(
            factory.last().muted.load(Ordering::SeqCst),
            "预览实例必须常驻静音"
        );
        assert_eq!(svc.quota().in_use(), 1, "预览必须占用共用额度");
        assert_eq!(svc.speaker(), None, "预览不得注册音频焦点");
    }

    #[test]
    fn preview_is_rejected_when_disabled() {
        let (mut svc, _) = service(4);
        svc.set_preview_enabled(false);
        assert_eq!(svc.start_preview(sample()), Err(PreviewRejection::Disabled));
    }

    #[test]
    fn preview_is_rejected_without_highlight() {
        let (mut svc, _) = service(4);
        let mut req = sample();
        req.has_highlight = false;
        assert_eq!(svc.start_preview(req), Err(PreviewRejection::NoHighlight));
        assert!(!svc.preview_active());
    }

    #[test]
    fn preview_is_rejected_when_screens_exhaust_quota() {
        // 单路播放时预览不被多屏规则拒绝；额度（= 1）已被这一路占满，预览只能因额度不足被拒绝
        let (mut svc, _) = service(1);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();

        assert_eq!(svc.start_preview(sample()), Err(PreviewRejection::QuotaExceeded));
        assert_eq!(svc.screen_count(), 1, "不得为预览抢占已开启的播放窗口");
    }

    #[test]
    fn switching_poster_terminates_previous_preview() {
        let (mut svc, factory) = service(4);
        svc.start_preview(sample()).expect("第一路预览");
        let first = factory.last();

        // 移到另一张海报：先终止上一路再从头播放新的
        svc.start_preview(PreviewRequest {
            ref_: PlayableRef::local("highlight://it-2"),
            has_highlight: true,
        })
        .expect("第二路预览");

        assert_eq!(svc.quota().in_use(), 1, "同一时刻只允许 1 路预览");
        assert!(first.muted.load(Ordering::SeqCst));
    }

    #[test]
    fn closing_speaker_transfers_focus_in_slot_order() {
        let (mut svc, factory) = service(4);
        // 屏位 2 只在四宫格布局下可用（默认 1 × 2 只有 0/1 两个格位）
        svc.set_layout(Layout::TwoByTwo).unwrap();
        svc.open_screen(1, PlayableRef::local("a"), None).unwrap();
        svc.open_screen(2, PlayableRef::local("b"), None).unwrap();
        assert_eq!(svc.speaker(), Some(1), "首个进入的窗口默认发声");

        svc.close_screen(1);
        assert_eq!(svc.speaker(), Some(2), "关闭发声窗口后按屏位顺序移交");
        assert!(
            !factory.last().muted.load(Ordering::SeqCst),
            "接管的窗口应取消静音"
        );
    }

    #[test]
    fn overload_terminates_preview_first() {
        let (mut svc, _) = service(4);
        svc.open_screen(1, PlayableRef::local("a"), None).unwrap();
        svc.start_preview(sample()).unwrap();

        assert_eq!(
            svc.overload(),
            OverloadOutcome::PreviewTerminated,
            "应优先终止预览"
        );
        assert_eq!(svc.screen_count(), 1, "不得关闭用户已开的窗口");
        assert!(!svc.preview_active());
        assert_eq!(svc.quota().in_use(), 1, "预览释放后额度归还");

        // 已无预览可终止：只提示降级，仍不关闭窗口
        assert_eq!(svc.overload(), OverloadOutcome::AdviseDegrade);
        assert_eq!(svc.screen_count(), 1);
    }

    #[test]
    fn layout_caps_available_slots() {
        let (mut svc, _) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        assert!(
            svc.open_screen(2, PlayableRef::local("b"), None).is_err(),
            "1 × 2 布局只有 2 个格位（0/1）"
        );
        svc.set_layout(Layout::TwoByTwo).expect("扩到四宫格");
        svc.open_screen(2, PlayableRef::local("b"), None).unwrap();
        assert_eq!(svc.screen_count(), 2);
    }

    #[test]
    fn layout_shrink_is_refused_when_windows_exceed_slots() {
        let (mut svc, _) = service(4);
        svc.set_layout(Layout::TwoByTwo).unwrap();
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        svc.open_screen(2, PlayableRef::local("c"), None).unwrap();

        assert!(
            svc.set_layout(Layout::OneByTwo).is_err(),
            "格位数不足时必须拒绝，由用户先移除窗口"
        );
        assert_eq!(svc.screen_count(), 3, "拒绝后不得关闭任何窗口");
    }

    #[test]
    fn duplicate_slot_is_refused() {
        let (mut svc, _) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        assert!(svc.open_screen(0, PlayableRef::local("b"), None).is_err());
        assert_eq!(svc.screen_count(), 1);
    }

    #[test]
    fn switching_speaker_is_immediate_and_keeps_others_muted() {
        let (mut svc, factory) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        let first = factory.last();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        let second = factory.last();

        assert_eq!(svc.speaker(), Some(0), "首个进入的窗口默认发声");
        assert!(!first.muted.load(Ordering::SeqCst));
        assert!(second.muted.load(Ordering::SeqCst), "其余路静音播放");

        svc.set_speaker(1).expect("切换发声窗口");
        assert_eq!(svc.speaker(), Some(1));
        assert!(first.muted.load(Ordering::SeqCst), "旧发声者立即静音");
        assert!(!second.muted.load(Ordering::SeqCst), "新发声者立即出声");
    }

    #[test]
    fn concurrent_audio_unmutes_every_screen_and_restores_on_disable() {
        let (mut svc, factory) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        let first = factory.last();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        let second = factory.last();
        assert!(
            second.muted.load(Ordering::SeqCst),
            "默认关闭并发发声：第二路静音（既有语义不变）"
        );

        svc.set_concurrent_audio(true);
        assert!(!first.muted.load(Ordering::SeqCst));
        assert!(
            !second.muted.load(Ordering::SeqCst),
            "并发发声开启后全部已开窗口出声"
        );

        // 并发期间新开的格也出声
        svc.set_layout(Layout::TwoByTwo).unwrap();
        svc.open_screen(2, PlayableRef::local("c"), None).unwrap();
        let third = factory.last();
        assert!(!third.muted.load(Ordering::SeqCst), "并发期间新开窗口直接出声");

        // 关闭并发：回落单路发声，只留焦点窗口
        svc.set_concurrent_audio(false);
        assert_eq!(svc.speaker(), Some(0));
        assert!(!first.muted.load(Ordering::SeqCst), "焦点窗口保持出声");
        assert!(second.muted.load(Ordering::SeqCst), "其余回落为静音");
        assert!(third.muted.load(Ordering::SeqCst));
    }

    #[test]
    fn concurrent_audio_does_not_mute_others_on_speaker_switch() {
        let (mut svc, factory) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        let first = factory.last();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        let second = factory.last();

        svc.set_concurrent_audio(true);
        svc.set_speaker(1).expect("切换发声窗口");
        assert!(
            !first.muted.load(Ordering::SeqCst),
            "并发发声时切换发声窗不得压制其它格"
        );
        assert!(!second.muted.load(Ordering::SeqCst));
        assert_eq!(svc.speaker(), Some(1), "焦点仍被记录（单路回落与作用对象用）");
    }

    #[test]
    fn concurrent_audio_is_off_by_default() {
        let (svc, _) = service(4);
        assert!(!svc.concurrent_audio(), "硬约束 C6：新增能力默认关闭");
    }

    #[test]
    fn keep_only_returns_speaker_window_and_closes_rest() {
        let (mut svc, factory) = service(4);
        svc.set_layout(Layout::TwoByTwo).unwrap();
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        let kept = factory.last();

        let closed = svc.keep_only(1);
        assert_eq!(closed, vec![0], "保留发声窗口，关掉其余");
        assert_eq!(svc.screens(), vec![1]);
        assert_eq!(svc.speaker(), Some(1), "焦点固定在保留的窗口");
        assert!(!kept.muted.load(Ordering::SeqCst), "保留窗口出声");
        assert_eq!(svc.quota().in_use(), 1, "其余路额度已归还");
    }

    #[test]
    fn close_all_releases_every_slot() {
        let (mut svc, _) = service(4);
        svc.set_layout(Layout::TwoByTwo).unwrap();
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        svc.open_screen(2, PlayableRef::local("c"), None).unwrap();

        let closed = svc.close_all();
        assert_eq!(closed, vec![0, 1, 2]);
        assert_eq!(svc.screen_count(), 0);
        assert_eq!(svc.speaker(), None);
        assert_eq!(svc.quota().in_use(), 0, "全部额度归还");
    }

    #[test]
    fn preview_is_withheld_while_multi_screen_is_active() {
        let (mut svc, _) = service(4);
        svc.open_screen(0, PlayableRef::local("a"), None).unwrap();
        // 单路播放时预览不在本规则内拒绝
        assert!(svc.start_preview(sample()).is_ok());
        svc.stop_preview();

        svc.open_screen(1, PlayableRef::local("b"), None).unwrap();
        assert!(svc.multi_screen_active());
        assert_eq!(
            svc.start_preview(sample()),
            Err(PreviewRejection::MultiScreen),
            "多屏播放期间不提供悬停预览"
        );
        assert_eq!(svc.screen_count(), 2, "不得抢占正在播放的窗口");
    }

    #[test]
    fn limit_can_be_adjusted_without_closing_windows() {
        let (mut svc, _) = service(4);
        svc.set_layout(Layout::TwoByTwo).unwrap();
        for slot in 0..3 {
            svc.open_screen(slot, PlayableRef::local(format!("it-{slot}")), None)
                .unwrap();
        }
        svc.set_limit(2);
        assert_eq!(svc.quota().limit(), 2);
        assert_eq!(svc.screen_count(), 3, "上限降低不得关闭已开启的窗口");
        assert!(
            svc.open_screen(3, PlayableRef::local("it-3"), None).is_err(),
            "达到上限时新增请求被拒绝并提示"
        );
    }
}
