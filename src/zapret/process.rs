use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast};
use tokio::io::{BufReader, AsyncBufReadExt};
use tokio::time::{timeout, Duration};
use crate::contracts::{Strategy, RuntimeStatus, RunningMode, UiEvent};
use crate::ports::Runner;
use sysinfo::System;

extern "system" {
    fn GenerateConsoleCtrlEvent(dwCtrlEvent: u32, dwProcessGroupId: u32) -> i32;
}

pub struct ProcessRunner {
    install_dir: PathBuf,
    event_tx: broadcast::Sender<UiEvent>,
    active_child: Arc<Mutex<Option<tokio::process::Child>>>,
    active_strategy_id: Arc<Mutex<Option<String>>>,
    service_name: String,
}

impl ProcessRunner {
    pub fn new(install_dir: PathBuf, event_tx: broadcast::Sender<UiEvent>) -> Self {
        Self {
            install_dir,
            event_tx,
            active_child: Arc::new(Mutex::new(None)),
            active_strategy_id: Arc::new(Mutex::new(None)),
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

    fn detect_service_running(&self) -> bool {
        use windows_service::{
            service::ServiceAccess,
            service_manager::{ServiceManager, ServiceManagerAccess},
        };
        if let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT) {
            if let Ok(service) = manager.open_service(&self.service_name, ServiceAccess::QUERY_STATUS) {
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
        crate::zapret::batparse::ensure_user_lists(&self.install_dir);

        // winws.exe is launched with bin/ as the working directory (matches the .bat: `cd /d %BIN%`).
        let bin_dir = winws_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| self.install_dir.clone());

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
        let pid = child.id().ok_or_else(|| anyhow::anyhow!("Failed to get process ID"))?;

        // Spawn stdout log capturer
        if let Some(stdout) = child.stdout.take() {
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::new();
                while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let trimmed = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                    let _ = event_tx.send(UiEvent::LogLine(trimmed));
                    buf.clear();
                }
            });
        }

        // Spawn stderr log capturer
        if let Some(stderr) = child.stderr.take() {
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                while let Ok(n) = reader.read_until(b'\n', &mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let line = String::from_utf8_lossy(&buf);
                    let trimmed = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                    let _ = event_tx.send(UiEvent::LogLine(trimmed));
                    buf.clear();
                }
            });
        }

        *active_child = Some(child);
        *self.active_strategy_id.lock().await = Some(strategy.id.to_string());

        Ok(pid)
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let active_child_opt = self.active_child.lock().await.take();
        *self.active_strategy_id.lock().await = None;

        if let Some(mut child) = active_child_opt {
            let pid = child.id();
            if let Some(pid) = pid {
                #[cfg(windows)]
                unsafe {
                    GenerateConsoleCtrlEvent(1, pid); // 1 = CTRL_BREAK_EVENT
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
                            GenerateConsoleCtrlEvent(1, pid_val);
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
        let active_strategy_id = self.active_strategy_id.lock().await.clone();

        // 1. Most reliable: the process we spawned ourselves. Trust our handle.
        {
            let mut guard = self.active_child.lock().await;
            if let Some(child) = guard.as_mut() {
                match child.try_wait() {
                    Ok(None) => {
                        // Still running.
                        mode = RunningMode::UserProcess;
                        winws_pid = child.id();
                    }
                    _ => {
                        // Exited or errored: drop the dead handle.
                        *guard = None;
                    }
                }
            }
        }

        // 2. Windows service.
        if mode == RunningMode::None && self.detect_service_running() {
            mode = RunningMode::WindowsService;
        }

        // 3. Fallback: any winws.exe running (started by .bat/service/previous session).
        //    Match by name only — the user realistically has a single winws.
        if mode == RunningMode::None {
            let mut sys = System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            for (pid, process) in sys.processes() {
                let name = process.name().to_string_lossy();
                if name.eq_ignore_ascii_case("winws.exe") || name.eq_ignore_ascii_case("winws") {
                    winws_pid = Some(pid.as_u32());
                    mode = RunningMode::UserProcess;
                    break;
                }
            }
        }

        let detected_strategy = if mode == RunningMode::None { None } else { active_strategy_id };

        // Real uptime of the bypass: read the winws process run-time from the OS so
        // it reflects the actual bypass session (survives app restarts / page nav).
        let uptime_secs = if mode == RunningMode::None {
            None
        } else {
            let mut sys = System::new();
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
            let mut found = None;
            for (pid, process) in sys.processes() {
                let name = process.name().to_string_lossy();
                if name.eq_ignore_ascii_case("winws.exe") || name.eq_ignore_ascii_case("winws") {
                    // Prefer the exact pid we identified; fall back to any winws.
                    if Some(pid.as_u32()) == winws_pid {
                        found = Some(process.run_time());
                        break;
                    }
                    found.get_or_insert(process.run_time());
                }
            }
            found
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
