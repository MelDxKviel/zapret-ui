use crate::contracts::{Strategy, Category, RuntimeStatus, RunningMode, InstallStage, StrategyTestResult};

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
