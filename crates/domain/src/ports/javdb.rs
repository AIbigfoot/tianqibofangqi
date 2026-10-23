//! P-06 `JavdbClient` — 元数据数据源端口。
//!
//! 领域层只声明「需要哪些数据」，不关心 HTTP、HTML 结构或 Cookie 怎么带。
//! L3 的 [`crate::ports::javdb`] 实现负责把站点页面解析成这里的结构；
//! L1 的元数据服务负责把结构与本地库合并（用户锁定字段优先）。

use std::path::Path;

use crate::error::Result;
use crate::model::{ScrapedComment, ScrapedImage};

/// 数据源访问配置：站点地址 + 登录 Cookie。
///
/// `cookie` 允许为空——未登录也能拿到标题、封面、截图、标签、日期、演员，
/// 只有评论与预告片需要登录（站点侧限制，不是本程序的限制）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JavdbConfig {
    pub base_url: String,
    pub cookie: String,
}

impl JavdbConfig {
    /// Cookie 是否已填写（空串或只有空白视为未填写）。
    pub fn has_cookie(&self) -> bool {
        !self.cookie.trim().is_empty()
    }
}

/// 站点登录态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoginState {
    pub logged_in: bool,
    /// 站点上显示的账号名（拿不到时为空）。
    pub username: Option<String>,
}

/// 搜索命中的一条影片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHit {
    pub uid: String,
    pub code: String,
    pub title: String,
    pub cover_url: Option<String>,
    pub release_date: Option<String>,
}

/// 预告片的取流方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrailerKind {
    /// 直接可下载的视频文件（mp4 / ts）。
    Direct,
    /// HLS 播放列表（m3u8），需要拉取分片后合并。
    Hls,
}

/// 一条预告片来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailerSource {
    pub url: String,
    pub kind: TrailerKind,
}

/// 详情页解析结果。字段全部来自一次页面抓取，缺什么就是 `None` / 空集合。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoteDetail {
    pub uid: String,
    /// 番号（页面上的原始写法）。
    pub code: String,
    /// 标题（中文优先；站点没有中文标题时是原标题）。
    pub title: String,
    pub release_date: Option<String>,
    pub duration_ms: Option<i64>,
    pub rating: Option<f64>,
    pub cover_url: Option<String>,
    pub screenshots: Vec<ScrapedImage>,
    pub trailer: Option<TrailerSource>,
    pub categories: Vec<String>,
    pub series: Option<String>,
    pub maker: Option<String>,
    pub director: Option<String>,
    pub actors: Vec<String>,
    /// 站点返回的短评（已按点赞数降序，条数由站点决定）。
    pub comments: Vec<ScrapedComment>,
}

/// 元数据数据源端口。
pub trait JavdbClient: Send + Sync {
    /// 探测 Cookie 是否有效；未填写 Cookie 时应返回「未登录」而不是错误。
    fn login_state(&self, config: &JavdbConfig) -> Result<LoginState>;

    /// 按番号搜索，返回值已按「番号完全一致优先」排序。
    fn search(&self, config: &JavdbConfig, code: &str) -> Result<Vec<RemoteHit>>;

    /// 抓取详情页（含截图、标签、演员、短评、预告片）。
    fn detail(&self, config: &JavdbConfig, uid: &str) -> Result<RemoteDetail>;

    /// 下载任意资源字节（封面 / 截图）。
    fn fetch_bytes(&self, config: &JavdbConfig, url: &str) -> Result<Vec<u8>>;

    /// 把预告片落盘到 `dest`。站点给出的是 HLS 时由实现负责拉取分片并合并。
    ///
    /// 无法本地化时（如加密流）返回 `Unsupported`，调用方保留远端地址供在线播放。
    fn save_trailer(&self, config: &JavdbConfig, source: &TrailerSource, dest: &Path) -> Result<()>;
}
