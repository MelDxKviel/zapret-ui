// Build as a Windows GUI app in release so no console window pops up next to the
// UI. Debug builds keep the console attached so `tracing`/`eprintln!` logs are
// visible while developing (`cargo run`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

pub mod app;
pub mod config;
pub mod contracts;
pub mod i18n;
pub mod log;
pub mod notify;
pub mod ports;
pub mod selfupdate;
pub mod single_instance;
pub mod state;
pub mod tray;
pub mod winenv;
pub mod winicon;
pub mod zapret;

use crate::contracts::UiEvent;
use std::sync::Arc;
use tokio::sync::broadcast;

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
    explicit.unwrap_or_else(|| config::AppConfig::load().install_dir())
}

async fn run_elevated_task(
    task: &str,
    strategy_id: Option<String>,
    install_dir: std::path::PathBuf,
) -> anyhow::Result<()> {
    use ports::ServiceCtl;

    match task {
        "service-install" => {
            let strat_id = strategy_id
                .ok_or_else(|| anyhow::anyhow!("Strategy ID required for installation"))?;
            // Stage into the protected machine-wide directory, lock its ACLs and
            // run the service from THERE — never from the user-writable %APPDATA%
            // dir (privilege-escalation fix). Shared with the in-app elevated path.
            zapret::service::install_service_protected(&install_dir, &strat_id).await?;
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

fn lock_elevation_result_dir(dir: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let out = std::process::Command::new("icacls")
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "/grant:r",
            "*S-1-5-32-545:(OI)(CI)RX",
            "/T",
            "/C",
            "/Q",
        ])
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Failed to lock elevated result dir ACLs: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn valid_elevated_result_path(path: &std::path::Path, nonce: &str) -> bool {
    let expected_dir = zapret::paths::elevation_result_dir();
    let expected_file = format!("zapret-ui-elev-{nonce}.result");
    path.parent() == Some(expected_dir.as_path())
        && path.file_name().and_then(|n| n.to_str()) == Some(expected_file.as_str())
}

/// Write the one-shot task outcome to the nonce result file so the (unelevated)
/// parent can report success/failure instead of guessing from a status poll.
fn write_elevated_result(result_file: &std::path::Path, nonce: &str, outcome: &anyhow::Result<()>) {
    if !valid_elevated_result_path(result_file, nonce) {
        eprintln!(
            "Refusing to write elevated result to unexpected path: {}",
            result_file.display()
        );
        return;
    }
    let Some(parent) = result_file.parent() else {
        return;
    };
    if let Err(e) = lock_elevation_result_dir(parent) {
        eprintln!("Failed to prepare elevated result directory: {e:#}");
        return;
    }

    let body = match outcome {
        Ok(()) => format!("{nonce}\nOK"),
        // `{e:#}` walks the whole anyhow chain so the parent sees the real cause
        // (e.g. "StartService: Access is denied. (os error 5)"), not just the
        // outermost context line.
        Err(e) => format!("{nonce}\nERR\n{e:#}"),
    };
    let tmp_file = result_file.with_extension("tmp");
    let _ = std::fs::remove_file(result_file);
    let _ = std::fs::remove_file(&tmp_file);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_file)
    {
        Ok(mut file) => {
            use std::io::Write;
            if file.write_all(body.as_bytes()).is_ok() && file.flush().is_ok() {
                let _ = std::fs::rename(&tmp_file, result_file);
            }
        }
        Err(e) => eprintln!("Failed to create elevated result file: {e}"),
    }
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
            single_instance::focus_existing_window("zapret-ui");
            std::process::exit(0);
        }
    };

    // Clean up the binary left behind by a previous self-update (it can't delete
    // itself while still mapped, so the successor removes it on next launch).
    selfupdate::cleanup_old_binary();

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

    let install_dir = config.install_dir();

    // Instantiate ports
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let github_client = zapret::github::GithubClient::new(client.clone(), None);

    let installer = Arc::new(zapret::installer::ZapretInstaller::new(
        install_dir.clone(),
        github_client,
    ));
    let runner = Arc::new(zapret::process::ProcessRunner::new(
        install_dir.clone(),
        event_tx.clone(),
    ));
    let service_ctl = Arc::new(zapret::service::WindowsServiceCtl::new(install_dir.clone()));
    let catalog = Arc::new(zapret::catalog::LocalStrategyCatalog::new(
        install_dir.clone(),
    ));
    let tester = Arc::new(zapret::tester::ConnectivityTester::new(
        runner.clone(),
        install_dir.clone(),
    ));
    let maintenance = Arc::new(zapret::maintenance::ZapretMaintenance::new(
        install_dir.clone(),
        client.clone(),
    ));
    let self_updater = Arc::new(selfupdate::GithubSelfUpdater::from_repo_url(
        client.clone(),
        env!("CARGO_PKG_REPOSITORY"),
        env!("APP_VERSION"),
    ));

    // Instantiate and run App
    let mut app = app::App::new(
        installer,
        runner,
        service_ctl,
        catalog,
        tester,
        maintenance,
        self_updater,
        config,
        state,
        event_tx,
    );

    app.run(guard)?;

    Ok(())
}
