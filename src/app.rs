use std::sync::Arc;
use std::rc::Rc;
use std::cell::RefCell;
use tokio::sync::{mpsc, broadcast, RwLock};
use crate::contracts::{BackendCmd, UiEvent, RunningMode, InstallStage, GameFilterMode, IpsetMode, split_alt};
use crate::ports::{Installer, Runner, ServiceCtl, StrategyCatalog, StrategyTester, Maintenance};
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
    // Favorite strategy ids (mirrors AppConfig::favorites). Lives on the UI thread
    // so the model rebuilders (search, toggle, test results) can read it cheaply.
    static FAVORITES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

const LOG_BUF_CAP: usize = 4000;

/// Decode the bundled `icon.ico` into a Slint `Image` for use as the window
/// (title bar + taskbar) icon. The `.ico` is also embedded as a Win32 resource
/// via `build.rs` (Explorer icon), but winit needs the icon set at runtime to
/// show it on the window and in the taskbar.
fn app_window_icon() -> Option<slint::Image> {
    use image::ImageReader;
    use std::io::Cursor;

    const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");
    let img = ImageReader::with_format(Cursor::new(ICON_BYTES), image::ImageFormat::Ico)
        .decode()
        .ok()?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&img, w, h);
    Some(slint::Image::from_rgba8(buf))
}

/// Whether `id` is currently a favorite (reads the UI-thread mirror).
fn is_favorite(id: &str) -> bool {
    FAVORITES.with(|f| f.borrow().iter().any(|x| x == id))
}

/// Map a catalog strategy to its Slint row, tagging its current favorite state.
fn to_item(s: &crate::contracts::Strategy) -> StrategyItem {
    let (pretty, alt) = split_alt(&s.id);
    StrategyItem {
        id: s.id.as_str().into(),
        display_name: s.display_name.as_str().into(),
        category: format!("{:?}", s.category).into(),
        description: s.description.as_str().into(),
        pretty: pretty.into(),
        alt: alt.into(),
        favorite: is_favorite(&s.id),
    }
}

/// Rebuild the Slint `strategies` model from the catalog, applying the current
/// search query and floating favorites to the top (keeping catalog order within
/// each group). Runs on the UI thread.
fn rebuild_strategies(ui: &MainWindow, catalog: &Arc<dyn StrategyCatalog>) {
    let q = ui.get_strategies_query().to_string().trim().to_lowercase();
    let mut list: Vec<crate::contracts::Strategy> = catalog
        .all()
        .into_iter()
        .filter(|s| {
            q.is_empty()
                || format!("{} {} {}", s.id, s.display_name, s.description)
                    .to_lowercase()
                    .contains(&q)
        })
        .collect();
    // Stable sort: favorites first, original (catalog) order preserved otherwise.
    list.sort_by_key(|s| if is_favorite(&s.id) { 0 } else { 1 });
    let items: Vec<StrategyItem> = list.iter().map(to_item).collect();
    ui.set_strategies(Rc::new(slint::VecModel::from(items)).into());
}

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
                favorite: is_favorite(&r.id),
            }
        })
        .collect();
    ui.set_test_results(Rc::new(slint::VecModel::from(items)).into());
}

/// Open a path with the OS default handler (folder in Explorer, URL in browser).
/// Uses `ShellExecuteW` directly rather than `cmd /C start`, so shell
/// metacharacters in the target can't be interpreted (command-injection fix).
fn open_external(target: &str) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    let file_w: Vec<u16> = OsStr::new(target).encode_wide().chain(Some(0)).collect();
    unsafe {
        // null lpOperation => default verb ("open"), which handles URLs, files
        // and folders without going through a command interpreter.
        ShellExecuteW(
            ptr::null_mut(),
            ptr::null(),
            file_w.as_ptr(),
            ptr::null(),
            ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
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

/// Quote a single argument for a Windows command line so that the receiving
/// process's `CommandLineToArgvW`/`std::env::args` reproduces it verbatim — even
/// when it contains spaces or parentheses (real preset ids look like
/// `general (ALT2)`). Implements the documented MSVC argv quoting rules.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
            }
            '"' => {
                // Escape all pending backslashes (they precede a quote) + the quote.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                backslashes = 0;
                out.push('"');
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // Trailing backslashes precede the closing quote — double them.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Handle to a launched elevated one-shot task: the nonce-named result file the
/// helper writes its outcome into, plus the nonce used to authenticate it.
pub struct ElevationHandle {
    pub result_file: std::path::PathBuf,
    pub nonce: String,
}

/// Launch this exe elevated to run a one-shot service task. Arguments (including
/// the strategy id and an explicit install dir) are passed with correct quoting
/// so spaces/parentheses survive, and an explicit install dir is handed over so
/// the helper acts on the *same* directory regardless of which admin account UAC
/// elevates to. Returns a handle whose result file the caller can await.
pub fn relaunch_elevated(
    task: &str,
    strategy: Option<&str>,
    install_dir: &std::path::Path,
) -> anyhow::Result<ElevationHandle> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    let current_exe = std::env::current_exe()?;
    let exe_path_w: Vec<u16> = current_exe.as_os_str().encode_wide().chain(Some(0)).collect();

    // Unique nonce so the parent can authenticate the result file the helper
    // writes (and so concurrent tasks don't collide).
    let nonce = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{}", std::process::id(), nanos)
    };
    let result_file = std::env::temp_dir().join(format!("zapret-ui-elev-{nonce}.result"));

    let mut args = vec![format!("--elevated-task={task}")];
    if let Some(strat) = strategy {
        args.push(format!("--strategy={strat}"));
    }
    args.push(format!("--install-dir={}", install_dir.display()));
    args.push(format!("--result-file={}", result_file.display()));
    args.push(format!("--nonce={nonce}"));
    let params = args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ");
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

    Ok(ElevationHandle { result_file, nonce })
}

/// Await the result file written by an elevated one-shot task. Returns `Ok(())`
/// on success, or the helper's error message. Times out (the user may have
/// dismissed UAC, or the helper hung) after ~90s.
pub async fn wait_for_elevated_result(handle: ElevationHandle) -> Result<(), String> {
    use tokio::time::{sleep, Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(90);
    let outcome = loop {
        if let Ok(content) = std::fs::read_to_string(&handle.result_file) {
            let mut lines = content.lines();
            let got_nonce = lines.next().unwrap_or("");
            if got_nonce == handle.nonce {
                let status = lines.next().unwrap_or("");
                if status == "OK" {
                    break Ok(());
                } else {
                    let msg: String = lines.collect::<Vec<_>>().join("\n");
                    break Err(if msg.is_empty() { status.to_string() } else { msg });
                }
            }
        }
        if Instant::now() >= deadline {
            break Err("Elevated operation did not report a result (timed out or was cancelled).".to_string());
        }
        sleep(Duration::from_millis(250)).await;
    };
    let _ = std::fs::remove_file(&handle.result_file);
    outcome
}

/// Relaunch *this whole app* elevated (the normal UI, not a one-shot task) via
/// the `runas` verb. The new instance carries `--relaunch` so it retries the
/// single-instance mutex while this (unelevated) instance exits. Used by the
/// "run as administrator" banner.
pub fn relaunch_self_elevated() -> anyhow::Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    let current_exe = std::env::current_exe()?;
    let exe_path_w: Vec<u16> = current_exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let params_w: Vec<u16> = OsStr::new("--relaunch").encode_wide().chain(Some(0)).collect();
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

/// Fire a bypass start/stop toast when the user has notifications enabled.
/// Reads the live config so a toggle takes effect immediately; the toast itself
/// runs on a blocking thread so it never stalls the backend loop.
async fn notify_bypass(config: &Arc<RwLock<AppConfig>>, started: bool, strategy: Option<&str>) {
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
    let cfg = config.read().await;
    cfg.install_dir_override.clone().unwrap_or_else(|| {
        let base = directories::BaseDirs::new().unwrap();
        base.config_dir().join("zapret-ui").join("zapret")
    })
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
) {
    let install_dir = current_install_dir(config).await;
    match relaunch_elevated(task, strategy, &install_dir) {
        Ok(handle) => {
            if let Err(msg) = wait_for_elevated_result(handle).await {
                let _ = event_tx.send(UiEvent::Error(msg));
            }
        }
        Err(e) => {
            let _ = event_tx.send(UiEvent::Error(format!("Elevation failed: {}", e)));
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
        ui.global::<I18n>().on_t(|lang, key| crate::i18n::tr(lang.as_str(), key.as_str()).into());
        let initial_lang = self
            .config
            .try_read()
            .map(|c| c.language)
            .unwrap_or_default();
        ui.global::<I18n>().set_lang(crate::i18n::code(initial_lang).into());
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

        // App version (from Cargo) — replaces the hardcoded UI string.
        ui.set_app_version(concat!("v", env!("CARGO_PKG_VERSION")).into());
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
            let install_dir = c.install_dir_override.clone().unwrap_or_else(|| {
                directories::BaseDirs::new()
                    .map(|b| b.config_dir().join("zapret-ui").join("zapret"))
                    .unwrap_or_default()
            });
            ui.set_install_dir(install_dir.display().to_string().into());
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
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_install_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::Install);
            });
        }
        {
            let cmd_tx_c = self.cmd_tx.clone();
            ui.on_update_clicked(move || {
                dispatch(&cmd_tx_c, BackendCmd::Update);
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
                let _ = cmd_tx_c.try_send(BackendCmd::SetGameFilter(GameFilterMode::from_slug(&slug)));
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
        ui.on_restart_as_admin(move || {
            match relaunch_self_elevated() {
                Ok(_) => std::process::exit(0),
                Err(e) => tracing::error!("Failed to relaunch as administrator: {}", e),
            }
        });

        // Copy arbitrary text to the system clipboard (used by the hosts window).
        ui.on_copy_to_clipboard(move |text| {
            if let Err(e) = clipboard_win::set_clipboard_string(&text) {
                tracing::warn!("Failed to copy to clipboard: {}", e);
            }
        });

        // Handle Tray minimizing on close
        let tray = SystemTray::new()?;
        let window = ui.window();
        let ui_weak = ui.as_weak();
        window.on_close_requested(move || {
            // Honour the "minimize to tray on close" setting: when on, hide to the
            // tray; when off, actually exit the app (the tray menu's Quit still
            // works either way).
            let to_tray = ui_weak.upgrade().map(|ui| ui.get_minimize_to_tray()).unwrap_or(false);
            if to_tray {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.hide();
                }
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                std::process::exit(0);
            }
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
                                    if tot > 0 { bytes as f32 / tot as f32 } else { 0.0 }
                                } else {
                                    0.0
                                };
                                ui.set_progress(pct);
                            }
                            UiEvent::InstallProgress(stage) => {
                                ui.set_is_busy(!matches!(stage, InstallStage::Done));
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
                                ui.set_test_current_alt("".into());
                                rebuild_test_results(&ui);
                            }
                            UiEvent::TestProgress { index, total, strategy } => {
                                ui.set_test_running(true);
                                ui.set_test_current(index as i32);
                                ui.set_test_total(total as i32);
                                // Variant label for the sidebar pill (ALT/SIMPLE FAKE),
                                // falling back to the base name when there's no variant.
                                let (pretty, alt) = split_alt(&strategy);
                                ui.set_test_current_alt(if alt.is_empty() { pretty } else { alt }.into());
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
                            UiEvent::Maintenance(m) => {
                                ui.set_game_filter(m.game_filter.slug().into());
                                ui.set_ipset_mode(m.ipset_mode.slug().into());
                                ui.set_ipset_lines(m.ipset_lines as i32);
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
                                    _ => {}
                                }
                            }
                            UiEvent::HostsContent { content, hosts_path, hosts_dir } => {
                                ui.set_hosts_content(content.into());
                                ui.set_hosts_path(hosts_path.into());
                                ui.set_hosts_dir(hosts_dir.into());
                                ui.set_hosts_modal_open(true);
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
        }
        // Auto-start the last-used strategy on launch when enabled.
        if autoengage {
            if let Some(id) = last_strategy {
                let _ = self.cmd_tx.try_send(BackendCmd::Start(id));
            }
        }

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
        let maintenance = self.maintenance.clone();
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
                                    notify_bypass(&config, true, Some(&strategy_id)).await;
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
                                notify_bypass(&config, false, None).await;
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
                                    } else {
                                        notify_bypass(&config, true, Some(&strategy_id)).await;
                                    }
                                    let mut status = runner.detect_running().await;
                                    status.service_installed = service_ctl.is_installed().await;
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                }
                                Err(e) => {
                                    if e.to_string().contains("NeedsElevation") {
                                        elevate_service_task("service-install", Some(&strategy_id), &config, &event_tx).await;
                                        let mut status = runner.detect_running().await;
                                        status.service_installed = service_ctl.is_installed().await;
                                        state.set_status(status.clone()).await;
                                        let _ = event_tx.send(UiEvent::Status(status));
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
                                notify_bypass(&config, false, None).await;
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    elevate_service_task("service-remove", None, &config, &event_tx).await;
                                    let mut status = runner.detect_running().await;
                                    status.service_installed = service_ctl.is_installed().await;
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStart => {
                        match service_ctl.start().await {
                            Ok(_) => {
                                notify_bypass(&config, true, None).await;
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    elevate_service_task("service-start", None, &config, &event_tx).await;
                                    let mut status = runner.detect_running().await;
                                    status.service_installed = service_ctl.is_installed().await;
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
                                } else {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                }
                            }
                        }
                    }
                    BackendCmd::ServiceStop => {
                        match service_ctl.stop().await {
                            Ok(_) => {
                                notify_bypass(&config, false, None).await;
                                let mut status = runner.detect_running().await;
                                status.service_installed = service_ctl.is_installed().await;
                                state.set_status(status.clone()).await;
                                let _ = event_tx.send(UiEvent::Status(status));
                            }
                            Err(e) => {
                                if e.to_string().contains("NeedsElevation") {
                                    elevate_service_task("service-stop", None, &config, &event_tx).await;
                                    let mut status = runner.detect_running().await;
                                    status.service_installed = service_ctl.is_installed().await;
                                    state.set_status(status.clone()).await;
                                    let _ = event_tx.send(UiEvent::Status(status));
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
                        // Keep the Settings filter toggles in sync with disk.
                        let _ = event_tx.send(UiEvent::Maintenance(maintenance.status().await));
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
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
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
                    BackendCmd::TestStrategies => {
                        let strategies = catalog.all();
                        if strategies.is_empty() {
                            let _ = event_tx.send(UiEvent::Error("No strategies installed to test".to_string()));
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
                                    let _ = event_tx_c.send(UiEvent::TestComplete { best, results });
                                }
                                Err(e) => {
                                    let _ = event_tx_c.send(UiEvent::Error(format!("Strategy test failed: {}", e)));
                                    let _ = event_tx_c.send(UiEvent::TestComplete { best: String::new(), results: Vec::new() });
                                }
                            }

                            // Settle the status display after the test churns winws.
                            let mut status = runner_c.detect_running().await;
                            status.service_installed = service_ctl_c.is_installed().await;
                            state_c.set_status(status.clone()).await;
                            let _ = event_tx_c.send(UiEvent::Status(status));
                        });
                    }
                }
            }
        });
    }
}
