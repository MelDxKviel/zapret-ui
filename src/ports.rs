use crate::contracts::{Strategy, Category, RuntimeStatus, RunningMode, InstallStage, StrategyTestResult, GameFilterMode, IpsetMode, MaintenanceStatus, HostsCheck, DiscordCacheResult};

pub type ProgressCb = Box<dyn Fn(InstallStage, u64, Option<u64>) + Send + Sync>;

/// Called before each strategy is tested: `(index_1based, total, strategy_id)`.
pub type TestProgressCb = Box<dyn Fn(u32, u32, &str) + Send + Sync>;

#[async_trait::async_trait]
pub trait Installer: Send + Sync {
    async fn is_installed(&self) -> bool;
    async fn installed_version(&self) -> Option<String>;
    async fn latest_version(&self) -> anyhow::Result<String>;
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()>;
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
