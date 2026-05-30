//! Win32 shell execution + elevation relaunch helpers.
//!
//! Extracted from `app.rs` so the orchestrator module isn't carrying raw FFI.
//! None of this touches Slint or the UI-thread thread-locals, so it lives on its
//! own. See `app::mod` for the callers and the elevation model overview.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut std::ffi::c_void,
        lpOperation: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShowCmd: i32,
    ) -> *mut std::ffi::c_void;
}

/// Open a path with the OS default handler (folder in Explorer, URL in browser).
/// Uses `ShellExecuteW` directly rather than `cmd /C start`, so shell
/// metacharacters in the target can't be interpreted (command-injection fix).
pub(super) fn open_external(target: &str) {
    let file_w: Vec<u16> = OsStr::new(target).encode_wide().chain(Some(0)).collect();
    unsafe {
        // null lpOperation => default verb ("open"), which handles URLs, files
        // and folders without going through a command interpreter.
        ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            file_w.as_ptr(),
            ptr::null(),
            ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

/// Quote a single argument for a Windows command line so that the receiving
/// process's `CommandLineToArgvW`/`std::env::args` reproduces it verbatim — even
/// when it contains spaces or parentheses (real preset ids look like
/// `general (ALT2)`). Implements the documented MSVC argv quoting rules.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                // Escape all pending backslashes (they precede a quote) + the quote.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
                out.push('"');
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // Trailing backslashes precede the closing quote — double them.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Handle to a launched elevated one-shot task: the nonce-named result file the
/// helper writes its outcome into, plus the nonce used to authenticate it.
pub(super) struct ElevationHandle {
    pub result_file: std::path::PathBuf,
    pub nonce: String,
}

/// Launch this exe elevated to run a one-shot service task. Arguments (including
/// the strategy id and an explicit install dir) are passed with correct quoting
/// so spaces/parentheses survive, and an explicit install dir is handed over so
/// the helper acts on the *same* directory regardless of which admin account UAC
/// elevates to. Returns a handle whose result file the caller can await.
pub(super) fn relaunch_elevated(
    task: &str,
    strategy: Option<&str>,
    install_dir: &std::path::Path,
) -> anyhow::Result<ElevationHandle> {
    let current_exe = std::env::current_exe()?;
    let exe_path_w: Vec<u16> = current_exe.as_os_str().encode_wide().chain(Some(0)).collect();

    // Unique nonce so the parent can authenticate the result file the helper
    // writes (and so concurrent tasks don't collide).
    let nonce = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{}", std::process::id(), nanos)
    };
    let result_file = std::env::temp_dir().join(format!("zapret-ui-elev-{nonce}.result"));

    let mut args = vec![format!("--elevated-task={task}")];
    if let Some(strat) = strategy {
        args.push(format!("--strategy={strat}"));
    }
    args.push(format!("--install-dir={}", install_dir.display()));
    args.push(format!("--result-file={}", result_file.display()));
    args.push(format!("--nonce={nonce}"));
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
    let params_w: Vec<u16> = OsStr::new(&params).encode_wide().chain(Some(0)).collect();

    let verb_w: Vec<u16> = OsStr::new("runas").encode_wide().chain(Some(0)).collect();

    unsafe {
        let result = ShellExecuteW(
            ptr::null_mut(),
            verb_w.as_ptr(),
            exe_path_w.as_ptr(),
            params_w.as_ptr(),
            ptr::null(),
            1, // SW_SHOWNORMAL
        );
        if (result as usize) <= 32 {
            return Err(anyhow::anyhow!("Failed to relaunch elevated: error code {}", result as usize));
        }
    }

    Ok(ElevationHandle { result_file, nonce })
}

/// Await the result file written by an elevated one-shot task. Returns `Ok(())`
/// on success, or the helper's error message. Times out (the user may have
/// dismissed UAC, or the helper hung) after ~90s.
pub(super) async fn wait_for_elevated_result(handle: ElevationHandle) -> Result<(), String> {
    use tokio::time::{sleep, Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(90);
    let outcome = loop {
        if let Ok(content) = std::fs::read_to_string(&handle.result_file) {
            let mut lines = content.lines();
            let got_nonce = lines.next().unwrap_or("");
            if got_nonce == handle.nonce {
                let status = lines.next().unwrap_or("");
                if status == "OK" {
                    break Ok(());
                } else {
                    let msg: String = lines.collect::<Vec<_>>().join("\n");
                    break Err(if msg.is_empty() { status.to_string() } else { msg });
                }
            }
        }
        if Instant::now() >= deadline {
            break Err("Elevated operation did not report a result (timed out or was cancelled).".to_string());
        }
        sleep(Duration::from_millis(250)).await;
    };
    let _ = std::fs::remove_file(&handle.result_file);
    outcome
}

/// Relaunch *this whole app* elevated (the normal UI, not a one-shot task) via
/// the `runas` verb. The new instance carries `--relaunch` so it retries the
/// single-instance mutex while this (unelevated) instance exits. Used by the
/// "run as administrator" banner.
pub(super) fn relaunch_self_elevated() -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;
    let exe_path_w: Vec<u16> = current_exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let params_w: Vec<u16> = OsStr::new("--relaunch").encode_wide().chain(Some(0)).collect();
    let verb_w: Vec<u16> = OsStr::new("runas").encode_wide().chain(Some(0)).collect();

    unsafe {
        let result = ShellExecuteW(
            ptr::null_mut(),
            verb_w.as_ptr(),
            exe_path_w.as_ptr(),
            params_w.as_ptr(),
            ptr::null(),
            1, // SW_SHOWNORMAL
        );
        if (result as usize) <= 32 {
            return Err(anyhow::anyhow!("Failed to relaunch elevated: error code {}", result as usize));
        }
    }

    Ok(())
}

/// Relaunch the (freshly self-updated) binary at the original path, then let the
/// caller exit. Uses a plain spawn — no elevation — and passes `--relaunch` so
/// the new process retries the single-instance mutex while this one shuts down.
pub(super) fn relaunch_after_update() -> anyhow::Result<()> {
    let current_exe = std::env::current_exe()?;
    std::process::Command::new(current_exe)
        .arg("--relaunch")
        .spawn()
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("Failed to relaunch updated binary: {e}"))
}
