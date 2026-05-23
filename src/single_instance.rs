use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> *mut std::ffi::c_void;
    fn SetForegroundWindow(hWnd: *mut std::ffi::c_void) -> i32;
    fn ShowWindow(hWnd: *mut std::ffi::c_void, nCmdShow: i32) -> i32;
    fn IsIconic(hWnd: *mut std::ffi::c_void) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(
        lpMutexAttributes: *mut std::ffi::c_void,
        bInitialOwner: i32,
        lpName: *const u16,
    ) -> *mut std::ffi::c_void;
    fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
}

const ERROR_ALREADY_EXISTS: u32 = 183;
const SW_RESTORE: i32 = 9;
const SW_SHOW: i32 = 5;

pub struct SingleInstance {
    handle: *mut std::ffi::c_void,
}

unsafe impl Send for SingleInstance {}
unsafe impl Sync for SingleInstance {}

impl SingleInstance {
    pub fn new(name: &str) -> Result<Self, &'static str> {
        let mut name_w: Vec<u16> = OsStr::new(name).encode_wide().collect();
        name_w.push(0);

        let handle = unsafe { CreateMutexW(ptr::null_mut(), 1, name_w.as_ptr()) };
        if handle.is_null() {
            return Err("Failed to create mutex");
        }

        let err = unsafe { GetLastError() };
        if err == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            return Err("Already running");
        }

        Ok(Self { handle })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

pub fn focus_existing_window(window_title: &str) {
    let mut title_w: Vec<u16> = OsStr::new(window_title).encode_wide().collect();
    title_w.push(0);

    unsafe {
        let hwnd = FindWindowW(ptr::null(), title_w.as_ptr());
        if !hwnd.is_null() {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetForegroundWindow(hwnd);
        }
    }
}
