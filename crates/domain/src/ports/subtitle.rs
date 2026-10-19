//! P-03 `SubtitleEngine` — 识别与翻译端口。
//!
//! 可替换设计：本地离线处理与云端 API 是同一 trait 的两种实现，切换只改注册表配置，
//! 不改动 L1 的 `SubtitleService`。

use crate::error::Result;
use crate::model::{CueEvent, EngineLocality, LangSet, SessionCfg, Watermark};
use crate::ports::EventStream;

/// 音频入口。播放内核在需要时把 PCM 抽出来，交给引擎。
///
/// 由 [`crate::ports::player::PlayerCore::audio_tap`] 产出；多屏场景下仅主音轨
/// 窗口处于开启态，其余为 `None`（零开销）。
pub trait AudioTap: Send + Sync {
    /// 读取 PCM 帧（16k 单声道 i16）。返回实际写入的样本数；`0` 表示暂无数据。
    fn read_pcm(&self, out: &mut [i16]) -> Result<usize>;

    /// 抽样自检与资源治理用的队列水位。
    fn watermark(&self) -> Watermark;
}

/// 一次识别会话的句柄。
///
/// 约束：必须支持 `pause` / `resume` / `seek_reset` / `stop`，与播放状态联动
/// （PRD FR-05「拖动进度条后丢弃旧句」）。
pub trait SessionHandle: Send + Sync {
    /// 结果事件流：`Partial(原文) → Final(原文) → Translated(译文)`。
    fn events(&self) -> EventStream<CueEvent>;

    fn pause(&self) -> Result<()>;

    fn resume(&self) -> Result<()>;

    /// 播放进度跳转：清空缓冲并重置句界，丢弃旧句。
    fn seek_reset(&self, position_ms: i64) -> Result<()>;

    /// 释放会话与底层子进程。
    fn stop(&self) -> Result<()>;

    /// 引擎侧队列水位，供 L1 触发降级判断。
    fn watermark(&self) -> Watermark;
}

/// 识别与翻译端口。
pub trait SubtitleEngine: Send + Sync {
    fn id(&self) -> &str;

    /// `Local | Remote` —— 供 UI 显式告知隐私（PRD FR-05）。
    /// Remote 时开启前必须显式授权；本地模式必须可用。
    fn locality(&self) -> EngineLocality;

    /// 支持的原声集 / 目标语集。
    fn languages(&self) -> (LangSet, LangSet);

    /// 启动一次识别会话。音频进入本会话，结果以 [`SessionHandle::events`] 流出。
    fn start(&self, cfg: SessionCfg, audio: Box<dyn AudioTap>) -> Result<Box<dyn SessionHandle>>;
}
