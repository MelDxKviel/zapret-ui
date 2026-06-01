use crate::contracts::{RunningMode, Strategy};
use crate::ports::ServiceCtl;
use crate::zapret::elevation::check_elevation;
use sha2::{Digest, Sha256};
use std::ffi::{c_void, OsStr, OsString};
use std::io::Read;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_service::{
    service::{
        ServiceAccess, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType,
        ServiceState, ServiceType,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

/// Turn a `windows_service` error into an `anyhow` error that actually shows the
/// underlying OS failure. The crate's own `Display` for `Error::Winapi` collapses
/// to the useless "IO error in winapi call", throwing away the inner `io::Error`
/// (built from `GetLastError`) — so the real reason ("Access is denied. (os error
/// 5)", "An instance of the service is already running. (os error 1056)", …) is
/// invisible in logs and toasts. We render the inner error, whose `Display`
/// already includes the OS code, prefixed with the failing call site. Use this in
/// place of `?`/`.context()` on every SCM call so failures are diagnosable.
fn svc_err(ctx: &str, e: windows_service::Error) -> anyhow::Error {
    match e {
        windows_service::Error::Winapi(io) => anyhow::anyhow!("{ctx}: {io}"),
        other => anyhow::anyhow!("{ctx}: {other}"),
    }
}

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        hKey: *mut c_void,
        lpSubKey: *const u16,
        ulOptions: u32,
        samDesired: u32,
        phkResult: *mut *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(
        hKey: *mut c_void,
        lpValueName: *const u16,
        lpReserved: *mut u32,
        lpType: *mut u32,
        lpData: *mut u8,
        lpcbData: *mut u32,
    ) -> i32;
    fn RegCloseKey(hKey: *mut c_void) -> i32;
}

const HKEY_LOCAL_MACHINE: usize = 0x80000002;
const KEY_QUERY_VALUE: u32 = 0x0001;
const KEY_WOW64_64KEY: u32 = 0x0100;
const ERROR_SUCCESS: i32 = 0;
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;

fn wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn service_image_path_from_registry(service_name: &str) -> Option<OsString> {
    let key_path = wide_null(&format!(
        r"SYSTEM\CurrentControlSet\Services\{}",
        service_name
    ));
    let value_name = wide_null("ImagePath");
    let mut key = ptr::null_mut();
    unsafe {
        let rc = RegOpenKeyExW(
            HKEY_LOCAL_MACHINE as *mut c_void,
            key_path.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_WOW64_64KEY,
            &mut key,
        );
        if rc != ERROR_SUCCESS {
            return None;
        }

        let mut ty = 0u32;
        let mut bytes = 0u32;
        let rc = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            ptr::null_mut(),
            &mut bytes,
        );
        if rc != ERROR_SUCCESS || bytes == 0 || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
            RegCloseKey(key);
            return None;
        }

        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let rc = RegQueryValueExW(
            key,
            value_name.as_ptr(),
            ptr::null_mut(),
            &mut ty,
            buf.as_mut_ptr() as *mut u8,
            &mut bytes,
        );
        RegCloseKey(key);
        if rc != ERROR_SUCCESS {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        let end = buf[..len].iter().position(|&ch| ch == 0).unwrap_or(len);
        Some(OsString::from_wide(&buf[..end]))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileDigest {
    rel: String,
    len: u64,
    sha256: String,
}

pub struct ProtectedDir {
    pub path: PathBuf,
    snapshot: Vec<FileDigest>,
}

/// Lower-case hex encoding of a byte slice (avoids pulling in a hex crate).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hash_file(path: &Path) -> anyhow::Result<(u64, String)> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("opening {:?} for hashing: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| anyhow::anyhow!("reading {:?} for hashing: {}", path, e))?;
        if n == 0 {
            break;
        }
        len += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((len, to_hex(&hasher.finalize())))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn collect_file_digests(root: &Path) -> anyhow::Result<Vec<FileDigest>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<FileDigest>) -> anyhow::Result<()> {
        for entry in
            std::fs::read_dir(dir).map_err(|e| anyhow::anyhow!("reading {:?}: {}", dir, e))?
        {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                anyhow::bail!(
                    "Refusing to stage service files through a symlink: {:?}",
                    path
                );
            }
            if ty.is_dir() {
                walk(root, &path, out)?;
            } else if ty.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('/', "\\");
                let (len, sha256) = hash_file(&path)?;
                out.push(FileDigest { rel, len, sha256 });
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort_by_key(|d| d.rel.to_lowercase());
    Ok(out)
}

fn digest_for_rel<'a>(snapshot: &'a [FileDigest], rel: &str) -> Option<&'a FileDigest> {
    snapshot.iter().find(|d| d.rel.eq_ignore_ascii_case(rel))
}

fn path_ci(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('/', "\\").to_lowercase();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    while s.ends_with('\\') && s.len() > 3 {
        s.pop();
    }
    s
}

fn is_under_dir_ci(path: &Path, dir: &Path) -> bool {
    let path = path_ci(path);
    let dir = path_ci(dir);
    path == dir || path.starts_with(&format!("{dir}\\"))
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Extract the executable path from an SCM ImagePath. `windows-service` returns
/// the full command line for services with arguments, so parse the first argv.
fn extract_exe_from_image_path(image: &OsStr) -> Option<PathBuf> {
    let s = image.to_string_lossy();
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(PathBuf::from(&rest[..end]));
    }

    let lower = s.to_lowercase();
    if let Some(pos) = lower.find(".exe") {
        return Some(PathBuf::from(&s[..pos + ".exe".len()]));
    }
    s.split_whitespace().next().map(PathBuf::from)
}

pub(crate) fn service_image_belongs_to_dirs(image: &OsStr, owned_dirs: &[PathBuf]) -> bool {
    let Some(exe) = extract_exe_from_image_path(image) else {
        return false;
    };
    let Some(name) = exe.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.eq_ignore_ascii_case("winws.exe") {
        return false;
    }
    let exe = canonical_or_self(&exe);
    owned_dirs.iter().any(|dir| {
        let dir = canonical_or_self(dir);
        is_under_dir_ci(&exe, &dir)
    })
}

pub(crate) fn service_belongs_to_dirs(
    service_name: &str,
    service: &windows_service::service::Service,
    owned_dirs: &[PathBuf],
) -> anyhow::Result<bool> {
    if let Some(image) = service_image_path_from_registry(service_name) {
        return Ok(service_image_belongs_to_dirs(image.as_os_str(), owned_dirs));
    }

    match service.query_config() {
        Ok(cfg) => Ok(service_image_belongs_to_dirs(
            cfg.executable_path.as_os_str(),
            owned_dirs,
        )),
        Err(e) => Err(svc_err("QueryServiceConfig", e)),
    }
}

fn service_ownership_error(name: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "A different service named \"{}\" already exists and does not belong to zapret-ui. \
         Remove it manually before managing zapret-ui service mode.",
        name
    )
}

fn strategy_from_bat_content(
    stem: &str,
    content: &str,
    install_dir: &Path,
    gf: crate::contracts::GameFilterMode,
) -> Option<Strategy> {
    let args = crate::zapret::batparse::parse_winws_args(content, install_dir, gf)?;
    let lists = crate::zapret::batparse::referenced_lists(&args);
    Some(Strategy {
        id: stem.to_string(),
        display_name: stem.to_string(),
        category: crate::zapret::batparse::category_for(stem),
        description: crate::zapret::batparse::describe(stem),
        winws_args: args,
        requires_lists: lists,
    })
}

fn copy_file_contents(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let mut input = std::fs::File::open(src)
        .map_err(|e| anyhow::anyhow!("opening service source file {:?}: {}", src, e))?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(|e| anyhow::anyhow!("creating staged service file {:?}: {}", dst, e))?;
    std::io::copy(&mut input, &mut output)
        .map_err(|e| anyhow::anyhow!("copying {:?} to {:?}: {}", src, dst, e))?;
    Ok(())
}

fn run_icacls(path: &Path, args: &[&str], ctx: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new("icacls");
    cmd.arg(path).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("{ctx}: failed to run icacls: {e}"))?;
    if out.status.success() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "{ctx}: icacls failed with exit code {:?}: {}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    ))
}

fn run_hidden_command(program: &str, args: &[OsString], ctx: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("{ctx}: failed to run {program}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "{ctx}: {program} failed with exit code {:?}: {}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    ))
}

fn take_ownership_tree(path: &Path) -> anyhow::Result<()> {
    run_hidden_command(
        "takeown.exe",
        &[
            OsString::from("/F"),
            path.as_os_str().to_os_string(),
            OsString::from("/A"),
            OsString::from("/R"),
            OsString::from("/D"),
            OsString::from("Y"),
        ],
        "Taking ownership of protected service directory",
    )
}

fn clear_attributes_tree(path: &Path) -> anyhow::Result<()> {
    run_hidden_command(
        "attrib.exe",
        &[
            OsString::from("-R"),
            OsString::from("-S"),
            OsString::from("-H"),
            path.as_os_str().to_os_string(),
        ],
        "Clearing attributes on protected service directory",
    )?;

    run_hidden_command(
        "attrib.exe",
        &[
            OsString::from("-R"),
            OsString::from("-S"),
            OsString::from("-H"),
            path.join("*").as_os_str().to_os_string(),
            OsString::from("/S"),
            OsString::from("/D"),
        ],
        "Clearing attributes inside protected service directory",
    )
}

fn recover_dir_for_removal(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if let Err(e) = take_ownership_tree(path) {
        tracing::warn!("Failed to take ownership of {:?}: {e:#}", path);
    }
    if let Err(e) = lock_down_service_dir(path) {
        tracing::warn!(
            "Failed to repair ACLs under {:?} after takeown: {e:#}; retrying with explicit owner reset",
            path
        );
        run_icacls(
            path,
            &["/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"],
            "Resetting protected service directory owner",
        )?;
        lock_down_service_dir(path)?;
    }
    if let Err(e) = clear_attributes_tree(path) {
        tracing::warn!("Failed to clear attributes under {:?}: {e:#}", path);
    }
    Ok(())
}

fn remove_dir_all_recovering(path: &Path, ctx: &str) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    match std::fs::remove_dir_all(path) {
        Ok(_) => Ok(()),
        Err(first) => {
            tracing::warn!(
                "{}: initial removal of {:?} failed: {}; attempting ACL recovery",
                ctx,
                path,
                first
            );
            recover_dir_for_removal(path).map_err(|e| {
                anyhow::anyhow!(
                    "{ctx}: failed to recover ACLs for {:?} after removal failed with {}: {e:#}",
                    path,
                    first
                )
            })?;
            std::fs::remove_dir_all(path).map_err(|e| {
                anyhow::anyhow!(
                    "{ctx}: failed to remove {:?} after ACL recovery: {} (initial error: {})",
                    path,
                    e,
                    first
                )
            })
        }
    }
}

fn staging_dir_for(dst: &Path) -> anyhow::Result<PathBuf> {
    let parent = dst
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Service install dir {:?} has no parent", dst))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Ok(parent.join(format!("zapret.stage.{}.{}", std::process::id(), stamp)))
}

fn lock_down_service_dir(dir: &Path) -> anyhow::Result<()> {
    // Locale-independent SIDs:
    //   S-1-5-32-544  Administrators  -> full control
    //   S-1-5-18      LocalSystem     -> full control (the service account)
    //   S-1-5-32-545  Users           -> read & execute only (no write)
    run_icacls(
        dir,
        &["/inheritance:r", "/T", "/C", "/Q"],
        "Removing inherited ACLs from staged service files",
    )?;
    run_icacls(
        dir,
        &[
            "/grant:r",
            "*S-1-5-32-544:F",
            "*S-1-5-18:F",
            "*S-1-5-32-545:RX",
            "/T",
            "/C",
            "/Q",
        ],
        "Granting explicit ACLs to staged service files",
    )?;
    run_icacls(
        dir,
        &[
            "/grant",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
            "/T",
            "/C",
            "/Q",
        ],
        "Granting inheritable ACLs to staged service directories",
    )
}

/// Recursively copy `src` into `dst` (creating `dst`). Used to stage the
/// service binaries into the protected machine-wide directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_symlink() {
            anyhow::bail!(
                "Refusing to copy service files through a symlink: {:?}",
                from
            );
        } else if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            copy_file_contents(&from, &to)?;
        }
    }
    Ok(())
}

/// Stage a copy of `user_install_dir` into the admin-only machine directory and
/// lock down its ACLs so non-administrators cannot write to it. Returns the
/// protected directory the service should run from.
///
/// Must be called from an elevated context (the SCM install already requires
/// elevation). This closes the privilege-escalation hole where a `LocalSystem`
/// service ran `winws.exe` out of a user-writable `%APPDATA%` directory.
pub fn prepare_protected_dir(user_install_dir: &Path) -> anyhow::Result<ProtectedDir> {
    let dst = crate::zapret::paths::service_install_dir();
    let stage = staging_dir_for(&dst)?;

    let before = collect_file_digests(user_install_dir).map_err(|e| {
        anyhow::anyhow!(
            "Failed to snapshot service source {:?}: {e:#}",
            user_install_dir
        )
    })?;

    remove_dir_all_recovering(&stage, "Failed to clear stale service staging dir")?;
    copy_dir_recursive(user_install_dir, &stage).map_err(|e| {
        let _ = remove_dir_all_recovering(&stage, "Cleaning failed service staging dir");
        anyhow::anyhow!("Failed to stage service files into {:?}: {}", stage, e)
    })?;

    let after = collect_file_digests(user_install_dir).map_err(|e| {
        let _ = remove_dir_all_recovering(&stage, "Cleaning changed service staging dir");
        anyhow::anyhow!(
            "Failed to re-snapshot service source {:?}: {e:#}",
            user_install_dir
        )
    })?;
    if before != after {
        let _ = remove_dir_all_recovering(&stage, "Cleaning changed service staging dir");
        anyhow::bail!("Service install source changed while staging; please retry");
    }
    let staged = collect_file_digests(&stage).map_err(|e| {
        let _ = remove_dir_all_recovering(&stage, "Cleaning unverifiable service staging dir");
        anyhow::anyhow!("Failed to verify staged service copy {:?}: {e:#}", stage)
    })?;
    if staged != before {
        let _ = remove_dir_all_recovering(&stage, "Cleaning mismatched service staging dir");
        anyhow::bail!("Staged service copy does not match the verified source snapshot");
    }

    if let Err(e) = lock_down_service_dir(&stage) {
        let _ = remove_dir_all_recovering(&stage, "Cleaning ACL-failed service staging dir");
        return Err(e);
    }

    remove_dir_all_recovering(&dst, "Failed to clear protected service dir")?;
    if let Err(e) = std::fs::rename(&stage, &dst) {
        let _ = remove_dir_all_recovering(&stage, "Cleaning unpromoted service staging dir");
        return Err(anyhow::anyhow!(
            "Failed to promote staged service dir {:?} to {:?}: {}",
            stage,
            dst,
            e
        ));
    }

    Ok(ProtectedDir {
        path: dst,
        snapshot: before,
    })
}

/// Best-effort teardown of a pre-existing "zapret" service that belongs to us —
/// i.e. one whose ImagePath is a `winws.exe` inside any directory in
/// `owned_dirs` (the user install dir and/or the protected machine dir). Stops
/// it (releasing the winws.exe file lock) and deletes its SCM registration so a
/// fresh install can re-create it without an ownership conflict.
///
/// A "zapret" service that points somewhere else is left untouched, so we never
/// tear down an unrelated service that merely shares the name. Caller must be
/// elevated (it's only ever reached from `install_service_protected`).
fn remove_prior_zapret_service(owned_dirs: &[PathBuf]) {
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(m) => m,
        Err(_) => return,
    };
    let service = match manager.open_service(
        "zapret",
        ServiceAccess::STOP
            | ServiceAccess::DELETE
            | ServiceAccess::QUERY_STATUS
            | ServiceAccess::QUERY_CONFIG,
    ) {
        Ok(s) => s,
        Err(e) => {
            // ERROR_SERVICE_DOES_NOT_EXIST is normal (nothing to clean up); log
            // anything else so an access problem here is visible.
            let missing = matches!(&e, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST));
            if !missing {
                tracing::warn!("remove_prior: could not open existing zapret service: {e}");
            }
            return;
        }
    };

    let ours = service_belongs_to_dirs("zapret", &service, owned_dirs).unwrap_or(false);
    if !ours {
        return;
    }

    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            if let Err(e) = service.stop() {
                tracing::warn!("remove_prior: stopping existing zapret service failed: {e}");
            }
            wait_for_stopped(&service, std::time::Duration::from_secs(10));
        }
    }
    if let Err(e) = service.delete() {
        tracing::warn!("remove_prior: deleting existing zapret service failed: {e}");
    } else {
        tracing::info!("remove_prior: removed pre-existing zapret service");
    }
    drop(service);
    wait_for_deletion(&manager, "zapret", std::time::Duration::from_secs(10));
}

/// Resolve `strategy_id` into a runnable `Strategy` from the verified staged copy
/// in `protected`, so post-staging edits in the user-writable install dir cannot
/// change the LocalSystem service command line.
fn resolve_protected_strategy(
    user_dir: &Path,
    protected: &Path,
    snapshot: &[FileDigest],
    strategy_id: &str,
    gf: crate::contracts::GameFilterMode,
) -> anyhow::Result<Strategy> {
    let bat_path = protected.join(format!("{strategy_id}.bat"));
    if let Some(s) = crate::zapret::batparse::strategy_from_bat(&bat_path, protected, gf) {
        return Ok(s);
    }

    // Some machines deny reading the ACL-locked staged .bat even though listing
    // the directory works. We may still use the user-writable source file, but
    // only if its current bytes hash exactly matches the staged copy. That keeps
    // the LocalSystem command line tied to the verified snapshot copied earlier.
    let source_bat_path = user_dir.join(format!("{strategy_id}.bat"));
    if source_bat_path.exists() && bat_path.exists() {
        let rel = format!("{strategy_id}.bat");
        let expected = digest_for_rel(snapshot, &rel);
        let source_bytes = std::fs::read(&source_bat_path).ok();
        if let (Some(expected), Some(source_bytes)) = (expected, source_bytes) {
            if source_bytes.len() as u64 == expected.len
                && hash_bytes(&source_bytes).eq_ignore_ascii_case(&expected.sha256)
            {
                let content = String::from_utf8_lossy(&source_bytes);
                if let Some(s) = strategy_from_bat_content(strategy_id, &content, protected, gf) {
                    tracing::warn!(
                        "Resolved service strategy {} from source .bat after verifying it matches the staged snapshot",
                        strategy_id
                    );
                    return Ok(s);
                }
            }
        }
    }

    let list_bats = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .map(|x| x.eq_ignore_ascii_case("bat"))
                            .unwrap_or(false)
                    })
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    Err(anyhow::anyhow!(
        "Strategy '{}' could not be resolved.\n  preset expected at: {}\n  .bat files in source {}: [{}]\n  .bat files staged at {}: [{}]",
        strategy_id,
        bat_path.display(),
        user_dir.display(),
        list_bats(user_dir).join(", "),
        protected.display(),
        list_bats(protected).join(", "),
    ))
}

/// Securely install + start the Windows service for `strategy_id`, always
/// running it out of the locked-down machine-wide directory rather than the
/// user-writable install dir.
///
/// This is the single entry point both the elevated one-shot helper
/// (`main.rs::run_elevated_task`) and the already-elevated in-app path
/// (`app.rs`) must use: it stages the files into the protected dir, locks the
/// ACLs, **re-resolves the strategy against that dir** (so the `winws.exe` path
/// *and* its `--hostlist`/`--ipset` file arguments all point at admin-only
/// locations), then registers the `LocalSystem` service. Skipping this — e.g.
/// installing straight from `%APPDATA%` because the app already happens to be
/// elevated — would reopen the privilege-escalation hole described in
/// `paths::service_install_dir`.
pub async fn install_service_protected(
    user_install_dir: &Path,
    strategy_id: &str,
) -> anyhow::Result<()> {
    check_elevation()?;
    // Tear down any prior "zapret" service of ours first. This matters for two
    // reasons, both of which otherwise make a reinstall silently fail:
    //   1. A service still running out of the protected dir locks its winws.exe,
    //      so `prepare_protected_dir`'s `remove_dir_all` errors with "in use".
    //   2. Older builds registered the service pointing at the user-writable
    //      install dir (%APPDATA%); the new ownership check in `install()` only
    //      accepts the protected dir, so it would reject that stale service as
    //      "a different service named zapret". Removing it here clears the path.
    remove_prior_zapret_service(&[
        user_install_dir.to_path_buf(),
        crate::zapret::paths::service_install_dir(),
    ]);
    let protected = prepare_protected_dir(user_install_dir)?;
    // Resolve the exact preset snapshot we just copied and verified. Reading from
    // the staged copy avoids a race where the user-writable .bat changes after
    // staging but before CreateService.
    let gf = crate::zapret::batparse::read_game_filter(&protected.path);
    let strategy = resolve_protected_strategy(
        user_install_dir,
        &protected.path,
        &protected.snapshot,
        strategy_id,
        gf,
    )?;
    let ctl = WindowsServiceCtl::new(protected.path);
    ctl.install(&strategy).await?;
    ctl.start().await?;
    Ok(())
}

pub struct WindowsServiceCtl {
    install_dir: PathBuf,
    service_name: String,
}

impl WindowsServiceCtl {
    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            install_dir,
            service_name: "zapret".to_string(),
        }
    }

    pub fn with_service_name(mut self, name: String) -> Self {
        self.service_name = name;
        self
    }

    fn get_winws_path(&self) -> PathBuf {
        let bin_path = self.install_dir.join("bin").join("winws.exe");
        if bin_path.exists() {
            bin_path
        } else {
            self.install_dir.join("winws.exe")
        }
    }
}

/// ERROR_SERVICE_DOES_NOT_EXIST — the service name is simply not registered.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
/// ERROR_SERVICE_ALREADY_RUNNING — `StartService` on a service that is already
/// running. Benign: the bypass is up, so we treat a redundant start as success.
const ERROR_SERVICE_ALREADY_RUNNING: i32 = 1056;
/// ERROR_SERVICE_NOT_ACTIVE — `ControlService(STOP)` on an already-stopped
/// service. Benign: the desired end state (stopped) already holds.
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
/// ERROR_ACCESS_DENIED — SCM/service DACL does not allow the requested operation.
const ERROR_ACCESS_DENIED: i32 = 5;

fn is_access_denied(e: &windows_service::Error) -> bool {
    matches!(e, windows_service::Error::Winapi(io) if io.raw_os_error() == Some(ERROR_ACCESS_DENIED))
}

fn repair_service_dacl(service_name: &str) -> anyhow::Result<()> {
    // Grant LocalSystem and Administrators full service control, while leaving
    // interactive/service users query-only. This mirrors the default SCM shape
    // but makes upgrades resilient if an older/broken service DACL got persisted.
    const SERVICE_DACL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";
    let mut cmd = std::process::Command::new("sc.exe");
    cmd.args(["sdset", service_name, SERVICE_DACL]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "sc sdset {} failed: {}{}",
            service_name,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ))
    }
}

fn start_service_via_sc(service_name: &str) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new("sc.exe");
    cmd.args(["start", service_name]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "sc start {} failed: {}{}",
            service_name,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ))
    }
}

fn open_service_with_repair(
    manager: &ServiceManager,
    name: &str,
    access: ServiceAccess,
    ctx: &str,
) -> anyhow::Result<windows_service::service::Service> {
    match manager.open_service(name, access) {
        Ok(service) => Ok(service),
        Err(e) if is_access_denied(&e) => {
            repair_service_dacl(name)?;
            manager
                .open_service(name, access)
                .map_err(|e| svc_err(ctx, e))
        }
        Err(e) => Err(svc_err(ctx, e)),
    }
}

/// Poll a service's state until it reaches `Stopped` or the timeout elapses.
fn wait_for_stopped(service: &windows_service::service::Service, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => break,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

/// Poll the SCM until the named service is gone (post-`delete`) or timeout.
fn wait_for_deletion(manager: &ServiceManager, name: &str, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if manager
            .open_service(name, ServiceAccess::QUERY_STATUS)
            .is_err()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
}

#[async_trait::async_trait]
impl ServiceCtl for WindowsServiceCtl {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
        check_elevation()?;

        let winws_path = self.get_winws_path();
        if !winws_path.exists() {
            return Err(anyhow::anyhow!("winws.exe not found at {:?}", winws_path));
        }

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )
        .map_err(|e| svc_err("OpenSCManager(CREATE_SERVICE)", e))?;

        // If a "zapret" service already exists, stop and delete it first — otherwise
        // create_service fails with ERROR_SERVICE_EXISTS. But only if it's *ours*:
        // refuse to touch a same-named service that points somewhere else, so we
        // never tear down an unrelated service that happens to be called "zapret".
        if let Ok(existing) = open_service_with_repair(
            &manager,
            &self.service_name,
            ServiceAccess::ALL_ACCESS,
            "OpenService(existing)",
        ) {
            let owned_dirs = [
                self.install_dir.clone(),
                crate::zapret::paths::service_install_dir(),
            ];
            let owned = service_belongs_to_dirs(&self.service_name, &existing, &owned_dirs)
                .unwrap_or(false);
            if !owned {
                return Err(service_ownership_error(&self.service_name));
            }
            if let Ok(status) = existing.query_status() {
                if status.current_state != ServiceState::Stopped {
                    let _ = existing.stop();
                    wait_for_stopped(&existing, std::time::Duration::from_secs(10));
                }
            }
            existing
                .delete()
                .map_err(|e| svc_err("DeleteService(existing)", e))?;
            // Deletion is finalized once all handles close; wait for the name to free up.
            drop(existing);
            wait_for_deletion(
                &manager,
                &self.service_name,
                std::time::Duration::from_secs(10),
            );
        }

        // Prepare the launch arguments.
        let launch_arguments: Vec<OsString> = strategy
            .winws_args
            .iter()
            .map(|s| OsString::from(s.as_str()))
            .collect();

        // Ensure user list files exist so the service's winws.exe can start.
        crate::zapret::batparse::ensure_user_lists(&self.install_dir)?;

        // Create the ServiceInfo structure.
        let service_info = ServiceInfo {
            name: OsString::from(&self.service_name),
            display_name: OsString::from(&self.service_name),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: winws_path,
            launch_arguments,
            dependencies: Vec::new(),
            account_name: None,
            account_password: None,
        };

        // Create, retrying briefly on ERROR_SERVICE_MARKED_FOR_DELETE in case the
        // old service's deletion hasn't fully settled on a slow machine.
        const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;
        let mut attempt = 0;
        loop {
            match manager.create_service(&service_info, ServiceAccess::ALL_ACCESS) {
                Ok(_) => {
                    repair_service_dacl(&self.service_name)?;
                    break;
                }
                Err(e) => {
                    let marked = matches!(
                        &e,
                        windows_service::Error::Winapi(io)
                            if io.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE)
                    );
                    if marked && attempt < 20 {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        continue;
                    }
                    return Err(svc_err("CreateService", e));
                }
            }
        }

        Ok(())
    }

    async fn remove(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| svc_err("OpenSCManager(remove)", e))?;
        let service = open_service_with_repair(
            &manager,
            &self.service_name,
            ServiceAccess::ALL_ACCESS,
            "OpenService(remove)",
        )?;

        let owned_dirs = [
            self.install_dir.clone(),
            crate::zapret::paths::service_install_dir(),
        ];
        if !service_belongs_to_dirs(&self.service_name, &service, &owned_dirs)? {
            return Err(service_ownership_error(&self.service_name));
        }

        // Stop the service first; Windows will not actually remove it until all
        // handles are closed and it has stopped running. Without this the service
        // entry stays in the SCM and the next `install` call fails with
        // ERROR_SERVICE_MARKED_FOR_DELETE.
        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                let _ = service.stop();
                wait_for_stopped(&service, std::time::Duration::from_secs(10));
            }
        }

        service.delete().map_err(|e| svc_err("DeleteService", e))?;
        // Wait for the SCM to actually drop the registration so a follow-up
        // install/refresh sees a consistent state.
        drop(service);
        wait_for_deletion(
            &manager,
            &self.service_name,
            std::time::Duration::from_secs(10),
        );
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| svc_err("OpenSCManager(start)", e))?;
        // Also ask for QUERY_STATUS so we can confirm the service actually came up
        // (StartService only reports that the process was *created*, see below).
        let service = open_service_with_repair(
            &manager,
            &self.service_name,
            ServiceAccess::ALL_ACCESS,
            "OpenService(start)",
        )?;

        let owned_dirs = [
            self.install_dir.clone(),
            crate::zapret::paths::service_install_dir(),
        ];
        if !service_belongs_to_dirs(&self.service_name, &service, &owned_dirs)? {
            return Err(service_ownership_error(&self.service_name));
        }

        match service.start(&[] as &[&str]) {
            Ok(_) => {}
            // Already running → the bypass is up; a redundant start is a no-op, not
            // a failure. (Without this it surfaces as a scary "IO error in winapi
            // call" even though traffic is being processed.)
            Err(windows_service::Error::Winapi(io))
                if io.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING) =>
            {
                return Ok(());
            }
            Err(e) if is_access_denied(&e) => {
                repair_service_dacl(&self.service_name)?;
                let service = open_service_with_repair(
                    &manager,
                    &self.service_name,
                    ServiceAccess::ALL_ACCESS,
                    "OpenService(start-retry)",
                )?;
                match service.start(&[] as &[&str]) {
                    Ok(_) => {}
                    Err(windows_service::Error::Winapi(io))
                        if io.raw_os_error() == Some(ERROR_SERVICE_ALREADY_RUNNING) => {}
                    Err(e) if is_access_denied(&e) => {
                        tracing::warn!(
                            "StartService still returned access denied after DACL repair; falling back to sc.exe start"
                        );
                        start_service_via_sc(&self.service_name)?;
                    }
                    Err(e) => return Err(svc_err("StartService", e)),
                }
            }
            Err(e) => return Err(svc_err("StartService", e)),
        }

        // StartService only reports whether the service *process* was created. A
        // winws.exe that launches as a service but then exits during init — most
        // commonly because another bypass still holds the WinDivert driver, or the
        // chosen strategy's arguments are rejected — still returns success above.
        // Briefly watch the service state: if it falls back to Stopped, surface
        // winws's exit code instead of pretending the start worked. Any other
        // state (Running / StartPending / …) means it's up, so we return at once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match service.query_status() {
                Ok(s) if s.current_state == ServiceState::Stopped => {
                    let code = match s.exit_code {
                        ServiceExitCode::Win32(c) => c,
                        ServiceExitCode::ServiceSpecific(c) => c,
                    };
                    return Err(anyhow::anyhow!(
                        "The zapret service started but winws.exe exited immediately \
                         (exit code {code}). This usually means another bypass is already \
                         using the WinDivert driver (stop any running bypass first), or the \
                         selected strategy's arguments are invalid."
                    ));
                }
                // Confirmed up.
                Ok(s) if s.current_state == ServiceState::Running => break,
                // StartPending / other transitional state — keep watching until the
                // grace period elapses (a winws that hasn't reported Running yet may
                // still be coming up, or may be about to fall back to Stopped).
                Ok(_) => {}
                // Transient query hiccup — don't fail a successful start over it.
                Err(_) => break,
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| svc_err("OpenSCManager(stop)", e))?;
        let service = open_service_with_repair(
            &manager,
            &self.service_name,
            ServiceAccess::ALL_ACCESS,
            "OpenService(stop)",
        )?;

        let owned_dirs = [
            self.install_dir.clone(),
            crate::zapret::paths::service_install_dir(),
        ];
        if !service_belongs_to_dirs(&self.service_name, &service, &owned_dirs)? {
            return Err(service_ownership_error(&self.service_name));
        }

        match service.stop() {
            Ok(_) => {}
            // Already stopped → the desired end state already holds; not an error.
            Err(windows_service::Error::Winapi(io))
                if io.raw_os_error() == Some(ERROR_SERVICE_NOT_ACTIVE) => {}
            Err(e) => return Err(svc_err("ControlService(STOP)", e)),
        }
        // Wait for it to actually reach Stopped so the UI status is accurate.
        wait_for_stopped(&service, std::time::Duration::from_secs(10));
        Ok(())
    }

    async fn status(&self) -> anyhow::Result<RunningMode> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| svc_err("OpenSCManager(status)", e))?;

        let service_res = manager.open_service(
            &self.service_name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        );

        match service_res {
            Ok(service) => {
                let owned_dirs = [
                    self.install_dir.clone(),
                    crate::zapret::paths::service_install_dir(),
                ];
                if !service_belongs_to_dirs(&self.service_name, &service, &owned_dirs)? {
                    return Ok(RunningMode::None);
                }
                let status = service
                    .query_status()
                    .map_err(|e| svc_err("QueryServiceStatus", e))?;
                if status.current_state == ServiceState::Running {
                    Ok(RunningMode::WindowsService)
                } else {
                    Ok(RunningMode::None)
                }
            }
            Err(windows_service::Error::Winapi(io))
                if io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
            {
                // Genuinely not registered → not running.
                Ok(RunningMode::None)
            }
            Err(e) => {
                // Access denied / SCM problem: surface it rather than silently
                // claiming "not running" (the caller logs and keeps the prior state).
                Err(svc_err("OpenService(status)", e))
            }
        }
    }

    async fn is_installed(&self) -> bool {
        let manager =
            match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
                Ok(m) => m,
                Err(_) => return false,
            };
        let Ok(service) = manager.open_service(&self.service_name, ServiceAccess::QUERY_CONFIG)
        else {
            return false;
        };
        let owned_dirs = [
            self.install_dir.clone(),
            crate::zapret::paths::service_install_dir(),
        ];
        service_belongs_to_dirs(&self.service_name, &service, &owned_dirs).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_image_ownership_respects_path_boundaries() {
        let owned = vec![PathBuf::from(r"C:\ProgramData\zapret-ui\zapret")];
        assert!(service_image_belongs_to_dirs(
            OsStr::new(r#""C:\ProgramData\zapret-ui\zapret\bin\winws.exe" --wf-tcp=80"#),
            &owned,
        ));
        assert!(!service_image_belongs_to_dirs(
            OsStr::new(r#""C:\ProgramData\zapret-ui\zapret2\bin\winws.exe" --wf-tcp=80"#),
            &owned,
        ));
    }
}
