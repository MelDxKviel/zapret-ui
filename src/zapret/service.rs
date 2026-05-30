use std::path::{Path, PathBuf};
use std::ffi::OsString;
use anyhow::Context;
use crate::contracts::{Strategy, RunningMode};
use crate::ports::ServiceCtl;
use crate::zapret::elevation::check_elevation;
use windows_service::{
    service::{
        ServiceAccess, ServiceState, ServiceType, ServiceStartType, ServiceErrorControl, ServiceInfo,
    },
    service_manager::{ServiceManager, ServiceManagerAccess},
};

/// Recursively copy `src` into `dst` (creating `dst`). Used to stage the
/// service binaries into the protected machine-wide directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
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
pub fn prepare_protected_dir(user_install_dir: &Path) -> anyhow::Result<PathBuf> {
    let dst = crate::zapret::paths::service_install_dir();

    // Refresh the protected copy from the (verified) user install.
    if dst.exists() {
        std::fs::remove_dir_all(&dst)
            .map_err(|e| anyhow::anyhow!("Failed to clear protected service dir {:?}: {}", dst, e))?;
    }
    copy_dir_recursive(user_install_dir, &dst)
        .map_err(|e| anyhow::anyhow!("Failed to stage service files into {:?}: {}", dst, e))?;

    // Lock down ACLs using well-known SIDs (locale-independent):
    //   S-1-5-32-544  Administrators  -> full control
    //   S-1-5-18      LocalSystem     -> full control (the service account)
    //   S-1-5-32-545  Users           -> read & execute only (no write)
    // `/inheritance:r` drops inherited (user-writable) ACEs first.
    let status = std::process::Command::new("icacls")
        .arg(&dst)
        .args([
            "/inheritance:r",
            "/grant:r", "*S-1-5-32-544:(OI)(CI)F",
            "/grant:r", "*S-1-5-18:(OI)(CI)F",
            "/grant:r", "*S-1-5-32-545:(OI)(CI)RX",
            "/T", "/C", "/Q",
        ])
        .output();
    match status {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return Err(anyhow::anyhow!(
                "Failed to lock down service directory ACLs: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Err(e) => return Err(anyhow::anyhow!("Failed to run icacls on service directory: {}", e)),
    }

    Ok(dst)
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
async fn remove_prior_zapret_service(owned_dirs: &[PathBuf]) {
    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
        Ok(m) => m,
        Err(_) => return,
    };
    let service = match manager.open_service(
        "zapret",
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
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

    let ours = match service.query_config() {
        Ok(cfg) => {
            // The stored ImagePath of a service-with-args is the full quoted
            // command line (`"C:\...\winws.exe" --wf-tcp=... ...`), so `file_name()`
            // / `starts_with()` on it don't match. Compare as a case-insensitive
            // substring instead: it must run winws.exe out of one of our dirs.
            let image = cfg.executable_path.to_string_lossy().to_lowercase();
            image.contains("winws.exe")
                && owned_dirs.iter().any(|d| {
                    let d = d.to_string_lossy().to_lowercase();
                    !d.is_empty() && image.contains(&d)
                })
        }
        Err(_) => false,
    };
    if !ours {
        return;
    }

    if let Ok(status) = service.query_status() {
        if status.current_state != ServiceState::Stopped {
            if let Err(e) = service.stop() {
                tracing::warn!("remove_prior: stopping existing zapret service failed: {e}");
            }
            wait_for_stopped(&service, std::time::Duration::from_secs(10)).await;
        }
    }
    if let Err(e) = service.delete() {
        tracing::warn!("remove_prior: deleting existing zapret service failed: {e}");
    } else {
        tracing::info!("remove_prior: removed pre-existing zapret service");
    }
    drop(service);
    wait_for_deletion(&manager, "zapret", std::time::Duration::from_secs(10)).await;
}

/// Resolve `strategy_id` into a runnable `Strategy` whose winws.exe arg paths
/// all point at `protected`, reading the preset template from `user_dir` (which
/// is always readable, unlike the freshly ACL-locked protected copy). The id is
/// the `.bat` file stem, so the preset is `user_dir/<id>.bat`. On a missing /
/// unparseable preset it returns a diagnostic error listing the `.bat` files
/// actually present in both locations.
fn resolve_protected_strategy(
    user_dir: &Path,
    protected: &Path,
    strategy_id: &str,
    gf: crate::contracts::GameFilterMode,
) -> anyhow::Result<Strategy> {
    let bat_path = user_dir.join(format!("{strategy_id}.bat"));
    if let Some(s) = crate::zapret::batparse::strategy_from_bat(&bat_path, protected, gf) {
        return Ok(s);
    }
    let list_bats = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x.eq_ignore_ascii_case("bat")).unwrap_or(false))
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
pub async fn install_service_protected(user_install_dir: &Path, strategy_id: &str) -> anyhow::Result<()> {
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
    ])
    .await;
    let protected = prepare_protected_dir(user_install_dir)?;
    // Resolve the preset from the user dir's own .bat (always readable) but with
    // every %BIN%/%LISTS%/%~dp0 path rebased onto the protected dir. We do NOT
    // re-scan the freshly-staged copy: that scan proved unreliable in the field
    // (it can come back empty even when the copy succeeded, surfacing as a bogus
    // "Strategy not found"). winws.exe and the hostlist/ipset files are still
    // taken from the ACL-locked protected dir, so the service never runs out of
    // the user-writable location — the escalation fix is preserved.
    let gf = crate::zapret::batparse::read_game_filter(user_install_dir);
    let strategy = resolve_protected_strategy(user_install_dir, &protected, strategy_id, gf)?;
    let ctl = WindowsServiceCtl::new(protected);
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

/// Poll a service's state until it reaches `Stopped` or the timeout elapses.
async fn wait_for_stopped(service: &windows_service::service::Service, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match service.query_status() {
            Ok(s) if s.current_state == ServiceState::Stopped => break,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// Poll the SCM until the named service is gone (post-`delete`) or timeout.
async fn wait_for_deletion(manager: &ServiceManager, name: &str, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if manager.open_service(name, ServiceAccess::QUERY_STATUS).is_err() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
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
        ).context("OpenSCManager(CREATE_SERVICE)")?;

        // If a "zapret" service already exists, stop and delete it first — otherwise
        // create_service fails with ERROR_SERVICE_EXISTS. But only if it's *ours*:
        // refuse to touch a same-named service that points somewhere else, so we
        // never tear down an unrelated service that happens to be called "zapret".
        if let Ok(existing) = manager.open_service(
            &self.service_name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        ) {
            let owned = match existing.query_config() {
                Ok(cfg) => {
                    // The ImagePath is the full quoted command line, so match it as a
                    // case-insensitive substring (see remove_prior_zapret_service):
                    // ours if it runs winws.exe out of our dir or the machine dir.
                    let image = cfg.executable_path.to_string_lossy().to_lowercase();
                    let here = self.install_dir.to_string_lossy().to_lowercase();
                    let svc_dir = crate::zapret::paths::service_install_dir().to_string_lossy().to_lowercase();
                    image.contains("winws.exe")
                        && ((!here.is_empty() && image.contains(&here))
                            || (!svc_dir.is_empty() && image.contains(&svc_dir)))
                }
                Err(_) => false,
            };
            if !owned {
                return Err(anyhow::anyhow!(
                    "A different service named \"{}\" already exists and does not belong to zapret-ui. \
                     Remove it manually before installing.",
                    self.service_name
                ));
            }
            if let Ok(status) = existing.query_status() {
                if status.current_state != ServiceState::Stopped {
                    let _ = existing.stop();
                    wait_for_stopped(&existing, std::time::Duration::from_secs(10)).await;
                }
            }
            existing.delete().context("DeleteService(existing)")?;
            // Deletion is finalized once all handles close; wait for the name to free up.
            drop(existing);
            wait_for_deletion(&manager, &self.service_name, std::time::Duration::from_secs(10)).await;
        }

        // Prepare the launch arguments.
        let launch_arguments: Vec<OsString> = strategy.winws_args.iter().map(|s| OsString::from(s.as_str())).collect();

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
                Ok(_) => break,
                Err(e) => {
                    let marked = matches!(
                        &e,
                        windows_service::Error::Winapi(io)
                            if io.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE)
                    );
                    if marked && attempt < 20 {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        continue;
                    }
                    return Err(anyhow::Error::new(e).context("CreateService"));
                }
            }
        }

        Ok(())
    }

    async fn remove(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ).context("OpenSCManager(remove)")?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
        ).context("OpenService(remove)")?;

        // Stop the service first; Windows will not actually remove it until all
        // handles are closed and it has stopped running. Without this the service
        // entry stays in the SCM and the next `install` call fails with
        // ERROR_SERVICE_MARKED_FOR_DELETE.
        if let Ok(status) = service.query_status() {
            if status.current_state != ServiceState::Stopped {
                let _ = service.stop();
                wait_for_stopped(&service, std::time::Duration::from_secs(10)).await;
            }
        }

        service.delete().context("DeleteService")?;
        // Wait for the SCM to actually drop the registration so a follow-up
        // install/refresh sees a consistent state.
        drop(service);
        wait_for_deletion(&manager, &self.service_name, std::time::Duration::from_secs(10)).await;
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ).context("OpenSCManager(start)")?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::START,
        ).context("OpenService(start)")?;

        service.start(&[] as &[&str]).context("StartService")?;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        check_elevation()?;

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ).context("OpenSCManager(stop)")?;
        let service = manager.open_service(
            &self.service_name,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS,
        ).context("OpenService(stop)")?;

        service.stop().context("ControlService(STOP)")?;
        // Wait for it to actually reach Stopped so the UI status is accurate.
        wait_for_stopped(&service, std::time::Duration::from_secs(10)).await;
        Ok(())
    }

    async fn status(&self) -> anyhow::Result<RunningMode> {
        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        )?;

        let service_res = manager.open_service(
            &self.service_name,
            ServiceAccess::QUERY_STATUS,
        );

        match service_res {
            Ok(service) => {
                let status = service.query_status()?;
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
                Err(anyhow::anyhow!("Failed to query service status: {}", e))
            }
        }
    }

    async fn is_installed(&self) -> bool {
        let manager = match ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT,
        ) {
            Ok(m) => m,
            Err(_) => return false,
        };
        // Opening with QUERY_STATUS succeeds iff the service is registered.
        manager
            .open_service(&self.service_name, ServiceAccess::QUERY_STATUS)
            .is_ok()
    }
}
