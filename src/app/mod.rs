use crate::config::AppConfig;
use crate::contracts::{
    split_alt, AutoEngageOutcome, BackendCmd, GameFilterMode, InstallStage, IpsetMode, RunningMode,
    UiEvent,
};
use crate::ports::{
    Installer, Maintenance, Runner, SelfUpdater, ServiceCtl, StrategyCatalog, StrategyTester,
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

mod ui_models;
mod winexec;
use ui_models::{is_favorite, rebuild_logs, rebuild_strategies, rebuild_test_results, to_item};
use winexec::{
    open_external, relaunch_after_update, relaunch_elevated, relaunch_self_elevated,
    wait_for_elevated_result,
};

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
}

const LOG_BUF_CAP: usize = 4000;

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
            | BackendCmd::Update
            | BackendCmd::Start(_)
            | BackendCmd::AutoEngage
            | BackendCmd::Stop
            | BackendCmd::ServiceInstall(_)
            | BackendCmd::ServiceRemove
            | BackendCmd::ServiceStart
            | BackendCmd::ServiceStop
            | BackendCmd::TestStrategies
    )
}

/// Candidate strategies for auto-engage, ordered so the most likely winner is
/// tried first: the last-known-good strategy, then starred favorites, then the
/// remaining catalog order. De-duplicated, preserving first occurrence.
fn ordered_candidates(
    catalog: &Arc<dyn StrategyCatalog>,
    last_strategy: Option<&str>,
    favorites: &[String],
) -> Vec<crate::contracts::Strategy> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |out: &mut Vec<crate::contracts::Strategy>, s: crate::contracts::Strategy| {
        if seen.insert(s.id.clone()) {
            out.push(s);
        }
    };
    if let Some(id) = last_strategy {
        if let Some(s) = catalog.by_id(id) {
            push(&mut out, s);
        }
    }
    for fav in favorites {
        if let Some(s) = catalog.by_id(fav) {
            push(&mut out, s);
        }
    }
    for s in catalog.all() {
        push(&mut out, s);
    }
    out
}

/// Relaunch elevated for a one-shot service task and wait for its result,
/// surfacing the helper's actual error to the UI instead of silently relying on
/// the next status poll. Passes the current install dir explicitly so the
/// elevated helper acts on the same directory the UI is using.
async fn elevate_service_task(
    task: &str,
    strategy: Option<&str>,
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    let install_dir = current_install_dir(config).await;
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
    runner: &Arc<dyn Runner>,
    service_ctl: &Arc<dyn ServiceCtl>,
    state: &AppState,
    event_tx: &broadcast::Sender<UiEvent>,
) {
    let mut status = runner.detect_running().await;
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
    let needs_elevation = msg.contains("NeedsElevation")
        || msg.contains("os error 5")
        || msg.contains("Access is denied")
        || msg.contains("Отказано в доступе");
    if needs_elevation {
        elevate_service_task(elevate_task, strategy, config, event_tx).await
    } else {
        let _ = event_tx.send(UiEvent::Error(msg));
        false
    }
}

async fn stop_bypass_before_install(
    runner: &Arc<dyn Runner>,
    service_ctl: &Arc<dyn ServiceCtl>,
    config: &Arc<RwLock<AppConfig>>,
    event_tx: &broadcast::Sender<UiEvent>,
) -> bool {
    if let Err(e) = runner.stop().await {
        let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
        return false;
    }

    match service_ctl.status().await {
        Ok(RunningMode::WindowsService) => match service_ctl.stop().await {
            Ok(()) => true,
            Err(e) => elevate_or_report(e, "service-stop", None, config, event_tx).await,
        },
        Ok(_) => true,
        Err(e) => {
            let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
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
        config: AppConfig,
        state: AppState,
        event_tx: broadcast::Sender<UiEvent>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);

        Self {
            installer,
            runner,
            service_ctl,
            catalog,
            tester,
            maintenance,
            self_updater,
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
            let config = self.config.clone();
            ui.on_set_language(move |code| {
                let config = config.clone();
                let lang = crate::config::Language::from_code(&code);
                tokio::spawn(async move {
                    let mut cfg = config.write().await;
                    cfg.language = lang;
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Failed to persist language: {}", e);
                    }
                });
            });
        }

        // Persist the dashboard mode (simple dial vs. advanced) when the user
        // flips the toggle. The Slint side switches the view itself; we just save.
        {
            let config = self.config.clone();
            ui.on_set_ui_mode(move |slug| {
                let config = config.clone();
                let mode = crate::config::UiMode::from_slug(&slug);
                tokio::spawn(async move {
                    let mut cfg = config.write().await;
                    cfg.ui_mode = mode;
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Failed to persist UI mode: {}", e);
                    }
                });
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
        ui.set_is_admin(crate::zapret::elevation::is_elevated());

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
            ui.set_ui_mode(c.ui_mode.slug().into());
            ui.set_install_dir(c.install_dir().display().to_string().into());
        }

        // Populate strategies list (favorites first).
        rebuild_strategies(&ui, &self.catalog);

        // Restore the previously selected strategy as the user's current pick.
        if let Some(id) = last_strategy {
            if let Some(s) = self.catalog.by_id(&id) {
                ui.set_selected_item(to_item(&s));
                ui.set_selected_strategy(id.as_str().into());
            }
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
            let appdata = std::env::var("APPDATA").unwrap_or_default();
            let path = format!("{}\\zapret-ui\\logs\\app.log", appdata);
            open_external(&path);
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
        // Simple-mode dial: auto-pick a working strategy and turn the bypass on.
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_auto_engage_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::AutoEngage);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_cancel_engage_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::CancelAutoEngage);
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
            ui.on_service_start_clicked(move || {
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
            let cmd_tx_c = self.cmd_tx.clone();
            let config = self.config.clone();
            ui.on_strategy_selected(move |id| {
                // Remember the pick immediately so it survives a restart even if
                // the user never presses Engage.
                let id = id.to_string();
                let config = config.clone();
                tokio::spawn(async move {
                    let mut cfg = config.write().await;
                    cfg.last_strategy = Some(id);
                    if let Err(e) = cfg.save() {
                        tracing::warn!("Failed to persist selected strategy: {}", e);
                    }
                });
                let _ = cmd_tx_c.try_send(BackendCmd::RefreshStatus);
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
                dispatch(&cmd_tx_c, BackendCmd::CancelTest);
            });
        }
        // "Use this strategy" from a test result row: resolve it from the catalog
        // and apply it as the user's selection, then jump to the dashboard.
        {
            let catalog = self.catalog.clone();
            let config = self.config.clone();
            let ui_weak = ui.as_weak();
            ui.on_test_use_strategy(move |id| {
                if let Some(ui) = ui_weak.upgrade() {
                    let id = id.to_string();
                    if let Some(s) = catalog.by_id(&id) {
                        ui.set_selected_item(to_item(&s));
                        ui.set_selected_strategy(id.as_str().into());
                        ui.set_current_page("home".into());
                        // Persist the manual pick so it survives a restart even if
                        // the user never presses Engage (matches strategy_selected).
                        let config = config.clone();
                        tokio::spawn(async move {
                            let mut cfg = config.write().await;
                            cfg.last_strategy = Some(id);
                            if let Err(e) = cfg.save() {
                                tracing::warn!("Failed to persist tester-selected strategy: {}", e);
                            }
                        });
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
            ui.on_set_theme(move |theme| {
                let _ = cmd_tx_c.try_send(BackendCmd::SetTheme(theme.to_string()));
            });
        }
        // "Run as administrator" banner button: relaunch the whole app elevated,
        // then exit this unelevated instance so the new one can take over.
        ui.on_restart_as_admin(move || match relaunch_self_elevated() {
            Ok(_) => std::process::exit(0),
            Err(e) => tracing::error!("Failed to relaunch as administrator: {}", e),
        });

        // Copy arbitrary text to the system clipboard (used by the hosts window).
        ui.on_copy_to_clipboard(move |text| {
            if let Err(e) = clipboard_win::set_clipboard_string(&text) {
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
                    let _ = ui.hide();
                }
                // Show the one-time "still running in the tray" toast (the backend
                // decides whether it's the first time and persists the flag).
                let _ = cmd_tx_close.try_send(BackendCmd::MinimizedToTray);
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                std::process::exit(0);
            }
        });

        // Event listener task for Tray actions (use OS thread since SystemTray is not Send)
        let ui_weak = ui.as_weak();
        let open_id = tray.open_item_id.clone();
        let start_id = tray.start_item_id.clone();
        let stop_id = tray.stop_item_id.clone();
        let settings_id = tray.settings_item_id.clone();
        let quit_id = tray.quit_item_id.clone();
        let cmd_tx_tray = self.cmd_tx.clone();
        std::thread::spawn(move || {
            // Re-show and raise the window on the UI thread. `ui.show()` alone
            // re-creates a window hidden to the tray, but won't restore one the
            // user minimized to the taskbar nor raise it above other windows —
            // so we also force a Win32 restore + foreground.
            let show_window = |w: slint::Weak<MainWindow>| {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        let _ = ui.show();
                        crate::winicon::restore_and_focus_window("zapret-ui");
                    }
                });
            };
            loop {
                if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    let event_id = event.id.0.clone();
                    if event_id == open_id {
                        show_window(ui_weak.clone());
                    } else if event_id == settings_id {
                        let w = ui_weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = w.upgrade() {
                                let _ = ui.show();
                                ui.set_current_page("settings".into());
                            }
                        });
                    } else if event_id == start_id {
                        // Engage the user's selected strategy (fall back to the
                        // currently active one). Read the pick on the UI thread.
                        let w = ui_weak.clone();
                        let cmd_tx = cmd_tx_tray.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = w.upgrade() {
                                let mut id = ui.get_selected_item().id.to_string();
                                if id.is_empty() {
                                    id = ui.get_active_item().id.to_string();
                                }
                                if !id.is_empty() {
                                    let _ = cmd_tx.try_send(BackendCmd::Start(id));
                                }
                            }
                        });
                    } else if event_id == stop_id {
                        let _ = cmd_tx_tray.try_send(BackendCmd::Stop);
                    } else if event_id == quit_id {
                        std::process::exit(0);
                    }
                }

                // Left-click (button release) opens the app; a double-click does
                // too (some users double-click out of habit, and Windows may emit
                // only the DoubleClick for the second press). Right-click is
                // reserved for the context menu, so other events are ignored.
                match tray_icon::TrayIconEvent::receiver().try_recv() {
                    Ok(tray_icon::TrayIconEvent::Click {
                        button: tray_icon::MouseButton::Left,
                        button_state: tray_icon::MouseButtonState::Up,
                        ..
                    })
                    | Ok(tray_icon::TrayIconEvent::DoubleClick {
                        button: tray_icon::MouseButton::Left,
                        ..
                    }) => {
                        show_window(ui_weak.clone());
                    }
                    _ => {}
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
                                ui.set_status_installed_version(
                                    status.installed_version.unwrap_or_default().into(),
                                );
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
                                ui.set_is_busy(false);
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
                                ui.set_is_busy(!matches!(stage, InstallStage::Done));
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
                            UiEvent::LatestVersion(latest) => {
                                ui.set_latest_version(latest.into());
                            }
                            UiEvent::Error(err) => {
                                tracing::error!("UI Error: {}", err);
                                ui.set_is_busy(false);
                                // Surface the failure as a toast — errors used to be
                                // logged only, so a failed action looked like it
                                // silently did nothing. But dedupe: a single failed
                                // operation can emit a burst of identical errors, so
                                // suppress repeats of the same message within 30s and
                                // rate-limit any toast to one per 5s. (Always logged.)
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
                                    let body = err.clone();
                                    std::thread::spawn(move || crate::notify::show(&title, &body));
                                }
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
                                ui.set_test_running(false);
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
                            UiEvent::AutoEngageProgress { index, total } => {
                                ui.set_engage_index(index as i32);
                                ui.set_engage_total(total as i32);
                            }
                            UiEvent::AutoEngageFailed => {
                                // Distinct from a user cancel: flip the dial to its
                                // red error state and release the busy lock.
                                ui.set_simple_failed(true);
                                ui.set_pending_op("".into());
                                ui.set_is_busy(false);
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

        // Trigger initial refresh; only check for updates if the user wants it.
        let _ = self.cmd_tx.try_send(BackendCmd::RefreshStatus);
        let (autoupdate, autoengage, last_strategy) = self
            .config
            .try_read()
            .map(|c| (c.autoupdate_check, c.autoengage, c.last_strategy.clone()))
            .unwrap_or((true, false, None));
        if autoupdate {
            let _ = self.cmd_tx.try_send(BackendCmd::CheckUpdate);
            // Also check whether zapret-ui itself has a newer release.
            let _ = self.cmd_tx.try_send(BackendCmd::CheckSelfUpdate);
        }
        // Auto-start the last-used strategy on launch when enabled.
        if autoengage {
            if let Some(id) = last_strategy {
                let _ = self.cmd_tx.try_send(BackendCmd::Start(id));
            }
        }

        // Push the embedded icon onto the native window as ICON_SMALL/ICON_BIG.
        // The Slint `icon` property covers the taskbar, but the title-bar small
        // icon needs WM_SETICON. The window can take a few seconds to appear
        // (renderer warm-up), so retry on a timer until it lands, then stop.
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

        // Show + `run_event_loop_until_quit` (instead of `ui.run()`) so hiding the
        // window to the tray doesn't quit the app when it's the last open window.
        // The tray "Quit" item / close-without-tray path call `process::exit`.
        ui.show()?;
        slint::run_event_loop_until_quit()?;
        Ok(())
    }

    fn run_backend_loop(&self, mut rx: mpsc::Receiver<BackendCmd>) {
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

        tokio::spawn(async move {
            let test_running = Arc::new(AtomicBool::new(false));
            // Simple-mode auto-engage in flight. Like `test_running`, this both
            // rejects conflicting commands and suppresses the periodic status
            // refresh (which would otherwise briefly flip the dial to "active" as
            // a candidate is probed). Shared into the spawned auto-engage task.
            let auto_engaging = Arc::new(AtomicBool::new(false));
            // Last-notified bypass running state, so start/stop toasts fire only on
            // a real transition (see `notify_bypass`). Seeded to "stopped". An
            // `Arc<Mutex>` so the spawned auto-engage task can update it too.
            let notified_running = Arc::new(tokio::sync::Mutex::new(Some(false)));
            while let Some(cmd) = rx.recv().await {
                // While a simple-mode auto-engage is probing candidates, skip the
                // periodic status refresh: catching a candidate's winws mid-run
                // would briefly flip the dial to "active" and back.
                if auto_engaging.load(Ordering::SeqCst)
                    && matches!(cmd, BackendCmd::RefreshStatus)
                {
                    continue;
                }
                if (test_running.load(Ordering::SeqCst) || auto_engaging.load(Ordering::SeqCst))
                    && conflicts_with_running_test(&cmd)
                {
                    let _ =
                        event_tx.send(UiEvent::Error(tr_config(&config, "err.test_running").await));
                    continue;
                }
                match cmd {
                    BackendCmd::Install | BackendCmd::Update => {
                        // Install and Update are the same backend operation —
                        // `install_or_update` handles both a fresh install and an
                        // upgrade — so they share one handler; only the button differs.
                        let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Resolving));
                        if !stop_bypass_before_install(&runner, &service_ctl, &config, &event_tx)
                            .await
                        {
                            let _ = event_tx.send(UiEvent::InstallProgress(InstallStage::Done));
                            refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx).await;
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
                                refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
                                    .await;
                            }
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
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
                                        url: "https://github.com/Flowseal/zapret-discord-youtube/releases/latest".to_string(),
                                    });
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
                        if let Some(strategy) = catalog.by_id(&strategy_id) {
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
                                    status.running_mode = RunningMode::UserProcess;
                                    status.active_strategy = Some(strategy_id);
                                    status.winws_pid = Some(pid);
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                                }
                            }
                        } else {
                            let msg = tr_config(&config, "err.strategy_not_found")
                                .await
                                .replace("{strategy}", &strategy_id);
                            let _ = event_tx.send(UiEvent::Error(msg));
                        }
                    }
                    BackendCmd::AutoEngage => {
                        if auto_engaging
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            // Already engaging — ignore the duplicate click.
                            continue;
                        }
                        // Already running? Nothing to do — settle the status (the
                        // dial is already "active") and release the flag.
                        let status = runner.detect_running().await;
                        if status.running_mode != RunningMode::None {
                            auto_engaging.store(false, Ordering::SeqCst);
                            refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx).await;
                            continue;
                        }
                        // Build the candidate order from the saved last-good +
                        // favorites (the dial handles install on a separate press,
                        // so an empty catalog here just means nothing is installed).
                        let (last, favs) = {
                            let c = config.read().await;
                            (c.last_strategy.clone(), c.favorites.clone())
                        };
                        let candidates = ordered_candidates(&catalog, last.as_deref(), &favs);
                        if candidates.is_empty() {
                            auto_engaging.store(false, Ordering::SeqCst);
                            let _ = event_tx.send(UiEvent::Error(
                                tr_config(&config, "err.no_strategies_to_test").await,
                            ));
                            let _ = event_tx.send(UiEvent::AutoEngageFailed);
                            refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx).await;
                            continue;
                        }

                        // Run on its own task so the loop keeps servicing commands
                        // (crucially CancelAutoEngage, which flips the cancel flag).
                        let tester_c = tester.clone();
                        let runner_c = runner.clone();
                        let service_ctl_c = service_ctl.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let event_tx_c = event_tx.clone();
                        let auto_engaging_c = auto_engaging.clone();
                        let notified_c = notified_running.clone();
                        tokio::spawn(async move {
                            let ev_progress = event_tx_c.clone();
                            let on_progress = Box::new(move |index, total, _id: &str| {
                                let _ = ev_progress
                                    .send(UiEvent::AutoEngageProgress { index, total });
                            });
                            match tester_c.auto_engage(candidates, on_progress).await {
                                Ok(AutoEngageOutcome::Engaged(id)) => {
                                    {
                                        let mut cfg = config_c.write().await;
                                        cfg.last_strategy = Some(id.clone());
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
                            // Release the flag before the final refresh so the next
                            // periodic poll is no longer suppressed.
                            auto_engaging_c.store(false, Ordering::SeqCst);
                            refresh_and_broadcast(
                                &runner_c,
                                &service_ctl_c,
                                &state_c,
                                &event_tx_c,
                            )
                            .await;
                        });
                    }
                    BackendCmd::CancelAutoEngage => {
                        // Same cancel flag as the tester; the in-flight auto_engage
                        // notices it, stops winws, and resolves to Cancelled.
                        tester.cancel();
                    }
                    BackendCmd::Stop => {
                        let status = runner.detect_running().await;
                        if status.running_mode == RunningMode::WindowsService {
                            match service_ctl.stop().await {
                                Ok(_) => {
                                    notify_bypass(&config, &notified_running, false, None)
                                        .await;
                                    refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
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
                                    notify_bypass(&config, &notified_running, false, None)
                                        .await;
                                    refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
                                        .await;
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(format!("{:#}", e)));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceInstall(strategy_id) => {
                        if catalog.by_id(&strategy_id).is_some() {
                            // A user-process bypass holds the WinDivert driver, which
                            // would make the service's own winws.exe fail to start.
                            // Stop it first so the service can take over cleanly.
                            let _ = runner.stop().await;
                            // Always install via the protected machine dir — even when
                            // we're already elevated — so the LocalSystem service never
                            // runs winws.exe out of the user-writable install dir.
                            let install_dir = current_install_dir(&config).await;
                            match crate::zapret::service::install_service_protected(
                                &install_dir,
                                &strategy_id,
                            )
                            .await
                            {
                                Ok(_) => {
                                    notify_bypass(
                                        &config,
                                        &notified_running,
                                        true,
                                        Some(&strategy_id),
                                    )
                                    .await;
                                    refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
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
                            refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx).await;
                        }
                        Err(e) => {
                            if elevate_or_report(e, "service-remove", None, &config, &event_tx)
                                .await
                            {
                                refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
                                    .await;
                            }
                        }
                    },
                    BackendCmd::ServiceStart => {
                        // Release the WinDivert driver from any user-process bypass so
                        // the service's winws.exe isn't blocked from starting.
                        let _ = runner.stop().await;
                        match service_ctl.start().await {
                            Ok(_) => {
                                notify_bypass(&config, &notified_running, true, None).await;
                                refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
                                    .await;
                            }
                            Err(e) => {
                                if elevate_or_report(e, "service-start", None, &config, &event_tx)
                                    .await
                                {
                                    refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
                                        .await;
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStop => match service_ctl.stop().await {
                        Ok(_) => {
                            notify_bypass(&config, &notified_running, false, None).await;
                            refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx).await;
                        }
                        Err(e) => {
                            if elevate_or_report(e, "service-stop", None, &config, &event_tx).await
                            {
                                refresh_and_broadcast(&runner, &service_ctl, &state, &event_tx)
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
                        // Keep the Settings filter toggles in sync with disk.
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
                    }
                    BackendCmd::OpenInstallFolder => {
                        let install_dir = current_install_dir(&config).await;
                        let _ = std::process::Command::new("explorer")
                            .arg(&install_dir)
                            .spawn();
                    }
                    BackendCmd::OpenIpsetFile => {
                        let install_dir = current_install_dir(&config).await;
                        let path = install_dir.join("lists").join("ipset-all.txt");
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
                        // The hosts file has no extension (no default association),
                        // so open it explicitly in Notepad rather than via the shell.
                        let system_root = std::env::var("SystemRoot")
                            .unwrap_or_else(|_| r"C:\Windows".to_string());
                        let hosts_path = std::path::PathBuf::from(system_root)
                            .join("System32")
                            .join("drivers")
                            .join("etc")
                            .join("hosts");
                        let _ = std::process::Command::new("notepad.exe")
                            .arg(&hosts_path)
                            .spawn();
                    }
                    BackendCmd::CancelTest => {
                        tester.cancel();
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
                                let message = if check.up_to_date {
                                    crate::i18n::tr(lang, "msg.hosts_up_to_date")
                                } else {
                                    crate::i18n::tr(lang, "msg.hosts_out_of_date")
                                };
                                let _ = event_tx.send(UiEvent::MaintenanceResult {
                                    kind: "hosts".to_string(),
                                    ok: true,
                                    message,
                                });
                                if !check.up_to_date {
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
                            let _ = event_tx
                                .send(UiEvent::Error(tr_config(&config, "err.test_running").await));
                            continue;
                        }

                        let strategies = catalog.all();
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
                        let _ = event_tx.send(UiEvent::TestStarted { total });

                        // Run the test on its own task so the backend loop keeps
                        // servicing commands — crucially CancelTest, which just
                        // flips the tester's cancel flag (otherwise the loop would
                        // be blocked on `test_all` and never see the cancel).
                        let tester_c = tester.clone();
                        let runner_c = runner.clone();
                        let service_ctl_c = service_ctl.clone();
                        let state_c = state.clone();
                        let config_c = config.clone();
                        let event_tx_c = event_tx.clone();
                        let test_running_c = test_running.clone();
                        tokio::spawn(async move {
                            let ev_result = event_tx_c.clone();
                            let on_each = Box::new(move |r| {
                                let _ = ev_result.send(UiEvent::TestResult(r));
                            });
                            let ev_progress = event_tx_c.clone();
                            let on_progress = Box::new(move |index, total, id: &str| {
                                let _ = ev_progress.send(UiEvent::TestProgress {
                                    index,
                                    total,
                                    strategy: id.to_string(),
                                });
                            });

                            match tester_c.test_all(strategies, on_each, on_progress).await {
                                Ok(results) => {
                                    let best = results
                                        .first()
                                        .filter(|r| r.ok > 0)
                                        .map(|r| r.id.clone())
                                        .unwrap_or_default();
                                    if !best.is_empty() {
                                        let mut cfg = config_c.write().await;
                                        cfg.last_strategy = Some(best.clone());
                                        let _ = cfg.save();
                                    }
                                    let _ =
                                        event_tx_c.send(UiEvent::TestComplete { best, results });
                                }
                                Err(e) => {
                                    let msg = tr_config(&config_c, "err.strategy_test_failed")
                                        .await
                                        .replace("{error}", &e.to_string());
                                    let _ = event_tx_c.send(UiEvent::Error(msg));
                                    let _ = event_tx_c.send(UiEvent::TestComplete {
                                        best: String::new(),
                                        results: Vec::new(),
                                    });
                                }
                            }

                            // Settle the status display after the test churns winws.
                            refresh_and_broadcast(&runner_c, &service_ctl_c, &state_c, &event_tx_c)
                                .await;
                            test_running_c.store(false, Ordering::SeqCst);
                        });
                    }
                }
            }
        });
    }
}
