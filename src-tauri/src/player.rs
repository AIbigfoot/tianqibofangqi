//! P-04 的装配：把 libmpv 播放核心接到 L1 的 `PlayerFactory` 上。
//!
//! `PlayerFactory::create` 只拿到「实例类型 + 屏位」，不拿到窗口句柄，因此渲染目标
//! （`wid`）由组合根在每次 `create` 之前写入 [`MpvPlayerFactory::set_target`]。
//! 命令层是串行的，这个「先设目标再创建」的约定不会并发冲突。
//!
//! 工厂同时按**屏位**留下正式播放实例句柄，供命令层读取每一路的播放位置以落库进度 ——
//! 这样无需给 P-04 端口增加「位置读数」方法，也支撑「每格进度按条目维度持久化」。
//! 悬停预览实例从不进入该集合（不进进度、不参与音频焦点）。

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cinema_adapters::player::MpvPlayerCore;
use cinema_app::playback_service::{InstanceKind, PlayerFactory};
use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{PlayableRef, PlayerCmd, PlayerEvent, Progress, TapMode};
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::subtitle::AudioTap;
use cinema_domain::ports::EventStream;

/// 把共享的核心句柄适配成端口要求的 `Box<dyn PlayerCore>`，同时让组合根保留一份。
struct SharedCore(Arc<MpvPlayerCore>);

impl PlayerCore for SharedCore {
    fn load(&self, ref_: PlayableRef, start_at: Option<Progress>) -> Result<()> {
        self.0.load(ref_, start_at)
    }

    fn command(&self, cmd: PlayerCmd) {
        self.0.command(cmd);
    }

    fn events(&self) -> EventStream<PlayerEvent> {
        self.0.events()
    }

    fn audio_tap(&self, mode: TapMode) -> Option<Box<dyn AudioTap>> {
        self.0.audio_tap(mode)
    }

    fn set_muted(&self, muted: bool) {
        self.0.set_muted(muted);
    }
}

/// libmpv 播放实例工厂。每格一路实例，屏位为键。
#[derive(Default)]
pub struct MpvPlayerFactory {
    target: Mutex<Option<i64>>,
    screens: Mutex<BTreeMap<u32, Arc<MpvPlayerCore>>>,
    preview: Mutex<Option<Arc<MpvPlayerCore>>>,
}

impl MpvPlayerFactory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定下一次 `create` 的渲染目标（`wid`）。`None` 表示 headless（`vo=null`）。
    pub fn set_target(&self, wid: Option<i64>) {
        if let Ok(mut slot) = self.target.lock() {
            *slot = wid;
        }
    }

    /// 某屏位的正式播放实例。
    pub fn screen(&self, slot: u32) -> Option<Arc<MpvPlayerCore>> {
        self.screens
            .lock()
            .ok()
            .and_then(|map| map.get(&slot).cloned())
    }

    /// 释放某屏位的实例句柄。若本工厂只持有这一份 `Arc`，实例随之销毁、
    /// mpv 上下文与画面窗口一起释放。
    pub fn forget_screen(&self, slot: u32) {
        if let Ok(mut map) = self.screens.lock() {
            map.remove(&slot);
        }
    }

    /// 释放预览实例句柄，语义同 [`MpvPlayerFactory::forget_screen`]。
    pub fn forget_preview(&self) {
        if let Ok(mut slot) = self.preview.lock() {
            *slot = None;
        }
    }
}

impl PlayerFactory for MpvPlayerFactory {
    fn create(&self, kind: InstanceKind) -> Result<Box<dyn PlayerCore>> {
        let target = *self
            .target
            .lock()
            .map_err(|_| PortError::Backend("渲染目标锁不可用".into()))?;
        let core = Arc::new(MpvPlayerCore::new(target)?);
        match kind {
            InstanceKind::Screen { slot } => {
                if let Ok(mut map) = self.screens.lock() {
                    map.insert(slot, Arc::clone(&core));
                }
            }
            InstanceKind::Preview => {
                if let Ok(mut slot) = self.preview.lock() {
                    *slot = Some(Arc::clone(&core));
                }
            }
        }
        Ok(Box::new(SharedCore(core)))
    }
}

/// 让 `PlaybackService` 持有 `Arc<MpvPlayerFactory>` 的同时，命令层仍能读位置。
pub struct FactoryHandle(pub Arc<MpvPlayerFactory>);

impl PlayerFactory for FactoryHandle {
    fn create(&self, kind: InstanceKind) -> Result<Box<dyn PlayerCore>> {
        self.0.create(kind)
    }
}
