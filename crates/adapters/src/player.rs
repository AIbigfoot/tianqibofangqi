//! P-04 实现：无渲染的 `NullPlayerCore`（单测 / 契约 / 无内核时的降级），
//! 以及真实的 libmpv 播放核心 [`MpvPlayerCore`]。

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{PlayableRef, PlayerCmd, PlayerEvent, Progress, TapMode, TrackSelection, Watermark};
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::subtitle::AudioTap;
use cinema_domain::ports::{EmptyEvents, EventSource, EventStream};

use crate::mpv::{apply_common_options, observe_playback_properties, MpvInstance, MpvLib};

/// 空实现：接受一切指令但不产生任何音画与事件。
///
/// 用途：L1/L2 单测、契约测试、以及 libmpv 不可用时的降级实现。
/// 它真实记录了 `set_muted` 状态，便于验收「预览实例常驻静音」。
#[derive(Debug, Default)]
pub struct NullPlayerCore {
    muted: AtomicBool,
}

impl NullPlayerCore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前静音态。契约测试用它验证「预览实例常驻 `set_muted(true)`」。
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }
}

impl PlayerCore for NullPlayerCore {
    fn load(&self, _ref_: PlayableRef, _start_at: Option<Progress>) -> Result<()> {
        Ok(())
    }

    fn command(&self, _cmd: PlayerCmd) {}

    fn events(&self) -> EventStream<PlayerEvent> {
        Box::new(EmptyEvents::new())
    }

    fn audio_tap(&self, _mode: TapMode) -> Option<Box<dyn AudioTap>> {
        None
    }

    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
    }
}

/// libmpv 播放核心：一路实例对应一格画面。
///
/// 关键约定（架构文档 §4 P-04 / §7）：
/// - `render` 为 `None` 时走 headless（`vo=null`），这是单测与无窗口环境的形态；
/// - 预览实例由 L1 创建后立即 `set_muted(true)`，本实现把静音下发到 mpv 的 `mute` 属性；
/// - 「续播」起点通过 `load` 的 `start_at` 传入，落为一次 seek（见 [`PlaybackEvents`]）。
pub struct MpvPlayerCore {
    inst: Arc<MpvInstance>,
    muted: AtomicBool,
    /// 待执行的续播 seek（秒）；由事件流在文件真正开始播放后消费一次。
    pending_seek: Arc<Mutex<Option<f64>>>,
    /// 是否允许抽取音频（架构 §7：预览实例恒为 false）。
    tap_allowed: AtomicBool,
    /// 纯音频抽取实例（惰性创建），与主实例镜像播放状态。
    tap: Mutex<Option<Arc<TapState>>>,
    /// 最近一次 `load` 的位置，供抽取实例事后补加载。
    last_location: Mutex<Option<String>>,
}

impl MpvPlayerCore {
    /// 创建一路播放实例。`render` 为目标窗口句柄（mpv `wid`）。
    pub fn new(render: Option<i64>) -> Result<Self> {
        let lib = MpvLib::shared()?;
        let inst = Arc::new(MpvInstance::new(lib)?);
        apply_common_options(&inst, render)?;
        inst.initialize()?;
        // 运行时再兜底一次：`config=no` 可能让 `input-conf` 不生效，而 mpv 自带的
        // UP/DOWN 是 seek ±60 秒 —— 正是「上下键变成快进快退」的来源。这里把方向键
        // 直接绑成空操作，键盘统一交给前端处理。
        for key in ["LEFT", "RIGHT", "UP", "DOWN"] {
            let _ = inst.command(&["keybind", key, "ignore"]);
        }
        observe_playback_properties(&inst)?;
        Ok(Self {
            inst,
            muted: AtomicBool::new(false),
            pending_seek: Arc::new(Mutex::new(None)),
            tap_allowed: AtomicBool::new(true),
            tap: Mutex::new(None),
            last_location: Mutex::new(None),
        })
    }

    /// 是否允许抽取音频。预览实例由 L1 置为 false，此时 [`PlayerCore::audio_tap`] 恒为 `None`。
    pub fn set_tap_allowed(&self, allowed: bool) {
        self.tap_allowed.store(allowed, Ordering::SeqCst);
        if !allowed {
            // 释放已创建的抽取实例，避免预览实例悄悄占着解码资源
            if let Ok(mut slot) = self.tap.lock() {
                *slot = None;
            }
        }
    }

    /// 当前静音态（验收「预览实例常驻静音」的可观测落点）。
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::SeqCst)
    }

    /// 画面的显示宽高比（宽 / 高）；尚未解码出画面时返回 `None`。
    ///
    /// 宿主窗口恒为矩形，mpv 按视频比例居中留黑边；前端据此把字幕放进**画面内**底部，
    /// 而不是窗口底部（否则字幕会落在下方黑边上）。
    pub fn video_aspect(&self) -> Option<f64> {
        if let Some(aspect) = self
            .inst
            .get_string("video-params/aspect")
            .and_then(|raw| raw.parse::<f64>().ok())
            .filter(|aspect| *aspect > 0.0)
        {
            return Some(aspect);
        }
        let width = self.inst.get_string("width")?.parse::<f64>().ok()?;
        let height = self.inst.get_string("height")?.parse::<f64>().ok()?;
        (width > 0.0 && height > 0.0).then_some(width / height)
    }

    /// 取当前抽取实例（若已创建），用于镜像播放状态。
    fn tap_instance(&self) -> Option<Arc<MpvInstance>> {
        self.tap
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(|state| Arc::clone(&state.inst)))
    }

    /// 取或惰性创建抽取实例；任一步失败返回 `None`（不 panic）。
    fn ensure_tap(&self) -> Option<Arc<TapState>> {
        let mut guard = self.tap.lock().ok()?;
        if let Some(state) = guard.as_ref() {
            return Some(Arc::clone(state));
        }
        let state = Arc::new(self.create_tap_state()?);
        *guard = Some(Arc::clone(&state));
        Some(state)
    }

    /// 创建只解码音频的 headless 实例，输出 16k 单声道 s16 到临时 PCM 文件。
    fn create_tap_state(&self) -> Option<TapState> {
        let lib = MpvLib::shared().ok()?;
        let inst = MpvInstance::new(lib).ok()?;
        let file = unique_tap_path();

        // 只解码音频、极省 CPU；`ao=pcm` 把样本写进裸 PCM 文件（无 WAV 头）
        for (name, value) in [
            ("config", "no"),
            ("idle", "yes"),
            ("vo", "null"),
            ("vid", "no"),
            ("ao", "pcm"),
            ("ao-pcm-waveheader", "no"),
            ("audio-format", "s16"),
            ("audio-samplerate", "16000"),
            ("audio-channels", "mono"),
        ] {
            inst.set_option(name, value).ok()?;
        }
        inst.set_option("ao-pcm-file", file.to_str()?).ok()?;
        inst.initialize().ok()?;

        // 若主实例已经加载过，抽取实例立即跟上（后续由 load/command 镜像）
        if let Some(location) = self.last_location.lock().ok().and_then(|g| g.clone()) {
            let _ = inst.command(&["loadfile", &location, "replace"]);
        }

        Some(TapState {
            inst: Arc::new(inst),
            file,
            cursor: Arc::new(AtomicU64::new(0)),
            pending: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// 当前位置（毫秒）；未播放时为 `None`。供组合根落库进度。
    pub fn position_ms(&self) -> Option<i64> {
        self.inst
            .get_double("time-pos")
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(|s| (s * 1000.0).round() as i64)
    }

    /// 当前媒体总时长（毫秒）。
    pub fn duration_ms(&self) -> Option<i64> {
        self.inst
            .get_double("duration")
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (s * 1000.0).round() as i64)
    }

    /// 当前音量（0–100）；音量属性不可读时返回 `None`。
    pub fn volume(&self) -> Option<f64> {
        self.inst.get_double("volume")
    }

    /// 在画面上短暂显示一行提示。
    ///
    /// 用 mpv 自带的 OSD 而不是页面层：原生画面窗口压在 WebView 之上，
    /// HTML 的任何提示都会被影片画面盖住。
    pub fn show_osd(&self, text: &str, duration_ms: i64) {
        let _ = self
            .inst
            .command(&["show-text", text, &duration_ms.to_string()]);
    }

    /// 是否已播到结尾（mpv `eof-reached`）。
    ///
    /// 实例以 `keep-open=yes` 创建，因此播完后该属性会稳定保持为真，
    /// 组合根据此自动收起播放页并登记「已看」。
    pub fn at_end(&self) -> bool {
        self.inst.get_flag("eof-reached").unwrap_or(false)
    }
}

impl PlayerCore for MpvPlayerCore {
    fn load(&self, ref_: PlayableRef, start_at: Option<Progress>) -> Result<()> {
        if ref_.headers.is_empty() {
            let _ = self.inst.set_string("http-header-fields", "");
        } else {
            // mpv 期望 "K: V,K2: V2" 形式
            let joined = ref_
                .headers
                .iter()
                .map(|(k, v)| format!("{k}: {v}"))
                .collect::<Vec<_>>()
                .join(",");
            self.inst.set_string("http-header-fields", &joined)?;
        }

        // 续播起点先记下，等文件真正开始播放再 seek：
        // 紧随 loadfile 的 seek 会被异步加载吞掉，这是 mpv 的已知时序特性。
        *self
            .pending_seek
            .lock()
            .map_err(|_| PortError::Backend("续播位锁不可用".into()))? =
            start_at.map(|p| p.position_ms as f64 / 1000.0);

        // 记下最近一次位置，供音频抽取实例事后补加载 / 镜像
        if let Ok(mut slot) = self.last_location.lock() {
            *slot = Some(ref_.location.clone());
        }

        // replace：每次打开条目都从头开始，绝不复用上一条的播放列表
        let result = self.inst.command(&["loadfile", &ref_.location, "replace"]);
        // 抽取实例与主实例同步加载（这样 pause/seek 才能对得上）
        if let Some(tap) = self.tap_instance() {
            let _ = tap.command(&["loadfile", &ref_.location, "replace"]);
        }
        result
    }

    fn command(&self, cmd: PlayerCmd) {
        let tap = self.tap_instance();
        match cmd {
            PlayerCmd::Play => {
                let _ = self.inst.command(&["set", "pause", "no"]);
                if let Some(tap) = tap {
                    let _ = tap.command(&["set", "pause", "no"]);
                }
            }
            PlayerCmd::Pause => {
                let _ = self.inst.command(&["set", "pause", "yes"]);
                if let Some(tap) = tap {
                    let _ = tap.command(&["set", "pause", "yes"]);
                }
            }
            PlayerCmd::Seek { position_ms } => {
                let secs = (position_ms as f64 / 1000.0).to_string();
                let _ = self.inst.command(&["seek", &secs, "absolute"]);
                if let Some(tap) = tap {
                    let _ = tap.command(&["seek", &secs, "absolute"]);
                }
            }
            PlayerCmd::SetRate(rate) => {
                let _ = self.inst.command(&["set", "speed", &rate.to_string()]);
                if let Some(tap) = tap {
                    let _ = tap.command(&["set", "speed", &rate.to_string()]);
                }
            }
            PlayerCmd::SetVolume(volume) => {
                let _ = self.inst.command(&["set", "volume", &volume.to_string()]);
            }
            PlayerCmd::SelectTrack(TrackSelection::Audio(index)) => {
                let _ = self.inst.set_string("aid", &index.to_string());
            }
            PlayerCmd::SelectTrack(TrackSelection::Subtitle(index)) => {
                let _ = self.inst.set_string("sid", &index.to_string());
            }
            PlayerCmd::SelectTrack(TrackSelection::SubtitleOff) => {
                let _ = self.inst.set_string("sid", "no");
            }
            // 字幕由 L0 统一渲染（PRD FR-05），不交给 mpv 内嵌字幕渲染器
            PlayerCmd::SetSubtitleStyle(_) => {}
        }
    }

    fn events(&self) -> EventStream<PlayerEvent> {
        Box::new(PlaybackEvents {
            inst: Arc::clone(&self.inst),
            pending_seek: Arc::clone(&self.pending_seek),
        })
    }

    fn audio_tap(&self, mode: TapMode) -> Option<Box<dyn AudioTap>> {
        // 关闭态零开销；预览实例（tap 被禁止）恒为 None
        if mode == TapMode::Off || !self.tap_allowed.load(Ordering::SeqCst) {
            return None;
        }
        // libmpv 不可用时不 panic，返回 None 让上层降级
        let state = self.ensure_tap()?;
        Some(Box::new(MpvPcmTap {
            file: state.file.clone(),
            cursor: Arc::clone(&state.cursor),
            pending: Arc::clone(&state.pending),
            _inst: Arc::clone(&state.inst),
        }))
    }

    fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        let _ = self.inst.set_flag("mute", muted);
        // 预览实例不参与音频焦点，所以只需静音，不需要降低音量曲线
    }
}

/// 把 mpv 事件流接到端口事件流上，并兑现延迟的续播 seek。
struct PlaybackEvents {
    inst: Arc<MpvInstance>,
    pending_seek: Arc<Mutex<Option<f64>>>,
}

impl EventSource<PlayerEvent> for PlaybackEvents {
    fn try_next(&self) -> Option<PlayerEvent> {
        let event = self.inst.poll_event()?;

        // 收到第一条事件说明文件已经开始播放，此时 seek 才会被 mpv 接受
        let pending = self
            .pending_seek
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        if let Some(secs) = pending {
            let secs = secs.to_string();
            let _ = self.inst.command(&["seek", &secs, "absolute"]);
        }

        Some(event)
    }
}

// ---------------------------------------------------------------------------
// 音频抽取（P-04 `audio_tap`）
// ---------------------------------------------------------------------------

/// 抽取实例的临时文件名唯一化计数器。
static TAP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tap_path() -> PathBuf {
    let n = TAP_FILE_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("cinema-tap-{}-{n}.pcm", std::process::id()))
}

/// 抽取实例的共享状态：实例、PCM 文件、读取游标与遗留的奇数尾字节。
struct TapState {
    inst: Arc<MpvInstance>,
    file: PathBuf,
    cursor: Arc<AtomicU64>,
    pending: Arc<Mutex<Vec<u8>>>,
}

/// 从 `ao=pcm` 写出的裸 PCM 文件读取新样本。
///
/// 说明：真实 libmpv 抽取路径依赖 `ao=pcm` / `ao-pcm-file` 选项与实际解码，
/// 在本开发沙箱（无 GUI、无真实媒体文件）**无法实机验证**，需人工确认。
struct MpvPcmTap {
    file: PathBuf,
    /// 已读取的字节偏移。
    cursor: Arc<AtomicU64>,
    /// 上次遗留的奇数尾字节（最多 1 字节），凑够 2 字节才算一个样本。
    pending: Arc<Mutex<Vec<u8>>>,
    /// 保持抽取实例存活（重命名不可读字段以规避 dead_code）。
    _inst: Arc<MpvInstance>,
}

impl AudioTap for MpvPcmTap {
    fn read_pcm(&self, out: &mut [i16]) -> Result<usize> {
        let cursor = self.cursor.load(Ordering::SeqCst);
        let file_len = std::fs::metadata(&self.file).map(|m| m.len()).unwrap_or(0);
        if file_len <= cursor {
            return Ok(0);
        }

        let want = (out.len() * 2) as u64;
        let read_len = (file_len - cursor).min(want) as usize;

        let mut file = File::open(&self.file)
            .map_err(|e| PortError::Backend(format!("打开音频抽取文件失败：{e}")))?;
        file.seek(SeekFrom::Start(cursor))
            .map_err(|e| PortError::Backend(format!("定位音频抽取游标失败：{e}")))?;
        let mut raw = vec![0u8; read_len];
        file.read_exact(&mut raw)
            .map_err(|e| PortError::Backend(format!("读取音频抽取数据失败：{e}")))?;
        self.cursor.store(cursor + read_len as u64, Ordering::SeqCst);

        let mut pending = self
            .pending
            .lock()
            .map_err(|_| PortError::Backend("音频抽取尾字节锁不可用".into()))?;
        let mut bytes = Vec::with_capacity(pending.len() + raw.len());
        bytes.extend_from_slice(&pending);
        bytes.extend_from_slice(&raw);
        pending.clear();

        let samples = bytes.len() / 2;
        for i in 0..samples {
            out[i] = i16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]);
        }
        // 奇数尾字节保留到下次，避免样本错位
        if bytes.len() % 2 == 1 {
            pending.push(bytes[bytes.len() - 1]);
        }
        Ok(samples)
    }

    fn watermark(&self) -> Watermark {
        let cursor = self.cursor.load(Ordering::SeqCst);
        let file_len = std::fs::metadata(&self.file).map(|m| m.len()).unwrap_or(0);
        let unread = file_len.saturating_sub(cursor);
        if unread > 128 * 1024 {
            Watermark::Critical
        } else if unread > 32 * 1024 {
            Watermark::High
        } else {
            Watermark::Ok
        }
    }
}
