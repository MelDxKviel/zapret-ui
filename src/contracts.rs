#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Strategy {
    pub id: String,                  // ".bat" filename without extension, e.g. "general (ALT2)"
    pub display_name: String,        // human-friendly name
    pub category: Category,
    pub description: String,
    pub winws_args: Vec<String>,     // ready-to-run argv for winws.exe (paths already resolved)
    pub requires_lists: Vec<String>, // hostlist files the strategy references
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Category { Discord, Youtube, Mixed, Mgts, Rostelecom, Mts, Beeline, Other }

/// Split a strategy id like `general (ALT2)` into its pretty name (`general`)
/// and ALT tag (`ALT2`). Returns an empty tag when there are no parentheses.
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

/// Game filter mode — controls the `%GameFilter%`/`%GameFilterTCP%`/`%GameFilterUDP%`
/// port-range substitution in the `.bat` presets, mirroring `service.bat`'s
/// `utils\game_filter.enabled` flag. When disabled the ports collapse to `12`
/// (a no-op marker); when enabled the chosen protocol(s) cover `1024-65535`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GameFilterMode {
    #[default]
    Disabled,
    All,
    Tcp,
    Udp,
}

impl GameFilterMode {
    /// The full port range applied when the filter is on for a protocol.
    const RANGE: &'static str = "1024-65535";
    /// The "off" marker value `service.bat` uses (an unused high port).
    const OFF: &'static str = "12";

    pub fn tcp_value(&self) -> &'static str {
        match self {
            Self::All | Self::Tcp => Self::RANGE,
            _ => Self::OFF,
        }
    }
    pub fn udp_value(&self) -> &'static str {
        match self {
            Self::All | Self::Udp => Self::RANGE,
            _ => Self::OFF,
        }
    }
    /// `%GameFilter%` is the range whenever the filter is enabled in any mode.
    pub fn generic_value(&self) -> &'static str {
        match self {
            Self::Disabled => Self::OFF,
            _ => Self::RANGE,
        }
    }
    /// Slug stored in the flag file and exchanged with the UI.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Disabled => "off",
            Self::All => "all",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
    pub fn from_slug(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "all" => Self::All,
            "tcp" => Self::Tcp,
            "udp" => Self::Udp,
            _ => Self::Disabled,
        }
    }
}

/// IPSet filter mode — mirrors the any/none/loaded states `service.bat` toggles on
/// `lists\ipset-all.txt`. `Unknown` means the file is absent (nothing installed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IpsetMode {
    #[default]
    Unknown,
    /// Empty list → winws matches any IP.
    Any,
    /// Single placeholder entry → winws matches nothing.
    None,
    /// A real downloaded IP list is in effect.
    Loaded,
}

impl IpsetMode {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Any => "any",
            Self::None => "none",
            Self::Loaded => "loaded",
        }
    }
    pub fn from_slug(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "any" => Self::Any,
            "none" => Self::None,
            "loaded" => Self::Loaded,
            _ => Self::Unknown,
        }
    }
}

/// Snapshot of the zapret filter/maintenance toggles, pushed to the Settings page.
#[derive(Clone, Debug, Default)]
pub struct MaintenanceStatus {
    pub game_filter: GameFilterMode,
    pub ipset_mode: IpsetMode,
    /// Non-empty line count of `ipset-all.txt`, for display.
    pub ipset_lines: u32,
    /// Age in whole days of `ipset-all.txt` (from its mtime); `None` when the
    /// file is absent. Drives the "list is getting stale" reminder in the UI.
    pub ipset_age_days: Option<u32>,
}

/// Result of the "Update Hosts File" check.
#[derive(Clone, Debug, Default)]
pub struct HostsCheck {
    /// Whether the system hosts file already contains the repo's entries.
    pub up_to_date: bool,
    /// The repository hosts file content (for the in-app review window).
    pub content: String,
    /// Absolute path to the system hosts file.
    pub hosts_path: String,
    /// Folder containing the system hosts file (for "open folder").
    pub hosts_dir: String,
}

#[derive(Clone, Debug)]
pub enum BackendCmd {
    Install,
    CheckUpdate,
    Update,
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
    /// Persist the game-filter mode (writes `utils\game_filter.enabled`).
    SetGameFilter(GameFilterMode),
    /// Switch `ipset-all.txt` between any/none/loaded.
    SetIpsetMode(IpsetMode),
    /// Download the latest ipset list into `lists\ipset-all.txt`.
    UpdateIpsetList,
    /// Compare the system hosts file to the repo hosts and open it for merge if stale.
    UpdateHostsFile,
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
    /// Current state of the zapret filter toggles (game filter + ipset).
    Maintenance(MaintenanceStatus),
    /// Outcome of a one-shot maintenance action, for inline UI feedback.
    /// `kind` is `"ipset"` or `"hosts"`.
    MaintenanceResult { kind: String, ok: bool, message: String },
    /// The repo hosts file is out of date — open the review window with its content.
    HostsContent { content: String, hosts_path: String, hosts_dir: String },
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
    /// How long the bypass (winws) process has been running, in seconds. `None`
    /// when not running. Sourced from the OS, so it survives app restarts and
    /// page navigation instead of resetting.
    pub uptime_secs: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunningMode { #[default] None, UserProcess, WindowsService }

#[derive(Clone, Debug)]
pub enum InstallStage { Resolving, Downloading, Extracting, Verifying, Done }
