//! 影片画面宿主窗口。
//!
//! 做法：在主窗口（Tauri 窗口）内建一个**自己的子窗口**，再把它的句柄作为 `wid`
//! 交给 mpv；mpv 会在其中自行建立渲染子窗口。这样我们能精确控制影片的矩形 ——
//! 正式播放时占满客户区，悬停预览时只占海报所在的一小块 —— 而不用让 mpv 去猜。
//!
//! 之所以不用 `wid = 主窗口本身`：那样 mpv 的窗口会盖住整个客户区，
//! 海报墙与顶栏都无法再交互。

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{Arc, Mutex, OnceLock};

use cinema_domain::error::{PortError, Result};

type Hwnd = *mut c_void;
type Hinstance = *mut c_void;
type Hgdiobj = *mut c_void;

const WS_CHILD: u32 = 0x4000_0000;
const WS_CLIPSIBLINGS: u32 = 0x0400_0000;
const WS_CLIPCHILDREN: u32 = 0x0200_0000;
const CS_HREDRAW: u32 = 0x0002;
const CS_VREDRAW: u32 = 0x0001;
const SW_HIDE: i32 = 0;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;
const BLACK_BRUSH: i32 = 4;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_MOUSEMOVE: u32 = 0x0200;
const GWL_EXSTYLE: i32 = -20;
const WS_EX_TRANSPARENT: u32 = 0x0000_0020;

/// 供 `wnd_proc` 查表用的点击回调，按窗口句柄索引。
///
/// `wnd_proc` 是静态函数，拿不到 `HostWindow` 实例，因此用一张全局表把
/// 「句柄 → 回调」关联起来，窗口销毁时移除。点击必须在这里截获：画面窗口
/// 压在 webview 之上，落在它上面的鼠标事件不会到达页面。
fn click_registry() -> &'static Mutex<HashMap<isize, Arc<dyn Fn() + Send + Sync>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<isize, Arc<dyn Fn() + Send + Sync>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 鼠标移动回调：控制条自动隐藏后，指针落在原生画面上时也要能让控制条重新出现。
///
/// 回调带上客户区坐标：窗口在指针下方被撑开时系统也会补发 `WM_MOUSEMOVE`，
/// 这类「位置没变」的事件绝不能当成用户在移动鼠标，否则控制条会收起后立刻弹回。
fn move_registry() -> &'static Mutex<HashMap<isize, Arc<dyn Fn(i32, i32) + Send + Sync>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<isize, Arc<dyn Fn(i32, i32) + Send + Sync>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

#[repr(C)]
struct WndClassExW {
    cb_size: u32,
    style: u32,
    lpfn_wnd_proc: Option<unsafe extern "system" fn(Hwnd, u32, usize, isize) -> isize>,
    cb_cls_extra: i32,
    cb_wnd_extra: i32,
    h_instance: Hinstance,
    h_icon: *mut c_void,
    h_cursor: *mut c_void,
    hbr_background: *mut c_void,
    lpsz_menu_name: *const u16,
    lpsz_class_name: *const u16,
    h_icon_sm: *mut c_void,
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(name: *const u16) -> Hinstance;
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassExW(class: *const WndClassExW) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Hwnd,
        menu: *mut c_void,
        instance: Hinstance,
        param: *mut c_void,
    ) -> Hwnd;
    fn DefWindowProcW(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize;
    fn DestroyWindow(hwnd: Hwnd) -> i32;
    fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
    fn SetWindowPos(
        hwnd: Hwnd,
        insert_after: Hwnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    fn GetClientRect(hwnd: Hwnd, rect: *mut Rect) -> i32;
    fn GetStockObject(index: i32) -> Hgdiobj;
    fn GetWindowLongW(hwnd: Hwnd, index: i32) -> i32;
    fn SetWindowLongW(hwnd: Hwnd, index: i32, value: i32) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn class_name() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| wide("CinemaVideoHost"))
}

/// 窗口类只需注册一次；注册失败（例如已被占用）不视为致命。
fn ensure_class() -> Result<()> {
    static DONE: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let outcome = DONE.get_or_init(|| {
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = WndClassExW {
            cb_size: std::mem::size_of::<WndClassExW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfn_wnd_proc: Some(wnd_proc),
            cb_cls_extra: 0,
            cb_wnd_extra: 0,
            h_instance: instance,
            h_icon: null_mut(),
            h_cursor: null_mut(),
            hbr_background: unsafe { GetStockObject(BLACK_BRUSH) },
            lpsz_menu_name: std::ptr::null(),
            lpsz_class_name: class_name().as_ptr(),
            h_icon_sm: null_mut(),
        };
        if unsafe { RegisterClassExW(&class) } == 0 {
            Err("注册画面宿主窗口类失败".to_string())
        } else {
            Ok(())
        }
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(msg) => Err(PortError::Backend(msg.clone())),
    }
}

unsafe extern "system" fn wnd_proc(hwnd: Hwnd, msg: u32, wp: usize, lp: isize) -> isize {
    // 本窗口只给 mpv 当画布，不接收键盘；但**左键抬起**要转发出去，
    // 否则鼠标落在影片画面上时页面拿不到点击（预览开启后尤其明显）。
    if msg == WM_LBUTTONUP {
        let key = hwnd as isize;
        let handler = click_registry()
            .lock()
            .ok()
            .and_then(|registry| registry.get(&key).cloned());
        if let Some(handler) = handler {
            handler();
        }
    } else if msg == WM_MOUSEMOVE {
        let key = hwnd as isize;
        let handler = move_registry()
            .lock()
            .ok()
            .and_then(|registry| registry.get(&key).cloned());
        if let Some(handler) = handler {
            // lParam 低 16 位为 x、高 16 位为 y（客户区坐标，有符号）
            let x = (lp as usize & 0xffff) as u16 as i16 as i32;
            let y = ((lp as usize >> 16) & 0xffff) as u16 as i16 as i32;
            handler(x, y);
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// 一个画面宿主子窗口。`Drop` 时销毁。
pub struct HostWindow {
    hwnd: Hwnd,
}

impl HostWindow {
    /// 建立未显示的子窗口；调用方随后用 [`HostWindow::place`] 定位并显示。
    ///
    /// `parent` 必须是主窗口句柄：`WS_CHILD` 窗口没有父窗口就是非法形态，
    /// 会被当成顶层窗口，画面将脱出应用窗口。
    pub fn create(parent: i64, title: &str) -> Result<Self> {
        ensure_class()?;
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let name = wide(title);
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name().as_ptr(),
                name.as_ptr(),
                WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                0,
                0,
                0,
                0,
                parent as Hwnd,
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(PortError::Backend("创建画面宿主窗口失败".into()));
        }
        Ok(Self { hwnd })
    }

    /// 句柄的整数值，供 mpv `wid` 选项使用。
    pub fn wid(&self) -> i64 {
        self.hwnd as i64
    }

    /// 移动到指定矩形并显示（同时置顶，压住同级的 webview 子窗口）。
    pub fn place(&self, x: i32, y: i32, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            self.hide();
            return;
        }
        unsafe {
            SetWindowPos(
                self.hwnd,
                null_mut(), // HWND_TOP
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    /// 占满父窗口客户区，但**底部留出 `reserve_bottom` 像素**。
    ///
    /// 留出的那条正是 webview 里播放控制条所在位置：影片只盖住画面上方，
    /// 控制条仍可见、可点击。传入 0 时画面铺满整个客户区（控制条自动隐藏时用）。
    pub fn fill_parent(&self, parent: i64, reserve_bottom: i32) {
        let mut rect = Rect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { GetClientRect(parent as Hwnd, &mut rect) } != 0 {
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top - reserve_bottom.max(0);
            self.place(0, 0, width, height);
        }
    }

    /// 鼠标穿透：让点击落到下面的 webview。
    ///
    /// 详情页的手动预告片没有 mpv 的点击交互，画面铺满后需要靠 HTML 层接收点击来关闭，
    /// 因此播放期间开启穿透，停止后立即关闭。悬停预览仍保持可点击（点击进详情）。
    pub fn set_click_through(&self, on: bool) {
        unsafe {
            let style = GetWindowLongW(self.hwnd, GWL_EXSTYLE) as u32;
            let next = if on {
                style | WS_EX_TRANSPARENT
            } else {
                style & !WS_EX_TRANSPARENT
            };
            SetWindowLongW(self.hwnd, GWL_EXSTYLE, next as i32);
        }
    }

    pub fn hide(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// 注册左键抬起回调。窗口压在 webview 之上，因此落在画面上的点击只能在
    /// 这里截获后转发给页面。
    pub fn set_on_click<F>(&self, handler: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        if let Ok(mut registry) = click_registry().lock() {
            registry.insert(self.hwnd as isize, Arc::new(handler));
        }
    }
    /// 注册鼠标移动回调（控制条自动隐藏 / 重新出现的判定），回传客户区坐标。
    pub fn set_on_move<F>(&self, handler: F)
    where
        F: Fn(i32, i32) + Send + Sync + 'static,
    {
        if let Ok(mut registry) = move_registry().lock() {
            registry.insert(self.hwnd as isize, Arc::new(handler));
        }
    }
}

impl std::fmt::Debug for HostWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostWindow").field("hwnd", &self.wid()).finish()
    }
}

// HWND 是进程级资源而非线程私有；本结构只在持锁状态下被访问，
// 因此跨线程传递是安全的。
unsafe impl Send for HostWindow {}
unsafe impl Sync for HostWindow {}

impl Drop for HostWindow {
    fn drop(&mut self) {
        if !self.hwnd.is_null() {
            if let Ok(mut registry) = click_registry().lock() {
                registry.remove(&(self.hwnd as isize));
            }
            if let Ok(mut registry) = move_registry().lock() {
                registry.remove(&(self.hwnd as isize));
            }
            unsafe {
                DestroyWindow(self.hwnd);
            }
            self.hwnd = null_mut();
        }
    }
}
