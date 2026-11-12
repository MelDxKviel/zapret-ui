use crate::config::AppConfig;
use crate::contracts::{
    split_alt, AutoEngageOutcome, BackendCmd, EngineKind, EngineMode, GameFilterMode, InstallStage,
    IpsetMode, RunningMode, ScanKind, UiEvent,
};
use crate::ports::{
    Installer, Maintenance, Runner, SelfUpdater, ServiceCtl, StrategyCatalog, StrategyTester,
    TelegramProxy,
};
use crate::state::AppState;
use crate::tray::SystemTray;
use std::cell::RefCell;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{broadcast, mpsc, RwLock};

slint::include_modules!();

#[cfg(test)]
mod engine_tests;
mod monitor;
mod preferences;
#[cfg(target_os = "macos")]
#[path = "../platform/macos/reopen.rs"]
mod reopen;
mod telegram;
mod ui_models;
#[cfg_attr(target_os = "macos", path = "macexec.rs")]
mod winexec;
use ui_models::{is_favorite, rebuild_logs, rebuild_strategies, rebuild_test_results, to_item};
use winexec::{open_external, relaunch_after_update};
#[cfg(windows)]
use winexec::{relaunch_elevated, relaunch_self_elevated, wait_for_elevated_result};

// ── Log buffer (lives on the Slint UI thread; both the event listener's
//    invoke_from_event_loop closures and the UI callbacks run there) ──
thread_local! {
    static LOG_BUF: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static LOG_FILTER: RefCell<(String, String)> = RefCell::new((String::new(), "ALL".to_string()));
    // Strategy-test results, in arrival order until the final ranked list lands.
    static TEST_RESULTS: RefCell<Vec<crate::contracts::StrategyTestResult>> = const { RefCell::new(Vec::new()) };
    // Favorite strategy ids (mirrors AppConfig::favorites). Lives on the UI thread
    // so the model rebuilders (search, toggle, test results) can read it cheaply.
    static FAVORITES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    // Last error toast (message + when) so we can dedupe a burst of identical/rapid
    // errors into a single notification instead of spamming the user.
    static LAST_ERROR_TOAST: RefCell<Option<(String, std::time::Instant)>> = const { RefCell::new(None) };
    static SCAN_PRESENTATION: RefCell<ScanPresentation> = RefCell::new(ScanPresentation::default());
}

const LOG_BUF_CAP: usize = 4000;

/// Explicit ownership of the scan display. A status refresh, nonfatal error or
/// partial result must never turn a live scan back into an idle/startable UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScanPresentation {
    kind: Option<ScanKind>,
    index: u32,
    total: u32,
}

impl ScanPresentation {
    fn apply(&mut self, event: &UiEvent) -> bool {
        let previous = self.clone();
        match event {
            UiEvent::ScanStarted { kind, total } => {
                self.kind = Some(*kind);
                self.index = 0;
                self.total = *total;
            }
            UiEvent::TestProgress { index, total, .. } if self.kind == Some(ScanKind::Test) => {
                self.index = *index;
                self.total = *total;
            }
            UiEvent::AutoEngageProgress { index, total, .. }
                if self.kind == Some(ScanKind::Auto) =>
            {
                self.index = *index;
                self.total = *total;
            }
            UiEvent::ScanFinished => *self = Self::default(),
            _ => {}
        }
        *self != previous
    }
}

fn set_scan_candidate(ui: &MainWindow, catalog: &Arc<dyn StrategyCatalog>, id: &str) {
    let candidate = catalog.by_id(id);
    ui.set_scan_engine(
        candidate
            .as_ref()
            .map(|s| s.engine.slug())
            .unwrap_or_default()
            .into(),
    );
    ui.set_scan_strategy(candidate.map(|s| s.display_name).unwrap_or_default().into());
}

/// UI-thread error surface shared by core and independent Telegram actions.
/// Callers release only the busy state of their own subsystem.
fn report_ui_error(ui: &MainWindow, err: String) {
    tracing::error!("UI Error: {}", err);
    let now = std::time::Instant::now();
    let show = LAST_ERROR_TOAST.with(|c| {
        let mut last = c.borrow_mut();
        let suppress = last.as_ref().is_some_and(|(msg, at)| {
            let dt = now.duration_since(*at);
            (*msg == err && dt < std::time::Duration::from_secs(30))
                || dt < std::time::Duration::from_secs(5)
        });
        if suppress {
            false
        } else {
            *last = Some((err.clone(), now));
            true
        }
    });
    if show {
        let title = crate::i18n::tr(
            ui.global::<I18n>().get_lang().as_str(),
            "notify.error_title",
        );
        std::thread::spawn(move || crate::notify::show(&title, &err));
    }
}

/// Decode the bundled `icon.ico` into a Slint `Image` for use as the window
/// (title bar + taskbar) icon. The `.ico` is also embedded as a Win32 resource
/// via `build.rs` (Explorer icon), but winit needs the icon set at runtime to
/// show it on the window and in the taskbar.
fn app_window_icon() -> Option<slint::Image> {
    use image::ImageReader;
    use std::io::Cursor;

    const ICON_BYTES: &[u8] = include_bytes!("../../assets/icon.ico");
    let img = ImageReader::with_format(Cursor::new(ICON_BYTES), image::ImageFormat::Ico)
        .decode()
        .ok()?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&img, w, h);
    Some(slint::Image::from_rgba8(buf))
}

/// Restore the same window from the tray, Dock or a second process launch.
fn show_main_window(ui: &MainWindow, cmd_tx: &mpsc::Sender<BackendCmd>) {
    ui.set_window_visible(true);
    let _ = ui.show();
    #[cfg(windows)]
    crate::winicon::restore_and_focus_window("zapret-ui");
    #[cfg(target_os = "macos")]
    {
        crate::winenv::apply_window_theme(ui.window(), &ui.get_theme());
        crate::winenv::focus_window(ui.window());
    }
    let _ = cmd_tx.try_send(BackendCmd::RefreshStatus);
}

/// Fire a bypass start/stop toast when the user has notifications enabled.
/// Reads the live config so a toggle takes effect immediately; the toast itself
/// runs on a blocking thread so it never stalls the backend loop.
///
/// `last` tracks the last-notified running state so only genuine transitions
/// toast: repeated start (or stop) requests — whether from re-engaging an
/// already-running bypass, a service flap, or a second launch — are suppressed
/// instead of spamming identical notifications.
async fn notify_bypass(
    config: &Arc<RwLock<AppConfig>>,
    last: &Arc<tokio::sync::Mutex<Option<bool>>>,
    started: bool,
    strategy: Option<&str>,
) {
    {
        let mut last = last.lock().await;
        if *last == Some(started) {
            return;
        }
        *last = Some(started);
    }

    let (enabled, lang) = {
        let c = config.read().await;
        (c.notifications_enabled, crate::i18n::code(c.language))
    };
    if !enabled {
        return;
    }
    let (title, body) = if started {
        let body = strategy
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| crate::i18n::tr(lang, "notify.started_body"));
        (crate::i18n::tr(lang, "notify.started_title"), body)
    } else {
        (
            crate::i18n::tr(lang, "notify.stopped_title"),
            crate::i18n::tr(lang, "notify.stopped_body"),
        )
    };
    tokio::task::spawn_blocking(move || crate::notify::show(&title, &body));
}

/// Resolve the user-facing install dir from config (the same dir the UI uses).
async fn current_install_dir(config: &Arc<RwLock<AppConfig>>) -> std::path::PathBuf {
    config.read().await.install_dir()
}

async fn tr_config(config: &Arc<RwLock<AppConfig>>, key: &str) -> String {
    let lang = crate::i18n::code(config.read().await.language);
    crate::i18n::tr(lang, key)
}

fn conflicts_with_running_test(cmd: &BackendCmd) -> bool {
    matches!(
        cmd,
        BackendCmd::Install
            | BackendCmd::InstallAndEngage
            | BackendCmd::SetEngineMode(_)
            | BackendCmd::SelfUpdate
            | BackendCmd::Update
            | BackendCmd::Start(_)
            | BackendCmd::AutoEngage
            | BackendCmd::AutoEngageFresh
            | BackendCmd::Stop
            | BackendCmd::ServiceInstall(_)
            | BackendCmd::ServiceRemove
            | BackendCmd::ServiceStart
            | BackendCmd::ServiceStop
            | BackendCmd::TestStrategies
    )
}

/// Commands can already be queued when startup begins an automatic scan. Treat
/// repeated Start as the same intent; Stop always cancels whichever scan owns it.
fn command_during_scan(cmd: BackendCmd, active: bool) -> Option<BackendCmd> {
    if !active {
        return Some(cmd);
    }
    match cmd {
        BackendCmd::Stop | BackendCmd::ServiceStop => Some(BackendCmd::CancelScan),
        BackendCmd::RefreshStatus => None,
        cmd if conflicts_with_running_test(&cmd) => None,
        cmd => Some(cmd),
    }
}

fn requires_scan_handoff(cmd: &BackendCmd) -> bool {
    matches!(
        cmd,
        BackendCmd::SetEngineMode(_) | BackendCmd::SetZapret2Enabled(false)
    )
}

async fn cancel_scan_before_handoff(
    task: &mut Option<tokio::task::JoinHandle<()>>,
    tester: &Arc<dyn StrategyTester>,
    auto_cancelled: &AtomicBool,
    test_cancelled: &AtomicBool,
    completion: &std::sync::Mutex<()>,
) {
    {
        let _completion = completion.lock().unwrap();
        auto_cancelled.store(true, Ordering::SeqCst);
        test_cancelled.store(true, Ordering::SeqCst);
        tester.cancel();
    }
    if let Some(task) = task.take() {
        let _ = task.await;
    }
}

/// Keep the completed scan available for a cancel already queued by the UI,
/// but never let that old cancel stop a later manual start or mode transition.
fn retire_finished_scan(task: &mut Option<tokio::task::JoinHandle<()>>, cmd: &BackendCmd) {
    if task.as_ref().is_some_and(|task| task.is_finished())
        && (conflicts_with_running_test(cmd)
            || matches!(cmd, BackendCmd::SetZapret2Enabled(_) | BackendCmd::Quit))
    {
        task.take();
    }
}

async fn cancel_scan_and_stop(
    task: &mut Option<tokio::task::JoinHandle<()>>,
    tester: &Arc<dyn StrategyTester>,
    runner: &Arc<dyn Runner>,
    auto_cancelled: &AtomicBool,
    test_cancelled: &AtomicBool,
    completion: &std::sync::Mutex<()>,
) -> anyhow::Result<bool> {
    if task.is_none() {
        return Ok(false);
    }
    cancel_scan_before_handoff(task, tester, auto_cancelled, test_cancelled, completion).await;
    // The task may already have committed its winner while persisting results
    // or refreshing status. Joining alone cannot undo that completed start.
    runner.stop().await?;
    Ok(true)
}

fn proposed_engine_preferences(config: &AppConfig, cmd: &BackendCmd) -> Option<(EngineMode, bool)> {
    match cmd {
        BackendCmd::SetEngineMode(EngineMode::Zapret2) => Some((EngineMode::Zapret2, true)),
        // Auto remains readable in older configs, but is no longer a GUI mode.
        BackendCmd::SetEngineMode(EngineMode::Zapret1 | EngineMode::Auto) => {
            Some((EngineMode::Zapret1, config.zapret2_enabled))
        }
        BackendCmd::SetZapret2Enabled(true) => Some((config.effective_engine_mode(), true)),
        BackendCmd::SetZapret2Enabled(false) => Some((EngineMode::Zapret1, false)),
        _ => None,
    }
}

fn save_engine_preferences(
    config: &mut AppConfig,
    mode: EngineMode,
    enabled: bool,
    save: impl FnOnce(&AppConfig) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut updated = config.clone();
    updated.engine_mode = mode;
    updated.zapret2_enabled = enabled;
    save(&updated)?;
    *config = updated;
    Ok(())
}

async fn complete_scan_results(
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
    best: String,
    results: Vec<crate::contracts::StrategyTestResult>,
) {
    let mut cfg = config.write().await;
    // Cancellation or cleanup failure does not invalidate already measured
    // candidates. If none finished, retain the previous saved diagnostics.
    if !results.is_empty() {
        cfg.strategy_test_results = results;
        if let Err(error) = cfg.save() {
            let _ = event_tx.send(UiEvent::Error(format!("{error:#}")));
        }
    }
    let results = cfg.strategy_test_results.clone();
    drop(cfg);
    let _ = event_tx.send(UiEvent::TestComplete { best, results });
}

/// Commit scan completion and cancellation under the same lock. A cancellation
/// accepted before this boundary always stops the winner before enabling Start.
async fn finish_scan(
    runner: &Arc<dyn Runner>,
    running: &AtomicBool,
    cancelled: &AtomicBool,
    completion: &std::sync::Mutex<()>,
) -> anyhow::Result<bool> {
    {
        let _guard = completion.lock().unwrap();
        if !cancelled.load(Ordering::SeqCst) {
            running.store(false, Ordering::SeqCst);
            return Ok(false);
        }
    }
    let stopped = runner.stop().await;
    {
        let _guard = completion.lock().unwrap();
        running.store(false, Ordering::SeqCst);
    }
    stopped?;
    Ok(true)
}

#[cfg(windows)]
async fn start_test_winner(
    runner: &Arc<dyn Runner>,
    catalog: &Arc<dyn StrategyCatalog>,
    results: &[crate::contracts::StrategyTestResult],
    cancelled: &AtomicBool,
    events: &broadcast::Sender<UiEvent>,
) -> String {
    let mut last_failure = None;
    for result in results.iter().filter(|r| r.eligible()) {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        let Some(strategy) = catalog.by_id(&result.id) else {
            continue;
        };
        let _ = events.send(UiEvent::TestProgress {
            index: results.len() as u32,
            total: results.len() as u32,
            strategy: strategy.id.clone(),
        });
        match runner.start(&strategy).await {
            Ok(_) => return result.id.clone(),
            Err(e) => {
                tracing::warn!("Test winner {} failed to restart: {e:#}", result.id);
                if let Err(cleanup) = runner.stop().await {
                    let _ = events.send(UiEvent::Error(format!("{e:#}; {cleanup:#}")));
                    return String::new();
                }
                last_failure = Some(format!("{e:#}"));
            }
        }
    }
    if let Some(error) = last_failure.filter(|_| !cancelled.load(Ordering::SeqCst)) {
        let _ = events.send(UiEvent::Error(error));
    }
    String::new()
}

/// Candidate strategies for auto-engage, ordered so the most likely winner is
/// tried first: the last-known-good strategy, then starred favorites, then the
/// remaining catalog order. De-duplicated, preserving first occurrence.
fn ordered_candidates(
    catalog: &Arc<dyn StrategyCatalog>,
    last_strategy: Option<&str>,
    favorites: &[String],
) -> Vec<crate::contracts::Strategy> {
    let available = catalog.all();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |out: &mut Vec<crate::contracts::Strategy>, s: crate::contracts::Strategy| {
        if seen.insert(s.id.clone()) {
            out.push(s);
        }
    };
    if let Some(id) = last_strategy {
        if let Some(s) = available.iter().find(|s| s.id == id) {
            push(&mut out, s.clone());
        }
    }
    for fav in favorites {
        if let Some(s) = available.iter().find(|s| &s.id == fav) {
            push(&mut out, s.clone());
        }
    }
    for s in available {
        push(&mut out, s);
    }
    out
}

/// Relaunch elevated for a one-shot service task and wait for its result,
/// surfacing the helper's actual error to the UI instead of silently relying on
/// the next status poll. Passes the current install dir explicitly so the
/// elevated helper acts on the same directory the UI is using.
#[cfg(windows)]
async fn elevate_service_task(
    task: &str,
    strategy: Option<&str>,
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    let install_dir = config.read().await.engine_install_dir(
        strategy
            .map(EngineKind::for_strategy_id)
            .unwrap_or_default(),
    );
    match relaunch_elevated(task, strategy, &install_dir) {
        Ok(handle) => match wait_for_elevated_result(handle).await {
            Ok(()) => true,
            Err(msg) => {
                let _ = event_tx.send(UiEvent::Error(msg));
                false
            }
        },
        Err(e) => {
            let msg = tr_config(config, "err.elevation_failed")
                .await
                .replace("{error}", &e.to_string());
            let _ = event_tx.send(UiEvent::Error(msg));
            false
        }
    }
}

/// Re-detect runtime status, patch `service_installed` from the SCM, store it in
/// `AppState` and broadcast it to the UI. Almost every command ends with this
/// same four-step "settle the status" sequence, so it lives here instead of
/// being copy-pasted into each handler (see CLAUDE.md "Status flow").
async fn refresh_and_broadcast(
    installer: &Arc<dyn Installer>,
    runner: &Arc<dyn Runner>,
    service_ctl: &Arc<dyn ServiceCtl>,
    state: &AppState,
    event_tx: &broadcast::Sender<UiEvent>,
) {
    let mut status = runner.detect_running().await;
    status.installed = installer.is_installed().await;
    status.installed_version = installer.installed_version().await;
    status.service_installed = service_ctl.is_installed().await;
    state.set_status(status.clone()).await;
    let _ = event_tx.send(UiEvent::Status(status));
}

/// Shared error handling for the SCM service commands (remove/start/stop/install).
/// On `NeedsElevation` it relaunches the elevated one-shot helper and returns
/// `true` so the caller refreshes status; on any other error it reports it and
/// returns `false`. The `{:#}` format surfaces the underlying winapi/OS error,
/// not just the opaque top-level message.
async fn elevate_or_report(
    e: anyhow::Error,
    elevate_task: &str,
    strategy: Option<&str>,
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    let msg = format!("{:#}", e);
    #[cfg(windows)]
    let needs_elevation = msg.contains("NeedsElevation")
        || msg.contains("os error 5")
        || msg.contains("Access is denied")
        || msg.contains("Отказано в доступе");
    #[cfg(windows)]
    if needs_elevation {
        return elevate_service_task(elevate_task, strategy, config, event_tx).await;
    }
    #[cfg(not(windows))]
    let _ = (elevate_task, strategy, config);
    let _ = event_tx.send(UiEvent::Error(msg));
    false
}

async fn stop_bypass_before_install(
    runner: &Arc<dyn Runner>,
    service_ctl: &Arc<dyn ServiceCtl>,
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    let service_stopped = match service_ctl.status().await {
        Ok(RunningMode::SystemService) => match service_ctl.stop().await {
            Ok(()) => true,
            Err(e) => elevate_or_report(e, "service-stop", None, config, event_tx).await,
        },
        Ok(_) => true,
        Err(e) => {
            let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
            false
        }
    };
    if !service_stopped {
        return false;
    }
    if let Err(e) = runner.stop().await {
        let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
        return false;
    }
    true
}

/// A failed process stop must abort the transition to service mode; otherwise
/// both instances can contend for the WinDivert driver.
#[cfg(any(windows, test))]
async fn stop_process_before_service(
    runner: &Arc<dyn Runner>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    // Let the service adapter snapshot its registration and running state before
    // replacement, so a failed install can restore both.
    if runner.detect_running().await.running_mode != RunningMode::UserProcess {
        return true;
    }
    match runner.stop().await {
        Ok(()) => true,
        Err(e) => {
            let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
            false
        }
    }
}

pub struct App {
    installer: Arc<dyn Installer>,
    runner: Arc<dyn Runner>,
    service_ctl: Arc<dyn ServiceCtl>,
    catalog: Arc<dyn StrategyCatalog>,
    tester: Arc<dyn StrategyTester>,
    maintenance: Arc<dyn Maintenance>,
    self_updater: Arc<dyn SelfUpdater>,
    telegram_proxy: Arc<dyn TelegramProxy>,
    config: Arc<RwLock<AppConfig>>,
    state: AppState,
    cmd_tx: mpsc::Sender<BackendCmd>,
    cmd_rx: Option<mpsc::Receiver<BackendCmd>>,
    event_tx: broadcast::Sender<UiEvent>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        installer: Arc<dyn Installer>,
        runner: Arc<dyn Runner>,
        service_ctl: Arc<dyn ServiceCtl>,
        catalog: Arc<dyn StrategyCatalog>,
        tester: Arc<dyn StrategyTester>,
        maintenance: Arc<dyn Maintenance>,
        self_updater: Arc<dyn SelfUpdater>,
        telegram_proxy: Arc<dyn TelegramProxy>,
        config: AppConfig,
        state: AppState,
        event_tx: broadcast::Sender<UiEvent>,
    ) -> Self {
        #[cfg(windows)]
        {
            installer.set_engine_mode(config.effective_engine_mode());
            catalog.set_engine_mode(config.effective_engine_mode());
        }
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        Self {
            installer,
            runner,
            service_ctl,
            catalog,
            tester,
            maintenance,
            self_updater,
            telegram_proxy,
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
        #[cfg(target_os = "macos")]
        crate::winenv::init_window_backend(
            self.config
                .try_read()
                .map(|c| c.theme)
                .unwrap_or_default()
                .slug(),
        )?;
        let ui = MainWindow::new()?;
        SCAN_PRESENTATION.with(|state| *state.borrow_mut() = ScanPresentation::default());
        ui.set_supports_engine_selection(cfg!(windows));
        if let Ok(config) = self.config.try_read() {
            ui.set_zapret2_enabled(config.zapret2_enabled);
            ui.set_engine_mode(config.effective_engine_mode().slug().into());
            TEST_RESULTS
                .with(|results| *results.borrow_mut() = config.strategy_test_results.clone());
            rebuild_test_results(&ui);
        }
        let preferences = preferences::Controller::new(self.config.clone());
        monitor::bind(&ui);
        let telegram = telegram::bind(
            &ui,
            self.telegram_proxy.clone(),
            self.config.clone(),
            self.event_tx.clone(),
        );

        // Window/taskbar icon (winit needs this set at runtime, separate from the
        // embedded .exe resource icon).
        if let Some(icon) = app_window_icon() {
            ui.set_app_icon(icon);
        }

        // ── i18n ──
        // Back the Slint `I18n.t(lang, key)` callback with the JSON catalogs, then
        // seed the active language from the saved config (Russian by default).
        ui.global::<I18n>()
            .on_t(|lang, key| crate::i18n::tr(lang.as_str(), key.as_str()).into());
        let initial_lang = self
            .config
            .try_read()
            .map(|c| c.language)
            .unwrap_or_default();
        ui.global::<I18n>()
            .set_lang(crate::i18n::code(initial_lang).into());
        // Persist a language switch from the Settings page. The Slint side flips
        // `I18n.lang` itself (so the UI re-renders instantly); we just save it.
        {
            let preferences = preferences.clone();
            ui.on_set_language(move |code| {
                preferences.dispatch(preferences::Action::Language(
                    crate::config::Language::from_code(&code),
                ));
            });
        }

        // Seed favorites + the last-used strategy from the saved config, so a
        // restart restores the user's selection instead of asking them to pick
        // again. (Both fall back to empty/default if the config can't be read.)
        let (fav_seed, last_strategy) = self
            .config
            .try_read()
            .map(|c| (c.favorites.clone(), c.last_strategy.clone()))
            .unwrap_or_default();
        FAVORITES.with(|f| *f.borrow_mut() = fav_seed);

        // Whether this process is elevated. Drives the admin banner + the
        // disabled state of the buttons that actually need admin (Engage,
        // Tester, service ops — all touch the WinDivert driver / SCM).
        #[cfg(windows)]
        ui.set_is_admin(crate::zapret::elevation::is_elevated());
        #[cfg(target_os = "macos")]
        ui.set_is_admin(true); // privileged operations request authorization individually
        ui.global::<I18n>().set_macos(cfg!(target_os = "macos"));

        // Seed the notifications toggle from the saved config (default on).
        let notifications_seed = self
            .config
            .try_read()
            .map(|c| c.notifications_enabled)
            .unwrap_or(true);
        ui.set_notifications(notifications_seed);

        // App version + repo, resolved at build time (build.rs) so the About
        // page tracks the real release tag and project URL automatically.
        ui.set_app_version(env!("APP_VERSION").into());
        ui.set_repo_url(env!("CARGO_PKG_REPOSITORY").into());
        // Real OS light/dark preference, so the "system" theme is accurate.
        ui.set_system_is_dark(crate::winenv::system_is_dark());

        // Seed the rest of the persisted settings so toggles reflect the saved
        // config on launch (and the install-dir row shows the real path, #24).
        if let Ok(c) = self.config.try_read() {
            ui.set_autostart(c.autostart);
            ui.set_autoupdate_check(c.autoupdate_check);
            ui.set_minimize_to_tray(c.minimize_to_tray);
            ui.set_autoengage(c.autoengage);
            ui.set_theme(c.theme.slug().into());
            ui.set_install_dir(
                c.engine_install_dir(
                    if cfg!(windows) && c.effective_engine_mode() == EngineMode::Zapret2 {
                        EngineKind::Zapret2
                    } else {
                        EngineKind::Zapret1
                    },
                )
                .display()
                .to_string()
                .into(),
            );
        }

        // Populate strategies list (favorites first).
        rebuild_strategies(&ui, &self.catalog);

        // Resolve only within the selected mode; a saved legacy choice remains
        // available when the user switches back to its engine.
        let available = self.catalog.all();
        if let Some(strategy) = available
            .iter()
            .find(|s| Some(&s.id) == last_strategy.as_ref())
            .or_else(|| available.first())
        {
            ui.set_selected_item(to_item(strategy));
            ui.set_selected_strategy(strategy.id.as_str().into());
        }

        // Search: rebuild the model from the catalog filtered by the query string.
        {
            let catalog = self.catalog.clone();
            let ui_weak = ui.as_weak();
            ui.on_strategies_search(move |_query| {
                if let Some(ui) = ui_weak.upgrade() {
                    rebuild_strategies(&ui, &catalog);
                }
            });
        }

        // Favorite toggle (Strategies + Tester pages): flip the UI-thread mirror,
        // rebuild both models so the star + ordering update, and persist.
        {
            let catalog = self.catalog.clone();
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_toggle_favorite(move |id| {
                if let Some(ui) = ui_weak.upgrade() {
                    let id = id.to_string();
                    let favs = FAVORITES.with(|f| {
                        let mut b = f.borrow_mut();
                        if let Some(pos) = b.iter().position(|x| *x == id) {
                            b.remove(pos);
                        } else {
                            b.push(id);
                        }
                        b.clone()
                    });
                    rebuild_strategies(&ui, &catalog);
                    rebuild_test_results(&ui);
                    let _ = cmd_tx_c.try_send(BackendCmd::SetFavorites(favs));
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
            open_external(&crate::log::log_dir().join("app.log").to_string_lossy());
        });
        ui.on_open_url_clicked(move |url| {
            open_external(&url);
        });

        // Connect UI callbacks to BackendCmd channel
        // User-initiated actions are dispatched with a guaranteed `send().await`
        // (on a spawned task) rather than `try_send`, so a click during a busy
        // period waits for room in the queue instead of being silently dropped —
        // which would otherwise leave the power button's spinner stuck (#20).
        fn dispatch(tx: &mpsc::Sender<BackendCmd>, cmd: BackendCmd) {
            let tx = tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(cmd).await;
            });
        }
        {
            let tx = self.cmd_tx.clone();
            ui.on_set_engine_mode(move |mode| {
                dispatch(&tx, BackendCmd::SetEngineMode(EngineMode::from_slug(&mode)))
            });
        }
        {
            let tx = self.cmd_tx.clone();
            ui.on_set_zapret2_enabled(move |enabled| {
                dispatch(&tx, BackendCmd::SetZapret2Enabled(enabled))
            });
        }
        {
            let tx = self.cmd_tx.clone();
            ui.on_cancel_scan_clicked(move || dispatch(&tx, BackendCmd::CancelScan));
        }
        {
            let tx = self.cmd_tx.clone();
            ui.on_install_and_engage(move || dispatch(&tx, BackendCmd::InstallAndEngage));
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_start_clicked(move |strat_id| {
                dispatch(&cmd_tx_c, BackendCmd::Start(strat_id.to_string()));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_stop_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::Stop);
            });
        }
        // Automatic action: auto-pick a working strategy and turn the bypass on.
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_auto_engage_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::AutoEngageFresh);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_cancel_engage_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::CancelScan);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_install_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::Install);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_check_update_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::CheckUpdate);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_update_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::Update);
            });
        }
        // App self-update: "Update" downloads + swaps + relaunches; "Check" only
        // re-resolves the latest release (Settings button).
        {
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_app_update_clicked(move || {
                if cfg!(target_os = "macos") {
                    open_external(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases"));
                    return;
                }
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_app_update_downloading(true);
                    ui.set_app_update_progress(0.0);
                    ui.set_app_update_msg("".into());
                }
                dispatch(&cmd_tx_c, BackendCmd::SelfUpdate);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_app_check_update_clicked(move || {
                if cfg!(target_os = "macos") {
                    open_external(concat!(env!("CARGO_PKG_REPOSITORY"), "/releases"));
                    return;
                }
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_app_update_checking(true);
                    ui.set_app_update_msg("".into());
                }
                dispatch(&cmd_tx_c, BackendCmd::CheckSelfUpdate);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_install_clicked(move |strat_id| {
                dispatch(&cmd_tx_c, BackendCmd::ServiceInstall(strat_id.to_string()));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_remove_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::ServiceRemove);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            #[cfg(target_os = "macos")]
            let w = ui.as_weak();
            ui.on_service_start_clicked(move || {
                // On Mac starting is also reinstalling the selected profile.
                // Match the strategy shown in the card after changing selection.
                #[cfg(target_os = "macos")]
                if let Some(ui) = w.upgrade() {
                    let id = ui.get_selected_item().id.to_string();
                    if !id.is_empty() {
                        dispatch(&cmd_tx_c, BackendCmd::Start(id));
                        return;
                    }
                }
                dispatch(&cmd_tx_c, BackendCmd::ServiceStart);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_service_stop_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::ServiceStop);
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
            ui.on_open_ipset_file_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::OpenIpsetFile);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_open_hosts_file_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::OpenHostsFile);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_refresh_status_clicked(move || {
                let _ = cmd_tx_c.try_send(BackendCmd::RefreshStatus);
            });
        }
        {
            let preferences = preferences.clone();
            ui.on_strategy_selected(move |id| {
                preferences.dispatch(preferences::Action::SelectedStrategy(id.to_string()));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_test_start_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::TestStrategies);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_test_cancel_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::CancelScan);
            });
        }
        // "Use this strategy" from a test result row: resolve it from the catalog
        // and apply it as the user's selection, then jump to the dashboard.
        {
            let catalog = self.catalog.clone();
            let preferences = preferences.clone();
            let ui_weak = ui.as_weak();
            ui.on_test_use_strategy(move |id| {
                if let Some(ui) = ui_weak.upgrade() {
                    let id = id.to_string();
                    if let Some(s) = catalog.all().into_iter().find(|strategy| strategy.id == id) {
                        ui.set_selected_item(to_item(&s));
                        ui.set_selected_strategy(id.as_str().into());
                        ui.set_current_page("home".into());
                        // Persist the manual pick so it survives a restart even if
                        // the user never presses Engage (matches strategy_selected).
                        preferences.dispatch(preferences::Action::SelectedStrategy(id));
                    }
                }
            });
        }

        // ── DPI bypass tuning (Settings page) ──
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_game_filter(move |slug| {
                let _ =
                    cmd_tx_c.try_send(BackendCmd::SetGameFilter(GameFilterMode::from_slug(&slug)));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_ipset_mode(move |slug| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetIpsetMode(IpsetMode::from_slug(&slug)));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_update_ipset_clicked(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_ipset_busy(true);
                    ui.set_ipset_msg("".into());
                    if cmd_tx_c.try_send(BackendCmd::UpdateIpsetList).is_err() {
                        ui.set_ipset_busy(false);
                    }
                }
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_update_hosts_clicked(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_hosts_busy(true);
                    ui.set_hosts_msg("".into());
                    if cmd_tx_c.try_send(BackendCmd::UpdateHostsFile).is_err() {
                        ui.set_hosts_busy(false);
                    }
                }
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            let ui_weak = ui.as_weak();
            ui.on_clear_discord_cache_clicked(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_discord_busy(true);
                    ui.set_discord_msg("".into());
                    if cmd_tx_c.try_send(BackendCmd::ClearDiscordCache).is_err() {
                        ui.set_discord_busy(false);
                    }
                }
            });
        }
        // Persist the notifications toggle (Settings → Application).
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_notifications(move |on| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetNotifications(on));
            });
        }
        // Persist the remaining application settings.
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_autostart(move |on| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetAutostart(on));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_autoupdate_check(move |on| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetAutoupdateCheck(on));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_minimize_to_tray(move |on| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetMinimizeToTray(on));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_set_autoengage(move |on| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetAutoengage(on));
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            #[cfg(target_os = "macos")]
            let ui_weak = ui.as_weak();
            ui.on_set_theme(move |theme| {
                #[cfg(target_os = "macos")]
                if let Some(ui) = ui_weak.upgrade() {
                    crate::winenv::apply_window_theme(ui.window(), &theme);
                }
                let _ = cmd_tx_c.try_send(BackendCmd::SetTheme(theme.to_string()));
            });
        }
        // "Run as administrator" banner button: relaunch the whole app elevated,
        // then exit this unelevated instance so the new one can take over.
        #[cfg(windows)]
        ui.on_restart_as_admin(move || match relaunch_self_elevated() {
            Ok(_) => std::process::exit(0),
            Err(e) => tracing::error!("Failed to relaunch as administrator: {}", e),
        });

        // Copy arbitrary text to the system clipboard (used by the hosts window).
        ui.on_copy_to_clipboard(move |text| {
            if let Err(e) = winexec::copy_to_clipboard(&text) {
                tracing::warn!("Failed to copy to clipboard: {}", e);
            }
        });

        // Handle Tray minimizing on close
        let tray_lang = self
            .config
            .try_read()
            .map(|c| crate::i18n::code(c.language))
            .unwrap_or(crate::i18n::RU);
        let tray = SystemTray::new(tray_lang)?;
        let window = ui.window();
        let ui_weak = ui.as_weak();
        let cmd_tx_close = self.cmd_tx.clone();
        window.on_close_requested(move || {
            // Honour the "minimize to tray on close" setting: when on, hide to the
            // tray; when off, actually exit the app (the tray menu's Quit still
            // works either way). Hiding the last window would normally quit the
            // event loop, so the app runs it via `run_event_loop_until_quit`.
            let to_tray = ui_weak
                .upgrade()
                .map(|ui| ui.get_minimize_to_tray())
                .unwrap_or(false);
            if to_tray {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_window_visible(false);
                    let _ = ui.hide();
                }
                // Show the one-time "still running in the tray" toast (the backend
                // decides whether it's the first time and persists the flag).
                let _ = cmd_tx_close.try_send(BackendCmd::MinimizedToTray);
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                dispatch(&cmd_tx_close, BackendCmd::Quit);
                slint::CloseRequestResponse::KeepWindowShown
            }
        });

        // Slint's native tray delivers callbacks on the UI thread. It also
        // preserves the backend's native text-editing menus (Copy/Paste etc.).
        {
            let w = ui.as_weak();
            let cmd_tx = self.cmd_tx.clone();
            tray.on_open(move || {
                if let Some(ui) = w.upgrade() {
                    show_main_window(&ui, &cmd_tx);
                }
            });
        }
        {
            let w = ui.as_weak();
            let cmd_tx = self.cmd_tx.clone();
            tray.on_settings(move || {
                if let Some(ui) = w.upgrade() {
                    show_main_window(&ui, &cmd_tx);
                    ui.set_current_page("settings".into());
                }
            });
        }
        {
            let w = ui.as_weak();
            let cmd_tx = self.cmd_tx.clone();
            tray.on_start(move || {
                if let Some(ui) = w.upgrade() {
                    let mut id = ui.get_selected_item().id.to_string();
                    if id.is_empty() {
                        id = ui.get_active_item().id.to_string();
                    }
                    let command = if id.is_empty() {
                        BackendCmd::AutoEngage
                    } else {
                        BackendCmd::Start(id)
                    };
                    let _ = cmd_tx.try_send(command);
                }
            });
        }
        {
            let cmd_tx = self.cmd_tx.clone();
            tray.on_stop(move || {
                let _ = cmd_tx.try_send(BackendCmd::Stop);
            });
        }
        let tx_quit = self.cmd_tx.clone();
        tray.on_quit(move || dispatch(&tx_quit, BackendCmd::Quit));

        #[cfg(target_os = "macos")]
        {
            let ui_weak = ui.as_weak();
            let cmd_tx = self.cmd_tx.clone();
            crate::single_instance::set_activation_handler(move || {
                let w = ui_weak.clone();
                let cmd_tx = cmd_tx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        show_main_window(&ui, &cmd_tx);
                    }
                });
            });
        }

        #[cfg(target_os = "macos")]
        let _reopen = {
            let w = ui.as_weak();
            let cmd_tx = self.cmd_tx.clone();
            reopen::install(move || {
                if let Some(ui) = w.upgrade() {
                    show_main_window(&ui, &cmd_tx);
                }
            })?
        };

        // Listen to UiEvents and update Slint properties
        let ui_weak = ui.as_weak();
        let catalog = self.catalog.clone();
        let directories = self
            .config
            .try_read()
            .map(|c| {
                [
                    c.engine_install_dir(EngineKind::Zapret1)
                        .display()
                        .to_string(),
                    c.engine_install_dir(EngineKind::Zapret2)
                        .display()
                        .to_string(),
                ]
            })
            .unwrap_or_default();
        let mut event_rx = self.event_tx.subscribe();
        tokio::spawn(async move {
            loop {
                let event = match event_rx.recv().await {
                    Ok(event) => event,
                    // A burst of engine/download logs must not permanently
                    // disconnect the UI from subsequent status events.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let ui_weak = ui_weak.clone();
                let catalog = catalog.clone();
                let directories = directories.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        let scan_active = SCAN_PRESENTATION.with(|state| {
                            let mut state = state.borrow_mut();
                            if state.apply(&event) {
                                ui.set_scan_kind(
                                    state.kind.map(ScanKind::slug).unwrap_or_default().into(),
                                );
                                ui.set_scan_index(state.index as i32);
                                ui.set_scan_total(state.total as i32);
                            }
                            state.kind.is_some()
                        });
                        match event {
                            UiEvent::ScanStarted { kind, total } => {
                                ui.set_is_busy(true);
                                ui.set_pending_op("".into());
                                ui.set_scan_engine("".into());
                                ui.set_scan_strategy("".into());
                                ui.set_core_error("".into());
                                ui.set_simple_failed(false);
                                ui.set_test_running(true);
                                ui.set_test_current(0);
                                ui.set_test_total(total as i32);
                                if kind == ScanKind::Auto {
                                    TEST_RESULTS.with(|results| results.borrow_mut().clear());
                                    ui.set_test_best_id("".into());
                                    rebuild_test_results(&ui);
                                }
                            }
                            UiEvent::ScanFinished => {
                                ui.set_scan_engine("".into());
                                ui.set_scan_strategy("".into());
                                ui.set_test_running(false);
                                ui.set_test_current_strategy("".into());
                                ui.set_test_current_alt("".into());
                                ui.set_is_busy(false);
                            }
                            UiEvent::Zapret2Enabled(enabled) => ui.set_zapret2_enabled(enabled),
                            UiEvent::TelegramStatus(status) => {
                                ui.set_telegram_running(status.running);
                                ui.set_telegram_connections(
                                    status.connections.min(i32::MAX as u32) as i32,
                                );
                                ui.set_telegram_error(status.error.into());
                            }
                            UiEvent::TelegramSettings(settings) => {
                                telegram::apply_settings(&ui, &settings);
                                ui.set_telegram_busy(false);
                            }
                            UiEvent::TelegramError(error) => {
                                ui.set_telegram_error(error.as_str().into());
                                ui.set_telegram_busy(false);
                                // Automatic startup and Settings actions may
                                // fail while the proxy page is not visible.
                                if ui.get_current_page().as_str() != "telegram" {
                                    let message = crate::i18n::tr(
                                        ui.global::<I18n>().get_lang().as_str(),
                                        &error,
                                    );
                                    report_ui_error(&ui, message);
                                }
                            }
                            UiEvent::TelegramVisibility(visible) => {
                                ui.set_show_telegram_proxy(visible);
                            }
                            UiEvent::TelegramAutostart(enabled) => {
                                ui.set_telegram_autostart(enabled);
                            }
                            UiEvent::Status(status) => {
                                ui.set_status_active_engine(
                                    status
                                        .active_engine
                                        .map(|e| e.slug())
                                        .unwrap_or_default()
                                        .into(),
                                );
                                ui.set_status_installed(status.installed);
                                ui.set_status_installed_version(
                                    status.installed_version.unwrap_or_default().into(),
                                );
                                ui.set_status_running_mode(match status.running_mode {
                                    RunningMode::None => "None".into(),
                                    RunningMode::UserProcess => "UserProcess".into(),
                                    RunningMode::SystemService => "WindowsService".into(),
                                });
                                let active = status.active_strategy.clone().unwrap_or_default();
                                ui.set_status_active_strategy(active.as_str().into());
                                ui.set_status_winws_pid(status.winws_pid.unwrap_or(0) as i32);
                                ui.set_status_service_installed(status.service_installed);
                                // Authoritative bypass uptime from the OS; the UI ticks
                                // locally between polls and re-syncs to this each refresh.
                                ui.set_status_uptime(status.uptime_secs.unwrap_or(0) as i32);

                                // Resolve the running strategy to a display item.
                                let display = catalog
                                    .by_id(&active)
                                    .map(|s| s.display_name)
                                    .unwrap_or_else(|| active.clone());
                                let (pretty, alt) = split_alt(&display);
                                let desc = catalog
                                    .by_id(&active)
                                    .map(|s| s.description)
                                    .unwrap_or_default();
                                let active_item = StrategyItem {
                                    id: active.as_str().into(),
                                    display_name: display.into(),
                                    category: "".into(),
                                    description: desc.into(),
                                    pretty: pretty.into(),
                                    alt: alt.into(),
                                    favorite: is_favorite(&active),
                                };
                                ui.set_active_item(active_item.clone());
                                // Seed the user's selection if they haven't picked one yet.
                                if !active.is_empty() && ui.get_selected_item().id.is_empty() {
                                    ui.set_selected_item(active_item);
                                }
                                if !scan_active {
                                    ui.set_is_busy(false);
                                }
                            }
                            UiEvent::DownloadProgress { bytes, total } => {
                                let pct = if let Some(tot) = total {
                                    if tot > 0 {
                                        bytes as f32 / tot as f32
                                    } else {
                                        0.0
                                    }
                                } else {
                                    0.0
                                };
                                ui.set_progress(pct);
                            }
                            UiEvent::InstallProgress(stage) => {
                                if matches!(stage, InstallStage::Resolving) {
                                    ui.set_core_error("".into());
                                    ui.set_progress(0.0);
                                }
                                if !scan_active {
                                    ui.set_is_busy(!matches!(stage, InstallStage::Done));
                                    if !matches!(stage, InstallStage::Done) {
                                        ui.set_pending_op("install".into());
                                    }
                                }
                                if let InstallStage::Done = stage {
                                    ui.set_progress(1.0);
                                    // The freshly-installed core's .bat presets are now
                                    // on disk; re-scan so its strategies appear without
                                    // needing an app restart.
                                    rebuild_strategies(&ui, &catalog);
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
                            UiEvent::UpToDate { latest } => {
                                ui.set_has_update(false);
                                if !latest.is_empty() {
                                    ui.set_latest_version(latest.into());
                                }
                            }
                            UiEvent::LatestVersion(latest) => {
                                ui.set_latest_version(latest.into());
                            }
                            UiEvent::Error(err) => {
                                if !scan_active {
                                    ui.set_is_busy(false);
                                }
                                ui.set_core_error(err.clone().into());
                                report_ui_error(&ui, err);
                            }
                            UiEvent::EngineMode(mode) => {
                                let changed = ui.get_engine_mode().as_str() != mode.slug();
                                ui.set_engine_mode(mode.slug().into());
                                ui.set_install_dir(
                                    directories[usize::from(mode == EngineMode::Zapret2)]
                                        .as_str()
                                        .into(),
                                );
                                ui.set_has_update(false);
                                ui.set_latest_version("".into());
                                ui.set_simple_failed(false);
                                if changed {
                                    ui.set_core_error("".into());
                                }
                                let selected = catalog.all().into_iter().next();
                                ui.set_selected_strategy(
                                    selected
                                        .as_ref()
                                        .map(|s| s.id.as_str())
                                        .unwrap_or_default()
                                        .into(),
                                );
                                ui.set_selected_item(
                                    selected.as_ref().map(to_item).unwrap_or_default(),
                                );
                                rebuild_strategies(&ui, &catalog);
                                rebuild_test_results(&ui);
                            }
                            UiEvent::TestStarted { total } => {
                                TEST_RESULTS.with(|b| b.borrow_mut().clear());
                                ui.set_test_running(true);
                                ui.set_test_best_id("".into());
                                ui.set_test_current(0);
                                ui.set_test_total(total as i32);
                                ui.set_test_current_strategy("".into());
                                ui.set_test_current_alt("".into());
                                rebuild_test_results(&ui);
                            }
                            UiEvent::TestProgress {
                                index,
                                total,
                                strategy,
                            } => {
                                set_scan_candidate(&ui, &catalog, &strategy);
                                ui.set_test_running(true);
                                ui.set_test_current(index as i32);
                                ui.set_test_total(total as i32);
                                // Variant label for the sidebar pill (ALT/SIMPLE FAKE),
                                // falling back to the base name when there's no variant.
                                let (pretty, alt) = split_alt(&strategy);
                                ui.set_test_current_alt(
                                    if alt.is_empty() { pretty } else { alt }.into(),
                                );
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
                                if !scan_active {
                                    ui.set_test_running(false);
                                }
                                ui.set_test_current_strategy("".into());
                                ui.set_test_current_alt("".into());
                                rebuild_test_results(&ui);

                                // Auto-select the winner as the user's strategy.
                                if !best.is_empty() {
                                    if let Some(s) = catalog.by_id(&best) {
                                        ui.set_selected_item(to_item(&s));
                                        ui.set_selected_strategy(best.as_str().into());
                                    }
                                }
                            }
                            UiEvent::AutoEngageProgress {
                                index,
                                total,
                                strategy,
                            } => {
                                set_scan_candidate(&ui, &catalog, &strategy);
                                ui.set_engage_index(index as i32);
                                ui.set_engage_total(total as i32);
                                ui.set_test_current(index as i32);
                                ui.set_test_total(total as i32);
                                ui.set_test_current_strategy(strategy.into());
                            }
                            UiEvent::AutoEngageFailed => {
                                // Distinct from a user cancel: flip the dial to its
                                // red error state and release the busy lock.
                                ui.set_simple_failed(true);
                                ui.set_pending_op("".into());
                                if !scan_active {
                                    ui.set_is_busy(false);
                                }
                            }
                            UiEvent::Maintenance(m) => {
                                ui.set_game_filter(m.game_filter.slug().into());
                                ui.set_ipset_mode(m.ipset_mode.slug().into());
                                ui.set_ipset_lines(m.ipset_lines as i32);
                                ui.set_ipset_age_days(
                                    m.ipset_age_days.map(|d| d as i32).unwrap_or(-1),
                                );
                            }
                            UiEvent::MaintenanceResult { kind, ok, message } => {
                                match kind.as_str() {
                                    "ipset" => {
                                        ui.set_ipset_busy(false);
                                        ui.set_ipset_msg(message.into());
                                        ui.set_ipset_ok(ok);
                                    }
                                    "hosts" => {
                                        ui.set_hosts_busy(false);
                                        ui.set_hosts_msg(message.into());
                                        ui.set_hosts_ok(ok);
                                    }
                                    "discord" => {
                                        ui.set_discord_busy(false);
                                        ui.set_discord_msg(message.into());
                                        ui.set_discord_ok(ok);
                                    }
                                    _ => {}
                                }
                            }
                            UiEvent::HostsContent {
                                content,
                                hosts_path,
                                hosts_dir,
                            } => {
                                ui.set_hosts_content(content.into());
                                ui.set_hosts_path(hosts_path.into());
                                ui.set_hosts_dir(hosts_dir.into());
                                ui.set_hosts_modal_open(true);
                            }
                            UiEvent::AppUpdateAvailable { latest, .. } => {
                                ui.set_app_has_update(true);
                                ui.set_app_latest_version(latest.into());
                                ui.set_app_update_checking(false);
                                ui.set_app_update_checked(true);
                                ui.set_app_update_ok(true);
                                ui.set_app_update_msg("".into());
                                // A fresh detection un-dismisses the home banner.
                                ui.set_app_update_dismissed(false);
                            }
                            UiEvent::AppUpToDate { latest } => {
                                ui.set_app_has_update(false);
                                ui.set_app_latest_version(latest.into());
                                ui.set_app_update_checking(false);
                                ui.set_app_update_checked(true);
                                ui.set_app_update_ok(true);
                                ui.set_app_update_msg("".into());
                            }
                            UiEvent::AppUpdateProgress { bytes, total } => {
                                ui.set_app_update_downloading(true);
                                let pct = match total {
                                    Some(t) if t > 0 => bytes as f32 / t as f32,
                                    _ => 0.0,
                                };
                                ui.set_app_update_progress(pct);
                            }
                            UiEvent::AppUpdateError(err) => {
                                tracing::error!("Self-update error: {}", err);
                                ui.set_app_update_checking(false);
                                ui.set_app_update_downloading(false);
                                ui.set_app_update_checked(true);
                                ui.set_app_update_ok(false);
                                ui.set_app_update_msg(err.into());
                            }
                        }
                    }
                });
            }
        });

        // Run the backend loop task
        if let Some(cmd_rx) = self.cmd_rx.take() {
            self.run_backend_loop(cmd_rx, preferences.clone(), telegram.clone());
        }

        // Periodically refresh status
        {
            let cmd_tx_c = self.cmd_tx.clone();
            tokio::spawn(async move {
                loop {
                    // Commands already push immediate status updates. This poll is
                    // only a safety net for external process/service changes.
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    if cmd_tx_c
                        .try_send(BackendCmd::RefreshStatus)
                        .is_err_and(|e| matches!(e, mpsc::error::TrySendError::Closed(_)))
                    {
                        break;
                    }
                }
            });
        }

        // Trigger initial refresh; only check for updates if the user wants it.
        let _ = self.cmd_tx.try_send(BackendCmd::RefreshStatus);
        let (autoupdate, autoengage, last_strategy) = self
            .config
            .try_read()
            .map(|c| (c.autoupdate_check, c.autoengage, c.last_strategy.clone()))
            .unwrap_or((true, false, None));
        // Snapshot core startup preferences before Telegram can acquire the
        // config write lock to persist its settings. Its own startup is isolated
        // from core downloads and the app's Windows startup preference.
        telegram.start_on_launch();
        if autoupdate {
            let _ = self.cmd_tx.try_send(BackendCmd::CheckUpdate);
            // Also check whether zapret-ui itself has a newer release.
            if !cfg!(target_os = "macos") {
                let _ = self.cmd_tx.try_send(BackendCmd::CheckSelfUpdate);
            }
        }
        // Auto-start the last-used strategy on launch when enabled.
        if autoengage {
            if cfg!(windows) {
                let _ = self.cmd_tx.try_send(BackendCmd::AutoEngage);
            } else if let Some(id) = last_strategy {
                let _ = self.cmd_tx.try_send(BackendCmd::Start(id));
            }
        }

        // Push the embedded icon onto the native window as ICON_SMALL/ICON_BIG.
        // The Slint `icon` property covers the taskbar, but the title-bar small
        // icon needs WM_SETICON. The window can take a few seconds to appear
        // (renderer warm-up), so retry on a timer until it lands, then stop.
        #[cfg(windows)]
        {
            let icon_timer = std::rc::Rc::new(slint::Timer::default());
            let icon_timer_weak = std::rc::Rc::downgrade(&icon_timer);
            let icon_attempts = std::cell::Cell::new(0u32);
            icon_timer.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_millis(250),
                move || {
                    let n = icon_attempts.get();
                    icon_attempts.set(n + 1);
                    if crate::winicon::set_window_icon("zapret-ui") || n >= 40 {
                        if let Some(t) = icon_timer_weak.upgrade() {
                            t.stop();
                        }
                    }
                },
            );
        }

        // Show + `run_event_loop_until_quit` (instead of `ui.run()`) so hiding the
        // window to the tray doesn't quit the app when it's the last open window.
        // Quit exits the event loop so the tray, instance lock and logs are flushed.
        ui.show()?;
        slint::run_event_loop_until_quit()?;
        Ok(())
    }

    fn run_backend_loop(
        &self,
        mut rx: mpsc::Receiver<BackendCmd>,
        preferences: preferences::Controller,
        telegram: telegram::Controller,
    ) {
        let installer = self.installer.clone();
        let runner = self.runner.clone();
        let service_ctl = self.service_ctl.clone();
        let catalog = self.catalog.clone();
        let tester = self.tester.clone();
        let maintenance = self.maintenance.clone();
        let self_updater = self.self_updater.clone();
        let event_tx = self.event_tx.clone();
        let config = self.config.clone();
        let state = self.state.clone();
        let command_tx = self.cmd_tx.clone();

        tokio::spawn(async move {
            let test_running = Arc::new(AtomicBool::new(false));
            let test_cancelled = Arc::new(AtomicBool::new(false));
            // Automatic strategy selection in flight. Like `test_running`, this both
            // rejects conflicting commands and suppresses the periodic status
            // refresh (which would otherwise briefly flip the dial to "active" as
            // a candidate is probed). Shared into the spawned auto-engage task.
            let auto_engaging = Arc::new(AtomicBool::new(false));
            let auto_cancelled = Arc::new(AtomicBool::new(false));
            // Last-notified bypass running state, so start/stop toasts fire only on
            // a real transition (see `notify_bypass`). Seeded to "stopped". An
            // `Arc<Mutex>` so the spawned auto-engage task can update it too.
            let notified_running = Arc::new(tokio::sync::Mutex::new(Some(false)));
            let scan_completion = Arc::new(std::sync::Mutex::new(()));
            let mut scan_task: Option<tokio::task::JoinHandle<()>> = None;
            while let Some(cmd) = rx.recv().await {
                let mut scan_active = scan_task.as_ref().is_some_and(|task| !task.is_finished())
                    || auto_engaging.load(Ordering::SeqCst)
                    || test_running.load(Ordering::SeqCst);
                if scan_active && requires_scan_handoff(&cmd) {
                    cancel_scan_before_handoff(
                        &mut scan_task,
                        &tester,
                        &auto_cancelled,
                        &test_cancelled,
                        &scan_completion,
                    )
                    .await;
                    scan_active = false;
                }
                let Some(cmd) = command_during_scan(cmd, scan_active) else {
                    continue;
                };
                retire_finished_scan(&mut scan_task, &cmd);
                match cmd {
                    BackendCmd::Quit => {
                        auto_cancelled.store(true, Ordering::SeqCst);
                        test_cancelled.store(true, Ordering::SeqCst);
                        tester.cancel();
                        if let Some(task) = scan_task.take() {
                            let _ = task.await;
                        }
                        if let Err(e) = telegram.shutdown().await {
                            let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
                            continue;
                        }
                        if runner.detect_running().await.running_mode == RunningMode::UserProcess {
                            if let Err(e) = runner.stop().await {
                                let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
                                continue;
                            }
                        }
                        preferences.flush().await;
                        let _ = slint::invoke_from_event_loop(|| {
                            let _ = slint::quit_event_loop();
                        });
                        break;
                    }
                    BackendCmd::SetEngineMode(_) | BackendCmd::SetZapret2Enabled(_) => {
                        if !cfg!(windows) {
                            continue;
                        }
                        let proposal = proposed_engine_preferences(&*config.read().await, &cmd);
                        if let Some((mode, enabled)) = proposal {
                            let result: anyhow::Result<()> = async {
                                #[cfg(windows)]
                                if enabled {
                                    config.read().await.validate_engine_directories()?;
                                }
                                let previous_mode = config.read().await.effective_engine_mode();
                                if mode != previous_mode
                                    || matches!(cmd, BackendCmd::SetZapret2Enabled(false))
                                {
                                    if !stop_bypass_before_install(
                                        &runner,
                                        &service_ctl,
                                        &config,
                                        &event_tx,
                                    )
                                    .await
                                    {
                                        return Ok(());
                                    }
                                    // Do not let an excluded automatic service launch
                                    // the old engine again at the next Windows boot.
                                    if service_ctl
                                        .installed_engine()
                                        .await
                                        .is_some_and(|engine| !mode.includes(engine))
                                    {
                                        if let Err(error) = service_ctl.remove().await {
                                            if !elevate_or_report(
                                                error,
                                                "service-remove",
                                                None,
                                                &config,
                                                &event_tx,
                                            )
                                            .await
                                            {
                                                return Ok(());
                                            }
                                        }
                                    }
                                }
                                let mut cfg = config.write().await;
                                save_engine_preferences(&mut cfg, mode, enabled, AppConfig::save)?;
                                installer.set_engine_mode(mode);
                                catalog.set_engine_mode(mode);
                                Ok(())
                            }
                            .await;
                            if let Err(error) = result {
                                let _ = event_tx.send(UiEvent::Error(format!("{error:#}")));
                            }
                        }
                        // Also restore optimistic controls after validation,
                        // stop, authorization or save failures.
                        let cfg = config.read().await;
                        let _ = event_tx.send(UiEvent::Zapret2Enabled(cfg.zapret2_enabled));
                        let _ = event_tx.send(UiEvent::EngineMode(cfg.effective_engine_mode()));
                        drop(cfg);
                        refresh_and_broadcast(&installer, &runner, &service_ctl, &state, &event_tx)
                            .await;
                    }
                    BackendCmd::Install | BackendCmd::Update | BackendCmd::InstallAndEngage => {
                        let engage = matches!(cmd, BackendCmd::InstallAndEngage);
                        // Install and Update are the same backend operation —
                        // `install_or_update` handles both a fresh install and an
                        // upgrade — so they share one handler; only the button differs.
                        let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Resolving));
                        if !stop_bypass_before_install(&runner, &service_ctl, &config, &event_tx)
                            .await
                        {
                            let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Done));
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                            continue;
                        }
                        let event_tx_c = event_tx.clone();
                        let progress_cb = Box::new(move |stage, bytes, total| {
                            let _ = event_tx_c.send(UiEvent::InstallProgress(stage));
                            let _ = event_tx_c.send(UiEvent::DownloadProgress { bytes, total });
                        });

                        match installer.install_or_update(progress_cb).await {
                            Ok(_) => {
                                let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Done));
                                // We just installed whatever latest is — always
                                // drop the "update available" arrow, even if
                                // version.txt is missing so the read is empty.
                                let installed =
                                    installer.installed_version().await.unwrap_or_default();
                                let _ = event_tx.send(UiEvent::UpToDate { latest: installed });
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                                if engage {
                                    let _ = command_tx.try_send(BackendCmd::AutoEngageFresh);
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                            }
                        }
                    }
                    BackendCmd::CheckUpdate => {
                        match installer.latest_version().await {
                            Ok(latest) => {
                                // Always surface the resolved latest version so the
                                // "latest" stat shows it even when we're up to date.
                                let _ = event_tx.send(UiEvent::LatestVersion(latest.clone()));
                                let current =
                                    installer.installed_version().await.unwrap_or_default();
                                // Use semver-aware comparison so equivalent / non-semver
                                // strings and downgrades don't show a spurious update.
                                if crate::zapret::updater::is_update_available(&current, &latest) {
                                    let _ = event_tx.send(UiEvent::UpdateAvailable {
                                        current,
                                        latest,
                                        url: if cfg!(target_os = "macos") { format!("{}/releases/latest", crate::zapret::macos_bundle::REPO) } else { "https://github.com/Flowseal/zapret-discord-youtube/releases/latest".to_string() },
                                    });
                                } else {
                                    // Up to date: clear any stale "update available"
                                    // notification (e.g. left over after the user just
                                    // updated the core). Without this the flag is sticky.
                                    let _ = event_tx.send(UiEvent::UpToDate { latest });
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                            }
                        }
                    }
                    BackendCmd::CheckSelfUpdate => match self_updater.latest_version().await {
                        Ok(latest) => {
                            let current = self_updater.current_version();
                            if crate::zapret::updater::is_update_available(&current, &latest) {
                                let _ =
                                    event_tx.send(UiEvent::AppUpdateAvailable { current, latest });
                            } else {
                                let _ = event_tx.send(UiEvent::AppUpToDate { latest });
                            }
                        }
                        Err(e) => {
                            let _ = event_tx.send(UiEvent::AppUpdateError(format!("{:#}", e)));
                        }
                    },
                    BackendCmd::SelfUpdate => {
                        let event_tx_c = event_tx.clone();
                        let progress_cb = Box::new(move |bytes, total| {
                            let _ = event_tx_c.send(UiEvent::AppUpdateProgress { bytes, total });
                        });
                        match self_updater.download_and_apply(progress_cb).await {
                            Ok(_) => {
                                // The new exe is now on disk at the original path.
                                // Relaunch it and exit so the user lands on the
                                // updated build; `--relaunch` makes the new process
                                // wait for this one to drop the single-instance mutex.
                                preferences.flush().await;
                                tracing::info!("Self-update applied — relaunching");
                                match relaunch_after_update() {
                                    Ok(_) => std::process::exit(0),
                                    Err(e) => {
                                        let _ = event_tx.send(UiEvent::AppUpdateError(format!(
                                            "Update installed but relaunch failed: {e}. Please restart the app."
                                        )));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::AppUpdateError(format!("{:#}", e)));
                            }
                        }
                    }
                    BackendCmd::Start(strategy_id) => {
                        if let Some(strategy) =
                            catalog.all().into_iter().find(|s| s.id == strategy_id)
                        {
                            match runner.start(&strategy).await {
                                Ok(pid) => {
                                    {
                                        let mut cfg = config.write().await;
                                        cfg.last_strategy = Some(strategy_id.clone());
                                        let _ = cfg.save();
                                    }
                                    notify_bypass(
                                        &config,
                                        &notified_running,
                                        true,
                                        Some(&strategy_id),
                                    )
                                    .await;
                                    let mut status = runner.detect_running().await;
                                    if status.running_mode != RunningMode::None {
                                        status.active_strategy = Some(strategy_id);
                                        status.active_engine = Some(strategy.engine);
                                        status.winws_pid = Some(pid);
                                    }
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                                    refresh_and_broadcast(
                                        &installer,
                                        &runner,
                                        &service_ctl,
                                        &state,
                                        &event_tx,
                                    )
                                    .await;
                                }
                            }
                        } else {
                            let msg = tr_config(&config, "err.strategy_not_found")
                                .await
                                .replace("{strategy}", &strategy_id);
                            let _ = event_tx.send(UiEvent::Error(msg));
                        }
                    }
                    BackendCmd::AutoEngage | BackendCmd::AutoEngageFresh => {
                        // Already running? Nothing to do — settle the status (the
                        // dial is already "active").
                        let status = runner.detect_running().await;
                        if status.running_mode != RunningMode::None {
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                            continue;
                        }

                        // ── FAST PATH ──
                        // A remembered, previously-verified strategy: start it
                        // immediately (no probing up front, so the dial turns on at
                        // once) and verify it quietly in the background. If that
                        // background check fails, we forget the remembered strategy
                        // so the NEXT engage runs a full scan.
                        // Windows scans compare against a fresh no-bypass baseline.
                        // Keep the existing fast path on macOS.
                        let last_good =
                            if cfg!(windows) || matches!(cmd, BackendCmd::AutoEngageFresh) {
                                None
                            } else {
                                config.read().await.simple_last_good.clone()
                            };
                        let mut fast_started = false;
                        if let Some(id) = last_good {
                            match catalog.by_id(&id) {
                                Some(strategy) => match runner.start(&strategy).await {
                                    Ok(pid) => {
                                        {
                                            let mut cfg = config.write().await;
                                            cfg.last_strategy = Some(id.clone());
                                            let _ = cfg.save();
                                        }
                                        notify_bypass(&config, &notified_running, true, Some(&id))
                                            .await;
                                        let mut status = runner.detect_running().await;
                                        status.running_mode = if cfg!(target_os = "macos") {
                                            RunningMode::SystemService
                                        } else {
                                            RunningMode::UserProcess
                                        };
                                        status.active_strategy = Some(id.clone());
                                        status.winws_pid = Some(pid);
                                        state.set_status(status.clone()).await;
                                        let _ = event_tx.send(UiEvent::Status(status));
                                        fast_started = true;

                                        // Background verify (invisible to the user).
                                        let tester_c = tester.clone();
                                        let runner_c = runner.clone();
                                        let config_c = config.clone();
                                        let id_c = id.clone();
                                        tokio::spawn(async move {
                                            match tester_c.verify().await {
                                                Ok(true) => tracing::info!(
                                                    "Cached automatic strategy verified: {id_c}"
                                                ),
                                                Ok(false) => {
                                                    // Only forget it if it's still the
                                                    // strategy we started (the user may
                                                    // have stopped/changed it meanwhile).
                                                    let s = runner_c.detect_running().await;
                                                    if s.running_mode != RunningMode::None
                                                        && s.active_strategy.as_deref()
                                                            == Some(id_c.as_str())
                                                    {
                                                        let mut cfg = config_c.write().await;
                                                        cfg.simple_last_good = None;
                                                        let _ = cfg.save();
                                                        tracing::info!(
                                                            "Cached automatic strategy {id_c} failed background verify; next engage will full-scan"
                                                        );
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::warn!("Background verify error: {e:#}")
                                                }
                                            }
                                        });
                                    }
                                    Err(e) => {
                                        if e.downcast_ref::<crate::contracts::AuthorizationCancelled>().is_some() {
                                            // Cancelling permission is not a failed strategy.
                                            // Keep the remembered choice and end this click
                                            // instead of opening another password dialog.
                                            let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
                                            let _ = event_tx.send(UiEvent::AutoEngageFailed);
                                            refresh_and_broadcast(&installer, &runner, &service_ctl, &state, &event_tx).await;
                                            continue;
                                        }
                                        // Couldn't start the remembered one — forget it
                                        // and fall through to a full scan.
                                        tracing::warn!("Fast-engage start failed for {id}: {e:#}");
                                        let mut cfg = config.write().await;
                                        cfg.simple_last_good = None;
                                        let _ = cfg.save();
                                    }
                                },
                                None => {
                                    // Remembered strategy no longer installed — forget it.
                                    let mut cfg = config.write().await;
                                    cfg.simple_last_good = None;
                                    let _ = cfg.save();
                                }
                            }
                        }
                        if fast_started {
                            continue;
                        }

                        // ── FULL SCAN PATH ──
                        if auto_engaging
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            // A scan is already running — ignore the duplicate.
                            continue;
                        }
                        auto_cancelled.store(false, Ordering::SeqCst);
                        // Build the candidate order from the saved last-good +
                        // favorites (the dial handles install on a separate press,
                        // so an empty catalog here just means nothing is installed).
                        let (last, favs) = {
                            let c = config.read().await;
                            (
                                c.simple_last_good
                                    .clone()
                                    .or_else(|| c.last_strategy.clone()),
                                c.favorites.clone(),
                            )
                        };
                        let candidates = ordered_candidates(&catalog, last.as_deref(), &favs);
                        if candidates.is_empty() {
                            auto_engaging.store(false, Ordering::SeqCst);
                            let _ = event_tx.send(UiEvent::Error(
                                tr_config(&config, "err.no_strategies_to_test").await,
                            ));
                            let _ = event_tx.send(UiEvent::AutoEngageFailed);
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                            continue;
                        }
                        let _ = event_tx.send(UiEvent::ScanStarted {
                            kind: ScanKind::Auto,
                            total: candidates.len() as u32,
                        });

                        // Run on its own task so the loop keeps servicing commands
                        // (crucially CancelAutoEngage, which flips the cancel flag).
                        let tester_c = tester.clone();
                        let runner_c = runner.clone();
                        let installer_c = installer.clone();
                        let service_ctl_c = service_ctl.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let event_tx_c = event_tx.clone();
                        let auto_engaging_c = auto_engaging.clone();
                        let auto_cancelled_c = auto_cancelled.clone();
                        let notified_c = notified_running.clone();
                        let completion_c = scan_completion.clone();
                        scan_task = Some(tokio::spawn(async move {
                            let ev_result = event_tx_c.clone();
                            let on_each = Box::new(move |result| {
                                let _ = ev_result.send(UiEvent::TestResult(result));
                            });
                            let ev_progress = event_tx_c.clone();
                            let cancelled = auto_cancelled_c.clone();
                            let cancel_tester = tester_c.clone();
                            let on_progress = Box::new(move |index, total, id: &str| {
                                // Replay a cancel that arrived before this task
                                // began and the adapter reset its cancel flag.
                                if cancelled.load(Ordering::SeqCst) {
                                    cancel_tester.cancel();
                                    return;
                                }
                                let _ = ev_progress.send(UiEvent::AutoEngageProgress {
                                    index,
                                    total,
                                    strategy: id.to_owned(),
                                });
                            });
                            let mut outcome = tester_c
                                .auto_engage_with_results(candidates, on_each, on_progress)
                                .await;
                            match finish_scan(
                                &runner_c,
                                &auto_engaging_c,
                                &auto_cancelled_c,
                                &completion_c,
                            )
                            .await
                            {
                                Ok(true) => outcome = Ok(AutoEngageOutcome::Cancelled),
                                Err(e) => outcome = Err(e),
                                Ok(false) => {}
                            }
                            match outcome {
                                Ok(AutoEngageOutcome::Engaged(id)) => {
                                    {
                                        let mut cfg = config_c.write().await;
                                        cfg.last_strategy = Some(id.clone());
                                        // A full scan just proved this one works —
                                        // remember it so the next engage fast-starts it.
                                        cfg.simple_last_good = Some(id.clone());
                                        let _ = cfg.save();
                                    }
                                    notify_bypass(&config_c, &notified_c, true, Some(&id)).await;
                                }
                                Ok(AutoEngageOutcome::NoneWorking) => {
                                    let _ = event_tx_c.send(UiEvent::Error(
                                        tr_config(&config_c, "err.auto_engage_failed").await,
                                    ));
                                    let _ = event_tx_c.send(UiEvent::AutoEngageFailed);
                                }
                                Ok(AutoEngageOutcome::Cancelled) => {
                                    // No error — the dial returns to "off" on refresh.
                                }
                                Err(e) => {
                                    let _ = event_tx_c.send(UiEvent::Error(format!("{:#}", e)));
                                    let _ = event_tx_c.send(UiEvent::AutoEngageFailed);
                                }
                            }
                            let best = if auto_cancelled_c.load(Ordering::SeqCst) {
                                String::new()
                            } else {
                                runner_c
                                    .detect_running()
                                    .await
                                    .active_strategy
                                    .unwrap_or_default()
                            };
                            complete_scan_results(
                                &config_c,
                                &event_tx_c,
                                best,
                                tester_c.last_results(),
                            )
                            .await;
                            refresh_and_broadcast(
                                &installer_c,
                                &runner_c,
                                &service_ctl_c,
                                &state_c,
                                &event_tx_c,
                            )
                            .await;
                            let _ = event_tx_c.send(UiEvent::ScanFinished);
                        }));
                    }
                    BackendCmd::CancelScan
                    | BackendCmd::CancelAutoEngage
                    | BackendCmd::CancelTest => {
                        let cancelled = cancel_scan_and_stop(
                            &mut scan_task,
                            &tester,
                            &runner,
                            &auto_cancelled,
                            &test_cancelled,
                            &scan_completion,
                        )
                        .await;
                        if !matches!(cancelled, Ok(false)) {
                            auto_engaging.store(false, Ordering::SeqCst);
                            test_running.store(false, Ordering::SeqCst);
                            if let Err(error) = cancelled {
                                let _ = event_tx.send(UiEvent::Error(format!("{error:#}")));
                            }
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                            let _ = event_tx.send(UiEvent::ScanFinished);
                        }
                    }
                    BackendCmd::Stop => {
                        let status = runner.detect_running().await;
                        if status.running_mode == RunningMode::SystemService {
                            match service_ctl.stop().await {
                                Ok(_) => {
                                    notify_bypass(&config, &notified_running, false, None).await;
                                    refresh_and_broadcast(
                                        &installer,
                                        &runner,
                                        &service_ctl,
                                        &state,
                                        &event_tx,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    if elevate_or_report(
                                        e,
                                        "service-stop",
                                        None,
                                        &config,
                                        &event_tx,
                                    )
                                    .await
                                    {
                                        notify_bypass(&config, &notified_running, false, None)
                                            .await;
                                        refresh_and_broadcast(
                                            &installer,
                                            &runner,
                                            &service_ctl,
                                            &state,
                                            &event_tx,
                                        )
                                        .await;
                                    }
                                }
                            }
                        } else {
                            match runner.stop().await {
                                Ok(_) => {
                                    notify_bypass(&config, &notified_running, false, None).await;
                                    refresh_and_broadcast(
                                        &installer,
                                        &runner,
                                        &service_ctl,
                                        &state,
                                        &event_tx,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceInstall(strategy_id) => {
                        if let Some(strategy) =
                            catalog.all().into_iter().find(|s| s.id == strategy_id)
                        {
                            // A user-process bypass holds the WinDivert driver, which
                            // would make the service's own winws.exe fail to start.
                            // Stop it first so the service can take over cleanly.
                            #[cfg(windows)]
                            if !stop_process_before_service(&runner, &event_tx).await {
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                                continue;
                            }
                            // Always install via the protected machine dir — even when
                            // we're already elevated — so the LocalSystem service never
                            // runs winws.exe out of the user-writable install dir.
                            match service_ctl.install_protected(&strategy).await {
                                Ok(_) => {
                                    notify_bypass(
                                        &config,
                                        &notified_running,
                                        true,
                                        Some(&strategy_id),
                                    )
                                    .await;
                                    refresh_and_broadcast(
                                        &installer,
                                        &runner,
                                        &service_ctl,
                                        &state,
                                        &event_tx,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    if elevate_or_report(
                                        e,
                                        "service-install",
                                        Some(&strategy_id),
                                        &config,
                                        &event_tx,
                                    )
                                    .await
                                    {
                                        refresh_and_broadcast(
                                            &installer,
                                            &runner,
                                            &service_ctl,
                                            &state,
                                            &event_tx,
                                        )
                                        .await;
                                    }
                                }
                            }
                        } else {
                            let msg = tr_config(&config, "err.strategy_not_found")
                                .await
                                .replace("{strategy}", &strategy_id);
                            let _ = event_tx.send(UiEvent::Error(msg));
                        }
                    }
                    BackendCmd::ServiceRemove => match service_ctl.remove().await {
                        Ok(_) => {
                            notify_bypass(&config, &notified_running, false, None).await;
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                        }
                        Err(e) => {
                            if elevate_or_report(e, "service-remove", None, &config, &event_tx)
                                .await
                            {
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                            }
                        }
                    },
                    BackendCmd::ServiceStart => {
                        #[cfg(windows)]
                        {
                            let cfg = config.read().await;
                            let mode = cfg.effective_engine_mode();
                            if service_ctl
                                .installed_engine()
                                .await
                                .is_some_and(|engine| !mode.includes(engine))
                            {
                                let strategy = ordered_candidates(
                                    &catalog,
                                    cfg.last_strategy.as_deref(),
                                    &cfg.favorites,
                                )
                                .into_iter()
                                .next();
                                drop(cfg);
                                if let Some(strategy) = strategy {
                                    let _ = command_tx
                                        .try_send(BackendCmd::ServiceInstall(strategy.id));
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(
                                        tr_config(&config, "err.no_strategies_to_test").await,
                                    ));
                                }
                                continue;
                            }
                        }
                        // Release the WinDivert driver from any user-process bypass so
                        // the service's winws.exe isn't blocked from starting.
                        #[cfg(windows)]
                        if !stop_process_before_service(&runner, &event_tx).await {
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                            continue;
                        }
                        match service_ctl.start().await {
                            Ok(_) => {
                                notify_bypass(&config, &notified_running, true, None).await;
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                            }
                            Err(e) => {
                                if elevate_or_report(e, "service-start", None, &config, &event_tx)
                                    .await
                                {
                                    refresh_and_broadcast(
                                        &installer,
                                        &runner,
                                        &service_ctl,
                                        &state,
                                        &event_tx,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStop => match service_ctl.stop().await {
                        Ok(_) => {
                            notify_bypass(&config, &notified_running, false, None).await;
                            refresh_and_broadcast(
                                &installer,
                                &runner,
                                &service_ctl,
                                &state,
                                &event_tx,
                            )
                            .await;
                        }
                        Err(e) => {
                            if elevate_or_report(e, "service-stop", None, &config, &event_tx).await
                            {
                                refresh_and_broadcast(
                                    &installer,
                                    &runner,
                                    &service_ctl,
                                    &state,
                                    &event_tx,
                                )
                                .await;
                            }
                        }
                    },
                    BackendCmd::RefreshStatus => {
                        let mut status = runner.detect_running().await;
                        status.service_installed = service_ctl.is_installed().await;
                        if status.running_mode == RunningMode::None {
                            match service_ctl.status().await {
                                Ok(srv_mode) if srv_mode != RunningMode::None => {
                                    status.running_mode = srv_mode;
                                }
                                Ok(_) => {}
                                // Don't change status on an SCM error, but log why
                                // (access denied, etc.) instead of swallowing it.
                                Err(e) => tracing::warn!("Service status query failed: {:#}", e),
                            }
                        }
                        status.installed = installer.is_installed().await;
                        // If a Windows service or user process is detected, treat zapret as installed
                        // even if our managed install dir is empty (system service may point elsewhere).
                        if !cfg!(windows) && status.running_mode != RunningMode::None {
                            status.installed = true;
                        }
                        status.installed_version = installer.installed_version().await;
                        state.set_status(status.clone()).await;
                        let _ = event_tx.send(UiEvent::Status(status));
                        // Keep the Settings filter toggles in sync with disk.
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
                    }
                    BackendCmd::OpenInstallFolder => {
                        let cfg = config.read().await;
                        let install_dir = cfg.engine_install_dir(
                            if cfg.effective_engine_mode() == EngineMode::Zapret2 {
                                EngineKind::Zapret2
                            } else {
                                EngineKind::Zapret1
                            },
                        );
                        open_external(&install_dir.to_string_lossy());
                    }
                    BackendCmd::OpenIpsetFile => {
                        let install_dir = current_install_dir(&config).await;
                        let path =
                            crate::zapret::paths::lists_dir(&install_dir).join("ipset-all.txt");
                        if path.exists() {
                            open_external(&path.display().to_string());
                        } else {
                            let msg = tr_config(&config, "err.ipset_missing")
                                .await
                                .replace("{path}", &path.display().to_string());
                            let _ = event_tx.send(UiEvent::Error(msg));
                        }
                    }
                    BackendCmd::OpenHostsFile => {
                        if let Err(e) = winexec::open_hosts_file() {
                            let _ = event_tx.send(UiEvent::Error(format!("{e:#}")));
                        }
                    }
                    BackendCmd::SetFavorites(favs) => {
                        let mut cfg = config.write().await;
                        cfg.favorites = favs;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist favorites: {}", e);
                        }
                    }
                    BackendCmd::SetNotifications(on) => {
                        let mut cfg = config.write().await;
                        cfg.notifications_enabled = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist notifications setting: {}", e);
                        }
                    }
                    BackendCmd::SetAutostart(on) => {
                        // Apply the HKCU Run key, then persist the preference.
                        crate::winenv::set_autostart(on);
                        let mut cfg = config.write().await;
                        cfg.autostart = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist autostart setting: {}", e);
                        }
                    }
                    BackendCmd::SetAutoupdateCheck(on) => {
                        let mut cfg = config.write().await;
                        cfg.autoupdate_check = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist autoupdate setting: {}", e);
                        }
                    }
                    BackendCmd::SetMinimizeToTray(on) => {
                        let mut cfg = config.write().await;
                        cfg.minimize_to_tray = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist minimize-to-tray setting: {}", e);
                        }
                    }
                    BackendCmd::MinimizedToTray => {
                        // Show the "still running in the tray" toast only the very
                        // first time, then persist the flag so it never repeats.
                        let mut cfg = config.write().await;
                        if !cfg.tray_notice_shown {
                            cfg.tray_notice_shown = true;
                            let lang = crate::i18n::code(cfg.language);
                            if let Err(e) = cfg.save() {
                                tracing::warn!("Failed to persist tray-notice flag: {}", e);
                            }
                            drop(cfg);
                            let title = crate::i18n::tr(lang, "notify.tray_title");
                            let body = crate::i18n::tr(lang, "notify.tray_body");
                            tokio::task::spawn_blocking(move || crate::notify::show(&title, &body));
                        }
                    }
                    BackendCmd::SetAutoengage(on) => {
                        let mut cfg = config.write().await;
                        cfg.autoengage = on;
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist autoengage setting: {}", e);
                        }
                    }
                    BackendCmd::SetTheme(slug) => {
                        let mut cfg = config.write().await;
                        cfg.theme = crate::config::Theme::from_slug(&slug);
                        if let Err(e) = cfg.save() {
                            tracing::warn!("Failed to persist theme setting: {}", e);
                        }
                    }
                    BackendCmd::SetGameFilter(mode) => {
                        match maintenance.set_game_filter(mode).await {
                            Ok(_) => {
                                tracing::info!("Game filter changed — restart the bypass to apply");
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                            }
                        }
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
                    }
                    BackendCmd::SetIpsetMode(mode) => {
                        if let Err(e) = maintenance.set_ipset_mode(mode).await {
                            let _ = event_tx.send(UiEvent::MaintenanceResult {
                                kind: "ipset".to_string(),
                                ok: false,
                                message: e.to_string(),
                            });
                        } else {
                            tracing::info!("IPSet filter changed — restart the bypass to apply");
                        }
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
                    }
                    BackendCmd::UpdateIpsetList => {
                        let lang = crate::i18n::code(config.read().await.language);
                        let (ok, message) = match maintenance.update_ipset_list().await {
                            Ok(count) => (
                                true,
                                crate::i18n::tr(lang, "msg.ipset_updated")
                                    .replace("{count}", &count.to_string()),
                            ),
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = event_tx.send(UiEvent::MaintenanceResult {
                            kind: "ipset".to_string(),
                            ok,
                            message,
                        });
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
                    }
                    BackendCmd::UpdateHostsFile => {
                        let lang = crate::i18n::code(config.read().await.language);
                        match maintenance.update_hosts_file().await {
                            Ok(check) => {
                                let message = if check.updated {
                                    crate::i18n::tr(lang, "msg.hosts_updated")
                                } else if check.up_to_date {
                                    crate::i18n::tr(lang, "msg.hosts_up_to_date")
                                } else {
                                    crate::i18n::tr(lang, "msg.hosts_out_of_date")
                                };
                                let _ = event_tx.send(UiEvent::MaintenanceResult {
                                    kind: "hosts".to_string(),
                                    ok: true,
                                    message,
                                });
                                if !check.up_to_date && !check.updated {
                                    // Open the folder containing the hosts file (so the
                                    // user can paste), then open the in-app review window.
                                    open_external(&check.hosts_dir);
                                    let _ = event_tx.send(UiEvent::HostsContent {
                                        content: check.content,
                                        hosts_path: check.hosts_path,
                                        hosts_dir: check.hosts_dir,
                                    });
                                }
                            }
                            Err(e) => {
                                #[cfg(windows)]
                                if e.to_string().contains("NeedsElevation") {
                                    let install_dir = current_install_dir(&config).await;
                                    let result =
                                        match relaunch_elevated("hosts-update", None, &install_dir)
                                        {
                                            Ok(handle) => wait_for_elevated_result(handle).await,
                                            Err(err) => Err(err.to_string()),
                                        };
                                    let (ok, message) = match result {
                                        Ok(()) => {
                                            (true, crate::i18n::tr(lang, "msg.hosts_updated"))
                                        }
                                        Err(err) => (false, err),
                                    };
                                    let _ = event_tx.send(UiEvent::MaintenanceResult {
                                        kind: "hosts".to_string(),
                                        ok,
                                        message,
                                    });
                                    continue;
                                }
                                let _ = event_tx.send(UiEvent::MaintenanceResult {
                                    kind: "hosts".to_string(),
                                    ok: false,
                                    message: e.to_string(),
                                });
                            }
                        }
                    }
                    BackendCmd::ClearDiscordCache => {
                        let lang = crate::i18n::code(config.read().await.language);
                        let (ok, message) = match maintenance.clear_discord_cache().await {
                            Ok(res) => {
                                let cleared = crate::i18n::tr(lang, "msg.discord_cache_cleared")
                                    .replace("{count}", &res.cleared.to_string());
                                let msg = if res.discord_was_running {
                                    format!(
                                        "{} · {}",
                                        crate::i18n::tr(lang, "msg.discord_closed"),
                                        cleared
                                    )
                                } else {
                                    cleared
                                };
                                (true, msg)
                            }
                            Err(e) => (false, e.to_string()),
                        };
                        let _ = event_tx.send(UiEvent::MaintenanceResult {
                            kind: "discord".to_string(),
                            ok,
                            message,
                        });
                    }
                    BackendCmd::TestStrategies => {
                        if test_running
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            continue;
                        }

                        let cfg = config.read().await;
                        let strategies = ordered_candidates(
                            &catalog,
                            cfg.simple_last_good
                                .as_deref()
                                .or(cfg.last_strategy.as_deref()),
                            &cfg.favorites,
                        );
                        drop(cfg);
                        if strategies.is_empty() {
                            test_running.store(false, Ordering::SeqCst);
                            let _ = event_tx.send(UiEvent::Error(
                                tr_config(&config, "err.no_strategies_to_test").await,
                            ));
                            continue;
                        }
                        let status = runner.detect_running().await;
                        if status.running_mode != RunningMode::None {
                            test_running.store(false, Ordering::SeqCst);
                            let _ = event_tx.send(UiEvent::Error(
                                tr_config(&config, "err.stop_bypass_before_test").await,
                            ));
                            continue;
                        }
                        let total = strategies.len() as u32;
                        test_cancelled.store(false, Ordering::SeqCst);
                        let _ = event_tx.send(UiEvent::ScanStarted {
                            kind: ScanKind::Test,
                            total,
                        });
                        let _ = event_tx.send(UiEvent::TestStarted { total });

                        // Run the test on its own task so the backend loop keeps
                        // servicing commands — crucially CancelTest, which just
                        // flips the tester's cancel flag (otherwise the loop would
                        // be blocked on `test_all` and never see the cancel).
                        let tester_c = tester.clone();
                        let runner_c = runner.clone();
                        let installer_c = installer.clone();
                        let service_ctl_c = service_ctl.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let event_tx_c = event_tx.clone();
                        let test_running_c = test_running.clone();
                        let test_cancelled_c = test_cancelled.clone();
                        let catalog_c = catalog.clone();
                        let completion_c = scan_completion.clone();
                        scan_task = Some(tokio::spawn(async move {
                            let ev_result = event_tx_c.clone();
                            let on_each = Box::new(move |r| {
                                let _ = ev_result.send(UiEvent::TestResult(r));
                            });
                            let ev_progress = event_tx_c.clone();
                            let cancelled = test_cancelled_c.clone();
                            let cancel_tester = tester_c.clone();
                            let on_progress = Box::new(move |index, total, id: &str| {
                                if cancelled.load(Ordering::SeqCst) {
                                    cancel_tester.cancel();
                                    return;
                                }
                                let _ = ev_progress.send(UiEvent::TestProgress {
                                    index,
                                    total,
                                    strategy: id.to_string(),
                                });
                            });

                            match tester_c.test_all(strategies, on_each, on_progress).await {
                                Ok(results) => {
                                    #[cfg(windows)]
                                    let mut best = start_test_winner(
                                        &runner_c,
                                        &catalog_c,
                                        &results,
                                        &test_cancelled_c,
                                        &event_tx_c,
                                    )
                                    .await;
                                    #[cfg(not(windows))]
                                    let mut best = results
                                        .first()
                                        .filter(|r| r.eligible())
                                        .map(|r| r.id.clone())
                                        .unwrap_or_default();
                                    #[cfg(not(windows))]
                                    let _ = &catalog_c;
                                    match finish_scan(
                                        &runner_c,
                                        &test_running_c,
                                        &test_cancelled_c,
                                        &completion_c,
                                    )
                                    .await
                                    {
                                        Ok(true) => best.clear(),
                                        Err(e) => {
                                            best.clear();
                                            let _ =
                                                event_tx_c.send(UiEvent::Error(format!("{e:#}")));
                                        }
                                        Ok(false) => {}
                                    }
                                    if !best.is_empty() {
                                        let mut cfg = config_c.write().await;
                                        cfg.last_strategy = Some(best.clone());
                                        #[cfg(windows)]
                                        {
                                            cfg.simple_last_good = Some(best.clone());
                                        }
                                        let _ = cfg.save();
                                    }
                                    complete_scan_results(&config_c, &event_tx_c, best, results)
                                        .await;
                                }
                                Err(e) => {
                                    let msg = tr_config(&config_c, "err.strategy_test_failed")
                                        .await
                                        .replace("{error}", &e.to_string());
                                    let _ = event_tx_c.send(UiEvent::Error(msg));
                                    complete_scan_results(
                                        &config_c,
                                        &event_tx_c,
                                        String::new(),
                                        tester_c.last_results(),
                                    )
                                    .await;
                                }
                            }

                            // Settle the status display after the test churns winws.
                            refresh_and_broadcast(
                                &installer_c,
                                &runner_c,
                                &service_ctl_c,
                                &state_c,
                                &event_tx_c,
                            )
                            .await;
                            {
                                let _completion = completion_c.lock().unwrap();
                                test_running_c.store(false, Ordering::SeqCst);
                            }
                            let _ = event_tx_c.send(UiEvent::ScanFinished);
                        }));
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{RuntimeStatus, Strategy};

    struct StopFailure(RunningMode);

    #[async_trait::async_trait]
    impl Runner for StopFailure {
        async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
            panic!("a service transition must not start another child");
        }

        async fn stop(&self) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("child is still running").context("cannot release WinDivert"))
        }

        async fn detect_running(&self) -> RuntimeStatus {
            RuntimeStatus {
                running_mode: self.0,
                ..RuntimeStatus::default()
            }
        }
    }

    #[tokio::test]
    async fn failed_process_stop_blocks_service_transition_and_reports_cause() {
        let runner: Arc<dyn Runner> = Arc::new(StopFailure(RunningMode::UserProcess));
        let (events, mut rx) = broadcast::channel(4);
        assert!(!stop_process_before_service(&runner, &events).await);
        assert!(matches!(rx.recv().await.unwrap(), UiEvent::Error(message)
            if message.contains("cannot release WinDivert") && message.contains("child is still running")));
    }

    #[tokio::test]
    async fn service_transition_preserves_running_service_for_adapter_rollback() {
        let runner: Arc<dyn Runner> = Arc::new(StopFailure(RunningMode::SystemService));
        let (events, mut rx) = broadcast::channel(4);
        assert!(stop_process_before_service(&runner, &events).await);
        assert!(rx.try_recv().is_err());
    }
}
