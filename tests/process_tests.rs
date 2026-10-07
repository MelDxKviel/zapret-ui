#![cfg(windows)]

use zapret_ui::contracts::{Category, EngineKind, RunningMode, Strategy};
use zapret_ui::ports::{Runner, ServiceCtl};
use zapret_ui::zapret::elevation::is_elevated;
use zapret_ui::zapret::process::ProcessRunner;
use zapret_ui::zapret::service::WindowsServiceCtl;

/// `MakeWriter` that forwards every formatted tracing line into a channel, so
/// the test can assert on what `ProcessRunner` logs (winws stdout/stderr go
/// through `tracing` with target "winws", not through a UiEvent).
#[derive(Clone)]
struct ChanWriter(tokio::sync::mpsc::UnboundedSender<String>);

impl std::io::Write for ChanWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let line = String::from_utf8_lossy(buf).trim_end().to_string();
        if !line.is_empty() {
            let _ = self.0.send(line);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ChanWriter {
    type Writer = ChanWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

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
    std::thread::sleep(std::time::Duration::from_secs(5));
}
"#,
    )
    .unwrap();

    let stub_exe = temp_dir.path().join("winws.exe");
    let status = std::process::Command::new("rustc")
        .arg(&stub_src)
        .arg("-o")
        .arg(&stub_exe)
        .status()
        .expect("Failed to run rustc to compile stub");
    assert!(status.success(), "Stub compilation failed");

    // Capture tracing output (winws stdout/stderr is logged there).
    let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let _ = tracing_subscriber::fmt()
        .with_writer(ChanWriter(log_tx))
        .with_ansi(false)
        .try_init();

    // Use a unique service name to avoid colliding with running service on developer's machine
    let runner = ProcessRunner::new(temp_dir.path().to_path_buf())
        .with_service_name("zapret-test-service".to_string())
        .with_tcp_preflight(false)
        .with_interceptor_guard(false);

    // Test detect_running before start
    let initial_status = runner.detect_running().await;
    assert!(initial_status.installed);
    assert_eq!(
        initial_status.installed_version,
        Some("1.2.3-test".to_string())
    );
    assert_eq!(initial_status.running_mode, RunningMode::None);
    assert_eq!(initial_status.winws_pid, None);

    // Mock strategy
    let strategy = Strategy {
        engine: EngineKind::Zapret1,
        id: "test_strategy".to_string(),
        display_name: "Test Strategy".to_string(),
        category: Category::Other,
        description: "Testing".to_string(),
        winws_args: vec!["--arg1".to_string(), "--arg2".to_string()],
        requires_lists: vec![],
    };

    // Start runner
    let pid = runner
        .start(&strategy)
        .await
        .expect("Failed to start process");
    assert!(pid > 0);

    // Test detect_running after start
    let running_status = runner.detect_running().await;
    assert!(running_status.installed);
    assert_eq!(running_status.running_mode, RunningMode::UserProcess);
    assert_eq!(running_status.winws_pid, Some(pid));

    // Capture the stub's output from the tracing channel. Each `recv` is
    // bounded by a timeout so a missing line can never hang the test.
    let stdout_line =
        |l: &String| l.contains("winws") && l.contains("Hello from winws stub stdout!");
    let stderr_line =
        |l: &String| l.contains("winws") && l.contains("Hello from winws stub stderr!");
    let mut logs: Vec<String> = Vec::new();
    while !(logs.iter().any(stdout_line) && logs.iter().any(stderr_line)) {
        match tokio::time::timeout(std::time::Duration::from_secs(3), log_rx.recv()).await {
            Ok(Some(line)) => logs.push(line),
            // Channel closed or timed out — stop waiting.
            _ => break,
        }
    }

    assert!(
        logs.iter().any(stdout_line),
        "stdout line missing: {logs:?}"
    );
    assert!(
        logs.iter().any(stderr_line),
        "stderr line missing: {logs:?}"
    );

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
"#,
    )
    .unwrap();

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
        engine: EngineKind::Zapret1,
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
        service_ctl
            .install(&strategy)
            .await
            .expect("Failed to install service");
        service_ctl.start().await.expect("Failed to start service");
        let mode = service_ctl.status().await.expect("Failed to query status");
        assert_eq!(mode, RunningMode::SystemService);
        service_ctl.stop().await.expect("Failed to stop service");
        service_ctl
            .remove()
            .await
            .expect("Failed to remove service");
    } else {
        eprintln!("elevated: skipping SCM lifecycle (set ZAPRET_UI_RUN_SERVICE_TESTS to run)");
    }
}

#[tokio::test]
async fn zapret2_requires_its_own_executable_and_rejects_zapret1_arguments() {
    let temp = tempfile::tempdir().unwrap();
    let runner = ProcessRunner::new_for_engine(temp.path().to_path_buf(), EngineKind::Zapret2)
        .with_service_name("zapret2-test-missing-executable".into())
        .with_tcp_preflight(false)
        .with_interceptor_guard(false);
    let mut strategy = Strategy {
        engine: EngineKind::Zapret2,
        id: "zapret2:test".into(),
        display_name: "test".into(),
        category: Category::Other,
        description: String::new(),
        winws_args: vec![],
        requires_lists: vec![],
    };
    std::fs::write(temp.path().join("winws.exe"), b"not zapret2").unwrap();
    let err = runner.start(&strategy).await.unwrap_err();
    assert!(err.to_string().contains("winws2.exe not found"));
    assert!(!runner.detect_running().await.installed);
    strategy.engine = EngineKind::Zapret1;
    assert!(runner
        .start(&strategy)
        .await
        .unwrap_err()
        .to_string()
        .contains("different engine"));
    assert_eq!(runner.detect_running().await.active_engine, None);
}

#[tokio::test]
async fn zapret2_process_uses_bin_directory_arguments_and_stops_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let source = temp.path().join("stub.rs");
    std::fs::write(
        &source,
        r#"
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    std::fs::write("started.txt", args.join("\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(30));
}

"#,
    )
    .unwrap();
    let compiled = std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(bin.join("winws2.exe"))
        .status()
        .unwrap();
    assert!(compiled.success());
    let runner = ProcessRunner::new_for_engine(temp.path().to_path_buf(), EngineKind::Zapret2)
        .with_service_name("zapret2-test-process-lifecycle".into())
        .with_tcp_preflight(false)
        .with_interceptor_guard(false);
    let strategy = Strategy {
        engine: EngineKind::Zapret2,
        id: "zapret2:lua-test".into(),
        display_name: "test".into(),
        category: Category::Other,
        description: String::new(),
        winws_args: vec![
            "--lua-desync=fake:repeats=2".into(),
            "argument with spaces".into(),
        ],
        requires_lists: vec![],
    };
    let pid = runner.start(&strategy).await.unwrap();
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(bin.join("started.txt")) {
                break text;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let running = runner.detect_running().await;
    runner.stop().await.unwrap();
    assert_eq!(observed.unwrap(), strategy.winws_args.join("\n"));
    assert_eq!(running.winws_pid, Some(pid));
    assert_eq!(running.active_engine, Some(EngineKind::Zapret2));
    assert_eq!(running.active_strategy, Some(strategy.id));
    assert_eq!(
        runner.detect_running().await.running_mode,
        RunningMode::None
    );
}

#[tokio::test]
async fn start_refuses_an_unmanaged_interceptor_without_terminating_it() {
    let external = tempfile::tempdir().unwrap();
    let source = external.path().join("stub.rs");
    std::fs::write(
        &source,
        "fn main() { std::thread::sleep(std::time::Duration::from_secs(30)); }",
    )
    .unwrap();
    let exe = external.path().join("winws2.exe");
    assert!(std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(&exe)
        .status()
        .unwrap()
        .success());
    let mut external_cmd = tokio::process::Command::new(&exe);
    external_cmd
        .creation_flags(0x0800_0000 | 0x0000_0200)
        .kill_on_drop(true);
    let mut external_child = external_cmd.spawn().unwrap();

    let owned = tempfile::tempdir().unwrap();
    std::fs::copy(&exe, owned.path().join("winws.exe")).unwrap();
    let runner = ProcessRunner::new(owned.path().to_path_buf())
        .with_service_name("zapret-test-unmanaged-interceptor".into())
        .with_tcp_preflight(false);
    let strategy = Strategy {
        engine: EngineKind::Zapret1,
        id: "test".into(),
        display_name: "test".into(),
        category: Category::Other,
        description: String::new(),
        winws_args: vec![],
        requires_lists: vec![],
    };
    let result = runner.start(&strategy).await;
    let remained_alive = external_child.try_wait().unwrap().is_none();
    external_child.kill().await.unwrap();
    assert!(result.unwrap_err().to_string().contains("still running"));
    assert!(
        remained_alive,
        "Unmanaged processes must never be terminated by engine switching"
    );
}

#[tokio::test]
async fn zapret2_start_reports_immediate_exit_instead_of_a_running_pid() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("bin")).unwrap();
    let source = temp.path().join("stub.rs");
    std::fs::write(
        &source,
        "fn main() { eprintln!(\"invalid Lua arguments\"); std::process::exit(23); }",
    )
    .unwrap();
    assert!(std::process::Command::new("rustc")
        .arg(&source)
        .arg("-o")
        .arg(temp.path().join("bin/winws2.exe"))
        .status()
        .unwrap()
        .success());
    let runner = ProcessRunner::new_for_engine(temp.path().into(), EngineKind::Zapret2)
        .with_service_name("zapret2-test-early-exit".into())
        .with_tcp_preflight(false)
        .with_interceptor_guard(false);
    let strategy = Strategy {
        engine: EngineKind::Zapret2,
        id: "zapret2:invalid".into(),
        display_name: "invalid".into(),
        category: Category::Other,
        description: String::new(),
        winws_args: vec![],
        requires_lists: vec![],
    };
    let error = runner.start(&strategy).await.unwrap_err().to_string();
    assert!(
        error.contains("winws2.exe exited during startup"),
        "{error}"
    );
    assert!(error.contains("23"), "{error}");
    let status = runner.detect_running().await;
    assert_eq!(status.running_mode, RunningMode::None);
    assert_eq!(status.active_engine, None);
    assert_eq!(status.winws_pid, None);
}
