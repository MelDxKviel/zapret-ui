//! Set the native Windows title-bar / window icon via `WM_SETICON`, loading the
//! icon straight from the embedded `.exe` resource (id 1, written by `build.rs`
//! through `winresource`).
//!
//! winit/Slint already drives the taskbar icon, but the title-bar *small* icon
//! (top-left of the window frame) stays empty unless `ICON_SMALL` is pushed onto
//! the window explicitly — winit registers its window class without a small
//! class icon, so there is nothing to fall back to. We set both icons here.
//!
//! The window is located with `EnumWindows` (matching this process + the static
//! title) rather than `FindWindowW`: `FindWindowW` reliably fails to match
//! winit's "Window Class" window on this platform, whereas enumerating the
//! current process's top-level windows finds it every time.

use std::os::windows::ffi::OsStrExt;
use std::ptr;

type Hwnd = *mut std::ffi::c_void;

#[link(name = "user32")]
extern "system" {
    fn EnumWindows(
        cb: unsafe extern "system" fn(Hwnd, isize) -> i32,
        lparam: isize,
    ) -> i32;
    fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
    fn GetWindowTextW(hwnd: Hwnd, buf: *mut u16, max: i32) -> i32;
    fn SendMessageW(hWnd: Hwnd, msg: u32, wParam: usize, lParam: isize) -> isize;
    fn LoadImageW(
        hInst: Hwnd,
        name: *const u16,
        type_: u32,
        cx: i32,
        cy: i32,
        fuLoad: u32,
    ) -> Hwnd;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleW(lpModuleName: *const u16) -> Hwnd;
    fn GetCurrentProcessId() -> u32;
}

const WM_SETICON: u32 = 0x0080;
const ICON_SMALL: usize = 0;
const ICON_BIG: usize = 1;
const IMAGE_ICON: u32 = 1;

/// Resource id `winresource` assigns to the app icon (`1 ICON "assets/icon.ico"`).
const APP_ICON_RESOURCE_ID: u16 = 1;

struct FindCtx {
    pid: u32,
    title: Vec<u16>,
    found: Hwnd,
}

unsafe extern "system" fn enum_cb(hwnd: Hwnd, lparam: isize) -> i32 {
    let ctx = &mut *(lparam as *mut FindCtx);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != ctx.pid {
        return 1; // continue
    }
    let mut buf = [0u16; 128];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len > 0 && buf[..len as usize] == ctx.title[..] {
        ctx.found = hwnd;
        return 0; // stop enumerating
    }
    1
}

/// Locate our own top-level window (this process, matching `window_title`) and
/// push the embedded icon onto it as both the small (title-bar) and big
/// (Alt-Tab) icon. Returns `false` while the window doesn't exist yet — the
/// caller should retry, since the window can take a few seconds to appear while
/// the renderer warms up — and `true` once the icon has been applied.
pub fn set_window_icon(window_title: &str) -> bool {
    let title: Vec<u16> = std::ffi::OsStr::new(window_title).encode_wide().collect();
    let mut ctx = FindCtx {
        pid: unsafe { GetCurrentProcessId() },
        title,
        found: ptr::null_mut(),
    };

    unsafe {
        EnumWindows(enum_cb, &mut ctx as *mut FindCtx as isize);
        let hwnd = ctx.found;
        if hwnd.is_null() {
            return false;
        }

        let hinst = GetModuleHandleW(ptr::null());
        // MAKEINTRESOURCEW(id): the resource id passed as a bare pointer value.
        let name = APP_ICON_RESOURCE_ID as usize as *const u16;

        // LoadImageW picks the best-matching frame from the icon *group*, so the
        // 16px title-bar icon and 32px Alt-Tab icon come from their own crisp
        // frames rather than a downscale.
        let small = LoadImageW(hinst, name, IMAGE_ICON, 16, 16, 0);
        if !small.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL, small as isize);
        }
        let big = LoadImageW(hinst, name, IMAGE_ICON, 32, 32, 0);
        if !big.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG, big as isize);
        }
    }
    true
}
