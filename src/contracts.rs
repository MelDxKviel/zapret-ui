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
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(RuntimeStatus),
    DownloadProgress { bytes: u64, total: Option<u64> },
    InstallProgress(InstallStage),
    LogLine(String),
    UpdateAvailable { current: String, latest: String, url: String },
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub installed_version: Option<String>,
    pub running_mode: RunningMode,
    pub active_strategy: Option<String>,
    pub winws_pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunningMode { #[default] None, UserProcess, WindowsService }

#[derive(Clone, Debug)]
pub enum InstallStage { Resolving, Downloading, Extracting, Verifying, Done }
