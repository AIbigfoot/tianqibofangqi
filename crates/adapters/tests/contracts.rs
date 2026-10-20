//! L3 契约测试（可复用）。
//!
//! 架构文档 §9 要求：「每个端口写一套可复用测试，任何新实现（新源类型 /
//! 新元数据源 / 新引擎）必须通过，保证可替换性不被破坏（C2）」。
//!
//! 因此本文件把断言写成**泛型函数**：新增实现时只需在对应 `#[test]` 里多加一行
//! `contract_xxx(&新实现)`，而不用抄写断言。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cinema_adapters::media_source::{
    normalize_local_root, normalized_lan_root, DirectUrlProvider, EmptyMediaSourceProvider,
    LanPathProvider, LocalFolderProvider,
};
use cinema_adapters::metadata::{EmptyMetadataProvider, FileNameMetadataProvider};
use cinema_adapters::player::{MpvPlayerCore, NullPlayerCore};
use cinema_adapters::subtitle::{
    DisabledSubtitleEngine, LocalAsrConfig, LocalAsrSubtitleEngine, RemoteAsrConfig,
    RemoteSubtitleEngine, ScriptedInlineEngine, ScriptedLine,
};
use cinema_adapters::tag_dimension::ConfigTagDimensionRegistry;

use cinema_domain::error::{PortError, Result as PortResult};
use cinema_domain::model::{
    CueEvent, DiscoveredItem, EngineLocality, MediaItem, MediaSource, ParseHint, PlayableRef,
    PlayerCmd, SessionCfg, SourceId, SourceKind, SourceState, TapMode, Watermark,
};
use cinema_domain::ports::media_source::{DiscoverSink, MediaSourceProvider};
use cinema_domain::ports::metadata::MetadataProvider;
use cinema_domain::ports::player::PlayerCore;
use cinema_domain::ports::subtitle::{AudioTap, SubtitleEngine};
use cinema_domain::ports::tag_dimension::{TagDimensionDef, TagDimensionRegistry};

// ---------------------------------------------------------------------------
// 测试替身
// ---------------------------------------------------------------------------

/// 收集被提交的候选项；可预设取消态。
struct CollectSink {
    items: Mutex<Vec<DiscoveredItem>>,
    cancelled: AtomicBool,
}

impl CollectSink {
    fn new(cancelled: bool) -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(cancelled),
        }
    }

    fn items(&self) -> Vec<DiscoveredItem> {
        self.items.lock().expect("锁不可中毒").clone()
    }
}

impl DiscoverSink for CollectSink {
    fn submit(&self, item: DiscoveredItem) -> PortResult<()> {
        self.items.lock().expect("锁不可中毒").push(item);
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 永远不提供 PCM 的音频抽头，用于驱动引擎会话生命周期。
struct SilentTap;

impl AudioTap for SilentTap {
    fn read_pcm(&self, _out: &mut [i16]) -> PortResult<usize> {
        Ok(0)
    }

    fn watermark(&self) -> Watermark {
        Watermark::Ok
    }
}

/// 会连续产出若干帧非静音 PCM、随后返回 0（无数据）的音频抽头，
/// 用于驱动子进程按帧产出识别结果。
struct ScriptedTap {
    remaining: AtomicUsize,
}

impl ScriptedTap {
    fn new(frames: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(frames),
        }
    }
}

impl AudioTap for ScriptedTap {
    fn read_pcm(&self, out: &mut [i16]) -> PortResult<usize> {
        let left = self.remaining.load(Ordering::SeqCst);
        if left == 0 {
            return Ok(0);
        }
        self.remaining.store(left - 1, Ordering::SeqCst);
        // 填一段有能量波形的样本，避免被能量 VAD 判成静音
        for (i, sample) in out.iter_mut().enumerate() {
            *sample = ((i % 100) as i16 - 50) * 100;
        }
        Ok(out.len())
    }

    fn watermark(&self) -> Watermark {
        Watermark::Ok
    }
}

fn local_source(location: &str) -> MediaSource {
    MediaSource {
        id: SourceId::from("src-local"),
        kind: SourceKind::LocalFolder,
        name: "本地目录".into(),
        location: location.into(),
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    }
}

fn url_source(location: &str) -> MediaSource {
    MediaSource {
        id: SourceId::from("src-url"),
        kind: SourceKind::DirectUrl,
        name: "直链".into(),
        location: location.into(),
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    }
}

/// 局域网路径源样本（M5 新增源类型）。
fn lan_source(location: &str) -> MediaSource {
    MediaSource {
        id: SourceId::from("src-lan"),
        kind: SourceKind::LanPath,
        name: "局域网".into(),
        location: location.into(),
        exclude_rules: None,
        state: SourceState::Idle,
        last_scan_at: None,
    }
}

// ---------------------------------------------------------------------------
// P-01 契约
// ---------------------------------------------------------------------------

/// 真实实现必须满足的 P-01 契约。
fn contract_media_source_provider<P: MediaSourceProvider>(provider: &P, sample: &MediaSource) {
    // kind 必须稳定（同一实例多次调用结果一致）
    assert_eq!(provider.kind(), provider.kind(), "kind() 必须稳定");

    // 可重复调用而不 panic：这是幂等合并的前提
    let sink = CollectSink::new(false);
    let _ = provider.discover(sample, &sink);
    let _ = provider.discover(sample, &sink);

    // 已取消的 sink：必须正常返回（不得 panic），已提交条目保留
    let cancelled = CollectSink::new(true);
    let _ = provider.discover(sample, &cancelled);

    // 缺少任何可播放信息的条目：必须报错，不得 panic
    let no_location = MediaItem::pending_fill("it-none", "src-1", "无路径条目");
    assert!(
        provider.open(&no_location).is_err(),
        "缺少路径/URL 时必须返回错误"
    );

    // 带可播放信息的条目：必须能 open 成功，且 location 非空
    let mut item = MediaItem::pending_fill("it-1", "src-1", "样本");
    item.rel_path = Some("a/b.mp4".into());
    item.url = Some("http://example.test/b.mp4".into());
    let ref_ = provider.open(&item).expect("带路径的条目应可 open");
    assert!(!ref_.location.is_empty(), "PlayableRef.location 不得为空");

    // probe 不得 panic
    let _ = provider.probe(&ref_);
}

/// 「空实现」只需满足：类型可构造、方法可调用、且明确声明不支持。
fn smoke_empty_media_source_provider<P: MediaSourceProvider>(provider: &P, sample: &MediaSource) {
    assert_eq!(provider.kind(), provider.kind());
    let sink = CollectSink::new(false);
    let _ = provider.discover(sample, &sink);
    let item = MediaItem::pending_fill("it-1", "src-1", "样本");
    assert!(provider.open(&item).is_err(), "空实现不提供播放引用");
    assert!(
        provider.probe(&PlayableRef::local("x")).is_err(),
        "空实现不提供探测"
    );
}

#[test]
fn p01_local_folder_meets_contract() {
    // 用系统临时目录（一定存在）保证 discover 与 open 都可正常执行
    let tmp = std::env::temp_dir().to_string_lossy().to_string();
    let provider = LocalFolderProvider::new(Path::new(&tmp));
    contract_media_source_provider(&provider, &local_source(&tmp));
}

#[test]
fn p01_direct_url_meets_contract() {
    contract_media_source_provider(&DirectUrlProvider, &url_source("http://example.test/v.mp4"));
}

#[test]
fn p01_empty_impl_is_well_behaved() {
    smoke_empty_media_source_provider(&EmptyMediaSourceProvider, &local_source(""));
}

#[test]
fn p01_local_folder_reports_missing_directory() {
    let missing = "Z:/__cinema_definitely_missing__/nope";
    let sink = CollectSink::new(false);
    assert!(
        LocalFolderProvider::new(Path::new(missing))
            .discover(&local_source(missing), &sink)
            .is_err(),
        "目录不存在必须显式报错，便于失败隔离到该源"
    );
}

#[test]
fn p01_local_folder_resolves_absolute_path_from_root() {
    let root = std::env::temp_dir();
    let provider = LocalFolderProvider::new(root.as_path());
    let mut item = MediaItem::pending_fill("it-1", "src-local", "样本");
    item.rel_path = Some("sub/clip.mp4".into());

    let ref_ = provider.open(&item).expect("应能把相对路径解析出来");
    assert_eq!(
        Path::new(&ref_.location),
        root.join("sub").join("clip.mp4"),
        "open 必须把相对路径拼成根目录下的绝对路径，否则磁盘上的文件根本打不开"
    );
}

/// 造一棵临时目录树并返回根：`sub/a.mp4`、`b.mkv`、`notes.txt`（后者非视频）。
fn temp_tree(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("cinema-contract-{tag}-{unique}"));
    std::fs::create_dir_all(root.join("sub")).expect("建临时目录");
    for path in ["sub/a.mp4", "b.mkv", "notes.txt"] {
        std::fs::write(root.join(path), b"x").expect("写样本文件");
    }
    root
}

#[test]
fn p01_local_folder_discovers_nested_videos() {
    let root = temp_tree("nested");
    let sink = CollectSink::new(false);
    let stat = LocalFolderProvider::new(root.as_path())
        .discover(&local_source(&root.to_string_lossy()), &sink)
        .expect("扫描应成功");

    assert_eq!(stat.discovered, 2, "必须递归进子目录，只收视频文件");
    assert_eq!(stat.skipped, 0, "正常目录树不该有跳过项");
    let keys: Vec<String> = sink.items().iter().map(|i| i.stable_key.clone()).collect();
    assert!(
        keys.iter().any(|k| k.ends_with("sub/a.mp4")),
        "子目录里的视频必须被发现，且相对路径用 / 分隔：{keys:?}"
    );
}

/// 「不跟随链接」：自引用链接若被跟随会导致无限递归，因此必须跳过。
///
/// 创建符号链接需要权限（管理员或开发者模式），无权限时本用例**跳过**：
/// 环境限制不该被记成实现缺陷。
#[cfg(windows)]
#[test]
fn p01_local_folder_does_not_follow_directory_symlinks() {
    use std::os::windows::fs::symlink_dir;

    let root = temp_tree("symlink");
    let outside = temp_tree("symlink-outside");
    if symlink_dir(&outside, root.join("link")).is_err() {
        return;
    }

    let sink = CollectSink::new(false);
    let stat = LocalFolderProvider::new(root.as_path())
        .discover(&local_source(&root.to_string_lossy()), &sink)
        .expect("扫描应成功");

    assert_eq!(
        stat.discovered, 2,
        "链接指向外部的视频不得被收录，只有根本身的两个视频"
    );
    assert_eq!(stat.skipped, 1, "被避开的链接应计入 skipped，便于界面如实告知");
}

#[test]
fn p01_local_root_normalizes_bare_drive_letter() {
    assert_eq!(
        normalize_local_root("D:"),
        r"D:\",
        "裸盘符是相对路径，必须归一为盘根：否则扫的是进程当前目录"
    );
    assert_eq!(
        normalize_local_root("  d:  "),
        r"d:\",
        "两端空白应被忽略；只归一裸盘符这一种形态"
    );
    assert_eq!(normalize_local_root(r"D:\Movies"), r"D:\Movies", "已含盘根的原样保留");
    assert_eq!(normalize_local_root(""), "", "空串不得被伪造成盘根");
}

#[test]
fn p01_direct_url_stable_key_is_scoped_to_source() {
    let sink_a = CollectSink::new(false);
    let sink_b = CollectSink::new(false);
    let url = "http://example.test/v.mp4";

    DirectUrlProvider
        .discover(&url_source(url), &sink_a)
        .expect("源 A 发现");
    let mut other = url_source(url);
    other.id = SourceId::from("src-url-2");
    DirectUrlProvider
        .discover(&other, &sink_b)
        .expect("源 B 发现");

    assert_ne!(
        sink_a.items()[0].stable_key,
        sink_b.items()[0].stable_key,
        "不同源配置同一 URL 时稳定键必须不同，否则会撞 UNIQUE 约束被误合并"
    );
}

#[test]
fn p01_direct_url_discovers_single_candidate() {
    let sink = CollectSink::new(false);
    let stat = DirectUrlProvider
        .discover(&url_source("http://example.test/v.mp4"), &sink)
        .expect("直链发现应成功");
    assert_eq!(stat.discovered, 1, "P0 只支持单个视频直链");
    assert_eq!(sink.items().len(), 1);
}

// M5 新增源类型：新增实现即通过同一套契约，断言代码零改动
#[test]
fn p01_lan_path_meets_contract() {
    // 用系统临时目录当作「已挂载的网络位置」，保证 discover 与 open 都可执行
    let tmp = std::env::temp_dir().to_string_lossy().to_string();
    contract_media_source_provider(&LanPathProvider::new(&tmp), &lan_source(&tmp));
}

#[test]
fn p01_lan_path_normalizes_network_location() {
    let unc = r"\\NAS\媒体\电影";
    assert_eq!(normalized_lan_root("smb://NAS/媒体/电影"), unc, "smb:// 必须归一为 UNC");
    assert_eq!(normalized_lan_root("//NAS/媒体/电影"), unc, "// 必须归一为 UNC");
    assert_eq!(
        normalized_lan_root(r"\\NAS\媒体"),
        r"\\NAS\媒体",
        "已是 UNC 的写法必须原样保留，不得改写"
    );
    assert_eq!(
        normalized_lan_root(r"Z:\媒体"),
        r"Z:\媒体",
        "本地盘符写法必须原样保留，不得猜测为网络路径"
    );
}

#[test]
fn p01_lan_path_reports_empty_location() {
    let sink = CollectSink::new(false);
    assert!(
        LanPathProvider::new("  ")
            .discover(&lan_source("  "), &sink)
            .is_err(),
        "空路径必须显式报错，便于失败隔离到该源"
    );
}

#[test]
fn p01_lan_path_resolves_absolute_path_from_root() {
    let tmp = std::env::temp_dir();
    let provider = LanPathProvider::new(&tmp.to_string_lossy());
    let mut item = MediaItem::pending_fill("it-1", "src-lan", "样本");
    item.rel_path = Some("电影/a.mp4".into());

    let ref_ = provider.open(&item).expect("应能把相对路径拼到网络根下");
    assert_eq!(
        Path::new(&ref_.location),
        tmp.join("电影").join("a.mp4"),
        "open 必须把相对路径拼成根路径下的可播放路径"
    );
}

// ---------------------------------------------------------------------------
// P-02 契约
// ---------------------------------------------------------------------------

fn contract_metadata_provider<P: MetadataProvider>(provider: &P, hint: &ParseHint) {
    assert!(!provider.name().is_empty(), "提供者名称不得为空");
    assert_eq!(provider.priority(), provider.priority(), "priority() 必须稳定");

    let candidates = provider
        .candidates(hint)
        .expect("candidates 必须可调用且不得失败");
    for candidate in &candidates {
        assert_eq!(
            candidate.provider_ref.provider,
            provider.name(),
            "候选的 provider 字段必须等于提供者名称"
        );
        assert!(!candidate.title.is_empty(), "候选标题不得为空");
    }

    // 对自身给出的候选，fetch 必须成功——否则异步补齐流程永远无法收敛
    if let Some(first) = candidates.first() {
        let meta = provider
            .fetch(&first.provider_ref)
            .expect("对自身候选的 fetch 必须成功");
        assert!(
            meta.title.is_some() || meta.overview.is_some() || meta.poster_url.is_some(),
            "fetch 至少应给出标题 / 简介 / 封面之一"
        );
    }
}

#[test]
fn p02_file_name_provider_meets_contract() {
    let hint = ParseHint {
        raw_name: "Blade.Runner.1982.1080p.mkv".into(),
        year: None,
    };
    contract_metadata_provider(&FileNameMetadataProvider, &hint);
}

#[test]
fn p02_empty_provider_is_well_behaved() {
    let hint = ParseHint::default();
    let provider = EmptyMetadataProvider;
    assert!(!provider.name().is_empty());
    assert!(provider.candidates(&hint).expect("不得失败").is_empty());

    let missing = cinema_domain::model::ProviderRef {
        provider: "empty".into(),
        id: "nope".into(),
    };
    assert!(provider.fetch(&missing).is_err(), "空实现不可 fetch");
}

// ---------------------------------------------------------------------------
// P-03 契约
// ---------------------------------------------------------------------------

fn contract_subtitle_engine<E: SubtitleEngine>(engine: &E) {
    assert!(!engine.id().is_empty(), "引擎 id 不得为空");
    let _ = engine.locality();
    let (origin, target) = engine.languages();
    let _ = (origin, target);

    // start 允许在未配置时拒绝；但一旦成功，会话必须支持完整生命周期
    if let Ok(handle) = engine.start(SessionCfg::default(), Box::new(SilentTap)) {
        let _ = handle.watermark();
        handle.pause().expect("pause 必须支持");
        handle.resume().expect("resume 必须支持");
        handle.seek_reset(1_000).expect("seek_reset 必须支持");
        let stream = handle.events();
        let _ = stream.try_next();
        handle.stop().expect("stop 必须支持");
    }
}

#[test]
fn p03_local_asr_meets_contract() {
    // 默认（未配置子进程）引擎：start 返回 Unsupported，是被契约允许的分支
    let engine = LocalAsrSubtitleEngine::default();
    contract_subtitle_engine(&engine);
}

#[test]
fn p03_disabled_engine_is_well_behaved() {
    let engine = DisabledSubtitleEngine;
    assert!(!engine.id().is_empty());
    assert!(
        engine.start(SessionCfg::default(), Box::new(SilentTap)).is_err(),
        "关闭态引擎必须拒绝启动"
    );
}

// M5 新增引擎：新增实现即通过同一套契约，断言代码零改动
#[test]
fn p03_inline_engine_meets_contract() {
    contract_subtitle_engine(&ScriptedInlineEngine::new());
}

/// 内置引擎必须**完全进程内**产出原文与译文：不需要子进程、不需要网络。
#[test]
fn p03_inline_engine_emits_origin_and_translation_in_process() {
    let engine = ScriptedInlineEngine::with_script(vec![
        ScriptedLine::new("Bonjour", Some("你好")),
        ScriptedLine::new("Au revoir", Some("再见")),
    ]);
    assert_eq!(engine.locality(), EngineLocality::Local, "内置引擎不得外发");

    let session = SessionCfg {
        translate: true,
        ..SessionCfg::default()
    };
    // 音频抽头恒返回 0：本引擎不消费音频，只按脚本合成
    let handle = engine
        .start(session, Box::new(SilentTap))
        .expect("内置引擎必须可直接启动");

    let stream = handle.events();
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Final { .. }
        )),
        "内置引擎必须产出成句 Final"
    );
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Translated { .. }
        )),
        "开启翻译时必须产出 Translated"
    );

    // 新增引擎自带语种集：既有引擎不含法语，这正是「引擎可自带语种」的印证
    let (origin, _target) = engine.languages();
    assert!(origin.contains("fr"), "内置引擎的语种集必须由实现自带");

    handle.pause().expect("pause 必须支持");
    handle.resume().expect("resume 必须支持");
    handle.seek_reset(1_000).expect("seek_reset 必须支持");
    let _ = handle.watermark();
    handle.stop().expect("stop 必须支持");
}

#[test]
fn p03_inline_engine_skips_translation_when_disabled() {
    let engine = ScriptedInlineEngine::with_script(vec![ScriptedLine::new("Bonjour", Some("你好"))]);
    let handle = engine
        .start(SessionCfg::default(), Box::new(SilentTap))
        .expect("内置引擎必须可直接启动");

    let stream = handle.events();
    // 先等成句，确保合成线程已跑完整个脚本
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Final { .. }
        )),
        "关闭翻译时仍必须产出原文"
    );
    // 关闭翻译后（默认 cfg.translate = false）不得出现译文
    assert!(
        !wait_until(&*stream, Duration::from_millis(300), |ev| matches!(
            ev,
            CueEvent::Translated { .. }
        )),
        "关闭翻译时不得产出 Translated"
    );
    handle.stop().expect("stop 必须支持");
}

/// 轮询事件流直到命中目标或超时，返回是否命中。
fn wait_until(
    stream: &dyn cinema_domain::ports::EventSource<CueEvent>,
    timeout: Duration,
    mut hit: impl FnMut(&CueEvent) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match stream.try_next() {
            Some(ev) => {
                if hit(&ev) {
                    return true;
                }
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    false
}

#[test]
fn p03_local_asr_scripted_child_emits_final_and_translation() {
    let engine = LocalAsrSubtitleEngine::with_config(LocalAsrConfig {
        program: PathBuf::from(env!("CARGO_BIN_EXE_cinema-asr")),
        backend: "scripted".into(),
        script: None,
        model: None,
    });
    let session = SessionCfg {
        translate: true,
        ..SessionCfg::default()
    };
    let handle = engine
        .start(session, Box::new(ScriptedTap::new(4)))
        .expect("scripted 引擎应能启动子进程");

    let stream = handle.events();
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Final { .. }
        )),
        "scripted 后端必须产出成句 Final"
    );
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Translated { .. }
        )),
        "开启翻译时必须产出 Translated"
    );

    handle.pause().expect("pause 必须支持");
    handle.resume().expect("resume 必须支持");
    handle.seek_reset(1_000).expect("seek_reset 必须支持");
    let _ = handle.watermark();
    handle.stop().expect("stop 必须支持");
}

#[test]
fn p03_local_asr_reports_failed_when_model_missing() {
    let engine = LocalAsrSubtitleEngine::with_config(LocalAsrConfig {
        program: PathBuf::from(env!("CARGO_BIN_EXE_cinema-asr")),
        backend: "whisper".into(),
        script: None,
        model: None,
    });
    let handle = engine
        .start(SessionCfg::default(), Box::new(SilentTap))
        .expect("应能启动子进程");

    let stream = handle.events();
    assert!(
        wait_until(&*stream, Duration::from_secs(3), |ev| matches!(
            ev,
            CueEvent::Failed { .. }
        )),
        "缺少模型时子进程必须主动上报 Failed"
    );
    handle.stop().expect("stop 必须支持");
}

/// 在回环地址起一个极简 HTTP 服务端：读请求头 → 解析 chunked 请求体并丢弃 →
/// 回写记录行 → 关闭连接。返回监听端口与服务线程句柄。
fn spawn_loopback_asr_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定回环端口");
    let port = listener.local_addr().expect("读取端口").port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("接受连接");
        let mut reader = BufReader::new(sock.try_clone().expect("复制套接字"));

        // 读请求行与请求头，直到空行
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break;
            }
        }

        // 解析 chunked 请求体（收到多少丢弃多少），对端空闲即以读超时结束
        let _ = reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(300)));
        loop {
            let mut size_line = String::new();
            match reader.read_line(&mut size_line) {
                Ok(0) => break,
                Ok(_) => {
                    let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
                    if size == 0 {
                        break;
                    }
                    let mut chunk = vec![0u8; size];
                    if reader.read_exact(&mut chunk).is_err() {
                        break;
                    }
                    let mut crlf = [0u8; 2];
                    let _ = reader.read_exact(&mut crlf);
                }
                Err(_) => break,
            }
        }

        // Connection: close —— 回写纯文本行流后关闭
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n";
        let body = concat!(
            "final\t0\t1000\tzh\t回环一号\n",
            "final\t1000\t2000\tzh\t回环二号\n",
            "translated\t0\t1000\ten\tloopback\n"
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body.as_bytes());
        let _ = sock.flush();
    });
    (port, handle)
}

#[test]
fn p03_remote_engine_rejects_non_http_url() {
    let engine = RemoteSubtitleEngine::with_config(RemoteAsrConfig {
        url: "https://example.test/asr".into(),
    });
    assert_eq!(engine.locality(), EngineLocality::Remote);
    assert!(
        engine
            .start(SessionCfg::default(), Box::new(SilentTap))
            .is_err(),
        "非 http:// 地址必须被拒绝"
    );
}

#[test]
fn p03_remote_engine_streams_records_over_loopback_http() {
    let (port, server) = spawn_loopback_asr_server();
    let engine = RemoteSubtitleEngine::with_config(RemoteAsrConfig {
        url: format!("http://127.0.0.1:{port}/asr"),
    });
    assert_eq!(engine.locality(), EngineLocality::Remote);

    let session = SessionCfg {
        translate: true,
        ..SessionCfg::default()
    };
    let handle = engine
        .start(session, Box::new(SilentTap))
        .expect("回环 http 地址应能启动会话");

    let stream = handle.events();
    assert!(
        wait_until(&*stream, Duration::from_secs(5), |ev| matches!(
            ev,
            CueEvent::Final { .. }
        )),
        "远端行流必须回传成句 Final"
    );
    handle.stop().expect("stop 必须支持");

    server.join().expect("服务线程应正常结束");
}

// ---------------------------------------------------------------------------
// P-04 契约
// ---------------------------------------------------------------------------

fn contract_player_core<P: PlayerCore>(player: &P) {
    // 「关闭时为 None（零开销）」是文档明文约束，必须对所有实现成立
    assert!(
        player.audio_tap(TapMode::Off).is_none(),
        "audio_tap(Off) 必须为 None"
    );

    // 静音切换必须幂等且可反复调用
    player.set_muted(true);
    player.set_muted(false);
    player.set_muted(true);

    // load 允许因内核未就绪而失败，但不得 panic。
    // 用本地不存在的路径而不是网络地址：真实内核会走完整加载分支，
    // 但不会引入 DNS / HTTP 的异步时序，让契约测试保持确定性。
    let _ = player.load(PlayableRef::local("Z:/__cinema_no_such_media__.mp4"), None);

    player.command(PlayerCmd::Play);
    player.command(PlayerCmd::Pause);
    player.command(PlayerCmd::Seek { position_ms: 0 });

    let stream = player.events();
    let _ = stream.try_next();
}

#[test]
fn p04_null_core_meets_contract() {
    let player = NullPlayerCore::new();
    contract_player_core(&player);
    // 预览实例「常驻静音」的可观测落点
    player.set_muted(true);
    assert!(player.is_muted(), "set_muted 必须可被观测");
}

#[test]
fn p04_mpv_core_meets_contract_when_library_is_available() {
    // headless 构造（vo=null），不需要窗口；运行环境没有 libmpv-2.dll 时明确降级跳过
    match MpvPlayerCore::new(None) {
        Ok(player) => {
            contract_player_core(&player);
            player.set_muted(true);
            assert!(player.is_muted(), "set_muted 必须可被观测");
        }
        Err(PortError::Unsupported(_)) => {
            // 无内核环境：契约由 p04_null_core_meets_contract 兜底
        }
        Err(other) => panic!("libmpv 存在时构造不得失败：{other}"),
    }
}

#[test]
fn p04_audio_tap_off_is_none_and_preview_cannot_tap() {
    // 无 libmpv 时降级跳过（与既有 P-04 用例同款写法）
    match MpvPlayerCore::new(None) {
        Ok(player) => {
            assert!(
                player.audio_tap(TapMode::Off).is_none(),
                "audio_tap(Off) 必须为 None"
            );
            // 预览实例：架构 §7 要求 audio_tap 恒为 None
            player.set_tap_allowed(false);
            assert!(
                player.audio_tap(TapMode::Pcm16kMono).is_none(),
                "禁止抽取的实例必须拿不到 tap"
            );
        }
        Err(PortError::Unsupported(_)) => {
            // 无内核环境：契约由 p04_null_core_meets_contract 兜底
        }
        Err(other) => panic!("libmpv 存在时构造不得失败：{other}"),
    }
}

// ---------------------------------------------------------------------------
// P-05 契约
// ---------------------------------------------------------------------------

fn contract_tag_dimension_registry<R: TagDimensionRegistry>(registry: &mut R) {
    let before = registry.dimensions().len();

    // dimensions() 必须按 order 升序返回
    let defs = registry.dimensions();
    for pair in defs.windows(2) {
        assert!(pair[0].order <= pair[1].order, "dimensions 必须按 order 升序");
    }

    // 新增维度：登记后即可检索到——这是「新增维度零改检索代码」的验收点
    let def = TagDimensionDef::genre();
    let id = def.id.clone();
    registry.register(def.clone()).expect("首次登记应成功");
    assert_eq!(registry.dimensions().len(), before + 1);
    assert_eq!(
        registry.get(&id).map(|d| d.id),
        Some(id.clone()),
        "登记后必须可按 id 取回"
    );

    // 重复 id 必须被拒绝，不得静默覆盖
    assert!(registry.register(def).is_err(), "重复 id 必须被拒绝");
}

#[test]
fn p05_registry_meets_contract() {
    let mut registry = ConfigTagDimensionRegistry::new(); // 空实现起点
    contract_tag_dimension_registry(&mut registry);
}

#[test]
fn p05_builtin_defaults_contain_genre() {
    let registry = ConfigTagDimensionRegistry::with_builtin_defaults();
    assert!(registry.get("genre").is_some(), "应内置文档示例维度");
}
