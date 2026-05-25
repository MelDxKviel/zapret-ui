// Build as a Windows GUI app in release so no console window pops up next to the
// UI. Debug builds keep the console attached so `tracing`/`eprintln!` logs are
// visible while developing (`cargo run`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod contracts;
pub mod ports;
pub mod config;
pub mod i18n;
pub mod state;
pub mod log;
pub mod notify;
pub mod tray;
pub mod single_instance;
pub mod winicon;
pub mod winenv;
pub mod zapret;
pub mod app;

use std::sync::Arc;
use tokio::sync::broadcast;
use crate::contracts::UiEvent;

#[derive(Default)]
struct ElevatedArgs {
    task: Option<String>,
    strategy: Option<String>,
    install_dir: Option<std::path::PathBuf>,
    result_file: Option<std::path::PathBuf>,
    nonce: Option<String>,
}

fn parse_args() -> ElevatedArgs {
    let mut out = ElevatedArgs::default();
    for arg in std::env::args() {
        if let Some(v) = arg.strip_prefix("--elevated-task=") {
            out.task = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--strategy=") {
            out.strategy = Some(v.to_string());
        } else if let Some(v) = arg.strip_prefix("--install-dir=") {
            out.install_dir = Some(std::path::PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--result-file=") {
            out.result_file = Some(std::path::PathBuf::from(v));
        } else if let Some(v) = arg.strip_prefix("--nonce=") {
            out.nonce = Some(v.to_string());
        }
    }
    out
}

/// Resolve the install dir the elevated helper should act on: the one the parent
/// passed explicitly (so we don't accidentally use a *different* admin account's
/// `%APPDATA%`), falling back to config only if absent.
fn elevated_install_dir(explicit: Option<std::path::PathBuf>) -> std::path::PathBuf {
    explicit.unwrap_or_else(|| {
        let config = config::AppConfig::load();
        config.install_dir_override.clone().unwrap_or_else(|| {
            let base = directories::BaseDirs::new().unwrap();
            base.config_dir().join("zapret-ui").join("zapret")
        })
    })
}

async fn run_elevated_task(
    task: &str,
    strategy_id: Option<String>,
    install_dir: std::path::PathBuf,
) -> anyhow::Result<()> {
    use ports::{ServiceCtl, StrategyCatalog};

    match task {
        "service-install" => {
            let strat_id = strategy_id.ok_or_else(|| anyhow::anyhow!("Strategy ID required for installation"))?;
            // Stage the binaries into the protected machine-wide directory and
            // lock down its ACLs, then run the service from THERE — never from
            // the user-writable %APPDATA% dir (privilege-escalation fix).
            let protected = zapret::service::prepare_protected_dir(&install_dir)?;
            let catalog = zapret::catalog::LocalStrategyCatalog::new(protected.clone());
            let strategy = catalog.by_id(&strat_id).ok_or_else(|| anyhow::anyhow!("Strategy not found"))?;
            let service_ctl = zapret::service::WindowsServiceCtl::new(protected);
            service_ctl.install(&strategy).await?;
            service_ctl.start().await?;
        }
        "service-remove" => {
            let service_ctl = zapret::service::WindowsServiceCtl::new(install_dir);
            service_ctl.remove().await?;
        }
        "service-start" => {
            let service_ctl = zapret::service::WindowsServiceCtl::new(install_dir);
            service_ctl.start().await?;
        }
        "service-stop" => {
            let service_ctl = zapret::service::WindowsServiceCtl::new(install_dir);
            service_ctl.stop().await?;
        }
        _ => return Err(anyhow::anyhow!("Unknown elevated task: {}", task)),
    }
    Ok(())
}

/// Write the one-shot task outcome to the nonce result file so the (unelevated)
/// parent can report success/failure instead of guessing from a status poll.
fn write_elevated_result(result_file: &std::path::Path, nonce: &str, outcome: &anyhow::Result<()>) {
    let body = match outcome {
        Ok(()) => format!("{nonce}\nOK"),
        Err(e) => format!("{nonce}\nERR\n{e}"),
    };
    let _ = std::fs::write(result_file, body);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = parse_args();

    if let Some(task_name) = args.task {
        let install_dir = elevated_install_dir(args.install_dir);
        let outcome = run_elevated_task(&task_name, args.strategy, install_dir).await;
        if let (Some(rf), Some(nonce)) = (args.result_file.as_ref(), args.nonce.as_ref()) {
            write_elevated_result(rf, nonce, &outcome);
        }
        match outcome {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("Elevated task failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    // Single-instance check. When we relaunch ourselves elevated (--relaunch),
    // the unelevated instance is still shutting down, so retry briefly to let it
    // release the mutex instead of bouncing straight to "focus existing window".
    //
    // The name is scoped to the current user (domain\user) so it is not a fixed,
    // guessable string another process could pre-create to block our launch.
    let user_scope = format!(
        "{}-{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let mutex_name = format!("Local\\zapret-ui-single-instance-mutex-{user_scope}");
    let relaunched = std::env::args().any(|a| a == "--relaunch");
    let instance = if relaunched {
        let mut acquired = None;
        for _ in 0..30 {
            if let Ok(inst) = single_instance::SingleInstance::new(&mutex_name) {
                acquired = Some(inst);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        acquired
    } else {
        single_instance::SingleInstance::new(&mutex_name).ok()
    };
    let _instance = match instance {
        Some(inst) => inst,
        None => {
            single_instance::focus_existing_window("Zapret UI");
            std::process::exit(0);
        }
    };

    // Register our notification identity so bypass start/stop toasts render
    // under the app's name (and appear at all).
    notify::init();

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
    let github_client = zapret::github::GithubClient::new(client.clone(), None);

    let installer = Arc::new(zapret::installer::ZapretInstaller::new(install_dir.clone(), github_client));
    let runner = Arc::new(zapret::process::ProcessRunner::new(install_dir.clone(), event_tx.clone()));
    let service_ctl = Arc::new(zapret::service::WindowsServiceCtl::new(install_dir.clone()));
    let catalog = Arc::new(zapret::catalog::LocalStrategyCatalog::new(install_dir.clone()));
    let tester = Arc::new(zapret::tester::ConnectivityTester::new(runner.clone(), install_dir.clone()));
    let maintenance = Arc::new(zapret::maintenance::ZapretMaintenance::new(install_dir.clone(), client.clone()));

    // Instantiate and run App
    let mut app = app::App::new(
        installer,
        runner,
        service_ctl,
        catalog,
        tester,
        maintenance,
        config,
        state,
        event_tx,
    );

    app.run(guard)?;

    Ok(())
}
