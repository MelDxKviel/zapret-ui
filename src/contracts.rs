#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Strategy {
    pub id: String,                  // stable builtin id, e.g. "general-v2"
    pub display_name: String,        // human-friendly name (English placeholder; i18n in UI layer)
    pub category: Category,
    pub description: String,
    pub winws_args: Vec<String>,     // ready-to-run argv for winws2.exe (paths already resolved)
    pub requires_lists: Vec<String>, // files (relative to install root) the strategy reads at runtime
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Category { Discord, Youtube, Mixed, Mgts, Rostelecom, Mts, Beeline, Other }

impl Category {
    /// Lowercased label exchanged with the UI — used by the Slint side as the
    /// suffix of the `strategies.cat.<slug>` i18n key and as the colour class
    /// of the per-strategy chip. Stable: serialised into `StrategyItem.category`,
    /// so renaming a variant here means updating ru.json/en.json + tokens.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Youtube => "youtube",
            Self::Mixed => "mixed",
            Self::Mgts => "mgts",
            Self::Rostelecom => "rostelecom",
            Self::Mts => "mts",
            Self::Beeline => "beeline",
            Self::Other => "other",
        }
    }
}

/// Split a strategy id like `general (ALT2)` into its pretty name (`general`)
/// and ALT tag (`ALT2`). Returns an empty tag when there are no parentheses.
/// Kept for backwards compatibility with the Slint binding — zapret2 ids
/// (`general-v2`, etc.) have no parentheses, so this returns `(id, "")` for
/// every current strategy.
pub fn split_alt(id: &str) -> (String, String) {
    if let (Some(open), Some(close)) = (id.find('('), id.rfind(')')) {
        if close > open {
            let alt = id[open + 1..close].trim().to_string();
            let pretty = id[..open].trim().to_string();
            return (pretty, alt);
        }
    }
    (id.trim().to_string(), String::new())
}

/// One hostlist file we manage in `<install>/files/`. The DPI tuning page
/// renders one row per entry, with a "Last updated N days ago" hint driving
/// the user to refresh stale lists.
#[derive(Clone, Debug, Default)]
pub struct HostlistInfo {
    /// File basename inside `<install>/files/`, e.g. `"list-youtube.txt"`.
    pub name: String,
    /// Age in whole days of the file's mtime; `None` when the file is absent.
    pub age_days: Option<u32>,
    /// Non-empty line count, for display.
    pub line_count: u32,
}

/// Snapshot of DPI tuning state pushed to the Settings page after every
/// `RefreshStatus` and after every tuning-affecting command. Much smaller
/// than the pre-zapret-2 `MaintenanceStatus`: zapret2 on Windows has no
/// game-filter / ipset / hosts-file knobs to surface (those were Flowseal
/// `.bat`-port concepts), so all that remains is hostlist housekeeping.
#[derive(Clone, Debug, Default)]
pub struct DpiTuningState {
    pub hostlists: Vec<HostlistInfo>,
}

/// Result of the "Clear Discord cache" action (close Discord if running,
/// then delete its `Cache`, `Code Cache` and `GPUCache` folders under
/// `%appdata%\discord`). Unchanged from the pre-zapret-2 contract — this
/// piece of housekeeping is Windows-side and zapret-version-agnostic.
#[derive(Clone, Debug, Default)]
pub struct DiscordCacheResult {
    /// Discord.exe was running and had to be closed first.
    pub discord_was_running: bool,
    /// Number of cache folders actually deleted.
    pub cleared: u32,
}

#[derive(Clone, Debug)]
pub enum BackendCmd {
    Install,
    CheckUpdate,
    Update,
    /// Check GitHub for a newer release of zapret-ui itself.
    CheckSelfUpdate,
    /// Download the latest zapret-ui.exe, swap it in and relaunch.
    SelfUpdate,
    Start(String /* strategy_id */),
    Stop,
    ServiceInstall(String /* strategy_id */),
    ServiceRemove,
    ServiceStart,
    ServiceStop,
    RefreshStatus,
    OpenInstallFolder,
    /// Run a connectivity test across every available strategy, scoring each
    /// and picking the best (like upstream `test zapret.ps1`).
    TestStrategies,
    /// Request cancellation of a running strategy test.
    CancelTest,
    /// Download fresh copies of every hostlist we know how to update into
    /// `<install>/files/`. Replaces the pre-zapret-2 `UpdateIpsetList` and
    /// `UpdateHostsFile` commands (zapret2's filtration model doesn't need
    /// either).
    UpdateHostlists,
    /// Close Discord (if running) and clear its Cache/Code Cache/GPUCache folders.
    ClearDiscordCache,
    /// Persist the user's favorite-strategy ids (toggled with the star on the
    /// Strategies / Tester pages).
    SetFavorites(Vec<String>),
    /// Persist whether bypass start/stop toasts are shown.
    SetNotifications(bool),
    /// Persist + apply "launch at Windows logon" (writes the HKCU Run key).
    SetAutostart(bool),
    /// Persist "check for zapret updates on startup".
    SetAutoupdateCheck(bool),
    /// Persist "minimize to tray on window close".
    SetMinimizeToTray(bool),
    /// The window was just hidden to the tray. Shows the one-time "still running
    /// in the tray" toast (only the first time) and persists that it was shown.
    MinimizedToTray,
    /// Persist "auto-start the last strategy when the app launches".
    SetAutoengage(bool),
    /// Persist the UI theme ("dark" | "light" | "system").
    SetTheme(String),
}

/// Outcome of testing a single strategy against the target endpoints.
#[derive(Clone, Debug, Default)]
pub struct StrategyTestResult {
    pub id: String,
    pub display_name: String,
    /// Number of endpoints that became reachable with this strategy.
    pub ok: u32,
    /// Total number of endpoints checked.
    pub total: u32,
    /// Average latency (ms) over the reachable endpoints; 0 when none passed.
    pub avg_latency_ms: u32,
    /// 1-based rank after sorting (1 = best). 0 until ranking is computed.
    pub rank: u32,
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(RuntimeStatus),
    DownloadProgress { bytes: u64, total: Option<u64> },
    InstallProgress(InstallStage),
    LogLine(String),
    UpdateAvailable { current: String, latest: String, url: String },
    /// The latest upstream version, reported on every update check regardless of
    /// whether it's newer than what's installed — drives the "latest" stat.
    LatestVersion(String),
    Error(String),
    /// A strategy test run has begun; `total` strategies will be tested.
    TestStarted { total: u32 },
    /// Progress before testing strategy `index` (1-based) of `total`.
    TestProgress { index: u32, total: u32, strategy: String },
    /// One strategy finished testing; its result is ready to display (in test
    /// order, not yet ranked).
    TestResult(StrategyTestResult),
    /// The whole test run finished. `results` is the final ranked list (best
    /// first) and `best` is the auto-selected strategy id (empty when the run
    /// was cancelled or no strategy passed any check).
    TestComplete { best: String, results: Vec<StrategyTestResult> },
    /// Current state of the DPI tuning surface (hostlist ages, etc.).
    /// Replaces the pre-zapret-2 `Maintenance(MaintenanceStatus)` event.
    Tuning(DpiTuningState),
    /// Outcome of a one-shot tuning action, for inline UI feedback.
    /// `kind` is `"hostlists"` or `"discord_cache"`.
    TuningResult { kind: String, ok: bool, message: String },
    /// A newer release of zapret-ui itself is available.
    AppUpdateAvailable { current: String, latest: String },
    /// zapret-ui is already on the latest release (`latest` echoed for display).
    AppUpToDate { latest: String },
    /// Streaming progress while the new zapret-ui.exe downloads.
    AppUpdateProgress { bytes: u64, total: Option<u64> },
    /// A self-update check or download failed.
    AppUpdateError(String),
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub installed_version: Option<String>,
    pub running_mode: RunningMode,
    pub active_strategy: Option<String>,
    pub winws_pid: Option<u32>,
    /// Whether a Windows service is registered with the SCM (running or stopped).
    pub service_installed: bool,
    /// How long the bypass (winws2) process has been running, in seconds. `None`
    /// when not running. Sourced from the OS, so it survives app restarts and
    /// page navigation instead of resetting.
    pub uptime_secs: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunningMode { #[default] None, UserProcess, WindowsService }

#[derive(Clone, Debug)]
pub enum InstallStage { Resolving, Downloading, Extracting, Verifying, Done }
