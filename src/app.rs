use std::sync::Arc;
use std::rc::Rc;
use std::cell::RefCell;
use tokio::sync::{mpsc, broadcast, RwLock};
use crate::contracts::{BackendCmd, UiEvent, RunningMode, InstallStage, split_alt};
use crate::ports::{Installer, Runner, ServiceCtl, StrategyCatalog, StrategyTester};
use crate::config::AppConfig;
use crate::state::AppState;
use crate::tray::SystemTray;

slint::include_modules!();

// ── Log buffer (lives on the Slint UI thread; both the event listener's
//    invoke_from_event_loop closures and the UI callbacks run there) ──
thread_local! {
    static LOG_BUF: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static LOG_FILTER: RefCell<(String, String)> = RefCell::new((String::new(), "ALL".to_string()));
    // Strategy-test results, in arrival order until the final ranked list lands.
    static TEST_RESULTS: RefCell<Vec<crate::contracts::StrategyTestResult>> = const { RefCell::new(Vec::new()) };
}

const LOG_BUF_CAP: usize = 4000;

/// Split a raw log line into (timestamp, level, message) for coloured display.
fn parse_log_line(no: usize, raw: &str) -> LogLineItem {
    let mut rest = raw.trim_end();
    let mut timestamp = String::new();
    let mut level = String::new();

    // Leading ISO-8601 timestamp, e.g. 2026-05-23T16:14:34.808277Z
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let looks_ts = first.len() >= 20
            && first.as_bytes()[0].is_ascii_digit()
            && first.contains('T')
            && first.ends_with('Z');
        if looks_ts {
            timestamp = first.to_string();
            rest = tail.trim_start();
        }
    }

    // Level tag
    if let Some((first, tail)) = rest.split_once(char::is_whitespace) {
        let up = first.to_uppercase();
        if matches!(up.as_str(), "INFO" | "WARN" | "WARNING" | "ERROR" | "ERR" | "DEBUG" | "TRACE") {
            level = if up.starts_with("ERR") { "ERROR".to_string() }
                else if up.starts_with("WARN") { "WARN".to_string() }
                else { up };
            rest = tail.trim_start();
        }
    }

    LogLineItem {
        line_no: no as i32,
        timestamp: timestamp.into(),
        level: level.into(),
        message: rest.to_string().into(),
    }
}

fn line_passes(raw: &str, grep: &str, level: &str) -> bool {
    if level != "ALL" {
        let up = raw.to_uppercase();
        let want = if level == "ERROR" { "ERR" } else { level };
        if !up.contains(want) {
            return false;
        }
    }
    if !grep.is_empty() && !raw.to_lowercase().contains(&grep.to_lowercase()) {
        return false;
    }
    true
}

/// Re-parse + re-filter the whole buffer into the Slint `log_lines` model.
fn rebuild_logs(ui: &MainWindow) {
    let (grep, level) = LOG_FILTER.with(|f| f.borrow().clone());
    let (items, text) = LOG_BUF.with(|b| {
        let mut items: Vec<LogLineItem> = Vec::new();
        let mut text = String::new();
        for raw in b.borrow().iter().filter(|raw| line_passes(raw, &grep, &level)) {
            items.push(parse_log_line(items.len() + 1, raw));
            text.push_str(raw);
            text.push('\n');
        }
        (items, text)
    });
    ui.set_log_lines(Rc::new(slint::VecModel::from(items)).into());
    // Plain-text mirror for the selectable / copyable terminal view.
    ui.set_log_text(text.into());
}

/// Rebuild the Slint `test_results` model from the thread-local buffer.
/// Sorts live by reachability (then latency) so the best strategies bubble to
/// the top as results stream in, rather than appearing in catalog/name order.
fn rebuild_test_results(ui: &MainWindow) {
    let best_id = ui.get_test_best_id().to_string();
    let mut sorted = TEST_RESULTS.with(|b| b.borrow().clone());
    sorted.sort_by(|a, b| {
        b.ok.cmp(&a.ok).then_with(|| {
            let al = if a.avg_latency_ms == 0 { u32::MAX } else { a.avg_latency_ms };
            let bl = if b.avg_latency_ms == 0 { u32::MAX } else { b.avg_latency_ms };
            al.cmp(&bl)
        })
    });
    let items: Vec<TestResultItem> = sorted
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (pretty, alt) = split_alt(&r.id);
            TestResultItem {
                id: r.id.as_str().into(),
                display_name: r.display_name.as_str().into(),
                pretty: pretty.into(),
                alt: alt.into(),
                ok: r.ok as i32,
                total: r.total as i32,
                latency: r.avg_latency_ms as i32,
                rank: i as i32 + 1,
                is_best: !best_id.is_empty() && r.id == best_id,
            }
        })
        .collect();
    ui.set_test_results(Rc::new(slint::VecModel::from(items)).into());
}

/// Open a path with the OS default handler (folder in Explorer, URL in browser).
fn open_external(target: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", target])
        .spawn();
}

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
    tester: Arc<dyn StrategyTester>,
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
        tester: Arc<dyn StrategyTester>,
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
            tester,
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

        // Populate strategies list (full)
        fn to_item(s: &crate::contracts::Strategy) -> StrategyItem {
            let (pretty, alt) = split_alt(&s.id);
            StrategyItem {
                id: s.id.as_str().into(),
                display_name: s.display_name.as_str().into(),
                category: format!("{:?}", s.category).into(),
                description: s.description.as_str().into(),
                pretty: pretty.into(),
                alt: alt.into(),
            }
        }
        let all_items: Vec<StrategyItem> = self.catalog.all().iter().map(to_item).collect();
        ui.set_strategies(Rc::new(slint::VecModel::from(all_items)).into());

        // Search: rebuild the model from the catalog filtered by the query string.
        {
            let catalog = self.catalog.clone();
            let ui_weak = ui.as_weak();
            ui.on_strategies_search(move |query| {
                if let Some(ui) = ui_weak.upgrade() {
                    let q = query.to_string().trim().to_lowercase();
                    let filtered: Vec<StrategyItem> = catalog
                        .all()
                        .iter()
                        .filter(|s| {
                            q.is_empty()
                                || format!("{} {} {}", s.id, s.display_name, s.description)
                                    .to_lowercase()
                                    .contains(&q)
                        })
                        .map(to_item)
                        .collect();
                    ui.set_strategies(Rc::new(slint::VecModel::from(filtered)).into());
                }
            });
        }

        // Logs: filter changes + clear + open file.
        {
            let ui_weak = ui.as_weak();
            ui.on_logs_query_changed(move |grep, level| {
                if let Some(ui) = ui_weak.upgrade() {
                    LOG_FILTER.with(|f| *f.borrow_mut() = (grep.to_string(), level.to_string()));
                    rebuild_logs(&ui);
                }
            });
        }
        {
            let ui_weak = ui.as_weak();
            ui.on_logs_clear_clicked(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    LOG_BUF.with(|b| b.borrow_mut().clear());
                    rebuild_logs(&ui);
                }
            });
        }
        ui.on_open_log_file_clicked(move || {
            let appdata = std::env::var("APPDATA").unwrap_or_default();
            let path = format!("{}\\zapret-ui\\logs\\app.log", appdata);
            open_external(&path);
        });
        ui.on_open_url_clicked(move |url| {
            open_external(&url.to_string());
        });

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
            ui.on_service_install_clicked(move |strat_id| {
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
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_test_start_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::TestStrategies);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_test_cancel_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::CancelTest);
            });
        }
        // "Use this strategy" from a test result row: resolve it from the catalog
        // and apply it as the user's selection, then jump to the dashboard.
        {
            let catalog = self.catalog.clone();
            let ui_weak = ui.as_weak();
            ui.on_test_use_strategy(move |id| {
                if let Some(ui) = ui_weak.upgrade() {
                    let id = id.to_string();
                    if let Some(s) = catalog.by_id(&id) {
                        ui.set_selected_item(to_item(&s));
                        ui.set_selected_strategy(id.as_str().into());
                        ui.set_current_page("home".into());
                    }
                }
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
        let catalog = self.catalog.clone();
        let mut event_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            while let Ok(event) = event_rx.recv().await {
                let ui_weak = ui_weak.clone();
                let catalog = catalog.clone();
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
                                let active = status.active_strategy.clone().unwrap_or_default();
                                ui.set_status_active_strategy(active.as_str().into());
                                ui.set_status_winws_pid(status.winws_pid.unwrap_or(0) as i32);
                                ui.set_status_service_installed(status.service_installed);
                                // Authoritative bypass uptime from the OS; the UI ticks
                                // locally between polls and re-syncs to this each refresh.
                                ui.set_status_uptime(status.uptime_secs.unwrap_or(0) as i32);

                                // Resolve the running strategy to a display item.
                                let (pretty, alt) = split_alt(&active);
                                let display = catalog
                                    .by_id(&active)
                                    .map(|s| s.display_name)
                                    .unwrap_or_else(|| active.clone());
                                let desc = catalog.by_id(&active).map(|s| s.description).unwrap_or_default();
                                let active_item = StrategyItem {
                                    id: active.as_str().into(),
                                    display_name: display.into(),
                                    category: "".into(),
                                    description: desc.into(),
                                    pretty: pretty.into(),
                                    alt: alt.into(),
                                };
                                ui.set_active_item(active_item.clone());
                                // Seed the user's selection if they haven't picked one yet.
                                if !active.is_empty() && ui.get_selected_item().id.is_empty() {
                                    ui.set_selected_item(active_item);
                                }
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
                                LOG_BUF.with(|b| {
                                    let mut buf = b.borrow_mut();
                                    buf.push(line);
                                    let len = buf.len();
                                    if len > LOG_BUF_CAP {
                                        buf.drain(0..len - LOG_BUF_CAP);
                                    }
                                });
                                rebuild_logs(&ui);
                            }
                            UiEvent::UpdateAvailable { latest, .. } => {
                                ui.set_has_update(true);
                                ui.set_latest_version(latest.into());
                            }
                            UiEvent::Error(err) => {
                                tracing::error!("UI Error: {}", err);
                                ui.set_is_busy(false);
                            }
                            UiEvent::TestStarted { total } => {
                                TEST_RESULTS.with(|b| b.borrow_mut().clear());
                                ui.set_test_running(true);
                                ui.set_test_best_id("".into());
                                ui.set_test_current(0);
                                ui.set_test_total(total as i32);
                                ui.set_test_current_strategy("".into());
                                rebuild_test_results(&ui);
                            }
                            UiEvent::TestProgress { index, total, strategy } => {
                                ui.set_test_running(true);
                                ui.set_test_current(index as i32);
                                ui.set_test_total(total as i32);
                                ui.set_test_current_strategy(strategy.into());
                            }
                            UiEvent::TestResult(result) => {
                                TEST_RESULTS.with(|b| b.borrow_mut().push(result));
                                rebuild_test_results(&ui);
                            }
                            UiEvent::TestComplete { best, results } => {
                                // Replace the streamed (unranked) list with the
                                // final ranked one.
                                TEST_RESULTS.with(|b| *b.borrow_mut() = results);
                                ui.set_test_best_id(best.as_str().into());
                                ui.set_test_running(false);
                                ui.set_test_current_strategy("".into());
                                rebuild_test_results(&ui);

                                // Auto-select the winner as the user's strategy.
                                if !best.is_empty() {
                                    if let Some(s) = catalog.by_id(&best) {
                                        let (pretty, alt) = split_alt(&s.id);
                                        let item = StrategyItem {
                                            id: s.id.as_str().into(),
                                            display_name: s.display_name.as_str().into(),
                                            category: format!("{:?}", s.category).into(),
                                            description: s.description.as_str().into(),
                                            pretty: pretty.into(),
                                            alt: alt.into(),
                                        };
                                        ui.set_selected_item(item);
                                        ui.set_selected_strategy(best.as_str().into());
                                    }
                                }
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
        let tester = self.tester.clone();
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                                    let mut status = runner.detect_running().await;
                                    status.service_installed = service_ctl.is_installed().await;
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
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
                        status.service_installed = service_ctl.is_installed().await;
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
                    BackendCmd::CancelTest => {
                        tester.cancel();
                    }
                    BackendCmd::TestStrategies => {
                        let strategies = catalog.all();
                        if strategies.is_empty() {
                            let _ = event_tx.send(UiEvent::Error("No strategies installed to test".to_string()));
                            continue;
                        }
                        let total = strategies.len() as u32;
                        let _ = event_tx.send(UiEvent::TestStarted { total });

                        // Stream per-strategy results + progress back to the UI.
                        let ev_result = event_tx.clone();
                        let on_each = Box::new(move |r| {
                            let _ = ev_result.send(UiEvent::TestResult(r));
                        });
                        let ev_progress = event_tx.clone();
                        let on_progress = Box::new(move |index, total, id: &str| {
                            let _ = ev_progress.send(UiEvent::TestProgress {
                                index,
                                total,
                                strategy: id.to_string(),
                            });
                        });

                        match tester.test_all(strategies, on_each, on_progress).await {
                            Ok(results) => {
                                let best = results
                                    .first()
                                    .filter(|r| r.ok > 0)
                                    .map(|r| r.id.clone())
                                    .unwrap_or_default();
                                if !best.is_empty() {
                                    let mut cfg = config.write().await;
                                    cfg.last_strategy = Some(best.clone());
                                    let _ = cfg.save();
                                }
                                let _ = event_tx.send(UiEvent::TestComplete { best, results });
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(format!("Strategy test failed: {}", e)));
                                let _ = event_tx.send(UiEvent::TestComplete { best: String::new(), results: Vec::new() });
                            }
                        }

                        // Settle the status display after the test churns winws.
                        let mut status = runner.detect_running().await;
                        status.service_installed = service_ctl.is_installed().await;
                        state.set_status(status.clone()).await;
                        let _ = event_tx.send(UiEvent::Status(status));
                    }
                }
            }
        });
    }
}
