//! 从视频文件按时间点抽帧（详情页截图补足用）。
//!
//! 复用 libmpv 自带的 `image` 输出：每个时间点起一个 headless 实例，
//! `vo=image` + `frames=1` + `start=<秒>`，mpv 把该帧写成一张 jpg。
//! 运行时不依赖 ffmpeg（部署目录里只有 libmpv-2.dll / yt-dlp）。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cinema_domain::error::{PortError, Result};

use crate::mpv::{MpvInstance, MpvLib};

/// 单帧抽取最多等这么久：大文件 seek + 解码器冷启动偶尔要几秒。
const FRAME_TIMEOUT: Duration = Duration::from_secs(20);

/// 抽一帧，返回落盘的图片路径（在 `work_dir` 下）。
///
/// `work_dir` 应当是**本帧独占**的临时目录：mpv 的 `image` 输出固定写成
/// `out00000001.jpg`，共用一个目录会互相覆盖。
pub fn extract_frame(video: &Path, at_secs: f64, work_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(work_dir)
        .map_err(|e| PortError::Backend(format!("创建抽帧目录失败：{e}")))?;
    let out_dir = work_dir.to_string_lossy().into_owned();
    let start = format!("{:.3}", at_secs.max(0.0));

    let inst = MpvInstance::new(MpvLib::shared()?)?;
    for (name, value) in [
        ("config", "no".to_owned()),
        ("idle", "yes".to_owned()),
        ("vo", "image".to_owned()),
        ("vo-image-outdir", out_dir),
        ("vo-image-format", "jpg".to_owned()),
        ("vo-image-jpeg-quality", "90".to_owned()),
        ("ao", "null".to_owned()),
        // 只输出这一帧，写完就停：否则 image 输出会把后续帧也全写成图
        ("frames", "1".to_owned()),
        ("start", start),
    ] {
        inst.set_option(name, &value)?;
    }
    inst.initialize()?;
    inst.command(&["loadfile", &video.to_string_lossy(), "replace"])?;

    let deadline = Instant::now() + FRAME_TIMEOUT;
    let mut started = false;
    let mut frame: Option<PathBuf> = None;
    while Instant::now() < deadline {
        if let Some(path) = first_image(work_dir) {
            frame = Some(path);
            break;
        }
        // 文件打不开时 mpv 会回到空闲态：此时再等也没有结果
        let idle = inst.get_flag("idle-active").unwrap_or(false);
        if idle {
            if started {
                break;
            }
        } else {
            started = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 先销毁实例再读文件：确保 mpv 已经把 jpg 完整刷盘
    drop(inst);
    match frame {
        Some(path) => Ok(path),
        None => Err(PortError::Backend(format!(
            "从 {} 抽取 {:.1}s 画面失败",
            video.display(),
            at_secs
        ))),
    }
}

/// `work_dir` 下第一张已写出的图片（mpv 写完才会出现文件）。
fn first_image(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);
        if matches!(ext.as_deref(), Some("jpg") | Some("jpeg") | Some("png")) {
            return Some(path);
        }
    }
    None
}

/// 在 `[8%, 92%]` 的时长区间里按 `need` 等分，每段取一个随机时刻，返回升序秒数。
///
/// 等分保证截图在时间轴上铺开、彼此不重叠；段内随机保证每次补图的画面不同。
pub fn random_times(duration_secs: f64, need: usize) -> Vec<f64> {
    if need == 0 || !duration_secs.is_finite() || duration_secs <= 1.0 {
        return Vec::new();
    }
    let from = duration_secs * 0.08;
    let span = duration_secs * 0.84;
    let mut seed = seed();
    let mut times = Vec::with_capacity(need);
    for index in 0..need {
        let seg_from = from + span * index as f64 / need as f64;
        let seg_to = from + span * (index + 1) as f64 / need as f64;
        seed = xorshift(seed);
        let unit = (seed % 1_000_000) as f64 / 1_000_000.0;
        times.push(seg_from + (seg_to - seg_from) * unit);
    }
    times
}

/// 简易随机种子：时间纳秒 + 进程内自增，避免引入 rand 依赖。
fn seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn xorshift(mut state: u64) -> u64 {
    if state == 0 {
        state = 0x2545_F491_4F6C_DD1D;
    }
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

#[cfg(test)]
mod tests {
    use super::random_times;

    #[test]
    fn random_times_are_sorted_and_inside_bounds() {
        let times = random_times(600.0, 9);
        assert_eq!(times.len(), 9);
        assert!(times.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(times[0] >= 48.0);
        assert!(*times.last().unwrap() <= 552.0);
    }

    #[test]
    fn random_times_need_duration() {
        assert!(random_times(0.0, 3).is_empty());
        assert!(random_times(600.0, 0).is_empty());
    }
}
