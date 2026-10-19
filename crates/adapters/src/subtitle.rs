//! P-03 实现：被禁用的空引擎、本地 ASR 子进程引擎、远端 HTTP 引擎、内置脚本引擎。
//!
//! - 本地引擎（[`LocalAsrSubtitleEngine`]）：把 PCM 喂给 `cinema-asr` 子进程，
//!   读回记录行。**离线可用**，是默认路径。
//! - 远端引擎（[`RemoteSubtitleEngine`]）：把同样的帧用 chunked 编码发到云端，
//!   是可选实现，接的是**同一个** `SubtitleEngine` 端口。
//! - 内置脚本引擎（[`ScriptedInlineEngine`]）：**完全进程内**合成字幕，零子进程、
//!   零外发；同样接这个端口，用来印证端口的可替换性（M5 增量）。
//! - 三者共用同一套「队列 + 事件流 + 挂起/跳转/停止/水位」骨架（[`SessionCore`]），
//!   差别只在「帧怎么写出去」（本地 stdin / 远端 TCP / 不写）。

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{CueEvent, EngineLocality, LangSet, SessionCfg, SubtitleCue, Watermark};
use cinema_domain::ports::subtitle::{AudioTap, SessionHandle, SubtitleEngine};
use cinema_domain::ports::{EventSource, EventStream};

use crate::subtitle_proto::{audio_frame, control_frame, decode_record, seek_frame};

/// 音频泵缓冲：1600 样本 ≈ 100ms（16k 单声道）。
const PUMP_SAMPLES: usize = 1600;
/// 无数据时的轮询间隔，避免忙等。
const IDLE_SLEEP: Duration = Duration::from_millis(5);
/// `stop()` 后等待子进程自行退出的时限，超时才强杀。
const STOP_GRACE: Duration = Duration::from_millis(200);
/// 内置脚本引擎每行字幕占用的时间窗（毫秒）。
const INLINE_CUE_MS: i64 = 1000;

/// 空实现：引擎被关闭。对应「实时字幕默认关闭，需用户明确开启」。
#[derive(Debug, Clone, Copy, Default)]
pub struct DisabledSubtitleEngine;

impl SubtitleEngine for DisabledSubtitleEngine {
    fn id(&self) -> &str {
        "disabled"
    }

    fn locality(&self) -> EngineLocality {
        // 关闭态不涉及任何外发，声明为 Local 以免误触发隐私授权
        EngineLocality::Local
    }

    fn languages(&self) -> (LangSet, LangSet) {
        (LangSet::default(), LangSet::default())
    }

    fn start(&self, _cfg: SessionCfg, _audio: Box<dyn AudioTap>) -> Result<Box<dyn SessionHandle>> {
        Err(PortError::Unsupported("实时字幕引擎未启用".into()))
    }
}

// ---------------------------------------------------------------------------
// 本地 ASR：子进程配置与引擎
// ---------------------------------------------------------------------------

/// 本地识别子进程的启动配置。
#[derive(Debug, Clone)]
pub struct LocalAsrConfig {
    /// `cinema-asr` 可执行文件路径。
    pub program: PathBuf,
    /// 识别后端（`scripted` / `whisper`）。
    pub backend: String,
    /// 脚本化后端的分句脚本文本。
    pub script: Option<String>,
    /// 真实后端的模型路径 / 名称。
    pub model: Option<String>,
}

impl LocalAsrConfig {
    /// 从环境变量解析配置；找不到程序路径返回 `None`。
    ///
    /// 程序查找顺序：`CINEMA_ASR_CHILD` → 当前 exe 同目录的 `cinema-asr.exe`。
    pub fn from_env() -> Option<Self> {
        let program = match std::env::var_os("CINEMA_ASR_CHILD") {
            Some(raw) if !raw.is_empty() => PathBuf::from(raw),
            _ => {
                let sibling = std::env::current_exe().ok()?.parent()?.join("cinema-asr.exe");
                if !sibling.exists() {
                    return None;
                }
                sibling
            }
        };

        let backend = std::env::var("CINEMA_ASR_BACKEND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "whisper".to_owned());
        let script = std::env::var("CINEMA_ASR_SCRIPT")
            .ok()
            .filter(|v| !v.is_empty());
        let model = std::env::var("CINEMA_ASR_MODEL")
            .ok()
            .filter(|v| !v.is_empty());

        Some(Self {
            program,
            backend,
            script,
            model,
        })
    }
}

/// 本地 ASR 引擎。
///
/// [`LocalAsrSubtitleEngine::default`] 与 [`LocalAsrSubtitleEngine::new`] 不同：前者
/// 一定是「未配置」（`start` 返回 `Unsupported`），后者才去读环境变量，便于测试确定化。
#[derive(Debug, Clone, Default)]
pub struct LocalAsrSubtitleEngine {
    cfg: Option<Arc<LocalAsrConfig>>,
}

impl LocalAsrSubtitleEngine {
    /// 按环境变量构造（等价于 [`LocalAsrConfig::from_env`]）。
    pub fn new() -> Self {
        Self {
            cfg: LocalAsrConfig::from_env().map(Arc::new),
        }
    }

    /// 显式配置构造。
    pub fn with_config(cfg: LocalAsrConfig) -> Self {
        Self {
            cfg: Some(Arc::new(cfg)),
        }
    }

    /// 只指定程序路径，其余取默认（后端 `whisper`）。
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self::with_config(LocalAsrConfig {
            program: program.into(),
            backend: "whisper".to_owned(),
            script: None,
            model: None,
        })
    }
}

impl SubtitleEngine for LocalAsrSubtitleEngine {
    fn id(&self) -> &str {
        "local-asr"
    }

    fn locality(&self) -> EngineLocality {
        EngineLocality::Local
    }

    fn languages(&self) -> (LangSet, LangSet) {
        (
            LangSet::new(["zh", "en", "ja", "ko"]),
            LangSet::new(["zh", "en"]),
        )
    }

    fn start(&self, cfg: SessionCfg, audio: Box<dyn AudioTap>) -> Result<Box<dyn SessionHandle>> {
        match self.cfg.as_ref() {
            Some(asr) => start_local(asr, cfg, audio),
            None => Err(PortError::Unsupported(
                "未配置本地识别子进程（缺少 cinema-asr 程序路径）".into(),
            )),
        }
    }
}

/// 启动本地子进程会话。
fn start_local(
    asr: &LocalAsrConfig,
    session: SessionCfg,
    audio: Box<dyn AudioTap>,
) -> Result<Box<dyn SessionHandle>> {
    let mut command = Command::new(&asr.program);
    command.arg("--backend").arg(&asr.backend);
    if let Some(script) = &asr.script {
        command.arg("--script").arg(script);
    }
    if let Some(model) = &asr.model {
        command.arg("--model").arg(model);
    }
    command
        .arg("--origin")
        .arg(session.origin_lang.as_deref().unwrap_or("auto"));
    command
        .arg("--target")
        .arg(session.target_lang.as_deref().unwrap_or(""));
    command
        .arg("--translate")
        .arg(if session.translate { "yes" } else { "no" });
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .map_err(|e| PortError::Backend(format!("启动本地识别子进程失败：{e}")))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| PortError::Backend("识别子进程 stdin 不可用".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PortError::Backend("识别子进程 stdout 不可用".into()))?;

    let sink: Arc<dyn FrameSink> = Arc::new(LocalSink {
        stdin: Mutex::new(Some(stdin)),
    });
    let core = Arc::new(SessionCore::new(sink));

    spawn_pump(Arc::clone(&core), audio);
    spawn_reader(Arc::clone(&core), BufReader::new(stdout));

    Ok(Box::new(EngineSession {
        core,
        child: Mutex::new(Some(child)),
    }))
}

// ---------------------------------------------------------------------------
// 远端 ASR：HTTP 配置与引擎
// ---------------------------------------------------------------------------

/// 远端识别服务配置。
#[derive(Debug, Clone)]
pub struct RemoteAsrConfig {
    /// 形如 `http://host:port/path` 的服务地址。
    pub url: String,
}

impl RemoteAsrConfig {
    /// 读取 `CINEMA_ASR_REMOTE_URL`。
    pub fn from_env() -> Option<Self> {
        std::env::var("CINEMA_ASR_REMOTE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|url| Self { url })
    }
}

/// 远端（云端）识别引擎：可选实现，接的是同一个 `SubtitleEngine` 端口。
#[derive(Debug, Clone)]
pub struct RemoteSubtitleEngine {
    cfg: RemoteAsrConfig,
}

impl RemoteSubtitleEngine {
    /// 按环境变量构造；未配置返回 `None`。
    pub fn new() -> Option<Self> {
        RemoteAsrConfig::from_env().map(|cfg| Self { cfg })
    }

    /// 显式配置构造。
    pub fn with_config(cfg: RemoteAsrConfig) -> Self {
        Self { cfg }
    }
}

impl SubtitleEngine for RemoteSubtitleEngine {
    fn id(&self) -> &str {
        "remote-asr"
    }

    fn locality(&self) -> EngineLocality {
        EngineLocality::Remote
    }

    fn languages(&self) -> (LangSet, LangSet) {
        (
            LangSet::new(["zh", "en", "ja", "ko"]),
            LangSet::new(["zh", "en"]),
        )
    }

    fn start(&self, cfg: SessionCfg, audio: Box<dyn AudioTap>) -> Result<Box<dyn SessionHandle>> {
        start_remote(&self.cfg, cfg, audio)
    }
}

/// 启动远端会话：极简 HTTP/1.1（chunked 请求体，纯文本行流响应）。
fn start_remote(
    cfg: &RemoteAsrConfig,
    session: SessionCfg,
    audio: Box<dyn AudioTap>,
) -> Result<Box<dyn SessionHandle>> {
    let url = cfg.url.trim();
    if url.is_empty() {
        return Err(PortError::Unsupported("未配置远端识别服务地址".into()));
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(PortError::Unsupported(
            "远端识别仅支持 http:// 明文地址".into(),
        ));
    };
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| PortError::Unsupported(format!("远端地址端口非法：{p}")))?,
        ),
        None => (authority, 80u16),
    };
    if host.is_empty() {
        return Err(PortError::Unsupported("远端地址缺少主机名".into()));
    }

    let mut stream = TcpStream::connect((host, port))
        .map_err(|e| PortError::Backend(format!("连接远端识别服务失败：{e}")))?;

    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-Cinema-Origin-Lang: {origin}\r\nX-Cinema-Target-Lang: {target}\r\nX-Cinema-Translate: {translate}\r\n\r\n",
        origin = session.origin_lang.as_deref().unwrap_or(""),
        target = session.target_lang.as_deref().unwrap_or(""),
        translate = if session.translate { "yes" } else { "no" },
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| PortError::Backend(format!("发送远端请求头失败：{e}")))?;

    let read_stream = stream
        .try_clone()
        .map_err(|e| PortError::Backend(format!("复制远端连接失败：{e}")))?;

    let sink: Arc<dyn FrameSink> = Arc::new(RemoteSink {
        stream: Mutex::new(stream),
    });
    let core = Arc::new(SessionCore::new(sink));

    spawn_pump(Arc::clone(&core), audio);
    spawn_remote_reader(Arc::clone(&core), read_stream);

    Ok(Box::new(EngineSession {
        core,
        child: Mutex::new(None),
    }))
}

// ---------------------------------------------------------------------------
// 内置脚本引擎（M5 增量：只加实现 + 注册，不改动 L2 端口与 L1 流程）
// ---------------------------------------------------------------------------

/// 内置脚本的一行：原文及其译文（`None` 表示该行不产出译文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedLine {
    pub text: String,
    pub translation: Option<String>,
}

impl ScriptedLine {
    pub fn new(text: impl Into<String>, translation: Option<&str>) -> Self {
        Self {
            text: text.into(),
            translation: translation.map(str::to_owned),
        }
    }
}

/// 默认内置脚本。刻意采用**既有引擎语种集之外**的法语，以印证「新引擎可以自带
/// 自己的语种集，注册后即被选用」，而不需要改动任何共享代码。
pub const DEFAULT_INLINE_SCRIPT: &[(&str, &str)] = &[
    ("Bonjour tout le monde", "你好，世界"),
    ("Le film commence maintenant", "电影现在开始"),
    ("Bonne séance", "观影愉快"),
];

/// 内置脚本引擎：**完全进程内**合成字幕，零子进程、零网络外发。
///
/// 它与本地子进程引擎、远端 HTTP 引擎接的是**同一个** [`SubtitleEngine`] 端口，
/// 新增它只需「新增实现 + 注册一行」，领域层无需任何改动（FR-07 的验收手段）。
#[derive(Debug, Clone)]
pub struct ScriptedInlineEngine {
    script: Arc<Vec<ScriptedLine>>,
}

impl Default for ScriptedInlineEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedInlineEngine {
    /// 使用默认脚本构造。
    pub fn new() -> Self {
        Self {
            script: Arc::new(
                DEFAULT_INLINE_SCRIPT
                    .iter()
                    .map(|(text, translation)| ScriptedLine::new(*text, Some(translation)))
                    .collect(),
            ),
        }
    }

    /// 使用自定义脚本构造。
    pub fn with_script(lines: Vec<ScriptedLine>) -> Self {
        Self {
            script: Arc::new(lines),
        }
    }
}

impl SubtitleEngine for ScriptedInlineEngine {
    fn id(&self) -> &str {
        "builtin-scripted"
    }

    fn locality(&self) -> EngineLocality {
        // 全程进程内合成，不存在任何外发
        EngineLocality::Local
    }

    fn languages(&self) -> (LangSet, LangSet) {
        // 语种集由实现自带：默认脚本的法语，另含德语
        (
            LangSet::new(["fr", "de", "zh", "en"]),
            LangSet::new(["zh", "en"]),
        )
    }

    fn start(&self, cfg: SessionCfg, _audio: Box<dyn AudioTap>) -> Result<Box<dyn SessionHandle>> {
        Ok(start_inline(Arc::clone(&self.script), cfg))
    }
}

/// 启动进程内会话：合成线程按脚本产出事件，帧出口为空实现（不写任何地方）。
fn start_inline(script: Arc<Vec<ScriptedLine>>, cfg: SessionCfg) -> Box<dyn SessionHandle> {
    let sink: Arc<dyn FrameSink> = Arc::new(NullSink);
    let core = Arc::new(SessionCore::new(sink));
    let worker = spawn_inline(Arc::clone(&core), script, cfg);
    Box::new(InlineSession {
        core,
        worker: Mutex::new(Some(worker)),
    })
}

/// 合成线程：逐行产出 `Partial` → `Final`；开启翻译且有译文时再补 `Translated`。
///
/// 与子进程读线程的分工相同——只负责把事件推进队列，其余交给 [`SessionCore`]，
/// 因此挂起 / 跳转 / 停止的语义与另两个引擎完全一致。
fn spawn_inline(
    core: Arc<SessionCore>,
    script: Arc<Vec<ScriptedLine>>,
    cfg: SessionCfg,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // 未指定原声语种时落到脚本自带语种（法语）
        let origin = cfg.origin_lang.clone().unwrap_or_else(|| "fr".to_owned());
        // 未指定目标语种时落到默认译文语种
        let target = cfg.target_lang.clone().unwrap_or_else(|| "zh".to_owned());
        let mut cursor_ms = 0i64;
        for line in script.iter() {
            if core.stopped.load(Ordering::SeqCst) {
                return;
            }
            while core.paused.load(Ordering::SeqCst) {
                if core.stopped.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(IDLE_SLEEP);
            }
            let start_ms = cursor_ms;
            let end_ms = cursor_ms + INLINE_CUE_MS;
            cursor_ms = end_ms;
            core.push(CueEvent::Partial {
                text: line.text.clone(),
                conf_milli: 900,
            });
            core.push(CueEvent::Final {
                cue: SubtitleCue {
                    start_ms,
                    end_ms,
                    text: line.text.clone(),
                    lang: Some(origin.clone()),
                },
            });
            if cfg.translate {
                if let Some(translation) = &line.translation {
                    core.push(CueEvent::Translated {
                        cue: SubtitleCue {
                            start_ms,
                            end_ms,
                            text: translation.clone(),
                            lang: Some(target.clone()),
                        },
                    });
                }
            }
        }
    })
}

/// 进程内会话句柄：没有子进程，只有一个合成线程。
struct InlineSession {
    core: Arc<SessionCore>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SessionHandle for InlineSession {
    fn events(&self) -> EventStream<CueEvent> {
        Box::new(QueueEvents {
            core: Arc::clone(&self.core),
        })
    }

    fn pause(&self) -> Result<()> {
        if !self.core.stopped.load(Ordering::SeqCst) {
            self.core.paused.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    fn resume(&self) -> Result<()> {
        if !self.core.stopped.load(Ordering::SeqCst) {
            self.core.paused.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    fn seek_reset(&self, _position_ms: i64) -> Result<()> {
        if !self.core.stopped.load(Ordering::SeqCst) {
            // 跳转丢弃已缓冲的旧句，与子进程引擎的「清空缓冲」语义一致
            self.core.clear_queue();
        }
        Ok(())
    }

    fn stop(&self) -> Result<()> {
        self.core.stopped.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.worker.lock() {
            if let Some(worker) = guard.take() {
                let _ = worker.join();
            }
        }
        Ok(())
    }

    fn watermark(&self) -> Watermark {
        self.core.watermark()
    }
}

// ---------------------------------------------------------------------------
// 会话骨架（三个引擎共用）
// ---------------------------------------------------------------------------

fn io_other(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, msg)
}

/// 帧出口：本地写子进程 stdin，远端写 TCP chunked。
trait FrameSink: Send + Sync {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()>;
    /// 收尾：本地关闭 stdin；远端写终止块并关闭写端。
    fn finish(&self) -> std::io::Result<()>;
}

/// 本地子进程 stdin 出口。
struct LocalSink {
    stdin: Mutex<Option<ChildStdin>>,
}

impl FrameSink for LocalSink {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().map_err(|_| io_other("stdin 锁不可用"))?;
        match guard.as_mut() {
            Some(writer) => {
                writer.write_all(frame)?;
                writer.flush()
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "stdin 已关闭",
            )),
        }
    }

    fn finish(&self) -> std::io::Result<()> {
        let mut guard = self.stdin.lock().map_err(|_| io_other("stdin 锁不可用"))?;
        if let Some(mut writer) = guard.take() {
            let _ = writer.flush();
        }
        Ok(())
    }
}

/// 远端 TCP 出口：把帧字节嵌入 chunked 传输编码。
struct RemoteSink {
    stream: Mutex<TcpStream>,
}

impl FrameSink for RemoteSink {
    fn write_frame(&self, frame: &[u8]) -> std::io::Result<()> {
        let mut guard = self.stream.lock().map_err(|_| io_other("远端连接锁不可用"))?;
        write!(guard, "{:x}\r\n", frame.len())?;
        guard.write_all(frame)?;
        guard.write_all(b"\r\n")?;
        guard.flush()
    }

    fn finish(&self) -> std::io::Result<()> {
        let mut guard = self.stream.lock().map_err(|_| io_other("远端连接锁不可用"))?;
        guard.write_all(b"0\r\n\r\n")?;
        guard.flush()?;
        let _ = guard.shutdown(Shutdown::Write);
        Ok(())
    }
}

/// 进程内引擎的帧出口：不向任何地方写帧（既无子进程，也无网络）。
struct NullSink;

impl FrameSink for NullSink {
    fn write_frame(&self, _frame: &[u8]) -> std::io::Result<()> {
        Ok(())
    }

    fn finish(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 会话通用骨架：帧出口 + 事件队列 + 挂起/停止标志。
struct SessionCore {
    sink: Arc<dyn FrameSink>,
    queue: Mutex<VecDeque<CueEvent>>,
    paused: AtomicBool,
    stopped: AtomicBool,
}

impl SessionCore {
    fn new(sink: Arc<dyn FrameSink>) -> Self {
        Self {
            sink,
            queue: Mutex::new(VecDeque::new()),
            paused: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        }
    }

    fn push(&self, ev: CueEvent) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.push_back(ev);
        }
    }

    /// 清空待处理事件（跳转时丢弃旧句）。
    fn clear_queue(&self) {
        if let Ok(mut queue) = self.queue.lock() {
            queue.clear();
        }
    }

    /// 队列待处理长度分档：≥24→Critical，≥8→High，否则 Ok。
    fn watermark(&self) -> Watermark {
        let pending = self.queue.lock().map(|q| q.len()).unwrap_or(0);
        if pending >= 24 {
            Watermark::Critical
        } else if pending >= 8 {
            Watermark::High
        } else {
            Watermark::Ok
        }
    }
}

/// 从队列拉取事件的事件流。
struct QueueEvents {
    core: Arc<SessionCore>,
}

impl EventSource<CueEvent> for QueueEvents {
    fn try_next(&self) -> Option<CueEvent> {
        self.core.queue.lock().ok()?.pop_front()
    }
}

/// 本地 / 远端共用的会话句柄。`child` 仅本地有值（远端为 `None`）。
struct EngineSession {
    core: Arc<SessionCore>,
    child: Mutex<Option<Child>>,
}

impl EngineSession {
    fn pause_core(&self) -> Result<()> {
        if self.core.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.core.paused.store(true, Ordering::SeqCst);
        self.core
            .sink
            .write_frame(&control_frame(b'P'))
            .map_err(|e| PortError::Backend(format!("下发挂起帧失败：{e}")))
    }

    fn resume_core(&self) -> Result<()> {
        if self.core.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.core.paused.store(false, Ordering::SeqCst);
        self.core
            .sink
            .write_frame(&control_frame(b'R'))
            .map_err(|e| PortError::Backend(format!("下发继续帧失败：{e}")))
    }

    fn seek_core(&self, position_ms: i64) -> Result<()> {
        if self.core.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.core
            .sink
            .write_frame(&seek_frame(position_ms))
            .map_err(|e| PortError::Backend(format!("下发跳转帧失败：{e}")))
    }

    /// 置停止标志 → 发 X 帧 → 断开出口。幂等。
    fn stop_core(&self) -> Result<()> {
        if self.core.stopped.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _ = self.core.sink.write_frame(&control_frame(b'X'));
        let _ = self.core.sink.finish();
        Ok(())
    }

    /// 有限等待子进程退出，超时才强杀。
    fn reap_child(&self) {
        let Ok(mut guard) = self.child.lock() else {
            return;
        };
        let Some(child) = guard.as_mut() else {
            return;
        };
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    return;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        *guard = None;
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => {
                    let _ = child.kill();
                    *guard = None;
                    return;
                }
            }
        }
    }
}

impl SessionHandle for EngineSession {
    fn events(&self) -> EventStream<CueEvent> {
        Box::new(QueueEvents {
            core: Arc::clone(&self.core),
        })
    }

    fn pause(&self) -> Result<()> {
        self.pause_core()
    }

    fn resume(&self) -> Result<()> {
        self.resume_core()
    }

    fn seek_reset(&self, position_ms: i64) -> Result<()> {
        self.seek_core(position_ms)
    }

    fn stop(&self) -> Result<()> {
        self.stop_core()?;
        self.reap_child();
        Ok(())
    }

    fn watermark(&self) -> Watermark {
        self.core.watermark()
    }
}

// ---------------------------------------------------------------------------
// 后台线程
// ---------------------------------------------------------------------------

/// 音频泵：从抽头读 PCM 并写帧；挂起时停发，无数据时短睡避免忙等。
fn spawn_pump(core: Arc<SessionCore>, audio: Box<dyn AudioTap>) {
    std::thread::spawn(move || {
        let mut buf = vec![0i16; PUMP_SAMPLES];
        loop {
            if core.stopped.load(Ordering::SeqCst) {
                break;
            }
            if core.paused.load(Ordering::SeqCst) {
                std::thread::sleep(IDLE_SLEEP);
                continue;
            }
            match audio.read_pcm(&mut buf) {
                Ok(0) => std::thread::sleep(IDLE_SLEEP),
                Ok(n) => {
                    let frame = audio_frame(&buf[..n]);
                    if core.sink.write_frame(&frame).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

/// 本地读线程：逐行解码记录；对端 EOF 且非主动停止时上报 `Failed`。
fn spawn_reader(core: Arc<SessionCore>, reader: BufReader<ChildStdout>) {
    std::thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if let Some(ev) = decode_record(&text) {
                        core.push(ev);
                    }
                }
                Err(_) => break,
            }
        }
        if !core.stopped.load(Ordering::SeqCst) {
            core.push(CueEvent::Failed {
                reason: "识别子进程已退出".into(),
            });
        }
    });
}

/// 远端读线程：先跳过状态行与响应头，再把正文当纯文本行流解码。
fn spawn_remote_reader(core: Arc<SessionCore>, stream: TcpStream) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // `Connection: close` → 无需解析 chunked 响应，剩余正文即行流
        for line in reader.lines() {
            match line {
                Ok(text) => {
                    if let Some(ev) = decode_record(&text) {
                        core.push(ev);
                    }
                }
                Err(_) => break,
            }
        }
        if !core.stopped.load(Ordering::SeqCst) {
            core.push(CueEvent::Failed {
                reason: "远端识别连接已关闭".into(),
            });
        }
    });
}
