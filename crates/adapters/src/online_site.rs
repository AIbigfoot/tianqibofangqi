//! P-01 实现：在线站点源（联网导入）。
//!
//! **本文件是什么**：`OnlineSiteProvider` 是 [`SourceKind::OnlineSite`] 的 L3 实现。
//! 用户粘贴一个站点的**列表页 / 搜索页 URL**，本提供者联网枚举出该页上的条目
//! （标题、详情页地址），把封面按需缓存到本地海报目录；播放时把影片**边下边播**到
//! 本地缓存文件（见下）。它是「发现」与「解析」分离端口契约的又一个实现——只加实现，
//! 不改 L2 端口与 L1 流程。
//!
//! **与既有 `DirectUrlProvider` 的关系**：两者都是「在线」源、都产出可直接交给播放
//! 内核的引用，但职责不同。`DirectUrlProvider` 只处理**单个视频直链**（源本身即为
//! 一条候选，`open` 直接原样返回该直链）；本提供者处理**站点页面**，需要先把页面
//! 展开成多条候选，再把详情页「展开成」一个本地缓存文件。
//!
//! **为什么是「边下边播」而不是把直链交给播放内核**（实测结论，见架构文档 A.20）：
//! 目标站点走 Cloudflare，`mpv`（libcurl）直连其 CDN 一律 `[curl] error: SSL connect
//! error` / `loading failed`——只有 yt-dlp 自带的 curl_cffi 指纹伪装能取到流。因此
//! `open` 不再解析直链，而是让 yt-dlp 把影片写到 `<数据目录>/streams/<hash>.mp4`，
//! 播放内核打开这个**正在增长**的本地文件即可（实测该站 mp4 的 `moov` 在文件头，
//! 下载到 100MB+ 时 mpv 已能读出完整时长 910s）。
//!
//! **为什么 `open` 必须立刻返回**：Tauri 的同步命令跑在**主线程**上，命令体里任何
//! 阻塞都会冻结整个界面（包括 webview 渲染）。旧实现里 `open` 要串两次 yt-dlp
//! （抓封面 + 解析直链，合计约 10 秒），这就是「点击播放后软件卡住」的根因。
//! 现在 `open` 只做「算路径 + 起一个分离的下载子进程」，毫秒级返回；文件尚未长成
//! 时播放内核会先装载失败，由组合根在心跳里补一次 `loadfile`（见 `poll_screens`）。
//!
//! **为什么用子进程而不是内置 HTTP**：站点通常是动态页面、带登录/防盗链/多编码流，
//! 用纯 HTTP 抓 HTML 再解析既脆弱又无法覆盖各类站点；yt-dlp 已把这些「如何取流地址
//! 与请求头」的知识沉淀在稳定 CLI 契约里（`-j` / `--write-info-json`），因此本层只做
//! **子进程编排 + 取字段**，不引入 reqwest/tokio 等网络依赖，保持 L3 依赖方向干净
//! （仅 `cinema_domain` + 标准库 + serde_json）。
//!
//! **发现为什么要分两条路径**（实测结论，见架构文档 A.18）：
//! yt-dlp 的 `--flat-playlist --dump-single-json` 只对**自己写了 extractor 的站点**
//! 有效；对没有 extractor 的站点（如 hanime1.me），列表页会直接报
//! `Unsupported URL`。但这类站点的列表页 HTML 结构稳定，且 yt-dlp 在报错**之前**
//! 已经把页面抓了下来。因此 `discover` 先走路径一（结构化枚举，覆盖 yt-dlp 原生
//! 支持的站点），拿不到条目再走路径二（`--write-pages` 落盘页面 → 自行解析卡片）。
//! 详情页解析（`open`）不受影响，因为 yt-dlp 对**单集页面**能正常取到直链。
//!
//! **反爬前提**：目标站点走 Cloudflare，普通请求返回 403；yt-dlp 需带
//! `--impersonate chrome`（其自带的 curl_cffi 指纹伪装）才能取到页面。
//! 该参数对本文件的每次真实调用都必须携带，否则站点侧 403。

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{
    DiscoverStat, DiscoveredItem, MediaItem, MediaSource, PlayableRef, SourceKind, StreamFacts,
};
use cinema_domain::ports::media_source::{DiscoverSink, MediaSourceProvider};

// ---------------------------------------------------------------------------
// 分离下载：状态、句柄与登记表
// ---------------------------------------------------------------------------

/// 一次分离下载的当前状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadState {
    /// 已登记，进程仍在运行（文件还在增长）。
    Running,
    /// 进程已正常退出（文件已完整）。
    Finished,
    /// 进程非正常退出，附诊断片段（供错误卡显示）。
    Failed(String),
    /// 未登记：例如文件是上一次会话留下的完整文件，或本次尚未发起下载。
    Unknown,
}

/// 一个分离下载的句柄：能被查询状态与终止。
///
/// 之所以抽象成接口：单测环境不该真的拉起几分钟的下载进程，注入替身即可对
/// 「什么时候起下载、什么时候复用、什么时候停」逐条断言。
pub trait DownloadHandle: Send {
    /// 轮询一次进度：仍在运行返回 `None`，已结束返回终态（实现须缓存终态，
    /// 重复轮询结果一致）。
    fn poll(&mut self) -> Option<DownloadState>;
    /// 终止并回收子进程（窗口关闭 / 程序退出时防止孤儿进程残留）。
    fn kill(&mut self);
}

/// 分离下载的登记表：本地缓存文件路径 → 下载句柄 / 终态。
///
/// 它跨命令存活（挂在组合根的 `AppState` 上），因此「同一条目重复点播放」不会
/// 重复起下载；窗口关闭时由组合根调 [`DownloadRegistry::stop`] 收尾。
#[derive(Default)]
pub struct DownloadRegistry {
    slots: Mutex<HashMap<PathBuf, Slot>>,
}

enum Slot {
    Running(Box<dyn DownloadHandle>),
    Settled(DownloadState),
}

impl DownloadRegistry {
    /// 空登记表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一个刚起的下载。
    pub fn register(&self, target: PathBuf, handle: Box<dyn DownloadHandle>) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.insert(target, Slot::Running(handle));
        }
    }

    /// 查询某本地缓存文件的下载状态。
    pub fn state(&self, target: &Path) -> DownloadState {
        let Ok(mut slots) = self.slots.lock() else {
            return DownloadState::Unknown;
        };
        let settled = {
            let Some(slot) = slots.get_mut(target) else {
                return DownloadState::Unknown;
            };
            match slot {
                Slot::Settled(state) => return state.clone(),
                Slot::Running(handle) => match handle.poll() {
                    None => return DownloadState::Running,
                    Some(state) => state,
                },
            }
        };
        // 进程已结束：换成终态，后续轮询不再触碰进程
        slots.insert(target.to_path_buf(), Slot::Settled(settled.clone()));
        settled
    }

    /// 终止某文件的下载（无登记时什么都不做）。
    pub fn stop(&self, target: &Path) {
        if let Ok(mut slots) = self.slots.lock() {
            if let Some(Slot::Running(handle)) = slots.get_mut(target) {
                handle.kill();
            }
            slots.remove(target);
        }
    }

    /// 终止全部下载（退出多屏 / 关闭播放 / 程序退出时调用）。
    pub fn stop_all(&self) {
        if let Ok(mut slots) = self.slots.lock() {
            for slot in slots.values_mut() {
                if let Slot::Running(handle) = slot {
                    handle.kill();
                }
            }
            slots.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// 进程运行器（可注入，便于离线单测）
// ---------------------------------------------------------------------------

/// 运行外部程序的抽象。
///
/// 之所以做成可注入接口：`OnlineSiteProvider` 的真实依赖是「外部进程 + 网络」，
/// 单测环境既无网络也不该真的拉起 yt-dlp，注入一个替身即可对「命令行参数、页面
/// 解析、封面落盘与缓存复用」逐条断言，而不用去连真实站点。
pub trait ProcessRunner: Send + Sync {
    /// 运行外部程序并返回标准输出文本；失败返回 [`PortError::Backend`]（含 stderr 片段）。
    ///
    /// `cwd` 指定子进程工作目录：yt-dlp 的 `--write-pages` **只把页面写到当前目录**
    /// （文件名由 URL 派生），因此必须能控制工作目录，否则会把调试文件散落到
    /// 主程序的启动目录里。
    fn run(&self, program: &Path, args: &[String], cwd: Option<&Path>) -> Result<String>;

    /// 启动一个**分离**子进程（不等待退出），stdout/stderr 落到 `log` 指定文件。
    ///
    /// 与 [`ProcessRunner::run`] 的区别是「不等待」：边下边播的下载要跑几分钟，必须
    /// 立刻拿到句柄交给 [`DownloadRegistry`]，命令才不会阻塞界面。日志落文件而不是
    /// 管道，是因为没有线程去持续读它——写满管道缓冲会把下载卡死。
    fn spawn(&self, program: &Path, args: &[String], log: &Path) -> Result<Box<dyn DownloadHandle>>;
}

/// 真实实现：拉起子进程并带超时收集 stdout/stderr。
///
/// **为何不用 `Command::output()`**：它内部会阻塞等待子进程退出，没有超时机制；
/// 一旦 yt-dlp 因站点卡住/等待输入而长期不返回，就会永久阻塞扫描线程。这里改为
/// `spawn()` + 两个读取线程 draining 管道 + 主线程 `try_wait()` 轮询并设截止时间，
/// 超时即 `kill()`，保证任何情况下都有界返回。
#[derive(Debug, Clone)]
pub struct SystemProcessRunner {
    timeout: Duration,
}

impl SystemProcessRunner {
    /// 默认 120 秒超时。
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl Default for SystemProcessRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, program: &Path, args: &[String], cwd: Option<&Path>) -> Result<String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW：避免弹出控制台窗口，与项目其它子进程（cinema-asr）保持一致
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|e| {
            // 程序不存在（PATH 上没有 yt-dlp / 路径写错）必须给出可读错误
            if e.kind() == std::io::ErrorKind::NotFound {
                PortError::Backend(format!("未找到可执行文件: {}", program.display()))
            } else {
                PortError::Backend(format!("启动子进程失败: {e}"))
            }
        })?;

        // 管道交给独立线程持续读干：主线程只负责轮询退出，避免「写满缓冲」互锁
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PortError::Backend("子进程 stdout 不可用".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| PortError::Backend("子进程 stderr 不可用".into()))?;

        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut pipe = stdout;
            let _ = pipe.read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut pipe = stderr;
            let _ = pipe.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait(); // 回收僵尸进程
                        return Err(PortError::Backend(format!(
                            "yt-dlp 超时（{}s）",
                            self.timeout.as_secs()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(PortError::Backend(format!("等待子进程失败: {e}"))),
            }
        };

        // 退出码已拿到，管道随之 EOF，读取线程随即结束，可安全取回缓冲
        let out = out_thread.join().unwrap_or_default();
        let err = err_thread.join().unwrap_or_default();

        if !status.success() {
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "未知".into());
            let err_text = String::from_utf8_lossy(&err);
            return Err(PortError::Backend(format!(
                "yt-dlp 退出码 {code}: {}",
                snippet(err_text.trim(), 200)
            )));
        }

        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn spawn(
        &self,
        program: &Path,
        args: &[String],
        log: &Path,
    ) -> Result<Box<dyn DownloadHandle>> {
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                PortError::Backend(format!("建下载日志目录失败（{}）：{e}", dir.display()))
            })?;
        }
        let out = File::create(log)
            .map_err(|e| PortError::Backend(format!("创建下载日志失败（{}）：{e}", log.display())))?;
        let err = out
            .try_clone()
            .map_err(|e| PortError::Backend(format!("复制下载日志句柄失败：{e}")))?;

        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let child = command.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PortError::Backend(format!("未找到可执行文件: {}", program.display()))
            } else {
                PortError::Backend(format!("启动下载子进程失败: {e}"))
            }
        })?;

        Ok(Box::new(SystemDownload {
            child,
            log: log.to_path_buf(),
            settled: None,
        }))
    }
}

/// 真实下载句柄：持有一个跑着 yt-dlp 的子进程。
struct SystemDownload {
    child: Child,
    log: PathBuf,
    /// 已判定出的终态（进程退出后缓存，避免重复 `try_wait` 与重复读日志）。
    settled: Option<DownloadState>,
}

impl DownloadHandle for SystemDownload {
    fn poll(&mut self) -> Option<DownloadState> {
        if let Some(state) = &self.settled {
            return Some(state.clone());
        }
        let status = match self.child.try_wait() {
            Ok(Some(status)) => status,
            Ok(None) => return None,
            Err(e) => {
                let state = DownloadState::Failed(format!("等待下载进程失败：{e}"));
                self.settled = Some(state.clone());
                return Some(state);
            }
        };
        let state = if status.success() {
            DownloadState::Finished
        } else {
            DownloadState::Failed(self.failure_text(status.code()))
        };
        self.settled = Some(state.clone());
        Some(state)
    }

    fn kill(&mut self) {
        if self.settled.is_some() {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait(); // 回收僵尸进程
    }
}

impl SystemDownload {
    /// 组装失败说明：yt-dlp 的错误在 stderr（已落日志），取末尾一行最有信息量。
    fn failure_text(&self, code: Option<i32>) -> String {
        let code = code.map(|c| c.to_string()).unwrap_or_else(|| "未知".into());
        let tail = std::fs::read_to_string(&self.log)
            .ok()
            .map(|text| {
                text.lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .next_back()
                    .unwrap_or("")
                    .to_owned()
            })
            .unwrap_or_default();
        if tail.is_empty() {
            format!("yt-dlp 退出码 {code}")
        } else {
            format!("yt-dlp 退出码 {code}：{}", snippet(&tail, 160))
        }
    }
}

// ---------------------------------------------------------------------------
// 在线站点提供者
// ---------------------------------------------------------------------------

/// 在线站点源实现：枚举列表页条目，播放时把影片边下边播到本地缓存文件。
///
/// `covers` 是海报缓存目录（由组合根传入，通常是 `<数据目录>/posters`）；封面以
/// 「确定性文件名」缓存在此处，界面层用「文件是否存在」判断是否显示真实封面。
///
/// `streams` 是影片缓存目录（通常是 `<数据目录>/streams`）。`open` 把条目映射成
/// 该目录下一个确定性命名的 `.mp4`，并让 yt-dlp 后台写入；播放内核打开这个文件。
///
/// `downloads` 是**跨命令存活**的下载登记表（挂在组合根的 `AppState` 上）：
/// 同一条目重复点播放不会重复起下载，窗口关闭时由组合根收尾子进程。
pub struct OnlineSiteProvider {
    /// yt-dlp 可执行文件路径。
    ytdlp: PathBuf,
    /// 海报缓存目录。
    covers: PathBuf,
    /// 影片缓存目录。
    streams: PathBuf,
    /// 下载登记表（与组合根共享）。
    downloads: Arc<DownloadRegistry>,
    /// 进程运行器（真实实现或单测替身）。
    runner: Arc<dyn ProcessRunner>,
}

impl OnlineSiteProvider {
    /// 以真实子进程运行器创建提供者。
    pub fn new(
        ytdlp: PathBuf,
        covers: PathBuf,
        streams: PathBuf,
        downloads: Arc<DownloadRegistry>,
    ) -> Self {
        Self::with_runner(ytdlp, covers, streams, downloads, Arc::new(SystemProcessRunner::new()))
    }

    /// 注入自定义运行器（单测使用）。
    pub fn with_runner(
        ytdlp: PathBuf,
        covers: PathBuf,
        streams: PathBuf,
        downloads: Arc<DownloadRegistry>,
        runner: Arc<dyn ProcessRunner>,
    ) -> Self {
        Self {
            ytdlp,
            covers,
            streams,
            downloads,
            runner,
        }
    }

    /// 抓一遍列表页，把该页**每张卡片**的封面图下载到海报目录（已存在的跳过），
    /// 返回本次新落盘的张数。
    ///
    /// **为什么封面取自列表页卡片而不是详情页**（实测结论，见架构文档 A.21）：
    /// 详情页用 yt-dlp `--write-thumbnail` 取到的是 `og:image`——那是**视频帧**
    /// （16:9），不是站点真正的封面 art；列表页卡片的 `<img src>` 才是封面图
    /// （通常指向站点 CDN 的 `/image/cover/<id>.jpg`）。卡片封面是**异站直链**，
    /// yt-dlp 的 generic extractor 能直接把它下下来（实测
    /// `[generic] jpeg: Downloading 1 format(s): 0`）。
    ///
    /// **整体尽力而为**：拿不到列表页、单张下载失败、封面本就没有，一律不抛错——
    /// 封面缺失不该阻断导入与播放，前端会保留占位图。
    pub fn fetch_covers(&self, location: &str) -> usize {
        let Ok(cards) = self.listing_cards(location) else {
            return 0;
        };
        let mut fetched = 0;
        for card in cards {
            let Some(cover) = card.cover else {
                continue;
            };
            let file_name = cover_file_name(&card.detail);
            if self.covers.join(&file_name).is_file() {
                continue; // 已下过：重复导入 / 重启后重抓都不重复下载
            }
            if self.fetch_cover_image(&cover, &file_name) {
                fetched += 1;
            }
        }
        fetched
    }

    /// 把一张直链封面图下载到海报目录，返回是否真的落盘了一张 JPEG。
    ///
    /// 目标站点的图片走 CDN 且带 `secure` 令牌，普通 HTTP 客户端会被挡，因此同样借
    /// yt-dlp 的指纹伪装（`--impersonate chrome`）。**不把非零退出当失败**：只要
    /// 文件落盘且确为 JPEG 就算成功；否则删掉半成品，避免留下 HTML/JSON 错误页
    /// 冒充封面（那会是一张「破图」）。
    pub fn fetch_cover_image(&self, url: &str, file_name: &str) -> bool {
        let target = self.covers.join(file_name);
        let args = vec![
            "--impersonate".to_owned(),
            "chrome".to_owned(),
            "--no-warnings".to_owned(),
            "--ignore-config".to_owned(),
            "--no-playlist".to_owned(),
            "--force-overwrites".to_owned(),
            "-o".to_owned(),
            target.to_string_lossy().into_owned(),
            url.to_owned(),
        ];
        // 有意忽略返回值：非零退出时文件也常已落盘，以文件内容为准
        let _ = self.runner.run(&self.ytdlp, &args, None);
        let landed = jpeg_at(&target);
        if landed {
            true
        } else {
            let _ = std::fs::remove_file(&target);
            false
        }
    }

    /// 起一个分离下载：把详情页对应的影片写到 `target`，立刻返回。
    fn start_download(&self, detail: &str, target: &Path) -> Result<()> {
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| PortError::Backend(format!("建影片缓存目录失败：{e}")))?;
        }
        // `--no-part` 直接写目标文件（不写 `.part` 再改名）：播放内核要在下载过程中
        // 就读这个文件。`--continue` 让失败的下载能从已落盘的部分接着下。
        let args = vec![
            "--impersonate".to_owned(),
            "chrome".to_owned(),
            "--no-warnings".to_owned(),
            "--ignore-config".to_owned(),
            "--no-playlist".to_owned(),
            "--no-part".to_owned(),
            "--continue".to_owned(),
            "-o".to_owned(),
            target.to_string_lossy().into_owned(),
            detail.to_owned(),
        ];
        let log = target.with_extension("log");
        let handle = self.runner.spawn(&self.ytdlp, &args, &log)?;
        self.downloads.register(target.to_path_buf(), handle);
        Ok(())
    }

    /// 路径一：交给 yt-dlp 的结构化枚举（仅对它有 extractor 的站点有效）。
    fn flat_playlist_cards(&self, location: &str) -> Result<Vec<ListingCard>> {
        // `--flat-playlist` 只枚举条目、不进入每条详情页（快）；`--dump-single-json`
        // 让整页以**单个** JSON 对象输出，便于一次解析
        let args = vec![
            "--no-warnings".to_owned(),
            "--ignore-config".to_owned(),
            "--flat-playlist".to_owned(),
            "--dump-single-json".to_owned(),
            location.to_owned(),
        ];
        let output = self.runner.run(&self.ytdlp, &args, None)?;
        let value = parse_json(&output)?;
        // 单条视频页也会返回一个对象（`_type` 通常是 `video`）；两种形态都要能处理
        Ok(extract_entries(&value)
            .into_iter()
            .filter_map(|entry| {
                let detail = detail_url(entry)?;
                Some(ListingCard {
                    detail,
                    title: entry
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty()),
                    // 结构化枚举只给详情页地址，不带封面（封面由列表页卡片解析提供）
                    cover: None,
                })
            })
            .collect())
    }

    /// 路径二：抓下列表页 HTML 自行解析卡片（覆盖 yt-dlp 没有 extractor 的站点）。
    ///
    /// 用 `--write-pages` 让 yt-dlp 把**已抓到的页面**写到指定目录：即便站点没有
    /// extractor（yt-dlp 随后以「Unsupported URL」非零退出），页面也已经落盘，
    /// 因此**不把非零退出当成失败**，而是以「有没有页面文件」为准。
    fn listing_cards(&self, location: &str) -> Result<Vec<ListingCard>> {
        let dir = unique_temp_dir("cinema-listing");
        std::fs::create_dir_all(&dir)
            .map_err(|e| PortError::Backend(format!("建列表页临时目录失败：{e}")))?;

        let args = vec![
            "--impersonate".to_owned(),
            "chrome".to_owned(),
            "--no-warnings".to_owned(),
            "--ignore-config".to_owned(),
            "--skip-download".to_owned(),
            "--no-playlist".to_owned(),
            "--write-pages".to_owned(),
            location.to_owned(),
        ];
        let outcome = self.runner.run(&self.ytdlp, &args, Some(&dir));
        let html = read_page_dump(&dir);
        // 页面已读进内存，临时目录不再需要
        let _ = std::fs::remove_dir_all(&dir);

        let html = match html {
            Some(html) => html,
            // 连页面都没拿到：此时子进程的错误才是真正的原因（缺 yt-dlp、网络不可达等）
            None => {
                return Err(outcome.err().unwrap_or_else(|| {
                    PortError::Backend(format!("未能获取列表页内容：{location}"))
                }))
            }
        };
        Ok(parse_cards(&html, location))
    }
}

impl MediaSourceProvider for OnlineSiteProvider {
    fn kind(&self) -> SourceKind {
        SourceKind::OnlineSite
    }

    fn discover(&self, src: &MediaSource, sink: &dyn DiscoverSink) -> Result<DiscoverStat> {
        let location = src.location.trim();
        if location.is_empty() {
            return Err(PortError::NotFound("在线站点列表页地址为空".into()));
        }

        // 先试结构化枚举；站点没有 extractor 时它会失败，此时回落到页面解析
        let (mut cards, mut first_error) = match self.flat_playlist_cards(location) {
            Ok(cards) => (cards, None),
            Err(err) => (Vec::new(), Some(err)),
        };
        if cards.is_empty() {
            cards = self.listing_cards(location)?;
        }
        if cards.is_empty() {
            return Err(first_error.take().unwrap_or_else(|| {
                PortError::Backend(format!("未在列表页中找到可导入的条目：{location}"))
            }));
        }

        let mut stat = DiscoverStat::default();
        for card in cards {
            if sink.is_cancelled() {
                // 取消后已提交条目保留
                stat.cancelled = true;
                break;
            }
            let candidate = DiscoveredItem {
                // 稳定键必须含源 id：不同源配置同一页面时不得互相合并
                stable_key: format!("{}|{}", src.id, normalize_url(&card.detail)),
                rel_path: None,
                url: Some(card.detail.clone()),
                title_hint: card.title,
                // `poster_ref` 在此阶段只是「登记文件名」，真正的封面文件按需落盘
                poster_ref: Some(cover_file_name(&card.detail)),
            };
            if sink.submit(candidate).is_ok() {
                stat.discovered += 1;
            }
        }
        Ok(stat)
    }

    /// 把条目映射到本地缓存文件并**立刻返回**（起下载是异步的）。
    ///
    /// 三种情形：
    /// 1. 同一文件已有下载在跑 → 直接复用，不重复起进程（重复点播放是常态）；
    /// 2. 同一文件已有完整下载（`Finished` 或未登记但文件在）→ 直接复用缓存；
    /// 3. 其余（未登记且文件不在 / 上次失败）→ 起一个分离下载，`--continue` 会接着
    ///    已落盘的部分下。
    ///
    /// 返回的 `headers` 恒为空：播放内核读的是本地文件，不需要防盗链请求头。
    fn open(&self, item: &MediaItem) -> Result<PlayableRef> {
        let detail = item
            .url
            .as_ref()
            .ok_or_else(|| PortError::NotFound(format!("条目 {} 缺少详情页地址", item.id)))?;

        let target = self.streams.join(stream_file_name(detail));
        let need_start = match self.downloads.state(&target) {
            DownloadState::Running | DownloadState::Finished => false,
            // 失败过：重试（`--continue` 接着下），让用户「重试」按钮有实际效果
            DownloadState::Failed(_) => true,
            DownloadState::Unknown => !target.is_file(),
        };
        if need_start {
            self.start_download(detail, &target)?;
        }

        Ok(PlayableRef {
            location: target.to_string_lossy().into_owned(),
            headers: Vec::new(),
        })
    }

    /// 零开销返回空事实。
    ///
    /// 在线源在 `open` 时**不联网**（那正是卡死的来源），因此时长 / 分辨率这类
    /// 「需要解析才拿到」的事实此处给不出；真正的时长由播放内核装载后读出，
    /// 经心跳回填进度表，界面上依然可见。
    fn probe(&self, _ref_: &PlayableRef) -> Result<StreamFacts> {
        Ok(StreamFacts::default())
    }
}

// ---------------------------------------------------------------------------
// 列表页卡片解析（纯字符串处理，不引入 HTML 解析依赖）
// ---------------------------------------------------------------------------

/// 列表页上解析出的一条卡片。
struct ListingCard {
    /// 详情页地址（条目身份）。
    detail: String,
    /// 卡片标题文本（可能为空）。
    title: Option<String>,
    /// 卡片封面图地址（站点 CDN 直链，可能为空）。
    ///
    /// 与 `detail` 同为绝对地址，但**可以指向其它主机**（封面常放在独立 CDN 上），
    /// 因此解析时不能套用「同站」过滤。
    cover: Option<String>,
}

/// 从一个列表页 HTML 中解析出卡片。
///
/// 判定规则刻意保守，宁可漏也不要收进「非条目」链接：
/// 1. 只认带 `href` 的 `<a>...</a>`，且**必须是本页同站链接**（滤掉广告等外站跳转）；
/// 2. 锚点内部必须含 `<img>`（滤掉「下一页 / 分类」这类纯文字导航）；
/// 3. 同一详情页地址只保留一次（页面常出现同一卡片的重复渲染）。
/// 封面取锚点内**第一张 `<img>` 的 `src`**，绝对化时不做同站限制（封面常在独立 CDN 上）。
fn parse_cards(html: &str, listing_url: &str) -> Vec<ListingCard> {
    let host = url_host(listing_url);
    let origin = url_origin(listing_url);
    let listing_key = normalize_url(listing_url);
    let mut cards: Vec<ListingCard> = Vec::new();
    let mut seen: VecDeque<String> = VecDeque::new();
    let mut cursor = 0usize;

    while let Some(open) = find_from(html, cursor, "<a") {
        // `<a` 之后必须是空白或 `>`：否则会误吞 `<audio` / `<article` 等标签
        let is_anchor = html[open + 2..]
            .chars()
            .next()
            .map(|c| c.is_whitespace() || c == '>')
            .unwrap_or(false);
        if !is_anchor {
            cursor = open + 2;
            continue;
        }
        let Some(tag_len) = html[open..].find('>') else {
            break;
        };
        let tag_end = open + tag_len;
        let tag = &html[open..tag_end];
        let Some(close_len) = html[tag_end..].find("</a>") else {
            break;
        };
        let inner = &html[tag_end + 1..tag_end + close_len];
        cursor = tag_end + close_len + "</a>".len();

        let Some(href) = attr_value(tag, "href") else {
            continue;
        };
        let Some(detail) = resolve_href(&href, &origin) else {
            continue;
        };
        if !host.is_empty() && url_host(&detail) != host {
            continue; // 外站链接（广告位）不算条目
        }
        if !inner.contains("<img") {
            continue; // 没有封面图的锚点多半是导航
        }
        if normalize_url(&detail) == listing_key {
            continue; // 指向本页自身的链接
        }
        if seen.contains(&detail) {
            continue;
        }
        seen.push_back(detail.clone());
        cards.push(ListingCard {
            detail,
            title: extract_title(inner),
            // 封面是异站直链：绝对化即可，不能套用上面的同站过滤
            cover: first_img_src(inner).and_then(|src| resolve_href(&src, &origin)),
        });
    }
    cards
}

/// 取锚点内第一张 `<img>` 的 `src`（卡片封面图地址）。
fn first_img_src(inner: &str) -> Option<String> {
    let at = find_ci(inner, "<img")?;
    let tag_end = at + inner[at..].find('>')?;
    attr_value(&inner[at..=tag_end], "src")
}

/// 取卡片标题：优先「内含 `title` 字样的元素」的文本，其次 `alt` 属性。
fn extract_title(inner: &str) -> Option<String> {
    if let Some(at) = find_ci(inner, "title") {
        if let Some(gt) = inner[at..].find('>') {
            let start = at + gt + 1;
            if let Some(lt) = inner[start..].find('<') {
                let text = decode_entities(inner[start..start + lt].trim());
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    attr_value(inner, "alt")
        .map(|alt| decode_entities(alt.trim()))
        .filter(|alt| !alt.is_empty())
}

/// 取标签属性值，兼容 `name="v"` / `name='v'` / `name=v` 三种写法。
///
/// 只接受**紧跟在分隔符之后**的 `name=`，否则 `data-src=` 里的 `src=` 会被误命中。
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(&needle) {
        let at = from + rel;
        let boundary_ok = lower[..at]
            .chars()
            .last()
            .map(|c| c.is_whitespace() || c == '<')
            .unwrap_or(true);
        if boundary_ok {
            let rest = tag[at + needle.len()..].trim_start();
            let quote = rest.chars().next()?;
            if quote == '"' || quote == '\'' {
                let body = &rest[quote.len_utf8()..];
                let end = body.find(quote)?;
                return Some(body[..end].to_owned());
            }
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(rest.len());
            return Some(rest[..end].to_owned());
        }
        from = at + needle.len();
    }
    None
}

/// 把 `href` 归一到绝对地址：支持 `//host/path`、`/path` 与完整 URL，其余（锚点、
/// 脚本、相对路径）一律丢弃——拿不准的链接宁可不要。
fn resolve_href(href: &str, origin: &str) -> Option<String> {
    let href = decode_entities(href.trim());
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("mailto:") {
        return None;
    }
    if href.starts_with("//") {
        return Some(format!("https:{href}"));
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href);
    }
    if href.starts_with('/') && !origin.is_empty() {
        return Some(format!("{origin}{href}"));
    }
    None
}

/// 取 URL 主机名（小写、去 userinfo 与端口）；无法解析时返回空串。
fn url_host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    host.trim().to_ascii_lowercase()
}

/// 取 URL 的 `scheme://host` 前缀，用于把站内相对链接补成绝对地址。
fn url_origin(url: &str) -> String {
    let scheme = url.split("://").next().unwrap_or("");
    let host = url_host(url);
    if scheme.is_empty() || host.is_empty() {
        String::new()
    } else {
        format!("{scheme}://{host}")
    }
}

/// 解码页面里会影响「标题文本 / 链接参数」的实体：具名若干 + 数字引用。
///
/// 只做必要解码（`&amp;` 与数字引用在列表页最常见），不做完整 HTML 实体表。
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }
        let mut name = String::new();
        while let Some(&next) = chars.peek() {
            if (next.is_ascii_alphanumeric() || next == '#') && name.len() < 10 {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if chars.peek() != Some(&';') {
            // 不是实体：原样写回，避免吞掉正文里孤立的 `&`
            out.push('&');
            out.push_str(&name);
            continue;
        }
        chars.next();
        match name.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" | "#39" => out.push('\''),
            "nbsp" => out.push(' '),
            _ => {
                let code = name
                    .strip_prefix('#')
                    .and_then(|digits| match digits.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => digits.parse::<u32>().ok(),
                    })
                    .and_then(char::from_u32);
                match code {
                    Some(decoded) => out.push(decoded),
                    // 认不出就原样保留，便于在界面上看出原始文本
                    None => {
                        out.push('&');
                        out.push_str(&name);
                        out.push(';');
                    }
                }
            }
        }
    }
    out
}

/// 从 `from` 字节位置开始查找子串。
fn find_from(haystack: &str, from: usize, needle: &str) -> Option<usize> {
    haystack
        .get(from..)
        .and_then(|rest| rest.find(needle))
        .map(|rel| from + rel)
}

/// 大小写不敏感地查找子串，返回原串中的字节位置。
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
}

/// 进程内唯一的临时目录（`--write-pages` 的落盘位置）。
fn unique_temp_dir(tag: &str) -> PathBuf {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()))
}

/// 读取 `--write-pages` 落盘的页面：取目录里最大的 `.dump` 文件。
///
/// 该参数按 URL 派生文件名（含百分号编码等），名字不可预测，因此按扩展名筛选、
/// 取体积最大者——一个目录里只会有本次抓取的那一页。
fn read_page_dump(dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("dump") {
            continue;
        }
        let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
        if best.as_ref().map(|(best_size, _)| size > *best_size).unwrap_or(true) {
            best = Some((size, path));
        }
    }
    let (_, path) = best?;
    std::fs::read(&path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// 文件头三字节是否为 JPEG magic（`FF D8 FF`）。
/// 用「文件头」而不是「文件是否存在」来判断封面是否真的下来了：目标站点的 CDN
/// 在令牌过期 / 被挡时可能返回一张 HTML 或 JSON 错误页，那种「文件」并不是封面，
/// 贴到卡片上就是破图。只读前 3 字节，不把整张图读进内存。
fn jpeg_at(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut head = [0u8; 3];
    file.read_exact(&mut head).is_ok() && head == [0xff, 0xd8, 0xff]
}

// ---------------------------------------------------------------------------
// 纯函数：JSON 解析与字段提取
// ---------------------------------------------------------------------------

/// 从 yt-dlp 输出中取出 JSON。
///
/// yt-dlp 可能在 JSON 前/后打印杂项（如进度行），因此取**第一个 `{` 到最后一个 `}`**
/// 之间的子串再解析；解析失败返回可读错误并附带输出片段（截断到 200 字符）。
fn parse_json(raw: &str) -> Result<serde_json::Value> {
    let slice = match (raw.find('{'), raw.rfind('}')) {
        (Some(start), Some(end)) if start <= end => &raw[start..=end],
        _ => "",
    };
    serde_json::from_str::<serde_json::Value>(slice).map_err(|_| {
        // 领域层没有专门的 Format 变体，解析失败归类为底层失败（Backend）
        PortError::Backend(format!("解析 yt-dlp JSON 失败：{}", snippet(raw.trim(), 200)))
    })
}

/// 取出条目数组。若无 `entries`（或它不是数组），则把整个对象当作单条候选。
fn extract_entries(root: &serde_json::Value) -> Vec<&serde_json::Value> {
    match root.get("entries") {
        Some(entries) => match entries.as_array() {
            Some(array) => array.iter().collect(),
            None => vec![root],
        },
        None => vec![root],
    }
}

/// 取详情页地址：优先 `webpage_url`，否则 `url`，否则 `original_url`；须以 http 开头。
fn detail_url(entry: &serde_json::Value) -> Option<String> {
    for key in ["webpage_url", "url", "original_url"] {
        if let Some(value) = entry.get(key).and_then(|v| v.as_str()) {
            let value = value.trim();
            if value.starts_with("http") {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// 规范化 URL：与 `crates/adapters/src/media_source.rs` 的同名私有函数行为一致
/// （trim + 去掉末尾 `/`），本文件自持一份，不跨文件导出。
fn normalize_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_owned()
}

/// 截断文本到 `limit` 个字符，供诊断信息使用。
fn snippet(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// 影片缓存文件名（含 `.mp4`），按详情页地址确定性命名。
///
/// 与封面同理：同一详情页在任何一次会话里都映射到同一个文件，因此「重复点播放」
/// 天然复用已下载的内容，而不是重下一份。
fn stream_file_name(url: &str) -> String {
    format!("online-{:016x}.mp4", fnv1a_64(url))
}

/// 封面文件名（含 `.jpg`），确定性命名。
fn cover_file_name(url: &str) -> String {
    format!("{}.jpg", cover_stem(url))
}

/// 封面文件主名（不含扩展名），供 yt-dlp `-o` 使用。
fn cover_stem(url: &str) -> String {
    format!("online-{:016x}", fnv1a_64(url))
}

/// FNV-1a 64 位哈希（与 L0 `source_id_for` 同款算法：先小写化再逐字节散列）。
fn fnv1a_64(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// 单测：全部使用 FakeRunner，绝不真的调用 yt-dlp（沙箱内无网络）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::AtomicBool;

    /// 一段「JPEG 头正确」的最小字节串：`fetch_cover_image` 只校验前三字节 magic，
    /// 因此不必造一张真图。
    const JPEG_BYTES: [u8; 4] = [0xff, 0xd8, 0xff, 0xe0];

    /// 替身运行器的一次「调用结局」：直接返回文本，或模拟 `--write-pages`
    /// 把页面写进工作目录后以失败退出，或模拟下载直链图片把字节写到 `-o` 路径。
    enum FakeOut {
        Text(String),
        /// 把页面落到 `cwd/page.dump`，并像 yt-dlp 那样以「Unsupported URL」失败退出。
        Dump(String),
        /// 把给定字节写到 `-o` 指定的路径（模拟 yt-dlp 下载直链图片）。
        Image(Vec<u8>),
    }

    /// 预设「调用 → 结局」队列的替身运行器，并记录每次调用与每次分离下载。
    struct FakeRunner {
        outputs: Mutex<VecDeque<FakeOut>>,
        calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
        spawns: Mutex<Vec<Vec<String>>>,
        /// 起下载时抛错（模拟缺 yt-dlp / 无法建进程）。
        spawn_error: Option<String>,
        /// 每次 `spawn` 配发的开关，测试据此把「下载中」翻成终态。
        knobs: Mutex<Vec<DownloadKnob>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<FakeOut>) -> Self {
            Self {
                outputs: Mutex::new(outputs.into()),
                calls: Mutex::new(Vec::new()),
                spawns: Mutex::new(Vec::new()),
                spawn_error: None,
                knobs: Mutex::new(Vec::new()),
            }
        }

        /// 一次 `run` 都不该发生的场景（例如 `open` 必须零联网）。
        fn never_called() -> Self {
            Self::new(Vec::new())
        }

        fn calls(&self) -> Vec<(PathBuf, Vec<String>)> {
            self.calls.lock().expect("锁不可中毒").clone()
        }

        fn spawns(&self) -> Vec<Vec<String>> {
            self.spawns.lock().expect("锁不可中毒").clone()
        }

        /// 第 `index` 次 `spawn` 配发的开关。
        fn knob(&self, index: usize) -> DownloadKnob {
            self.knobs.lock().expect("锁不可中毒")[index].clone()
        }

        /// 在「起下载」这一步就失败（`spawn` 返回错误）。
        fn failing_spawn(mut self) -> Self {
            self.spawn_error = Some("未找到可执行文件: yt-dlp.exe".into());
            self
        }
    }

    /// 替身下载的遥控开关：测试用它把「下载中」翻成「完成 / 失败」。
    #[derive(Clone, Default)]
    struct DownloadKnob {
        /// 0 = 仍在运行，1 = 正常结束，2 = 失败。
        code: Arc<AtomicU32>,
        /// 是否被 `kill` 过。
        killed: Arc<AtomicBool>,
        /// 失败文本。
        message: Arc<Mutex<String>>,
    }

    impl DownloadKnob {
        fn finish(&self) {
            self.code.store(1, Ordering::SeqCst);
        }

        fn fail(&self, message: &str) {
            *self.message.lock().expect("锁不可中毒") = message.to_owned();
            self.code.store(2, Ordering::SeqCst);
        }

        fn killed(&self) -> bool {
            self.killed.load(Ordering::SeqCst)
        }
    }

    impl DownloadHandle for DownloadKnob {
        fn poll(&mut self) -> Option<DownloadState> {
            match self.code.load(Ordering::SeqCst) {
                1 => Some(DownloadState::Finished),
                2 => Some(DownloadState::Failed(
                    self.message.lock().expect("锁不可中毒").clone(),
                )),
                _ => None,
            }
        }

        fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
        }
    }

    impl ProcessRunner for FakeRunner {
        fn run(&self, program: &Path, args: &[String], cwd: Option<&Path>) -> Result<String> {
            self.calls
                .lock()
                .expect("锁不可中毒")
                .push((program.to_path_buf(), args.to_vec()));
            let mut outputs = self.outputs.lock().expect("锁不可中毒");
            let out = outputs
                .pop_front()
                .ok_or_else(|| PortError::Backend("FakeRunner 预设输出已用尽".into()))?;
            match out {
                FakeOut::Text(text) => Ok(text),
                FakeOut::Dump(html) => {
                    let dir = cwd.expect("模拟 --write-pages 时必须给出工作目录");
                    std::fs::write(dir.join("page.dump"), html).expect("写页面文件");
                    // 站点无 extractor 时 yt-dlp 抓完页面仍以非零退出
                    Err(PortError::Backend("yt-dlp 退出码 1: Unsupported URL".into()))
                }
                FakeOut::Image(bytes) => {
                    let at = args
                        .iter()
                        .position(|a| a == "-o")
                        .expect("模拟下载图片时必须给出 -o");
                    let path = PathBuf::from(&args[at + 1]);
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    std::fs::write(path, &bytes).expect("写图片文件");
                    Ok(String::new())
                }
            }
        }

        fn spawn(&self, _program: &Path, args: &[String], _log: &Path) -> Result<Box<dyn DownloadHandle>> {
            if let Some(message) = &self.spawn_error {
                return Err(PortError::Backend(message.clone()));
            }
            self.spawns
                .lock()
                .expect("锁不可中毒")
                .push(args.to_vec());
            let knob = DownloadKnob::default();
            self.knobs.lock().expect("锁不可中毒").push(knob.clone());
            Ok(Box::new(knob))
        }
    }

    /// 收集被提交的候选项，可预设取消态。
    struct CollectSink {
        items: Mutex<Vec<DiscoveredItem>>,
        cancelled: AtomicBool,
    }

    impl CollectSink {
        fn new() -> Self {
            Self {
                items: Mutex::new(Vec::new()),
                cancelled: AtomicBool::new(false),
            }
        }

        fn items(&self) -> Vec<DiscoveredItem> {
            self.items.lock().expect("锁不可中毒").clone()
        }
    }

    impl DiscoverSink for CollectSink {
        fn submit(&self, item: DiscoveredItem) -> Result<()> {
            self.items.lock().expect("锁不可中毒").push(item);
            Ok(())
        }

        fn is_cancelled(&self) -> bool {
            self.cancelled.load(Ordering::SeqCst)
        }
    }

    /// 建一个进程内唯一且已存在的临时目录（用于海报缓存目录）。
    fn unique_dir(tag: &str) -> PathBuf {
        let dir = unique_temp_dir(&format!("cinema-online-{tag}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn online_source(location: &str) -> MediaSource {
        MediaSource {
            id: cinema_domain::model::SourceId::from("src-online"),
            kind: SourceKind::OnlineSite,
            name: "在线站点".into(),
            location: location.into(),
            exclude_rules: None,
            state: cinema_domain::model::SourceState::Idle,
            last_scan_at: None,
        }
    }

    /// 以替身运行器构造提供者。传入 `Arc` 是为了让测试仍能查回调用记录。
    fn provider(runner: Arc<FakeRunner>, covers: PathBuf) -> OnlineSiteProvider {
        provider_with(runner, covers, unique_dir("streams"), Arc::new(DownloadRegistry::new()))
    }

    /// 构造提供者并显式给出影片缓存目录与下载登记表（下载类测试要能查回它们）。
    fn provider_with(
        runner: Arc<FakeRunner>,
        covers: PathBuf,
        streams: PathBuf,
        downloads: Arc<DownloadRegistry>,
    ) -> OnlineSiteProvider {
        OnlineSiteProvider::with_runner(
            PathBuf::from("yt-dlp.exe"),
            covers,
            streams,
            downloads,
            runner,
        )
    }

    /// 造一个带详情页地址的在线条目。
    fn online_item(detail: &str) -> MediaItem {
        let mut item = MediaItem::pending_fill("it-1", "src-online", "样本");
        item.url = Some(detail.into());
        item
    }

    /// 真实站点的卡片结构（含一个外站广告卡与一个无图导航链接，二者都必须被滤掉）。
    const LISTING_HTML: &str = r#"
      <ul class="pagination"><li><a class="page-link" href="?genre=x&amp;page=2">2</a></li></ul>
      <a href="https://l.erodalabs.com/s/7duYXu" target="_blank">
        <div class="home-rows-videos-div"><img src="https://cdn.test/ad.jpg"></div>
      </a>
      <a style="text-decoration: none;" href="https://site.test/watch?v=408297" >
        <div class="video-card-inner">
          <img loading="lazy" src="https://cdn.test/image/cover/408297.jpg?secure=abc">
          <div class="home-rows-videos-title"> 少女彈珠汽水 8 </div>
        </div>
      </a>
      <a href="https://site.test/watch?v=408310">
        <div class="video-card-inner">
          <img src="https://cdn.test/image/cover/408310.jpg">
          <div class="home-rows-videos-title">A &amp; B 第2話</div>
        </div>
      </a>
    "#;

    #[test]
    fn discover_parses_flat_playlist_into_candidates() {
        let json = r#"{"_type":"playlist","entries":[
            {"title":"第一集","webpage_url":"https://site.test/ep1/","url":"https://site.test/ep1/"},
            {"title":"第二集","webpage_url":"https://site.test/ep2"}
        ]}"#;
        let provider = provider(
            Arc::new(FakeRunner::new(vec![FakeOut::Text(json.into())])),
            unique_dir("playlist"),
        );
        let sink = CollectSink::new();

        let stat = provider
            .discover(&online_source("https://site.test/list"), &sink)
            .expect("列表页发现应成功");

        assert_eq!(stat.discovered, 2, "两个 entry 应产出两个候选");
        let items = sink.items();
        assert_eq!(items.len(), 2);
        for item in &items {
            let poster = item.poster_ref.as_deref().expect("在线条目应登记封面文件名");
            assert!(poster.starts_with("online-"), "封面文件名必须有 online- 前缀");
            assert!(poster.ends_with(".jpg"), "封面文件名必须以 .jpg 结尾");
            assert!(
                item.stable_key.starts_with("src-online"),
                "稳定键必须以源 id 开头"
            );
        }
        assert_eq!(
            items[0].stable_key, "src-online|https://site.test/ep1",
            "稳定键应使用规范化后的详情页地址（去掉末尾 /）"
        );
        assert_eq!(items[0].url.as_deref(), Some("https://site.test/ep1/"));
    }

    #[test]
    fn discover_parses_single_video_object() {
        let json = r#"{"_type":"video","title":"独条","webpage_url":"https://site.test/solo"}"#;
        let provider = provider(
            Arc::new(FakeRunner::new(vec![FakeOut::Text(json.into())])),
            unique_dir("single"),
        );
        let sink = CollectSink::new();

        let stat = provider
            .discover(&online_source("https://site.test/solo"), &sink)
            .expect("单条视频页发现应成功");

        assert_eq!(stat.discovered, 1, "无 entries 时应按单条处理");
        assert_eq!(
            sink.items()[0].title_hint.as_deref(),
            Some("独条"),
            "标题应取自顶层 title"
        );
    }

    #[test]
    fn discover_falls_back_to_listing_html_when_ytdlp_has_no_extractor() {
        // 第 1 次调用：yt-dlp 不认识该站点，返回非 JSON；第 2 次：落盘页面后失败退出
        let runner = Arc::new(FakeRunner::new(vec![
            FakeOut::Text("ERROR: Unsupported URL".into()),
            FakeOut::Dump(LISTING_HTML.into()),
        ]));
        let provider = provider(runner, unique_dir("fallback"));
        let sink = CollectSink::new();

        let stat = provider
            .discover(&online_source("https://site.test/search?genre=x"), &sink)
            .expect("无 extractor 的站点应回落到页面解析");

        assert_eq!(stat.discovered, 2, "只应采纳两个站内视频卡片");
        let items = sink.items();
        assert_eq!(items[0].url.as_deref(), Some("https://site.test/watch?v=408297"));
        assert_eq!(
            items[0].title_hint.as_deref(),
            Some("少女彈珠汽水 8"),
            "标题应取自卡片内 title 元素并去掉首尾空白"
        );
        assert_eq!(
            items[1].title_hint.as_deref(),
            Some("A & B 第2話"),
            "标题里的 HTML 实体应被解码"
        );
    }

    #[test]
    fn listing_parse_skips_foreign_and_imageless_links() {
        let cards = parse_cards(LISTING_HTML, "https://site.test/search?genre=x");
        let details: Vec<&str> = cards.iter().map(|c| c.detail.as_str()).collect();
        assert_eq!(
            details,
            vec![
                "https://site.test/watch?v=408297",
                "https://site.test/watch?v=408310"
            ],
            "外站广告与无封面图的翻页链接都不得进入候选"
        );
    }

    #[test]
    fn discover_reports_error_when_both_paths_fail() {
        // 两条路径都拿不到东西：结构化枚举解析失败、页面也没落盘
        let runner = Arc::new(FakeRunner::new(vec![
            FakeOut::Text("no json here at all".into()),
            FakeOut::Text("".into()),
        ]));
        let provider = provider(runner, unique_dir("both-fail"));
        let sink = CollectSink::new();

        let err = provider
            .discover(&online_source("https://site.test/list"), &sink)
            .expect_err("两条路径都失败时必须报错");

        assert!(
            err.reason().contains("未能获取列表页内容"),
            "错误应说明列表页没抓到，实际为：{}",
            err.reason()
        );
    }

    #[test]
    fn open_returns_local_cache_path_and_starts_a_download_without_any_foreground_call() {
        let detail = "https://site.test/movie";
        // 目录先建出来，好让测试能算出期望的缓存路径
        let streams = unique_dir("open-streams");
        let runner = Arc::new(FakeRunner::never_called());
        let downloads = Arc::new(DownloadRegistry::new());
        let provider = provider_with(
            runner.clone(),
            unique_dir("open-covers"),
            streams.clone(),
            Arc::clone(&downloads),
        );

        let ref_ = provider.open(&online_item(detail)).expect("起下载应成功");

        let expected = streams.join(stream_file_name(detail));
        assert_eq!(
            ref_.location,
            expected.to_string_lossy(),
            "open 必须返回本地缓存文件路径（不是远端直链）"
        );
        assert!(ref_.headers.is_empty(), "读本地文件不需要防盗链请求头");
        assert!(
            runner.calls().is_empty(),
            "open 必须零联网：任何前台 yt-dlp 调用都会阻塞主线程（卡死根因）"
        );

        let spawns = runner.spawns();
        assert_eq!(spawns.len(), 1, "文件不存在时应恰好起一个分离下载");
        let args = &spawns[0];
        for flag in ["--impersonate", "--no-part", "--continue"] {
            assert!(args.iter().any(|a| a == flag), "下载参数缺少 {flag}：{args:?}");
        }
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--impersonate" && w[1] == "chrome"),
            "必须带 --impersonate chrome，否则目标站点 403：{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "-o" && w[1] == expected.to_string_lossy()),
            "下载目标必须是缓存文件：{args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some(detail), "末位是详情页地址");

        assert_eq!(
            downloads.state(&expected),
            DownloadState::Running,
            "下载应已登记为运行中"
        );
    }

    #[test]
    fn open_reuses_a_running_download_instead_of_starting_a_second_one() {
        let detail = "https://site.test/movie";
        let runner = Arc::new(FakeRunner::never_called());
        let provider = provider(runner.clone(), unique_dir("reuse-running"));

        provider.open(&online_item(detail)).expect("首次起下载");
        provider.open(&online_item(detail)).expect("再次打开同一影片");

        assert_eq!(runner.spawns().len(), 1, "同一条目重复点播放不得重复起下载");

        // 下载完成后再次打开：仍复用同一份缓存文件，不得重新下载
        runner.knob(0).finish();
        provider.open(&online_item(detail)).expect("下载完成后再次打开");
        assert_eq!(runner.spawns().len(), 1, "已完成的缓存文件必须复用");
    }

    #[test]
    fn open_reuses_a_completed_cache_file_and_probe_stays_offline() {
        let detail = "https://site.test/movie";
        let streams = unique_dir("reuse-cache");
        let cached = streams.join(stream_file_name(detail));
        std::fs::write(&cached, b"mp4").expect("造一个已下载好的缓存文件");

        let runner = Arc::new(FakeRunner::never_called());
        let provider = provider_with(
            runner.clone(),
            unique_dir("reuse-cache-covers"),
            streams,
            Arc::new(DownloadRegistry::new()),
        );

        let ref_ = provider.open(&online_item(detail)).expect("复用缓存应成功");
        assert_eq!(ref_.location, cached.to_string_lossy());
        assert!(runner.spawns().is_empty(), "缓存已完整时不得再起下载");

        let facts = provider.probe(&ref_).expect("probe 不得报错");
        assert_eq!(facts.duration_ms, None, "在线源 probe 零开销，时长由播放内核读出");
        assert!(runner.calls().is_empty(), "probe 也不得联网");
    }

    #[test]
    fn open_restarts_the_download_after_a_failure() {
        let detail = "https://site.test/movie";
        let streams = unique_dir("retry");
        let runner = Arc::new(FakeRunner::never_called());
        let downloads = Arc::new(DownloadRegistry::new());
        let provider = provider_with(
            runner.clone(),
            unique_dir("retry-covers"),
            streams.clone(),
            Arc::clone(&downloads),
        );

        provider.open(&online_item(detail)).expect("首次起下载");
        runner.knob(0).fail("ERROR: 站点不可达");
        let expected = streams.join(stream_file_name(detail));
        assert_eq!(
            downloads.state(&expected),
            DownloadState::Failed("ERROR: 站点不可达".into()),
            "失败态应可被组合根读到（供错误卡显示）"
        );

        // 用户点「重试」：再次 open 必须重新起下载（`--continue` 会接着已下部分）
        provider.open(&online_item(detail)).expect("重试起下载");
        assert_eq!(runner.spawns().len(), 2, "失败后重试应真的再起一次");
        assert_eq!(downloads.state(&expected), DownloadState::Running);
    }

    #[test]
    fn stop_all_terminates_running_downloads() {
        let detail = "https://site.test/movie";
        let streams = unique_dir("stop-all");
        let runner = Arc::new(FakeRunner::never_called());
        let downloads = Arc::new(DownloadRegistry::new());
        let provider = provider_with(
            runner.clone(),
            unique_dir("stop-all-covers"),
            streams.clone(),
            Arc::clone(&downloads),
        );

        provider.open(&online_item(detail)).expect("起下载");
        let expected = streams.join(stream_file_name(detail));
        assert_eq!(downloads.state(&expected), DownloadState::Running);

        downloads.stop_all();

        assert!(runner.knob(0).killed(), "关闭播放后必须杀掉下载子进程（不得留孤儿）");
        assert_eq!(
            downloads.state(&expected),
            DownloadState::Unknown,
            "收尾后登记表应被清空"
        );
    }

    #[test]
    fn open_reports_rather_than_swallows_a_missing_ytdlp() {
        let runner = Arc::new(FakeRunner::never_called().failing_spawn());
        let provider = provider(runner.clone(), unique_dir("no-ytdlp"));

        let err = provider
            .open(&online_item("https://site.test/movie"))
            .expect_err("起下载失败必须报错，而不是给一个播不了的空引用");

        assert!(
            err.reason().contains("yt-dlp"),
            "错误应说明 yt-dlp 不可用，实际为：{}",
            err.reason()
        );
    }

    #[test]
    fn listing_parse_captures_the_card_cover_art() {
        let cards = parse_cards(LISTING_HTML, "https://site.test/search?genre=x");
        let covers: Vec<Option<&str>> = cards.iter().map(|c| c.cover.as_deref()).collect();
        assert_eq!(
            covers,
            vec![
                Some("https://cdn.test/image/cover/408297.jpg?secure=abc"),
                Some("https://cdn.test/image/cover/408310.jpg"),
            ],
            "封面必须取自卡片 <img src>，且是异站 CDN 直链（不做同站过滤）"
        );
    }

    #[test]
    fn fetch_covers_downloads_each_card_cover_and_skips_ones_already_on_disk() {
        let covers = unique_dir("fetch-covers");
        let listing = "https://site.test/search?genre=x";
        let runner = Arc::new(FakeRunner::new(vec![
            FakeOut::Dump(LISTING_HTML.into()),
            FakeOut::Image(JPEG_BYTES.to_vec()),
            FakeOut::Image(JPEG_BYTES.to_vec()),
        ]));
        let provider = provider(runner.clone(), covers.clone());

        assert_eq!(provider.fetch_covers(listing), 2, "两张站内卡片封面都应下载");
        for detail in ["https://site.test/watch?v=408297", "https://site.test/watch?v=408310"] {
            assert!(
                covers.join(cover_file_name(detail)).is_file(),
                "封面应落到 discover 阶段登记的同一文件名：{detail}"
            );
        }

        // 再抓一遍：封面文件已在，除列表页外不得再下任何封面
        runner
            .outputs
            .lock()
            .expect("锁不可中毒")
            .push_back(FakeOut::Dump(LISTING_HTML.into()));
        assert_eq!(provider.fetch_covers(listing), 0, "封面已存在时不得重复下载");
        let image_calls = runner
            .calls()
            .iter()
            .filter(|(_, args)| args.iter().any(|a| a == "-o"))
            .count();
        assert_eq!(image_calls, 2, "两轮合计只应下载过两张封面");
    }

    #[test]
    fn fetch_cover_image_writes_a_jpeg_to_the_registered_file_name() {
        let covers = unique_dir("cover-image");
        let url = "https://cdn.test/image/cover/408297.jpg?secure=abc";
        let target = covers.join("online-abcd.jpg");
        let runner = Arc::new(FakeRunner::new(vec![FakeOut::Image(JPEG_BYTES.to_vec())]));
        let provider = provider(runner.clone(), covers.clone());

        assert!(provider.fetch_cover_image(url, "online-abcd.jpg"), "落盘的 JPEG 应被接受");
        assert_eq!(
            std::fs::read(&target).expect("封面文件应存在"),
            JPEG_BYTES.to_vec(),
            "-o 指定的文件必须原样落盘（不额外补扩展名）"
        );

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "一张封面只应调一次 yt-dlp");
        let args = &calls[0].1;
        assert!(
            args.windows(2).any(|w| w[0] == "--impersonate" && w[1] == "chrome"),
            "直链图片下载同样要指纹伪装，否则站点 CDN 403：{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w[0] == "-o" && w[1] == target.to_string_lossy()),
            "-o 必须是登记的文件路径：{args:?}"
        );
        assert_eq!(args.last().map(String::as_str), Some(url), "末位是封面直链");
    }

    #[test]
    fn fetch_cover_image_rejects_and_removes_a_non_jpeg_payload() {
        let covers = unique_dir("cover-bad");
        // 令牌过期 / 被挡时 CDN 会返回 HTML 错误页：那种「文件」不是封面，必须删掉
        let runner = Arc::new(FakeRunner::new(vec![FakeOut::Image(b"<html>403</html>".to_vec())]));
        let provider = provider(runner, covers.clone());

        assert!(
            !provider.fetch_cover_image("https://cdn.test/x.jpg", "online-bad.jpg"),
            "非 JPEG 的内容不得算作封面"
        );
        assert!(
            !covers.join("online-bad.jpg").exists(),
            "非 JPEG 的半成品必须被删除，避免界面出现破图"
        );
    }
}
