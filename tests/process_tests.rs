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

    // Capture logs from broadcast channel
    let mut logs = Vec::new();
    let start_time = std::time::Instant::now();
    while start_time.elapsed() < std::time::Duration::from_secs(3) {
        if let Ok(event) = event_rx.recv().await {
            if let UiEvent::LogLine(line) = event {
                logs.push(line);
                if logs.len() >= 2 {
                    break;
                }
            }
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
    } else {
        // If the test runs with elevation, we can test the full lifecycle
        let _ = service_ctl.remove().await; // Clean up old test run if any
        service_ctl.install(&strategy).await.expect("Failed to install service");

        // Start service
        service_ctl.start().await.expect("Failed to start service");
        
        // Check status
        let mode = service_ctl.status().await.expect("Failed to query status");
        assert_eq!(mode, RunningMode::WindowsService);

        // Stop service
        service_ctl.stop().await.expect("Failed to stop service");

        // Remove service
        service_ctl.remove().await.expect("Failed to remove service");
    }
}
