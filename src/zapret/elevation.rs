use std::ffi::c_void;
use std::ptr;

extern "system" {
    fn OpenProcessToken(
        ProcessHandle: *mut c_void,
        DesiredAccess: u32,
        TokenHandle: *mut *mut c_void,
    ) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetTokenInformation(
        TokenHandle: *mut c_void,
        TokenInformationClass: i32,
        TokenInformation: *mut c_void,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> i32;
    fn CloseHandle(hObject: *mut c_void) -> i32;
}

const TOKEN_QUERY: u32 = 0x0008;
const TOKEN_ELEVATION: i32 = 20;

#[derive(Copy, Clone, Debug)]
#[repr(C)]
struct TokenElevationStruct {
    pub token_is_elevated: u32,
}

/// Returns true if the current process is running with administrator privileges.
pub fn is_elevated() -> bool {
    unsafe {
        let mut token: *mut c_void = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TokenElevationStruct {
            token_is_elevated: 0,
        };
        let size = std::mem::size_of::<TokenElevationStruct>() as u32;
        let mut return_length = 0;
        let res = GetTokenInformation(
            token,
            TOKEN_ELEVATION,
            &mut elevation as *mut _ as *mut c_void,
            size,
            &mut return_length,
        );
        CloseHandle(token);
        res != 0 && elevation.token_is_elevated != 0
    }
}

/// Helper to ensure the process is elevated, returning `anyhow::anyhow!("NeedsElevation")` otherwise.
pub fn check_elevation() -> anyhow::Result<()> {
    if is_elevated() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("NeedsElevation"))
    }
}
