//! ZapretMac is always supervised by launchd; there is no unprivileged utun mode.
use crate::contracts::{RunningMode, RuntimeStatus, Strategy};
use crate::ports::{Runner, StrategyCatalog};
use crate::zapret::{catalog::LocalStrategyCatalog, macos_bundle as bundle};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

static OPERATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    use std::os::unix::fs::MetadataExt;
    let root = Path::new(bundle::SERVICE_ROOT);
    let script = root.join(name);
    for path in [root, script.as_path()] {
        let m = std::fs::symlink_metadata(path)?;
        if m.file_type().is_symlink() || m.uid() != 0 || m.mode() & 0o022 != 0 {
            bail!(
                "Refusing unprotected privileged core path: {}",
                path.display()
            );
        }
    }
    Ok(script)
}

fn check_service_owner() -> Result<()> {
    if !registered() {
        return Ok(());
    }
    let plist = std::fs::read_to_string(bundle::SERVICE_PLIST)?;
    if !plist.contains("<string>/Library/Application Support/ZapretMac/run.sh</string>") {
        bail!("The ZapretMac launchd label belongs to a different service");
    }
    trusted_script("stop.sh")?;
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
    tokio::task::spawn_blocking(move || {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        sys.processes().iter().find_map(|(pid, p)| {
            (p.parent().map(|p| p.as_u32()) == Some(supervisor)
                && p.exe()
                    == Some(Path::new(
                        "/Library/Application Support/ZapretMac/bin/utunws",
                    )))
            .then(|| (pid.as_u32(), p.run_time()))
        })
    })
    .await
    .ok()
    .flatten()
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
    let q = |p: &Path| bundle::shell_quote(&p.to_string_lossy());
    let result = async {
        privileged(&format!(
            "/bin/sh {} {} {}",
            q(&source.join("install.sh")),
            q(&source),
            q(&data)
        ))
        .await?;
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
    if !registered() {
        return Ok(());
    }
    check_service_owner()?;
    let loaded = tokio::process::Command::new("/bin/launchctl")
        .args(["print", bundle::SERVICE_LABEL])
        .output()
        .await?;
    if !loaded.status.success() {
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
        // Avoid authorization dialogs on an initial install/status-only operation.
        if engine_process().await.is_none() && !registered() {
            return Ok(());
        }
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
