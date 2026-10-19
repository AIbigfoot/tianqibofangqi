//! P-04 `PlayerCore` — 播放能力端口。
//!
//! 每个实例对应一路画面：**多屏即多实例**，而不是一个能播多路的特殊播放器。
//! 悬停预览复用同一 trait，但与正式播放实例分两类（见 L1 `PlaybackService`）。

use crate::error::Result;
use crate::model::{PlayableRef, PlayerCmd, PlayerEvent, Progress, TapMode};
use crate::ports::subtitle::AudioTap;
use crate::ports::EventStream;

/// 播放能力端口。
///
/// 约束（架构文档 §4 P-04 / §7）：
/// - **多实例**：每格一个实例，故障与状态互不影响（C3）。
/// - **预览实例**：由 L1 单独创建、常驻 [`PlayerCore::set_muted`]`(true)`、
///   不注册音频焦点、不写进度仓库。
/// - **音频焦点**：「哪一路发声」是 L2 [`crate::rules::AudioFocus`] 的输出，
///   通过 `set_muted` 下发，UI 层不直接操作音频设备。
pub trait PlayerCore: Send + Sync {
    /// 载入媒体。`start_at` 为续播起点；预览实例一律传 `None`（从头播放，不记位置）。
    fn load(&self, ref_: PlayableRef, start_at: Option<Progress>) -> Result<()>;

    /// 下发指令：play / pause / seek / rate / volume / track-select / subtitle-style。
    fn command(&self, cmd: PlayerCmd);

    /// 事件流：position / tracks-changed / eof / decoded-error / buffering。
    fn events(&self) -> EventStream<PlayerEvent>;

    /// 供 `SubtitleEngine` 取 PCM 的音频抽头；关闭时为 `None`（零开销）。
    fn audio_tap(&self, mode: TapMode) -> Option<Box<dyn AudioTap>>;

    /// 音频焦点下发：非发声窗口置静音，但仍继续解码。
    fn set_muted(&self, muted: bool);
}
