use crate::contracts::{
    AutoEngageOutcome, Category, DiscordCacheResult, GameFilterMode, HostsCheck, InstallStage,
    IpsetMode, MaintenanceStatus, RunningMode, RuntimeStatus, Strategy, StrategyTestResult,
};

pub type ProgressCb = Box<dyn Fn(InstallStage, u64, Option<u64>) + Send + Sync>;

/// Called before each strategy is tested: `(index_1based, total, strategy_id)`.
pub type TestProgressCb = Box<dyn Fn(u32, u32, &str) + Send + Sync>;

/// Called as a download streams: `(bytes_so_far, total_bytes_if_known)`.
pub type DownloadProgressCb = Box<dyn Fn(u64, Option<u64>) + Send + Sync>;

#[async_trait::async_trait]
pub trait Installer: Send + Sync {
    async fn is_installed(&self) -> bool;
    async fn installed_version(&self) -> Option<String>;
    async fn latest_version(&self) -> anyhow::Result<String>;
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()>;
}

/// Self-update for the zapret-ui binary itself (distinct from [`Installer`],
/// which manages the zapret *core*). The concrete adapter resolves the latest
/// published release from GitHub, downloads the new `zapret-ui.exe`, verifies
/// it and swaps it in for the running binary. The orchestrator relaunches.
#[async_trait::async_trait]
pub trait SelfUpdater: Send + Sync {
    /// The version this running binary was built as (e.g. `"v0.1.0"`).
    fn current_version(&self) -> String;
    /// Resolve the latest published release tag (e.g. `"v0.2.0"`).
    async fn latest_version(&self) -> anyhow::Result<String>;
    /// Download the latest `zapret-ui.exe`, verify its checksum, and atomically
    /// replace the running binary on disk. Does **not** relaunch or exit — the
    /// caller spawns the freshly-written exe and terminates this process.
    async fn download_and_apply(&self, on_progress: DownloadProgressCb) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn detect_running(&self) -> RuntimeStatus;
}

#[async_trait::async_trait]
pub trait ServiceCtl: Send + Sync {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()>;
    async fn remove(&self) -> anyhow::Result<()>;
    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn status(&self) -> anyhow::Result<RunningMode>;
    /// True if the service is registered with the SCM, regardless of run state.
    async fn is_installed(&self) -> bool;
}

pub trait StrategyCatalog: Send + Sync {
    fn all(&self) -> Vec<Strategy>;
    fn by_id(&self, id: &str) -> Option<Strategy>;
    fn by_category(&self, c: Category) -> Vec<Strategy>;
}

#[async_trait::async_trait]
pub trait StrategyTester: Send + Sync {
    /// Run each strategy in turn, probe the target endpoints, and return the
    /// per-strategy results ranked best-first. `on_progress` fires before each
    /// strategy is started so the UI can show which one is being tested.
    /// Returns an empty vec if cancelled before any strategy completed.
    async fn test_all(
        &self,
        strategies: Vec<Strategy>,
        on_each: TestResultCb,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<Vec<StrategyTestResult>>;

    /// Simple-mode auto-engage: try `candidates` in order, starting each and
    /// probing the target endpoints, and **leave the first one that restores
    /// connectivity running**. `on_progress` fires before each candidate is
    /// tried. Honours the same cancel flag as [`Self::cancel`]. On success the
    /// winning strategy's winws is still running when this returns.
    async fn auto_engage(
        &self,
        candidates: Vec<Strategy>,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<AutoEngageOutcome>;

    /// Probe the *currently running* strategy (no start/stop) after a short
    /// settle wait, returning whether it still restores connectivity. Used by
    /// simple-mode fast-engage to verify the remembered strategy in the
    /// background while the user already sees the bypass as on.
    async fn verify(&self) -> anyhow::Result<bool>;

    /// Request cancellation of an in-flight `test_all`.
    fn cancel(&self);
}

/// Called as soon as a single strategy's result is ready (for incremental UI).
pub type TestResultCb = Box<dyn Fn(StrategyTestResult) + Send + Sync>;

/// The in-app port of the `service.bat` SETTINGS / UPDATES menu items: the game
/// filter, the ipset filter, and the ipset-list / hosts-file updaters. All
/// operations act on files inside the install dir (no elevation required).
#[async_trait::async_trait]
pub trait Maintenance: Send + Sync {
    /// Read the current game-filter + ipset state from the install dir.
    async fn status(&self) -> MaintenanceStatus;
    /// Persist the game filter mode (writes/removes `utils\game_filter.enabled`).
    async fn set_game_filter(&self, mode: GameFilterMode) -> anyhow::Result<()>;
    /// Switch `ipset-all.txt` to the target any/none/loaded state (with backup).
    async fn set_ipset_mode(&self, mode: IpsetMode) -> anyhow::Result<()>;
    /// Download the latest ipset list into `lists\ipset-all.txt`. Returns the
    /// number of entries loaded (the caller builds the localized message).
    async fn update_ipset_list(&self) -> anyhow::Result<usize>;
    /// Download the repo hosts file and compare it to the system hosts file.
    /// Returns the comparison plus the downloaded content for in-app review.
    async fn update_hosts_file(&self) -> anyhow::Result<HostsCheck>;
    /// Close Discord (if running) and delete its `Cache`/`Code Cache`/`GPUCache`
    /// folders under `%appdata%\discord`. Returns what was closed/cleared.
    async fn clear_discord_cache(&self) -> anyhow::Result<DiscordCacheResult>;
}
