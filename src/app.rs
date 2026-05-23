use std::sync::Arc;
use std::rc::Rc;
use tokio::sync::{mpsc, broadcast, RwLock};
use crate::contracts::{BackendCmd, UiEvent, RunningMode, InstallStage};
use crate::ports::{Installer, Runner, ServiceCtl, StrategyCatalog};
use crate::config::AppConfig;
use crate::state::AppState;
use crate::tray::SystemTray;
use slint::Model;

slint::include_modules!();

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

pub fn relaunch_elevated(task: &str, strategy: Option<&str>) -> anyhow::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    let current_exe = std::env::current_exe()?;
    let exe_path_w: Vec<u16> = current_exe.as_os_str().encode_wide().chain(Some(0)).collect();

    let mut params = format!("--elevated-task={}", task);
    if let Some(strat) = strategy {
        params.push_str(&format!(" --strategy={}", strat));
    }
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

    Ok(())
}

pub struct App {
    installer: Arc<dyn Installer>,
    runner: Arc<dyn Runner>,
    service_ctl: Arc<dyn ServiceCtl>,
    catalog: Arc<dyn StrategyCatalog>,
    config: Arc<RwLock<AppConfig>>,
    state: AppState,
    cmd_tx: mpsc::Sender<BackendCmd>,
    cmd_rx: Option<mpsc::Receiver<BackendCmd>>,
    event_tx: broadcast::Sender<UiEvent>,
}

impl App {
    pub fn new(
        installer: Arc<dyn Installer>,
        runner: Arc<dyn Runner>,
        service_ctl: Arc<dyn ServiceCtl>,
        catalog: Arc<dyn StrategyCatalog>,
        config: AppConfig,
        state: AppState,
        event_tx: broadcast::Sender<UiEvent>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(64);

        Self {
            installer,
            runner,
            service_ctl,
            catalog,
            config: Arc::new(RwLock::new(config)),
            state,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            event_tx,
        }
    }

    pub fn run(
        &mut self,
        _guard: tracing_appender::non_blocking::WorkerGuard,
    ) -> anyhow::Result<()> {
        let ui = MainWindow::new()?;

        // Populate strategies list (full, category "All")
        fn to_item(s: &crate::contracts::Strategy) -> StrategyItem {
            StrategyItem {
                id: s.id.as_str().into(),
                display_name: s.display_name.as_str().into(),
                category: format!("{:?}", s.category).into(),
                description: s.description.as_str().into(),
            }
        }
        let all_items: Vec<StrategyItem> = self.catalog.all().iter().map(to_item).collect();
        ui.set_strategies(Rc::new(slint::VecModel::from(all_items)).into());

        // Category filter: rebuild the model from the catalog when the user picks a category.
        {
            let catalog = self.catalog.clone();
            let ui_weak = ui.as_weak();
            ui.on_category_changed(move |cat| {
                if let Some(ui) = ui_weak.upgrade() {
                    let cat_s = cat.to_string();
                    let filtered: Vec<StrategyItem> = catalog
                        .all()
                        .iter()
                        .filter(|s| cat_s == "All" || format!("{:?}", s.category) == cat_s)
                        .map(to_item)
                        .collect();
                    ui.set_selected_category(cat);
                    ui.set_strategies(Rc::new(slint::VecModel::from(filtered)).into());
                }
            });
        }

        // Connect UI callbacks to BackendCmd channel
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_start_clicked(move |strat_id| {
                let _ = cmd_tx_c.try_send(BackendCmd::Start(strat_id.to_string()));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_stop_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::Stop);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_install_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::Install);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_update_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::Update);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_strategy_as_service(move |strat_id| {
                let _ = cmd_tx_c.try_send(BackendCmd::ServiceInstall(strat_id.to_string()));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_remove_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::ServiceRemove);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_start_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::ServiceStart);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_stop_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::ServiceStop);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_open_folder_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::OpenInstallFolder);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_refresh_status_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::RefreshStatus);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_strategy_selected(move |_| {
                let _ = cmd_tx_c.try_send(BackendCmd::RefreshStatus);
            });
        }

        // Handle Tray minimizing on close
        let tray = SystemTray::new()?;
        let window = ui.window();
        let ui_weak = ui.as_weak();
        window.on_close_requested(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let _ = ui.hide();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });

        // Event listener task for Tray actions (use OS thread since SystemTray is not Send)
        let ui_weak = ui.as_weak();
        let show_id = tray.show_item_id.clone();
        let quit_id = tray.quit_item_id.clone();
        std::thread::spawn(move || {
            loop {
                if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    let event_id = event.id.0.clone();
                    if event_id == show_id {
                        let ui_weak = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                let _ = ui.show();
                            }
                        });
                    } else if event_id == quit_id {
                        std::process::exit(0);
                    }
                }

                if let Ok(_event) = tray_icon::TrayIconEvent::receiver().try_recv() {
                    let ui_weak = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            let _ = ui.show();
                        }
                    });
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });

        // Listen to UiEvents and update Slint properties
        let ui_weak = ui.as_weak();
        let mut event_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                let ui_weak = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        match event {
                            UiEvent::Status(status) => {
                                ui.set_status_installed(status.installed);
                                ui.set_status_installed_version(status.installed_version.unwrap_or_default().into());
                                ui.set_status_running_mode(match status.running_mode {
                                    RunningMode::None => "None".into(),
                                    RunningMode::UserProcess => "UserProcess".into(),
                                    RunningMode::WindowsService => "WindowsService".into(),
                                });
                                ui.set_status_active_strategy(status.active_strategy.unwrap_or_default().into());
                                ui.set_status_winws_pid(status.winws_pid.unwrap_or(0) as i32);
                                ui.set_is_busy(false);
                            }
                            UiEvent::DownloadProgress { bytes, total } => {
                                let pct = if let Some(tot) = total {
                                    if tot > 0 { bytes as f32 / tot as f32 } else { 0.0 }
                                } else {
                                    0.0
                                };
                                ui.set_progress(pct);
                            }
                            UiEvent::InstallProgress(stage) => {
                                ui.set_is_busy(match stage {
                                    InstallStage::Done => false,
                                    _ => true,
                                });
                                if let InstallStage::Done = stage {
                                    ui.set_progress(1.0);
                                }
                            }
                            UiEvent::LogLine(line) => {
                                let mut text = ui.get_log_text().to_string();
                                text.push_str(&line);
                                text.push('\n');
                                // Cap buffer to keep the last ~60k chars.
                                if text.len() > 60_000 {
                                    if let Some(pos) = text.char_indices().nth(text.chars().count() - 50_000).map(|(i, _)| i) {
                                        text = text[pos..].to_string();
                                    }
                                }
                                ui.set_log_text(text.into());
                            }
                            UiEvent::UpdateAvailable { latest, .. } => {
                                ui.set_has_update(true);
                                ui.set_latest_version(latest.into());
                            }
                            UiEvent::Error(err) => {
                                tracing::error!("UI Error: {}", err);
                                ui.set_is_busy(false);
                            }
                        }
                    }
                });
            }
        });

        // Run the backend loop task
        if let Some(cmd_rx) = self.cmd_rx.take() {
            self.run_backend_loop(cmd_rx);
        }

        // Periodically refresh status
        {
            let cmd_tx_c = self.cmd_tx.clone();
            tokio::spawn(async move {
                loop {
                    let _ = cmd_tx_c.try_send(BackendCmd::RefreshStatus);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            });
        }

        // Trigger initial refresh + update check
        let _ = self.cmd_tx.try_send(BackendCmd::RefreshStatus);
        let _ = self.cmd_tx.try_send(BackendCmd::CheckUpdate);

        ui.run()?;
        Ok(())
    }

    fn run_backend_loop(
        &self,
        mut rx: mpsc::Receiver<BackendCmd>,
    ) {
        let installer = self.installer.clone();
        let runner = self.runner.clone();
        let service_ctl = self.service_ctl.clone();
        let catalog = self.catalog.clone();
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let state = self.state.clone();

        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    BackendCmd::Install => {
                        let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Resolving));
                        let event_tx_c = event_tx.clone();
                        let progress_cb = Box::new(move |stage, bytes, total| {
                            let _ = event_tx_c.send(UiEvent::InstallProgress(stage));
                            let _ = event_tx_c.send(UiEvent::DownloadProgress { bytes, total });
                        });

                        match installer.install_or_update(progress_cb).await {
                            Ok(_) => {
                                let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Done));
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
                            }
                        }
                    }
                    BackendCmd::CheckUpdate => {
                        match installer.latest_version().await {
                            Ok(latest) => {
                                let current = installer.installed_version().await.unwrap_or_default();
                                if latest != current {
                                    let _ = event_tx.send(UiEvent::UpdateAvailable {
                                        current,
                                        latest,
                                        url: "https://github.com/Flowseal/zapret-discord-youtube/releases/latest".to_string(),
                                    });
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
                            }
                        }
                    }
                    BackendCmd::Update => {
                        let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Resolving));
                        let event_tx_c = event_tx.clone();
                        let progress_cb = Box::new(move |stage, bytes, total| {
                            let _ = event_tx_c.send(UiEvent::InstallProgress(stage));
                            let _ = event_tx_c.send(UiEvent::DownloadProgress { bytes, total });
                        });

                        match installer.install_or_update(progress_cb).await {
                            Ok(_) => {
                                let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Done));
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
                            }
                        }
                    }
                    BackendCmd::Start(strategy_id) => {
                        if let Some(strategy) = catalog.by_id(&strategy_id) {
                            match runner.start(&strategy).await {
                                Ok(pid) => {
                                    {
                                        let mut cfg = config.write().await;
                                        cfg.last_strategy = Some(strategy_id.clone());
                                        let _ = cfg.save();
                                    }
                                    let mut status = runner.detect_running().await;
                                    status.running_mode = RunningMode::UserProcess;
                                    status.active_strategy = Some(strategy_id);
                                    status.winws_pid = Some(pid);
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        } else {
                            let _ = event_tx.send(UiEvent::Error(format!("Strategy {} not found", strategy_id)));
                        }
                    }
                    BackendCmd::Stop => {
                        match runner.stop().await {
                            Ok(_) => {
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
                            }
                        }
                    }
                    BackendCmd::ServiceInstall(strategy_id) => {
                        if let Some(strategy) = catalog.by_id(&strategy_id) {
                            match service_ctl.install(&strategy).await {
                                Ok(_) => {
                                    // Installed — now start it so the bypass is active immediately.
                                    if let Err(e) = service_ctl.start().await {
                                        let _ = event_tx.send(UiEvent::Error(format!("Service installed but failed to start: {}", e)));
                                    }
                                    let status = runner.detect_running().await;
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                }
                                Err(e) => {
                                    if e.to_string().contains("NeedsElevation") {
                                        if let Err(err) = relaunch_elevated("service-install", Some(&strategy_id)) {
                                            let _ = event_tx.send(UiEvent::Error(format!("Elevation failed: {}", err)));
                                        }
                                    } else {
                                        let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                    }
                                }
                            }
                        } else {
                            let _ = event_tx.send(UiEvent::Error(format!("Strategy {} not found", strategy_id)));
                        }
                    }
                    BackendCmd::ServiceRemove => {
                        match service_ctl.remove().await {
                            Ok(_) => {
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    if let Err(err) = relaunch_elevated("service-remove", None) {
                                        let _ = event_tx.send(UiEvent::Error(format!("Elevation failed: {}", err)));
                                    }
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStart => {
                        match service_ctl.start().await {
                            Ok(_) => {
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    if let Err(err) = relaunch_elevated("service-start", None) {
                                        let _ = event_tx.send(UiEvent::Error(format!("Elevation failed: {}", err)));
                                    }
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStop => {
                        match service_ctl.stop().await {
                            Ok(_) => {
                                let status = runner.detect_running().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    if let Err(err) = relaunch_elevated("service-stop", None) {
                                        let _ = event_tx.send(UiEvent::Error(format!("Elevation failed: {}", err)));
                                    }
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    BackendCmd::RefreshStatus => {
                        let mut status = runner.detect_running().await;
                        if status.running_mode == RunningMode::None {
                            if let Ok(srv_mode) = service_ctl.status().await {
                                if srv_mode != RunningMode::None {
                                    status.running_mode = srv_mode;
                                }
                            }
                        }
                        if !status.installed {
                            status.installed = installer.is_installed().await;
                        }
                        // If a Windows service or user process is detected, treat zapret as installed
                        // even if our managed install dir is empty (system service may point elsewhere).
                        if status.running_mode != RunningMode::None {
                            status.installed = true;
                        }
                        if status.installed_version.is_none() {
                            status.installed_version = installer.installed_version().await;
                        }
                        state.set_status(status.clone()).await;
                        let _ = event_tx.send(UiEvent::Status(status));
                    }
                    BackendCmd::OpenInstallFolder => {
                        let install_dir = {
                            let cfg = config.read().await;
                            cfg.install_dir_override.clone().unwrap_or_else(|| {
                                let base = directories::BaseDirs::new().unwrap();
                                base.config_dir().join("zapret-ui").join("zapret")
                            })
                        };
                        let _ = std::process::Command::new("explorer").arg(&install_dir).spawn();
                    }
                }
            }
        });
    }
}
