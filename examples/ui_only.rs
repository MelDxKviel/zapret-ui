#![allow(dead_code)]

use std::rc::Rc;

use zapret_ui::contracts::*;
use zapret_ui::ports::*;

slint::include_modules!();

// --- Mock implementations of all ports ---

struct MockInstaller;

#[async_trait::async_trait]
impl Installer for MockInstaller {
    async fn is_installed(&self) -> bool { true }
    async fn installed_version(&self) -> Option<String> { Some("v1.0.0-mock".to_string()) }
    async fn latest_version(&self) -> anyhow::Result<String> { Ok("v1.1.0-mock".to_string()) }
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()> {
        on_progress(InstallStage::Resolving, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        on_progress(InstallStage::Downloading, 50, Some(100));
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        on_progress(InstallStage::Downloading, 100, Some(100));
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        on_progress(InstallStage::Extracting, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        on_progress(InstallStage::Verifying, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        on_progress(InstallStage::Done, 1, Some(1));
        Ok(())
    }
}

struct MockRunner;

#[async_trait::async_trait]
impl Runner for MockRunner {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
        tracing::info!("Mock starting strategy: {}", strategy.display_name);
        Ok(12345)
    }
    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("Mock stopping process");
        Ok(())
    }
    async fn detect_running(&self) -> RuntimeStatus {
        RuntimeStatus {
            installed: true,
            installed_version: Some("v1.0.0-mock".to_string()),
            running_mode: RunningMode::None,
            active_strategy: None,
            winws_pid: None,
            service_installed: false,
            uptime_secs: None,
        }
    }
}

struct MockServiceCtl;

#[async_trait::async_trait]
impl ServiceCtl for MockServiceCtl {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
        tracing::info!("Mock installing service for strategy: {}", strategy.display_name);
        Ok(())
    }
    async fn remove(&self) -> anyhow::Result<()> {
        tracing::info!("Mock removing service");
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        tracing::info!("Mock starting service");
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("Mock stopping service");
        Ok(())
    }
    async fn status(&self) -> anyhow::Result<RunningMode> {
        Ok(RunningMode::None)
    }
    async fn is_installed(&self) -> bool {
        false
    }
}

struct MockCatalog;

impl MockCatalog {
    fn sample() -> Vec<Strategy> {
        ["general", "general (ALT)", "general (ALT2)"]
            .iter()
            .map(|name| Strategy {
                id: name.to_string(),
                display_name: name.to_string(),
                category: Category::Mixed,
                description: "Mock preset for UI-only preview".to_string(),
                winws_args: vec!["--wf-tcp=80,443".to_string()],
                requires_lists: vec![],
            })
            .collect()
    }
}

impl StrategyCatalog for MockCatalog {
    fn all(&self) -> Vec<Strategy> {
        Self::sample()
    }
    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.all().into_iter().find(|s| s.id == id)
    }
    fn by_category(&self, c: Category) -> Vec<Strategy> {
        self.all().into_iter().filter(|s| s.category == c).collect()
    }
}

fn main() -> anyhow::Result<()> {
    // Simple console logging for the example
    tracing_subscriber::fmt::init();

    let ui = MainWindow::new()?;

    // Populate strategies
    let catalog = MockCatalog;
    let slint_strategies: Vec<StrategyItem> = catalog
        .all()
        .iter()
        .map(|s| {
            let (pretty, alt) = zapret_ui::contracts::split_alt(&s.id);
            StrategyItem {
                id: s.id.as_str().into(),
                display_name: s.display_name.as_str().into(),
                category: format!("{:?}", s.category).into(),
                description: s.description.as_str().into(),
                pretty: pretty.into(),
                alt: alt.into(),
            }
        })
        .collect();
    ui.set_strategies(Rc::new(slint::VecModel::from(slint_strategies)).into());

    // Set initial status
    ui.set_status_installed(true);
    ui.set_status_installed_version("v1.0.0-mock".into());
    ui.set_status_running_mode("None".into());

    // Wire up some callbacks with simple logging
    ui.on_start_clicked(|strat_id| {
        println!("UI: Start clicked with strategy: {}", strat_id);
    });
    ui.on_stop_clicked(|| {
        println!("UI: Stop clicked");
    });
    ui.on_install_clicked(|| {
        println!("UI: Install clicked");
    });
    ui.on_update_clicked(|| {
        println!("UI: Update clicked");
    });
    ui.on_strategy_selected(|strat_id| {
        println!("UI: Strategy selected: {}", strat_id);
    });
    ui.on_service_install_clicked(|strat_id| {
        println!("UI: Install service for strategy: {}", strat_id);
    });
    ui.on_service_remove_clicked(|| {
        println!("UI: Service remove clicked");
    });
    ui.on_open_folder_clicked(|| {
        println!("UI: Open folder clicked");
    });
    ui.on_refresh_status_clicked(|| {
        println!("UI: Refresh status clicked");
    });

    // Mock log lines
    let mk = |no: i32, ts: &str, lvl: &str, msg: &str| LogLineItem {
        line_no: no,
        timestamp: ts.into(),
        level: lvl.into(),
        message: msg.into(),
    };
    let log_lines = vec![
        mk(1, "2026-05-23T16:14:34.808277Z", "INFO", "zapret-ui started in UI-only mode"),
        mk(2, "2026-05-23T16:14:34.812000Z", "INFO", "Mock installer ready, version v1.0.0-mock"),
        mk(3, "2026-05-23T16:14:34.815000Z", "INFO", "3 strategies loaded from catalog"),
    ];
    ui.set_log_lines(Rc::new(slint::VecModel::from(log_lines)).into());

    ui.run()?;
    Ok(())
}
