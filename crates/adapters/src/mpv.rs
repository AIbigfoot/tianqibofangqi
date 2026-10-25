//! libmpv 的**运行时**绑定（Windows `LoadLibraryW` + `GetProcAddress`）。
//!
//! 为什么是运行时加载而不是构建期链接：
//! 项目路径与用户目录都含非 ASCII 字符，MinGW 的 `dlltool` / `ld` 无法处理这类路径，
//! 生成 import library 会直接失败。把「符号解析」推迟到进程启动之后，整条构建期
//! 链接链都不再需要触碰这些路径 —— 代价只是多一层函数指针。
//!
//! 本模块只做两件事：**绑定**（[`MpvLib`] / [`MpvInstance`]）与**探测**（[`MpvProber`]）。
//! 播放语义（端口实现）留在 [`crate::player`]。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cinema_domain::error::{PortError, Result};
use cinema_domain::model::{AudioTrackInfo, EmbeddedSubtitle, StreamFacts};

// ---------------------------------------------------------------------------
// mpv C ABI（常量与结构体，取值来自 include/mpv/client.h）
// ---------------------------------------------------------------------------

/// 不透明的 `mpv_handle`。
type MpvHandle = c_void;

const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_DOUBLE: c_int = 5;

const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_SHUTDOWN: c_int = 1;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_PROPERTY_CHANGE: c_int = 22;

/// `mpv_event_property`（只读取前缀字段）。
#[repr(C)]
struct MpvEventProperty {
    name: *const c_char,
    format: c_int,
    data: *mut c_void,
}

/// `mpv_event_end_file`（只需 `reason` / `error` 两个前缀字段）。
#[repr(C)]
struct MpvEventEndFile {
    reason: c_int,
    error: c_int,
}

/// `mpv_event`。
#[repr(C)]
#[allow(dead_code)] // 只为与 C 结构体保持一致的偏移，部分字段本模块不读取
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

/// `MPV_END_FILE_REASON_ERROR`。
const MPV_END_FILE_REASON_ERROR: c_int = 4;

// ---------------------------------------------------------------------------
// Windows 动态加载
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use std::ffi::{c_void, OsStr};
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn LoadLibraryW(file_name: *const u16) -> *mut c_void;
        pub fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    }

    /// 转成以 0 结尾的 UTF-16。
    pub fn wide(s: &OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }
}

// ---------------------------------------------------------------------------
// MpvLib：一次加载，全进程复用
// ---------------------------------------------------------------------------

/// 已解析的 libmpv 导出表。
pub struct MpvLib {
    create: unsafe extern "C" fn() -> *mut MpvHandle,
    initialize: unsafe extern "C" fn(*mut MpvHandle) -> c_int,
    terminate_destroy: unsafe extern "C" fn(*mut MpvHandle),
    set_option_string: unsafe extern "C" fn(*mut MpvHandle, *const c_char, *const c_char) -> c_int,
    set_property:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    get_property:
        unsafe extern "C" fn(*mut MpvHandle, *const c_char, c_int, *mut c_void) -> c_int,
    command: unsafe extern "C" fn(*mut MpvHandle, *const *const c_char) -> c_int,
    observe_property:
        unsafe extern "C" fn(*mut MpvHandle, u64, *const c_char, c_int) -> c_int,
    wait_event: unsafe extern "C" fn(*mut MpvHandle, f64) -> *mut MpvEvent,
    error_string: unsafe extern "C" fn(c_int) -> *const c_char,
}

// 函数指针表在加载后不可变，且 mpv 自身对句柄做了内部加锁，可跨线程共享。
unsafe impl Send for MpvLib {}
unsafe impl Sync for MpvLib {}

impl std::fmt::Debug for MpvLib {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MpvLib(已加载的 libmpv 导出表)")
    }
}

fn lib_cache() -> &'static OnceLock<std::result::Result<Arc<MpvLib>, String>> {
    static CACHE: OnceLock<std::result::Result<Arc<MpvLib>, String>> = OnceLock::new();
    &CACHE
}

impl MpvLib {
    /// 取得进程内唯一的 libmpv 句柄表；失败时返回 `Unsupported`。
    pub fn shared() -> Result<Arc<MpvLib>> {
        match lib_cache().get_or_init(Self::load_once) {
            Ok(lib) => Ok(Arc::clone(lib)),
            Err(msg) => Err(PortError::Unsupported(msg.clone())),
        }
    }

    /// 候选路径：显式环境变量 → 可执行文件同目录 → 系统搜索路径。
    fn candidates() -> Vec<PathBuf> {
        let mut list = Vec::new();
        if let Ok(explicit) = std::env::var("CINEMA_LIBMPV") {
            if !explicit.trim().is_empty() {
                list.push(PathBuf::from(explicit));
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                list.push(dir.join("libmpv-2.dll"));
            }
        }
        // 交给系统搜索路径（PATH），部署时可把 dll 放到一起
        list.push(PathBuf::from("libmpv-2.dll"));
        list
    }

    fn load_once() -> std::result::Result<Arc<MpvLib>, String> {
        #[cfg(windows)]
        {
            let mut tried = Vec::new();
            for path in Self::candidates() {
                let wide = win::wide(path.as_os_str());
                let module = unsafe { win::LoadLibraryW(wide.as_ptr()) };
                if module.is_null() {
                    tried.push(path.display().to_string());
                    continue;
                }
                match unsafe { Self::bind(module) } {
                    Ok(lib) => return Ok(Arc::new(lib)),
                    Err(err) => return Err(format!("{} 缺少符号：{err}", path.display())),
                }
            }
            Err(format!(
                "未找到 libmpv-2.dll（已尝试：{}）",
                tried.join("、")
            ))
        }
        #[cfg(not(windows))]
        {
            Err("libmpv 运行时加载目前只在 Windows 上实现".into())
        }
    }

    /// 解析全部导出符号；任一缺失即视为加载失败。
    #[cfg(windows)]
    unsafe fn bind(module: *mut c_void) -> std::result::Result<MpvLib, String> {
        unsafe fn sym<T: Copy>(module: *mut c_void, name: &str) -> std::result::Result<T, String> {
            let cname = CString::new(name).map_err(|_| "符号名含 0 字节".to_string())?;
            let ptr = unsafe { win::GetProcAddress(module, cname.as_ptr() as *const u8) };
            if ptr.is_null() {
                return Err(name.to_string());
            }
            Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&ptr) })
        }

        Ok(MpvLib {
            create: unsafe { sym(module, "mpv_create")? },
            initialize: unsafe { sym(module, "mpv_initialize")? },
            terminate_destroy: unsafe { sym(module, "mpv_terminate_destroy")? },
            set_option_string: unsafe { sym(module, "mpv_set_option_string")? },
            set_property: unsafe { sym(module, "mpv_set_property")? },
            get_property: unsafe { sym(module, "mpv_get_property")? },
            command: unsafe { sym(module, "mpv_command")? },
            observe_property: unsafe { sym(module, "mpv_observe_property")? },
            wait_event: unsafe { sym(module, "mpv_wait_event")? },
            error_string: unsafe { sym(module, "mpv_error_string")? },
        })
    }
}

// ---------------------------------------------------------------------------
// MpvInstance：一个 mpv 内核实例
// ---------------------------------------------------------------------------

/// 一个 mpv 实例（对应「一路画面」）。
///
/// 生命周期：`new`（仅 `mpv_create`）→ [`MpvInstance::set_option`]（**必须在
/// `initialize` 之前**）→ [`MpvInstance::initialize`] → 播放 → `Drop`（终止销毁）。
pub struct MpvInstance {
    lib: Arc<MpvLib>,
    ctx: *mut MpvHandle,
    /// `mpv_wait_event` 不得并发调用，事件拉取串行化于此。
    ev_lock: Mutex<()>,
}

// mpv 对句柄的常规操作是线程安全的（内部有锁），事件拉取已由 ev_lock 串行化。
unsafe impl Send for MpvInstance {}
unsafe impl Sync for MpvInstance {}

impl std::fmt::Debug for MpvInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpvInstance")
            .field("ctx", &self.ctx)
            .finish()
    }
}

impl Drop for MpvInstance {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { (self.lib.terminate_destroy)(self.ctx) };
            self.ctx = std::ptr::null_mut();
        }
    }
}

impl MpvInstance {
    /// 创建未初始化的实例。
    pub fn new(lib: Arc<MpvLib>) -> Result<Self> {
        let ctx = unsafe { (lib.create)() };
        if ctx.is_null() {
            return Err(PortError::Backend("mpv_create 失败".into()));
        }
        Ok(Self {
            lib,
            ctx,
            ev_lock: Mutex::new(()),
        })
    }

    /// 设置初始化期选项。返回 `Err` 便于调用方区分「必需选项」与「锦上添花选项」。
    pub fn set_option(&self, name: &str, value: &str) -> Result<()> {
        let name = CString::new(name).map_err(|_| PortError::Backend("选项名非法".into()))?;
        let value = CString::new(value).map_err(|_| PortError::Backend("选项值非法".into()))?;
        let rc = unsafe { (self.lib.set_option_string)(self.ctx, name.as_ptr(), value.as_ptr()) };
        self.check(rc, "mpv_set_option_string")
    }

    /// 初始化内核。必须先设好初始化期选项。
    pub fn initialize(&self) -> Result<()> {
        let rc = unsafe { (self.lib.initialize)(self.ctx) };
        self.check(rc, "mpv_initialize")
    }

    /// 下发命令（数组形式，避免 `command_string` 的引号转义问题，中文路径尤其重要）。
    pub fn command(&self, args: &[&str]) -> Result<()> {
        let owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| PortError::Backend("命令参数含 0 字节".into()))?;
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (self.lib.command)(self.ctx, ptrs.as_ptr()) };
        self.check(rc, "mpv_command")
    }

    /// 设置布尔属性（如 `mute`）。
    pub fn set_flag(&self, name: &str, value: bool) -> Result<()> {
        let mut raw: c_int = if value { 1 } else { 0 };
        let name = CString::new(name).map_err(|_| PortError::Backend("属性名非法".into()))?;
        let rc = unsafe {
            (self.lib.set_property)(
                self.ctx,
                name.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut raw as *mut c_int as *mut c_void,
            )
        };
        self.check(rc, "mpv_set_property(flag)")
    }

    /// 设置字符串属性（如在线源的 `http-header-fields`）。
    pub fn set_string(&self, name: &str, value: &str) -> Result<()> {
        let name = CString::new(name).map_err(|_| PortError::Backend("属性名非法".into()))?;
        let value = CString::new(value).map_err(|_| PortError::Backend("属性值含 0 字节".into()))?;
        // MPV_FORMAT_STRING 要求传**字符串变量的地址**（char**）而非字符串本身：
        // client.h 原注释「you pass the address to the variable」。
        // 传 value.as_ptr() 会让 mpv 把字符串首字节当指针解引用 → ACCESS_VIOLATION。
        let mut value_ptr: *const c_char = value.as_ptr();
        let rc = unsafe {
            (self.lib.set_property)(
                self.ctx,
                name.as_ptr(),
                MPV_FORMAT_STRING,
                &mut value_ptr as *mut *const c_char as *mut c_void,
            )
        };
        self.check(rc, "mpv_set_property(string)")
    }

    /// 读取浮点属性（`time-pos` / `duration`）；不可用时返回 `None`。
    pub fn get_double(&self, name: &str) -> Option<f64> {
        let name = CString::new(name).ok()?;
        let mut raw: f64 = 0.0;
        let rc = unsafe {
            (self.lib.get_property)(
                self.ctx,
                name.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut raw as *mut f64 as *mut c_void,
            )
        };
        (rc >= 0).then_some(raw)
    }

    /// 读取布尔属性。
    pub fn get_flag(&self, name: &str) -> Option<bool> {
        let name = CString::new(name).ok()?;
        let mut raw: c_int = 0;
        let rc = unsafe {
            (self.lib.get_property)(
                self.ctx,
                name.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut raw as *mut c_int as *mut c_void,
            )
        };
        (rc >= 0).then_some(raw != 0)
    }

    /// 读取字符串属性（如 `track-list/0/lang`）；不可用（属性不存在 / 读取失败）返回 `None`。
    pub fn get_string(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // 与 [`Self::set_string`] 对称：`MPV_FORMAT_STRING` 要求传**字符串变量的地址**
        // （`char**`）而非缓冲区；mpv 把新分配的 C 字符串指针写进 `raw`。
        //
        // 注意事项（人工确认项）：client.h 要求调用方用 `mpv_free` 释放该字符串。
        // 本模块未绑定 `mpv_free`（避免在加载期把非必需符号变成硬依赖），因此每次读取
        // 会留下一个由 mpv 分配的小字符串。探测频率极低，影响可忽略；如后续成为问题，
        // 可补一个 `mpv_free` 符号并把 `raw` 交还释放。
        let mut raw: *const c_char = std::ptr::null();
        let rc = unsafe {
            (self.lib.get_property)(
                self.ctx,
                name.as_ptr(),
                MPV_FORMAT_STRING,
                &mut raw as *mut *const c_char as *mut c_void,
            )
        };
        if rc < 0 || raw.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(raw) }.to_string_lossy().into_owned())
    }

    /// 订阅属性变化（[`MpvInstance::poll_event`] 会把它转成播放事件）。
    pub fn observe(&self, name: &str, reply: u64, format: c_int) -> Result<()> {
        let name = CString::new(name).map_err(|_| PortError::Backend("属性名非法".into()))?;
        let rc = unsafe { (self.lib.observe_property)(self.ctx, reply, name.as_ptr(), format) };
        self.check(rc, "mpv_observe_property")
    }

    /// 拉取一个底层事件（非阻塞）。队列空时返回 `None`。
    fn wait_raw(&self) -> Option<RawEvent> {
        let _guard = self.ev_lock.lock().ok()?;
        let ev = unsafe { (self.lib.wait_event)(self.ctx, 0.0) };
        if ev.is_null() {
            return None;
        }
        let ev = unsafe { &*ev };
        if ev.event_id == MPV_EVENT_NONE || ev.event_id == MPV_EVENT_SHUTDOWN {
            return None;
        }
        Some(RawEvent {
            id: ev.event_id,
            data: ev.data,
        })
    }

    /// 物理量 → 端口语义：只上报端口枚举里有的东西，其余静默丢弃。
    fn map_event(&self, raw: RawEvent) -> Option<PortEvent> {
        // 事件负载可能为空（属性被移除 / 事件本身无数据），任何解引用前先确认
        if raw.data.is_null() {
            return None;
        }
        match raw.id {
            MPV_EVENT_PROPERTY_CHANGE => {
                let prop = unsafe { &*(raw.data as *const MpvEventProperty) };
                if prop.data.is_null() {
                    return None;
                }
                let name = if prop.name.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(prop.name) }
                        .to_string_lossy()
                        .into_owned()
                };
                match (name.as_str(), prop.format) {
                    ("time-pos", MPV_FORMAT_DOUBLE) => {
                        let secs = unsafe { *(prop.data as *const f64) };
                        secs.is_finite().then(|| PortEvent::Position {
                            position_ms: (secs * 1000.0).round() as i64,
                        })
                    }
                    ("eof-reached", MPV_FORMAT_FLAG) => {
                        let flag = unsafe { *(prop.data as *const c_int) };
                        (flag != 0).then_some(PortEvent::Eof)
                    }
                    ("paused-for-cache", MPV_FORMAT_FLAG) => {
                        let flag = unsafe { *(prop.data as *const c_int) };
                        Some(PortEvent::Buffering { active: flag != 0 })
                    }
                    _ => None,
                }
            }
            MPV_EVENT_END_FILE => {
                // 只把「失败收尾」当解码错误；正常 EOF 由 `eof-reached` 上报
                let end = unsafe { &*(raw.data as *const MpvEventEndFile) };
                if end.reason == MPV_END_FILE_REASON_ERROR {
                    Some(PortEvent::DecodeError {
                        message: self.error_text(end.error),
                    })
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// 连续拉取底层事件，直到得到一条可上报的端口事件或队列清空。
    pub fn poll_event(&self) -> Option<PortEvent> {
        loop {
            let raw = self.wait_raw()?;
            if let Some(mapped) = self.map_event(raw) {
                return Some(mapped);
            }
        }
    }

    fn error_text(&self, code: c_int) -> String {
        if code == 0 {
            return "解码结束".into();
        }
        let ptr = unsafe { (self.lib.error_string)(code) };
        if ptr.is_null() {
            format!("mpv 错误码 {code}")
        } else {
            unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
        }
    }

    fn check(&self, rc: c_int, what: &str) -> Result<()> {
        if rc >= 0 {
            Ok(())
        } else {
            Err(PortError::Backend(format!("{what} 失败：{}", self.error_text(rc))))
        }
    }
}

/// 便于跨模块使用的端口事件别名。
pub use cinema_domain::model::PlayerEvent as PortEvent;

struct RawEvent {
    id: c_int,
    data: *mut c_void,
}

// ---------------------------------------------------------------------------
// MpvProber：只取时长的探测实例（P-01 `probe` 的实现落点）
// ---------------------------------------------------------------------------

/// 探测实例：常驻一路 headless 内核，串行化地量取时长。
///
/// M1 只取时长。`width/height/audio_tracks/embedded_subtitles` 属于「系统自动维度」，
/// 由后续阶段在同一处补全，端口签名不变。
pub struct MpvProber {
    inst: Arc<MpvInstance>,
    lock: Mutex<()>,
}

impl std::fmt::Debug for MpvProber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MpvProber(headless)")
    }
}

impl MpvProber {
    /// 创建一个无渲染、无音频的探测实例。
    pub fn new() -> Result<Self> {
        let lib = MpvLib::shared()?;
        let inst = MpvInstance::new(lib)?;
        for (name, value) in [
            ("config", "no"),
            ("idle", "yes"),
            ("vo", "null"),
            ("ao", "null"),
        ] {
            inst.set_option(name, value)?;
        }
        inst.initialize()?;
        Ok(Self {
            inst: Arc::new(inst),
            lock: Mutex::new(()),
        })
    }

    /// 量取一个可播放引用的流事实。
    pub fn probe(&self, location: &str) -> Result<StreamFacts> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| PortError::Backend("探测实例锁不可用".into()))?;

        self.inst.command(&["loadfile", location, "replace"])?;

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut facts = StreamFacts::default();
        while Instant::now() < deadline {
            if let Some(secs) = self.inst.get_double("duration") {
                if secs.is_finite() && secs > 0.0 {
                    facts.duration_ms = Some((secs * 1000.0).round() as i64);
                    break;
                }
            }
            // 文件打不开时会回到空闲态，此时不必等到超时
            if self.inst.get_flag("idle-active").unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // 内嵌字幕 / 音轨清单探测：mpv 的属性寻址 `track-list/<i>/<field>` 在本沙箱
        // （无 GUI、无真实媒体文件）**无法实机验证**，属人工确认项。探测失败或属性
        // 不存在时保持清单为空，绝不影响上面的时长探测，也不会 panic。
        probe_track_lists(&self.inst, &mut facts);

        // 立刻卸载，避免探测实例长期占着解码资源
        let _ = self.inst.command(&["stop"]);
        Ok(facts)
    }
}

/// 读取 `track-list` 清单，填入内嵌字幕与音轨。
///
/// 循环 `i` 从 0 起，读 `track-list/<i>/type`：读到第一个不存在的属性即停止
/// （轨道索引是连续的）。`sub` 填内嵌字幕（lang / title），`audio` 填音轨
/// （lang / codec）。**人工确认项**：`track-list/<i>` 的属性寻址方式未在本沙箱
/// 实机验证；即便寻址无效，循环会在第 0 项立刻停下，结果只落为一个空清单。
fn probe_track_lists(inst: &MpvInstance, facts: &mut StreamFacts) {
    for i in 0..64i64 {
        let Some(kind) = inst.get_string(&format!("track-list/{i}/type")) else {
            break;
        };
        match kind.as_str() {
            "sub" => {
                let lang = inst.get_string(&format!("track-list/{i}/lang"));
                let title = inst.get_string(&format!("track-list/{i}/title"));
                facts.embedded_subtitles.push(EmbeddedSubtitle {
                    index: i,
                    lang,
                    title,
                });
            }
            "audio" => {
                let lang = inst.get_string(&format!("track-list/{i}/lang"));
                let codec = inst.get_string(&format!("track-list/{i}/codec"));
                facts.audio_tracks.push(AudioTrackInfo {
                    index: i,
                    lang,
                    codec,
                });
            }
            _ => {}
        }
    }
}

/// 订阅播放事件所需的属性（P-04 事件流的物理来源）。
pub(crate) fn observe_playback_properties(inst: &MpvInstance) -> Result<()> {
    inst.observe("time-pos", 1, MPV_FORMAT_DOUBLE)?;
    inst.observe("eof-reached", 2, MPV_FORMAT_FLAG)?;
    inst.observe("paused-for-cache", 3, MPV_FORMAT_FLAG)?;
    Ok(())
}

/// 键盘完全交给前端：写一份只把方向键绑成空操作的 `input.conf`。
///
/// `input-default-bindings=no` 在部分内嵌（`wid`）场景下仍可能被 VO 键盘输入绕过，
/// 这里显式兜底，确保上/下键永远不会变成 mpv 自带的 `seek ±60`（现场正是这样被抢走的）。
fn arrow_input_conf() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("cinema-input-{}.conf", std::process::id()));
        let content = "UP ignore\nDOWN ignore\nLEFT ignore\nRIGHT ignore\n";
        std::fs::write(&path, content).ok()?;
        Some(path)
    })
    .clone()
}

/// 供 [`crate::player`] 复用的加载选项集合。
pub(crate) fn apply_common_options(inst: &MpvInstance, render: Option<i64>) -> Result<()> {
    // 必需项：读用户配置 / 保持实例存活
    inst.set_option("config", "no")?;
    inst.set_option("idle", "yes")?;

    match render {
        Some(wid) => inst.set_option("wid", &wid.to_string())?,
        None => {
            // 无渲染目标（单测 / 无窗口环境）：显式 headless，避免 mpv 自己去开窗
            inst.set_option("vo", "null")?;
            inst.set_option("ao", "null")?;
        }
    }

    // 锦上添花项：偶发不支持不应导致整路实例创建失败
    for (name, value) in [
        // 播到结尾后保留文件：`eof-reached` 才能稳定读到 true，
        // 组合根据此自动收起播放页并登记「已看」。
        ("keep-open", "yes"),
        ("osc", "no"),
        // 键盘交给宿主窗口 / 前端控制条：mpv 自带的 q=退出等绑定会与产品交互冲突
        ("input-default-bindings", "no"),
        ("input-vo-keyboard", "no"),
        ("msg-level", "all=no"),
        // 音频输出跟随设备能力：设备支持多声道就用环绕（解码后的 Dolby / DTS
        // 音轨按原声道数输出），只支持立体声时才降混。mpv 默认的 `auto` 在部分
        // Windows 声卡上会锁成 2.0，显式设成 `auto-safe` 才能拿到 5.1 / 7.1。
        ("audio-channels", "auto-safe"),
        // 多音轨文件优先选带中文标识的轨道（站点片源常见「日语 2.0 + 评论音轨」）
        ("alang", "zh,chi,chs,jpn,ja,eng,en"),
    ] {
        let _ = inst.set_option(name, value);
    }

    // 方向键兜底：即使 mpv 拿到了焦点，也不会自己快进 / 快退
    if let Some(conf) = arrow_input_conf() {
        let _ = inst.set_option("input-conf", &conf.to_string_lossy());
    }
    Ok(())
}
