use crate::contracts::{RunningMode, RuntimeStatus, Strategy};
use crate::ports::Runner;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use sysinfo::System;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

extern "system" {
    fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
}

pub struct ProcessRunner {
    install_dir: PathBuf,
    active_child: Arc<Mutex<Option<tokio::process::Child>>>,
    active_strategy_id: Arc<Mutex<Option<String>>>,
    active_started_at: Arc<Mutex<Option<Instant>>>,
    process_snapshot: Arc<Mutex<System>>,
    tcp_preflight: bool,
    service_name: String,
}

impl ProcessRunner {
    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            install_dir,
            active_child: Arc::new(Mutex::new(None)),
            active_strategy_id: Arc::new(Mutex::new(None)),
            active_started_at: Arc::new(Mutex::new(None)),
            process_snapshot: Arc::new(Mutex::new(System::new())),
            tcp_preflight: true,
            service_name: "zapret".to_string(),
        }
    }

    pub fn with_service_name(mut self, name: String) -> Self {
        self.service_name = name;
        self
    }

    /// Disable the machine-wide TCP prerequisite for isolated process tests.
    /// Production runners keep it enabled by default.
    pub fn with_tcp_preflight(mut self, enabled: bool) -> Self {
        self.tcp_preflight = enabled;
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

    /// The set of canonical winws.exe paths we consider "ours": the configured
    /// install dir and the protected machine-wide service dir.
    fn owned_winws_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let p = self.get_winws_path();
        out.push(p.canonicalize().unwrap_or(p));
        let svc_dir = crate::zapret::paths::service_install_dir();
        for cand in [
            svc_dir.join("bin").join("winws.exe"),
            svc_dir.join("winws.exe"),
        ] {
            if cand.exists() {
                out.push(cand.canonicalize().unwrap_or(cand));
            }
        }
        out
    }

    /// Whether `process` is a winws.exe that belongs to our installation.
    fn is_owned_winws(process: &sysinfo::Process, owned: &[PathBuf]) -> bool {
        let name = process.name().to_string_lossy();
        if !(name.eq_ignore_ascii_case("winws.exe") || name.eq_ignore_ascii_case("winws")) {
            return false;
        }
        match process.exe() {
            Some(exe) => {
                let exe_c = exe.canonicalize().unwrap_or(exe.to_path_buf());
                owned.contains(&exe_c)
            }
            // If the path is unreadable, don't claim an arbitrary privileged
            // winws.exe as ours. Service mode is detected through SCM ownership.
            None => false,
        }
    }

    fn detect_service_running(&self) -> bool {
        use windows_service::{
            service::ServiceAccess,
            service_manager::{ServiceManager, ServiceManagerAccess},
        };
        if let Ok(manager) =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        {
            if let Ok(service) = manager.open_service(
                &self.service_name,
                ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
            ) {
                let owned_dirs = [
                    self.install_dir.clone(),
                    crate::zapret::paths::service_install_dir(),
                ];
                let owned = crate::zapret::service::service_belongs_to_dirs(
                    &self.service_name,
                    &service,
                    &owned_dirs,
                )
                .unwrap_or(false);
                if !owned {
                    return false;
                }
                if let Ok(status) = service.query_status() {
                    return status.current_state == windows_service::service::ServiceState::Running;
                }
            }
        }
        false
    }
}

#[async_trait::async_trait]
impl Runner for ProcessRunner {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
        // Upstream's general*.bat calls `service.bat status_zapret` before every
        // launch. That routine enables TCP timestamps; starting winws directly
        // without reproducing it can leave otherwise-valid strategies broken.
        if self.tcp_preflight {
            crate::zapret::tcp::ensure_tcp_timestamps_enabled().await?;
        }

        let mut active_child = self.active_child.lock().await;
        // If already running, stop it first
        if active_child.is_some() {
            drop(active_child);
            self.stop().await?;
            active_child = self.active_child.lock().await;
        }

        let winws_path = self.get_winws_path();
        if !winws_path.exists() {
            return Err(anyhow::anyhow!("winws.exe not found at {:?}", winws_path));
        }

        // Make sure the user list files winws expects exist (service.bat does this too).
        crate::zapret::batparse::ensure_user_lists(&self.install_dir)?;

        // winws.exe is launched with bin/ as the working directory (matches the .bat: `cd /d %BIN%`).
        let bin_dir = winws_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.install_dir.clone());

        // Configure process command
        let mut cmd = tokio::process::Command::new(&winws_path);
        cmd.args(&strategy.winws_args);
        cmd.current_dir(&bin_dir);

        // Windows-specific flags: CREATE_NO_WINDOW (0x08000000) and CREATE_NEW_PROCESS_GROUP (0x00000200)
        #[cfg(windows)]
        {
            cmd.creation_flags(0x08000000 | 0x00000200);
        }

        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("Failed to get process ID"))?;

        // winws output goes through `tracing` (target "winws") rather than a raw
        // UiEvent::LogLine: that stamps every line with a local timestamp +
        // level, persists it to app.log and still reaches the in-app Logs page
        // via the UiWriter broadcast.
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                    if !trimmed.is_empty() {
                        tracing::info!(target: "winws", "{}", trimmed);
                    }
                    buf.clear();
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
                    if !trimmed.is_empty() {
                        tracing::warn!(target: "winws", "{}", trimmed);
                    }
                    buf.clear();
                }
            });
        }

        *active_child = Some(child);
        *self.active_strategy_id.lock().await = Some(strategy.id.to_string());
        *self.active_started_at.lock().await = Some(Instant::now());

        Ok(pid)
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let active_child_opt = self.active_child.lock().await.take();
        *self.active_strategy_id.lock().await = None;
        *self.active_started_at.lock().await = None;

        if let Some(mut child) = active_child_opt {
            let pid = child.id();
            if let Some(pid) = pid {
                #[cfg(windows)]
                unsafe {
                    if GenerateConsoleCtrlEvent(1, pid) == 0 {
                        // Not fatal — we fall back to kill below — but log why the
                        // graceful CTRL_BREAK shutdown didn't take.
                        tracing::warn!(
                            "GenerateConsoleCtrlEvent for pid {pid} failed: {}",
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }

            // Wait for it to stop with timeout, fallback to kill/TerminateProcess
            match timeout(Duration::from_millis(2000), child.wait()).await {
                Ok(Ok(_)) => {
                    // Exited cleanly
                }
                _ => {
                    // Timeout or error, terminate it
                    let _ = child.kill().await;
                }
            }
        } else {
            // Clean up only winws.exe processes belonging to our installation
            let winws_path = self.get_winws_path();
            let winws_path_canonical = winws_path.canonicalize().unwrap_or(winws_path.clone());

            let mut sys = System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            for (pid, process) in sys.processes() {
                let name = process.name().to_string_lossy();
                if name.eq_ignore_ascii_case("winws.exe") || name.eq_ignore_ascii_case("winws") {
                    let matches_path = if let Some(exe) = process.exe() {
                        let exe_canonical = exe.canonicalize().unwrap_or(exe.to_path_buf());
                        exe_canonical == winws_path_canonical
                    } else {
                        false
                    };

                    if matches_path {
                        let pid_val = pid.as_u32();
                        #[cfg(windows)]
                        unsafe {
                            if GenerateConsoleCtrlEvent(1, pid_val) == 0 {
                                tracing::warn!(
                                    "GenerateConsoleCtrlEvent for pid {pid_val} failed: {}",
                                    std::io::Error::last_os_error()
                                );
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let _ = process.kill();
                    }
                }
            }
        }

        Ok(())
    }

    async fn detect_running(&self) -> RuntimeStatus {
        let winws_exists = self.get_winws_path().exists();
        let version = std::fs::read_to_string(self.install_dir.join("version.txt"))
            .ok()
            .map(|s| s.trim().to_string());

        let mut mode = RunningMode::None;
        let mut winws_pid = None;
        let mut uptime_secs = None;
        let mut spawned_child_exited = false;

        // 1. Most reliable: the process we spawned ourselves. Trust our handle.
        {
            let mut guard = self.active_child.lock().await;
            if let Some(child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        // Still running.
                        mode = RunningMode::UserProcess;
                        winws_pid = child.id();
                        uptime_secs = self
                            .active_started_at
                            .lock()
                            .await
                            .as_ref()
                            .map(|started| started.elapsed().as_secs());
                    }
                    _ => {
                        // Exited or errored: drop the dead handle.
                        *guard = None;
                        spawned_child_exited = true;
                    }
                }
            }
        }
        if spawned_child_exited {
            *self.active_strategy_id.lock().await = None;
            *self.active_started_at.lock().await = None;
        }

        // 2. Windows service.
        if mode == RunningMode::None && self.detect_service_running() {
            mode = RunningMode::WindowsService;
        }

        // 3. Take at most one process snapshot when we need fallback detection or
        //    OS-derived uptime. While our own child is alive, the monotonic start
        //    time above avoids enumerating every Windows process on each poll.
        if mode == RunningMode::None || uptime_secs.is_none() {
            let owned = self.owned_winws_paths();
            let mut sys = self.process_snapshot.lock().await;
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let mut first_owned = None;
            let mut exact_pid = None;
            for (pid, process) in sys.processes() {
                if Self::is_owned_winws(process, &owned) {
                    let observed = (pid.as_u32(), process.run_time());
                    first_owned.get_or_insert(observed);
                    if Some(pid.as_u32()) == winws_pid {
                        exact_pid = Some(observed);
                        break;
                    }
                }
            }
            let observed = exact_pid.or(first_owned);
            if mode == RunningMode::None {
                if let Some((pid, run_time)) = observed {
                    winws_pid = Some(pid);
                    mode = RunningMode::UserProcess;
                    uptime_secs = Some(run_time);
                }
            } else if uptime_secs.is_none() {
                uptime_secs = observed.map(|(_, run_time)| run_time);
            }
        }

        let active_strategy_id = self.active_strategy_id.lock().await.clone();
        let detected_strategy = if mode == RunningMode::None {
            None
        } else {
            active_strategy_id
        };

        RuntimeStatus {
            installed: winws_exists,
            installed_version: version,
            running_mode: mode,
            active_strategy: detected_strategy,
            winws_pid,
            service_installed: false,
            uptime_secs,
        }
    }
}
