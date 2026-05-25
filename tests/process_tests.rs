#[path = "../src/contracts.rs"]
pub mod contracts;

#[path = "../src/ports.rs"]
pub mod ports;

#[path = "../src/zapret/mod.rs"]
pub mod zapret;

use tokio::sync::broadcast;
use crate::contracts::{Strategy, Category, RunningMode, UiEvent};
use crate::ports::{Runner, ServiceCtl};
use crate::zapret::process::ProcessRunner;
use crate::zapret::service::WindowsServiceCtl;
use crate::zapret::elevation::is_elevated;

#[tokio::test]
async fn test_process_runner_lifecycle() {
    let temp_dir = tempfile::tempdir().unwrap();
    
    // Create version.txt
    std::fs::write(temp_dir.path().join("version.txt"), "1.2.3-test\n").unwrap();

    // Compile dummy winws.exe
    let stub_src = temp_dir.path().join("stub.rs");
    std::fs::write(
        &stub_src,
        r#"
fn main() {
    println!("Hello from winws stub stdout!");
    eprintln!("Hello from winws stub stderr!");
    std::thread::sleep(std::time::Duration::from_millis(500));
}
"#
    ).unwrap();

    let stub_exe = temp_dir.path().join("winws.exe");
    let status = std::process::Command::new("rustc")
        .arg(&stub_src)
        .arg("-o")
        .arg(&stub_exe)
        .status()
        .expect("Failed to run rustc to compile stub");
    assert!(status.success(), "Stub compilation failed");

    // Initialize broadcast channel for UI events
    let (event_tx, mut event_rx) = broadcast::channel(16);

    // Use a unique service name to avoid colliding with running service on developer's machine
    let runner = ProcessRunner::new(temp_dir.path().to_path_buf(), event_tx)
        .with_service_name("zapret-test-service".to_string());

    // Test detect_running before start
    let initial_status = runner.detect_running().await;
    assert!(initial_status.installed);
    assert_eq!(initial_status.installed_version, Some("1.2.3-test".to_string()));
    assert_eq!(initial_status.running_mode, RunningMode::None);
    assert_eq!(initial_status.winws_pid, None);

    // Mock strategy
    let strategy = Strategy {
        id: "test_strategy".to_string(),
        display_name: "Test Strategy".to_string(),
        category: Category::Other,
        description: "Testing".to_string(),
        winws_args: vec!["--arg1".to_string(), "--arg2".to_string()],
        requires_lists: vec![],
    };

    // Start runner
    let pid = runner.start(&strategy).await.expect("Failed to start process");
    assert!(pid > 0);

    // Test detect_running after start
    let running_status = runner.detect_running().await;
    assert!(running_status.installed);
    assert_eq!(running_status.running_mode, RunningMode::UserProcess);
    assert_eq!(running_status.winws_pid, Some(pid));

    // Capture logs from the broadcast channel. Each `recv` is bounded by a
    // timeout so a missing event can never hang the test indefinitely.
    let mut logs = Vec::new();
    while logs.len() < 2 {
        match tokio::time::timeout(std::time::Duration::from_secs(3), event_rx.recv()).await {
            Ok(Ok(UiEvent::LogLine(line))) => logs.push(line),
            Ok(Ok(_)) => {}
            // Channel closed or timed out — stop waiting.
            Ok(Err(_)) | Err(_) => break,
        }
    }

    assert!(logs.contains(&"Hello from winws stub stdout!".to_string()));
    assert!(logs.contains(&"Hello from winws stub stderr!".to_string()));

    // Stop runner
    runner.stop().await.expect("Failed to stop process");

    // Test detect_running after stop
    let stopped_status = runner.detect_running().await;
    assert_eq!(stopped_status.running_mode, RunningMode::None);
    assert_eq!(stopped_status.winws_pid, None);
}

#[tokio::test]
async fn test_service_ctl_elevation_or_ops() {
    let temp_dir = tempfile::tempdir().unwrap();
    
    // Compile dummy winws.exe
    let stub_src = temp_dir.path().join("stub.rs");
    std::fs::write(
        &stub_src,
        r#"
fn main() {
    std::thread::sleep(std::time::Duration::from_secs(10));
}
"#
    ).unwrap();

    let stub_exe = temp_dir.path().join("winws.exe");
    let status = std::process::Command::new("rustc")
        .arg(&stub_src)
        .arg("-o")
        .arg(&stub_exe)
        .status()
        .expect("Failed to compile stub");
    assert!(status.success());

    // Use a unique service name to avoid colliding with running service on developer's machine
    let service_ctl = WindowsServiceCtl::new(temp_dir.path().to_path_buf())
        .with_service_name("zapret-test-service".to_string());

    let strategy = Strategy {
        id: "test_service_strategy".to_string(),
        display_name: "Test Service Strategy".to_string(),
        category: Category::Other,
        description: "Testing service".to_string(),
        winws_args: vec!["--service-test".to_string()],
        requires_lists: vec![],
    };

    if !is_elevated() {
        // When not elevated, service commands must fail with NeedsElevation error
        let res = service_ctl.install(&strategy).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "NeedsElevation");

        let res = service_ctl.remove().await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "NeedsElevation");

        let res = service_ctl.start().await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "NeedsElevation");

        let res = service_ctl.stop().await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "NeedsElevation");
    } else if std::env::var("ZAPRET_UI_RUN_SERVICE_TESTS").is_ok() {
        // Full SCM lifecycle. Gated behind an env var and NOT run automatically:
        // the stub above is a plain console program, not a real service-control
        // dispatcher, so `start` would normally time out / be flaky. Run this
        // manually (elevated, with a real service stub) when exercising the SCM.
        let _ = service_ctl.remove().await; // Clean up old test run if any
        service_ctl.install(&strategy).await.expect("Failed to install service");
        service_ctl.start().await.expect("Failed to start service");
        let mode = service_ctl.status().await.expect("Failed to query status");
        assert_eq!(mode, RunningMode::WindowsService);
        service_ctl.stop().await.expect("Failed to stop service");
        service_ctl.remove().await.expect("Failed to remove service");
    } else {
        eprintln!("elevated: skipping SCM lifecycle (set ZAPRET_UI_RUN_SERVICE_TESTS to run)");
    }
}
