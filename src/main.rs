pub mod contracts;
pub mod ports;
pub mod config;
pub mod state;
pub mod log;
pub mod tray;
pub mod single_instance;
pub mod self_update;
pub mod zapret;
pub mod app;

use std::sync::Arc;
use tokio::sync::broadcast;
use crate::contracts::UiEvent;

fn parse_args() -> (Option<String>, Option<String>) {
    let mut task = None;
    let mut strategy = None;
    for arg in std::env::args() {
        if arg.starts_with("--elevated-task=") {
            task = Some(arg.trim_start_matches("--elevated-task=").to_string());
        } else if arg.starts_with("--strategy=") {
            strategy = Some(arg.trim_start_matches("--strategy=").to_string());
        }
    }
    (task, strategy)
}

async fn run_elevated_task(task: &str, strategy_id: Option<String>) -> anyhow::Result<()> {
    let config = config::AppConfig::load();
    let install_dir = config.install_dir_override.clone().unwrap_or_else(|| {
        let base = directories::BaseDirs::new().unwrap();
        base.config_dir().join("zapret-ui").join("zapret")
    });

    let service_ctl = zapret::service::WindowsServiceCtl::new(install_dir.clone());

    match task {
        "service-install" => {
            let strat_id = strategy_id.ok_or_else(|| anyhow::anyhow!("Strategy ID required for installation"))?;
            let catalog = zapret::catalog::LocalStrategyCatalog::new(install_dir.clone());
            use ports::StrategyCatalog;
            let strategy = catalog.by_id(&strat_id).ok_or_else(|| anyhow::anyhow!("Strategy not found"))?;
            use ports::ServiceCtl;
            service_ctl.install(&strategy).await?;
            service_ctl.start().await?;
        }
        "service-remove" => {
            use ports::ServiceCtl;
            service_ctl.remove().await?;
        }
        "service-start" => {
            use ports::ServiceCtl;
            service_ctl.start().await?;
        }
        "service-stop" => {
            use ports::ServiceCtl;
            service_ctl.stop().await?;
        }
        _ => return Err(anyhow::anyhow!("Unknown elevated task: {}", task)),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (task, strategy) = parse_args();

    if let Some(task_name) = task {
        if let Err(e) = run_elevated_task(&task_name, strategy).await {
            eprintln!("Elevated task failed: {}", e);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // Single-instance check
    let _instance = match single_instance::SingleInstance::new("Local\\zapret-ui-single-instance-mutex") {
        Ok(inst) => inst,
        Err(_) => {
            single_instance::focus_existing_window("Zapret UI");
            std::process::exit(0);
        }
    };

    // Initialize event channel
    let (event_tx, _event_rx) = broadcast::channel::<UiEvent>(256);

    // Initialize logging (broadcast to event_tx)
    let (log_tx, mut log_rx) = broadcast::channel::<String>(256);
    let guard = log::init_logging(log_tx)?;

    // Forward log lines from log_rx to event_tx
    let event_tx_c = event_tx.clone();
    tokio::spawn(async move {
        while let Ok(line) = log_rx.recv().await {
            let _ = event_tx_c.send(UiEvent::LogLine(line));
        }
    });

    // Load config and state
    let config = config::AppConfig::load();
    let state = state::AppState::default();

    let install_dir = config.install_dir_override.clone().unwrap_or_else(|| {
        let base = directories::BaseDirs::new().unwrap();
        base.config_dir().join("zapret-ui").join("zapret")
    });

    // Instantiate ports
    let client = reqwest::Client::builder().build()?;
    let github_client = zapret::github::GithubClient::new(client, None);

    let installer = Arc::new(zapret::installer::ZapretInstaller::new(install_dir.clone(), github_client));
    let runner = Arc::new(zapret::process::ProcessRunner::new(install_dir.clone(), event_tx.clone()));
    let service_ctl = Arc::new(zapret::service::WindowsServiceCtl::new(install_dir.clone()));
    let catalog = Arc::new(zapret::catalog::LocalStrategyCatalog::new(install_dir.clone()));

    // Instantiate and run App
    let mut app = app::App::new(
        installer,
        runner,
        service_ctl,
        catalog,
        config,
        state,
        event_tx,
    );

    app.run(guard)?;

    Ok(())
}
