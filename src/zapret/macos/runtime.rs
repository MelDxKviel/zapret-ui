//! ZapretMac is always supervised by launchd; there is no unprivileged utun mode.
use crate::contracts::{RunningMode, RuntimeStatus, Strategy};
use crate::ports::{Runner, StrategyCatalog};
use crate::zapret::{catalog::LocalStrategyCatalog, macos_bundle as bundle};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

static OPERATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const ENGINE_PATH: &str = "/Library/Application Support/ZapretMac/bin/utunws";
const RECOVERY_FILES: [&str; 2] = [
    "/var/run/zapret-macos.pf-token",
    "/var/db/zapret-macos.keepinit",
];

pub struct ProcessRunner {
    install_dir: PathBuf,
}
impl ProcessRunner {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }
}

/// Use macOS's password dialog, keeping the GUI and its network probes under
/// the login user. Upstream's PF rules explicitly exclude root traffic.
pub async fn privileged(command: &str) -> Result<()> {
    let script = format!("with timeout of 300 seconds\ndo shell script {} with administrator privileges\nend timeout", bundle::applescript_string(command));
    let output = tokio::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .output()
        .await
        .context("Opening macOS authorization dialog")?;
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains("(-128)") {
            return Err(crate::contracts::AuthorizationCancelled.into());
        }
        bail!(
            "macOS authorization or core operation failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub fn registered() -> bool {
    Path::new(bundle::SERVICE_PLIST).is_file()
}

fn trusted_script(name: &str) -> Result<PathBuf> {
    let root = Path::new(bundle::SERVICE_ROOT);
    let script = root.join(name);
    protected_path(root, true)?;
    protected_path(&script, false)?;
    Ok(script)
}

fn protected_path(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    if m.uid() != 0
        || m.mode() & 0o022 != 0
        || (directory && !m.is_dir())
        || (!directory && !m.is_file())
    {
        bail!(
            "Refusing unprotected privileged core path: {}",
            path.display()
        );
    }
    Ok(())
}

fn check_service_owner() -> Result<()> {
    // A leftover root or plist can exist before the first installation. Do not
    // let upstream rsync/sed follow an untrusted link while running as root.
    for (path, directory) in [(bundle::SERVICE_ROOT, true), (bundle::SERVICE_PLIST, false)] {
        match std::fs::symlink_metadata(path) {
            Ok(_) => protected_path(Path::new(path), directory)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if registered() {
        let plist = std::fs::read_to_string(bundle::SERVICE_PLIST)?;
        if !plist.contains("<string>/Library/Application Support/ZapretMac/run.sh</string>") {
            bail!("The ZapretMac launchd label belongs to a different service");
        }
        trusted_script("stop.sh")?;
    }
    Ok(())
}

pub async fn engine_process() -> Option<(u32, u64)> {
    let out = tokio::process::Command::new("/bin/launchctl")
        .args(["print", bundle::SERVICE_LABEL])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let supervisor: u32 = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))?
        .parse()
        .ok()?;
    tokio::task::spawn_blocking(move || engine_child(supervisor, Path::new(ENGINE_PATH)))
        .await
        .ok()
        .flatten()
}

fn engine_child(supervisor: u32, executable: &Path) -> Option<(u32, u64)> {
    use std::ffi::c_void;
    unsafe extern "C" {
        fn proc_listpids(kind: u32, typeinfo: u32, buffer: *mut c_void, size: i32) -> i32;
    }
    // PROC_PPID_ONLY (libproc.h/sys/proc_info.h) asks the kernel for children of
    // this launchd supervisor. Avoid reading paths, CPU, memory and disk usage
    // for every process on the Mac during the periodic status refresh.
    let mut children = [0i32; 64];
    let capacity = std::mem::size_of_val(&children);
    // SAFETY: libproc writes at most capacity bytes to this initialized buffer.
    let bytes =
        unsafe { proc_listpids(6, supervisor, children.as_mut_ptr().cast(), capacity as i32) };
    if bytes <= 0 || bytes as usize > capacity {
        return None;
    }
    let pids = children[..bytes as usize / std::mem::size_of::<i32>()]
        .iter()
        .filter(|pid| **pid > 0)
        .map(|pid| sysinfo::Pid::from_u32(*pid as u32))
        .collect::<Vec<_>>();
    if pids.is_empty() {
        return None;
    }
    let mut sys = sysinfo::System::new();
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&pids),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
    );
    sys.processes().iter().find_map(|(pid, p)| {
        // Recheck the parent after lookup to reject a reused PID.
        (p.parent().map(|p| p.as_u32()) == Some(supervisor) && p.exe() == Some(executable))
            .then(|| (pid.as_u32(), p.run_time()))
    })
}

async fn await_engine() -> Result<u32> {
    for _ in 0..30 {
        if let Some((pid, _)) = engine_process().await {
            // utunws creates utun50 before installing the PF routes.
            let ready = tokio::process::Command::new("/sbin/ifconfig")
                .arg("utun50")
                .output()
                .await?;
            if ready.status.success() {
                return Ok(pid);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let log = std::fs::read_to_string(Path::new(bundle::SERVICE_ROOT).join("engine.log"))
        .unwrap_or_default();
    let tail = log
        .lines()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "ZapretMac did not start. Check the network connection and disable tunneling VPNs.\n{tail}"
    );
}

pub async fn start_strategy(install: &Path, strategy: &Strategy) -> Result<u32> {
    let _operation = OPERATION.lock().await;
    if LocalStrategyCatalog::new(install.to_owned())
        .by_id(&strategy.id)
        .is_none()
    {
        bail!("Unknown ZapretMac strategy: {}", strategy.id);
    }
    if !bundle::valid_payload(install) {
        bail!("Install the ZapretMac core first");
    }
    check_service_owner()?;
    let data = bundle::user_data_dir()?;
    bundle::validate_data_dir(&data)?;
    bundle::initialize_user_data(install, &data)?;
    let old_strategy = std::fs::read(data.join("selected-strategy")).ok();
    std::fs::write(data.join("selected-strategy"), format!("{}\n", strategy.id))?;
    // Reinstall on each explicit start: the protected copy and its lists must
    // match the downloaded release, including after a core update/user switch.
    let source = std::fs::canonicalize(install)?;
    let result = async {
        privileged(&bundle::install_command(&source, &data)).await?;
        match await_engine().await {
            Ok(pid) => {
                tracing::info!(target: "utunws", "Started {} (PID {pid})", strategy.display_name);
                Ok(pid)
            }
            Err(error) => {
                // Restore PF/sysctl after an incomplete startup, just as upstream does.
                let _ = stop_engine_unlocked().await;
                Err(error)
            }
        }
    }
    .await;
    if result.is_err() {
        if let Some(old) = old_strategy {
            let _ = std::fs::write(data.join("selected-strategy"), old);
        }
    }
    result
}

pub async fn stop_engine() -> Result<()> {
    let _operation = OPERATION.lock().await;
    stop_engine_unlocked().await
}

async fn stop_engine_unlocked() -> Result<()> {
    check_service_owner()?;
    let loaded = tokio::process::Command::new("/bin/launchctl")
        .args(["print", bundle::SERVICE_LABEL])
        .output()
        .await?;
    // A crashed/unloaded daemon may still have the saved TCP value or PF
    // enable token. stop.sh restores both even when launchctl has no job.
    if !loaded.status.success() && !RECOVERY_FILES.iter().any(|path| Path::new(path).exists()) {
        return Ok(());
    }
    let script = trusted_script("stop.sh")?;
    privileged(&format!(
        "/bin/sh {}",
        bundle::shell_quote(&script.to_string_lossy())
    ))
    .await?;
    if engine_process().await.is_some() {
        bail!("ZapretMac is still running after stop");
    }
    tracing::info!(target: "utunws", "Stopped; upstream PF rules and TCP settings restored");
    Ok(())
}

pub async fn remove_service() -> Result<()> {
    let _operation = OPERATION.lock().await;
    if !registered() {
        return Ok(());
    }
    check_service_owner()?;
    let script = trusted_script("stop.sh")?;
    privileged(&format!(
        "/bin/sh {} && /bin/rm -f {}",
        bundle::shell_quote(&script.to_string_lossy()),
        bundle::shell_quote(bundle::SERVICE_PLIST)
    ))
    .await
}

#[async_trait::async_trait]
impl Runner for ProcessRunner {
    async fn start(&self, strategy: &Strategy) -> Result<u32> {
        start_strategy(&self.install_dir, strategy).await
    }
    async fn stop(&self) -> Result<()> {
        stop_engine().await
    }
    async fn detect_running(&self) -> RuntimeStatus {
        let process = engine_process().await;
        RuntimeStatus {
            installed: bundle::valid_payload(&self.install_dir),
            installed_version: std::fs::read_to_string(self.install_dir.join("version.txt"))
                .ok()
                .map(|s| s.trim().into()),
            running_mode: if process.is_some() {
                RunningMode::SystemService
            } else {
                RunningMode::None
            },
            active_strategy: process
                .and_then(|_| bundle::user_data_dir().ok())
                .and_then(|p| std::fs::read_to_string(p.join("selected-strategy")).ok())
                .map(|s| s.trim().into()),
            winws_pid: process.map(|p| p.0),
            service_installed: registered(),
            uptime_secs: process.map(|p| p.1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_matching_child_without_network_or_privileges() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let parent = std::process::id();
        let found = engine_child(parent, Path::new("/bin/sleep"));
        assert_eq!(found.map(|p| p.0), Some(child.0.id()));
        assert!(engine_child(parent, Path::new(ENGINE_PATH)).is_none());
        assert!(engine_child(child.0.id(), Path::new("/bin/sleep")).is_none());
    }
}
