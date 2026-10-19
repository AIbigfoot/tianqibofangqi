//! P-01 实现：本地目录源、直链源，以及空实现。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    DiscoverStat, DiscoveredItem, MediaItem, MediaSource, PlayableRef, SourceKind, StreamFacts,
};
use cinema_domain::ports::media_source::{DiscoverSink, MediaSourceProvider};

use crate::mpv::MpvProber;

/// 可被识别的视频扩展名。
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "ts", "m2ts", "webm", "rmvb", "mpg", "mpeg", "m4v",
];

/// 空实现：不发现任何条目、不提供播放引用。
///
/// 用途：单测、UI 空态、以及作为「未选择源类型」的占位。
/// 对应 M0 验收中「五个端口各有一个最小实现（含"空实现"）」。
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyMediaSourceProvider;

impl MediaSourceProvider for EmptyMediaSourceProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::LocalFolder
    }

    fn discover(&self, _src: &MediaSource, _sink: &dyn DiscoverSink) -> Result<DiscoverStat> {
        Ok(DiscoverStat::default())
    }

    fn open(&self, _item: &MediaItem) -> Result<PlayableRef> {
        Err(PortError::Unsupported("空实现：不提供播放引用".into()))
    }

    fn probe(&self, _ref_: &PlayableRef) -> Result<StreamFacts> {
        Err(PortError::Unsupported("空实现：不提供探测".into()))
    }
}

/// 本地目录源实现：递归枚举视频文件并提交候选项，不做解析与入库。
///
/// `root` 是该源配置的根目录。条目只持久化**相对路径**（`stable_key` 由「源 + 相对
/// 路径」构成，因此挪动整棵目录树不会造成重复入库），播放时再由 `root` 拼回绝对路径。
#[derive(Debug, Clone, Default)]
pub struct LocalFolderProvider {
    root: PathBuf,
    /// 时长探测器。未配置时 `probe` 返回空事实，不阻塞入库（M0 的降级形态）。
    prober: Option<Arc<MpvProber>>,
}

impl LocalFolderProvider {
    /// 以某个目录为根创建提供者（不探测时长）。
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            prober: None,
        }
    }

    /// 带时长探测器的本地目录源。
    ///
    /// 「续播」与「悬停预览可用性」都依赖时长，因此真实入库路径应使用本构造器。
    pub fn with_prober(root: &Path, prober: Arc<MpvProber>) -> Self {
        Self {
            root: root.to_path_buf(),
            prober: Some(prober),
        }
    }

    /// 该实例绑定的根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl MediaSourceProvider for LocalFolderProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::LocalFolder
    }

    fn discover(&self, src: &MediaSource, sink: &dyn DiscoverSink) -> Result<DiscoverStat> {
        discover_video_tree(Path::new(&src.location), src, sink)
    }

    fn open(&self, item: &MediaItem) -> Result<PlayableRef> {
        let rel = item.rel_path.as_ref().ok_or_else(|| {
            PortError::NotFound(format!("条目 {} 缺少本地相对路径", item.id))
        })?;
        if self.root.as_os_str().is_empty() {
            // 没有根目录就无法把相对路径还原成可播放路径，显式失败好过静默给出错误路径
            return Err(PortError::Unsupported(
                "本地目录源未绑定根目录，无法解析相对路径".into(),
            ));
        }
        Ok(PlayableRef::local(
            self.root.join(rel).to_string_lossy().to_string(),
        ))
    }

    fn probe(&self, ref_: &PlayableRef) -> Result<StreamFacts> {
        match self.prober.as_ref() {
            Some(prober) => prober.probe(&ref_.location),
            // 无探测器时返回空事实：入库流程照常进行，只是暂时没有时长
            None => Ok(StreamFacts::default()),
        }
    }
}

/// 在线直链源最小实现。P0 只支持**单个视频直链**：源本身即为一条候选。
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectUrlProvider;

impl MediaSourceProvider for DirectUrlProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::DirectUrl
    }

    fn discover(&self, src: &MediaSource, sink: &dyn DiscoverSink) -> Result<DiscoverStat> {
        let mut stat = DiscoverStat::default();
        if src.location.trim().is_empty() {
            return Err(PortError::NotFound("直链为空".into()));
        }
        let candidate = DiscoveredItem {
            // 稳定键必须含源 id：不同源配置同一 URL 时不得互相合并
            stable_key: format!("{}|{}", src.id, normalize_url(&src.location)),
            rel_path: None,
            url: Some(src.location.clone()),
            title_hint: None,
            poster_ref: None,
        };
        if sink.submit(candidate).is_ok() {
            stat.discovered = 1;
        }
        Ok(stat)
    }

    fn open(&self, item: &MediaItem) -> Result<PlayableRef> {
        match item.url.as_ref() {
            Some(url) => Ok(PlayableRef {
                location: url.clone(),
                // 需要时可附带请求头（如 Referer / User-Agent），M0 暂无
                headers: Vec::new(),
            }),
            None => Err(PortError::NotFound(format!("条目 {} 缺少 URL", item.id))),
        }
    }

    fn probe(&self, _ref_: &PlayableRef) -> Result<StreamFacts> {
        Ok(StreamFacts::default())
    }
}

// ---------------------------------------------------------------------------
// 局域网路径源（M5 增量：只加实现 + 注册，不改动 L2 端口与 L1 流程）
// ---------------------------------------------------------------------------

/// 局域网路径源实现（UNC / SMB 网络位置）。
///
/// 与本地目录源的**同**：都走同一套「递归枚举视频文件 → 提交候选项」流程（[`discover_video_tree`]）；
/// 与本地目录源的**异**：路径先按局域网写法归一（`smb://主机/共享/目录`、`//主机/共享/目录`
/// → `\\主机\共享\目录`），因此它代表的是「网络位置」这一独立源类型。
///
/// 稳定键仍按「源 id + 规范化相对路径」构造，所以同一目录以本地源与局域网源分别添加时
/// 不会互相合并（[`DirectUrlProvider`] 的同一条规则）。
#[derive(Debug, Clone, Default)]
pub struct LanPathProvider {
    /// 归一化后的局域网根路径。
    root: PathBuf,
    /// 时长探测器。未配置时 `probe` 返回空事实，不阻塞入库。
    prober: Option<Arc<MpvProber>>,
}

impl LanPathProvider {
    /// 以某个局域网路径为根创建提供者（不探测时长）。
    pub fn new(location: &str) -> Self {
        Self {
            root: PathBuf::from(normalized_lan_root(location)),
            prober: None,
        }
    }

    /// 带时长探测器的局域网路径源。
    pub fn with_prober(location: &str, prober: Arc<MpvProber>) -> Self {
        Self {
            root: PathBuf::from(normalized_lan_root(location)),
            prober: Some(prober),
        }
    }

    /// 归一化后的根路径。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl MediaSourceProvider for LanPathProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::LanPath
    }

    fn discover(&self, src: &MediaSource, sink: &dyn DiscoverSink) -> Result<DiscoverStat> {
        if self.root.as_os_str().is_empty() {
            // 空路径无法扫描，显式失败好过静默给出空结果
            return Err(PortError::NotFound("局域网路径为空".into()));
        }
        discover_video_tree(&self.root, src, sink)
    }

    fn open(&self, item: &MediaItem) -> Result<PlayableRef> {
        let rel = item.rel_path.as_ref().ok_or_else(|| {
            PortError::NotFound(format!("条目 {} 缺少局域网相对路径", item.id))
        })?;
        if self.root.as_os_str().is_empty() {
            return Err(PortError::Unsupported(
                "局域网源未绑定根路径，无法解析相对路径".into(),
            ));
        }
        // 归一化后的 UNC 路径由系统网络重定向器解析，因此与本地路径同款拼接即可
        Ok(PlayableRef::local(
            self.root.join(rel).to_string_lossy().to_string(),
        ))
    }

    fn probe(&self, ref_: &PlayableRef) -> Result<StreamFacts> {
        match self.prober.as_ref() {
            Some(prober) => prober.probe(&ref_.location),
            None => Ok(StreamFacts::default()),
        }
    }
}

/// 把局域网写法归一为 UNC 路径。
///
/// - `smb://主机/共享/目录` → `\\主机\共享\目录`
/// - `//主机/共享/目录` → `\\主机\共享\目录`
/// - 已是 UNC（`\\主机\共享`）或本地盘符（`Z:\媒体`）的写法**原样返回**，不改写、不猜测。
pub fn normalized_lan_root(location: &str) -> String {
    let raw = location.trim();
    let rest = match raw.strip_prefix("smb://") {
        Some(rest) => rest,
        None => match raw.strip_prefix("//") {
            Some(rest) => rest,
            None => return raw.to_owned(),
        },
    };
    let body = rest.trim_matches('/').replace('/', "\\");
    format!("\\\\{body}")
}

/// 递归枚举目录树下的视频文件并提交候选项。本地目录源与局域网路径源共用
/// （M5 抽出的共用发现逻辑：新增源类型只提供「路径形态」，不复制扫描代码）。
fn discover_video_tree(
    root: &Path,
    src: &MediaSource,
    sink: &dyn DiscoverSink,
) -> Result<DiscoverStat> {
    if !root.is_dir() {
        return Err(PortError::NotFound(format!("目录不存在: {}", root.display())));
    }

    let mut stat = DiscoverStat::default();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if sink.is_cancelled() {
            // 取消后已提交条目保留
            stat.cancelled = true;
            break;
        }
        let entries = std::fs::read_dir(&dir).map_err(|e| PortError::Backend(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| PortError::Backend(e.to_string()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !is_video(&path) {
                continue;
            }
            let rel = rel_path(&path, root);
            let title_hint = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_owned);
            let candidate = DiscoveredItem {
                // 稳定键：源 + 规范化相对路径，不随元数据变化
                stable_key: format!("{}|{}", src.id, rel),
                rel_path: Some(rel),
                url: None,
                title_hint,
                poster_ref: None,
            };
            if sink.submit(candidate).is_ok() {
                stat.discovered += 1;
            }
        }
    }
    Ok(stat)
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
        .unwrap_or(false)
}

fn rel_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 规范化 URL：稳定键不得随查询串变动以外的东西改变，这里只做最小归一。
fn normalize_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_owned()
}
