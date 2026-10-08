//! Offline orchestration regressions for selection and the cancellation boundary.
use super::*;
use crate::contracts::{Category, EngineKind, RuntimeStatus, Strategy};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

#[test]
fn scan_display_survives_status_errors_and_partial_results_until_explicit_completion() {
    let mut display = ScanPresentation::default();
    assert!(display.apply(&UiEvent::ScanStarted {
        kind: ScanKind::Auto,
        total: 4
    }));
    display.apply(&UiEvent::AutoEngageProgress {
        index: 2,
        total: 4,
        strategy: "zapret2:multisplit".into(),
    });
    let active = display.clone();
    for event in [
        UiEvent::Status(RuntimeStatus::default()),
        UiEvent::Error("unrelated update check failed".into()),
        UiEvent::TestComplete {
            best: "winner".into(),
            results: vec![],
        },
    ] {
        assert!(!display.apply(&event));
        assert_eq!(display, active);
    }
    assert!(display.apply(&UiEvent::ScanFinished));
    assert_eq!(display, ScanPresentation::default());
    // Delayed progress is not an implicit request to restart a completed scan.
    assert!(!display.apply(&UiEvent::AutoEngageProgress {
        index: 3,
        total: 4,
        strategy: "late".into(),
    }));
    assert_eq!(display.kind, None);
}

#[test]
fn duplicate_core_requests_are_coalesced_and_stop_cancels_either_scan() {
    for cmd in [
        BackendCmd::AutoEngage,
        BackendCmd::TestStrategies,
        BackendCmd::Start("general".into()),
        BackendCmd::InstallAndEngage,
        BackendCmd::RefreshStatus,
    ] {
        assert!(command_during_scan(cmd, true).is_none());
    }
    assert!(matches!(
        command_during_scan(BackendCmd::Stop, true),
        Some(BackendCmd::CancelScan)
    ));
    assert!(matches!(
        command_during_scan(BackendCmd::ServiceStop, true),
        Some(BackendCmd::CancelScan)
    ));
    assert!(matches!(
        command_during_scan(BackendCmd::CancelScan, true),
        Some(BackendCmd::CancelScan)
    ));
    assert!(matches!(
        command_during_scan(BackendCmd::Quit, true),
        Some(BackendCmd::Quit)
    ));
    assert!(matches!(
        command_during_scan(BackendCmd::Start("general".into()), false),
        Some(BackendCmd::Start(_))
    ));
}

#[test]
fn mode_changes_and_beta_disable_require_cancel_and_join_before_switching() {
    assert!(requires_scan_handoff(&BackendCmd::SetEngineMode(
        EngineMode::Zapret2
    )));
    assert!(requires_scan_handoff(&BackendCmd::SetEngineMode(
        EngineMode::Zapret1
    )));
    assert!(requires_scan_handoff(&BackendCmd::SetZapret2Enabled(false)));
    assert!(!requires_scan_handoff(&BackendCmd::SetZapret2Enabled(true)));
}

struct CancelOnlyTester(AtomicUsize);

#[async_trait::async_trait]
impl StrategyTester for CancelOnlyTester {
    async fn test_all(
        &self,
        _: Vec<Strategy>,
        _: crate::ports::TestResultCb,
        _: crate::ports::TestProgressCb,
    ) -> anyhow::Result<Vec<crate::contracts::StrategyTestResult>> {
        unreachable!()
    }
    async fn auto_engage(
        &self,
        _: Vec<Strategy>,
        _: crate::ports::TestProgressCb,
    ) -> anyhow::Result<AutoEngageOutcome> {
        unreachable!()
    }
    async fn verify(&self) -> anyhow::Result<bool> {
        unreachable!()
    }
    fn cancel(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn switching_mode_waits_for_the_actual_scan_task_to_finish_cleanup() {
    let observed = Arc::new(CancelOnlyTester(AtomicUsize::new(0)));
    let tester: Arc<dyn StrategyTester> = observed.clone();
    let auto_cancelled = AtomicBool::new(false);
    let test_cancelled = AtomicBool::new(false);
    let completion = std::sync::Mutex::new(());
    let cleanup_started = Arc::new(Notify::new());
    let release_cleanup = Arc::new(Notify::new());
    let cleanup_finished = Arc::new(AtomicBool::new(false));
    let mode_changed = AtomicBool::new(false);
    let started = cleanup_started.clone();
    let release = release_cleanup.clone();
    let finished = cleanup_finished.clone();
    let mut task = Some(tokio::spawn(async move {
        started.notify_one();
        release.notified().await;
        finished.store(true, Ordering::SeqCst);
    }));
    cleanup_started.notified().await;
    let mut switch = Box::pin(async {
        cancel_scan_before_handoff(
            &mut task,
            &tester,
            &auto_cancelled,
            &test_cancelled,
            &completion,
        )
        .await;
        // This continuation is the same boundary used before changing adapters
        // or persisted engine preferences in the command handler.
        assert!(cleanup_finished.load(Ordering::SeqCst));
        mode_changed.store(true, Ordering::SeqCst);
    });
    assert!(futures_util::poll!(&mut switch).is_pending());
    assert_eq!(observed.0.load(Ordering::SeqCst), 1);
    assert!(auto_cancelled.load(Ordering::SeqCst));
    assert!(test_cancelled.load(Ordering::SeqCst));
    assert!(!mode_changed.load(Ordering::SeqCst));
    release_cleanup.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(2), switch)
        .await
        .unwrap();
    assert!(mode_changed.load(Ordering::SeqCst));
    assert!(task.is_none());
}

#[test]
fn beta_opt_in_gates_engine_selection_and_failed_persistence_preserves_preferences() {
    let mut config = AppConfig {
        engine_mode: EngineMode::Auto,
        last_strategy: Some("zapret2:remembered".into()),
        ..Default::default()
    };
    assert_eq!(
        proposed_engine_preferences(&config, &BackendCmd::SetEngineMode(EngineMode::Zapret2)),
        None
    );
    assert_eq!(
        proposed_engine_preferences(&config, &BackendCmd::SetZapret2Enabled(true)),
        Some((EngineMode::Zapret1, true))
    );
    let original = config.clone();
    assert!(
        save_engine_preferences(&mut config, EngineMode::Zapret1, true, |_| anyhow::bail!(
            "disk full"
        ))
        .is_err()
    );
    assert_eq!(config, original);
    save_engine_preferences(&mut config, EngineMode::Zapret1, true, |_| Ok(())).unwrap();
    assert_eq!(config.last_strategy, original.last_strategy);
    assert_eq!(
        proposed_engine_preferences(&config, &BackendCmd::SetEngineMode(EngineMode::Zapret2)),
        Some((EngineMode::Zapret2, true))
    );
    config.engine_mode = EngineMode::Zapret2;
    assert_eq!(
        proposed_engine_preferences(&config, &BackendCmd::SetZapret2Enabled(false)),
        Some((EngineMode::Zapret1, false))
    );
}

#[derive(Default)]
struct StopBarrier {
    release: Notify,
    stops: AtomicUsize,
    fail: bool,
}

#[async_trait::async_trait]
impl Runner for StopBarrier {
    async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
        panic!("finishing a scan must never start an engine");
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail, "engine did not stop");
        self.release.notified().await;
        Ok(())
    }

    async fn detect_running(&self) -> RuntimeStatus {
        RuntimeStatus::default()
    }
}

#[tokio::test]
async fn finish_cancelled_scan_stops_engine_before_allowing_another_start() {
    let barrier = Arc::new(StopBarrier::default());
    let runner: Arc<dyn Runner> = barrier.clone();
    let running = AtomicBool::new(true);
    let cancelled = AtomicBool::new(true);
    let completion = std::sync::Mutex::new(());
    let mut finish = Box::pin(finish_scan(&runner, &running, &cancelled, &completion));
    assert!(futures_util::poll!(&mut finish).is_pending());
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 1);
    assert!(running.load(Ordering::SeqCst));
    barrier.release.notify_one();
    assert!(finish.await.unwrap());
    assert!(!running.load(Ordering::SeqCst));
}

#[tokio::test]
async fn finish_successful_scan_keeps_winner_until_explicit_cancellation() {
    let barrier = Arc::new(StopBarrier::default());
    let runner: Arc<dyn Runner> = barrier.clone();
    let running = AtomicBool::new(true);
    let cancelled = AtomicBool::new(false);
    let completion = std::sync::Mutex::new(());
    assert!(!finish_scan(&runner, &running, &cancelled, &completion)
        .await
        .unwrap());
    assert!(!running.load(Ordering::SeqCst));
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn late_cancel_joins_postprocessing_then_stops_the_committed_winner() {
    let barrier = Arc::new(StopBarrier::default());
    let runner: Arc<dyn Runner> = barrier.clone();
    let tester: Arc<dyn StrategyTester> = Arc::new(CancelOnlyTester(AtomicUsize::new(0)));
    let running = AtomicBool::new(true);
    let auto_cancelled = AtomicBool::new(false);
    let test_cancelled = AtomicBool::new(false);
    let completion = std::sync::Mutex::new(());
    assert!(
        !finish_scan(&runner, &running, &auto_cancelled, &completion)
            .await
            .unwrap()
    );
    assert!(!running.load(Ordering::SeqCst));

    let release = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let release_task = release.clone();
    let finished_task = finished.clone();
    let mut task = Some(tokio::spawn(async move {
        release_task.notified().await;
        finished_task.notify_one();
    }));
    let mut cancel = Box::pin(cancel_scan_and_stop(
        &mut task,
        &tester,
        &runner,
        &auto_cancelled,
        &test_cancelled,
        &completion,
    ));
    assert!(futures_util::poll!(&mut cancel).is_pending());
    assert!(auto_cancelled.load(Ordering::SeqCst));
    assert!(test_cancelled.load(Ordering::SeqCst));
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 0);
    release.notify_one();
    finished.notified().await;
    assert!(futures_util::poll!(&mut cancel).is_pending());
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 1);
    barrier.release.notify_one();
    assert!(cancel.await.unwrap());
    assert!(task.is_none());
}

async fn completed_scan_task() -> Option<tokio::task::JoinHandle<()>> {
    let finished = Arc::new(Notify::new());
    let finished_task = finished.clone();
    let task = tokio::spawn(async move { finished_task.notify_one() });
    finished.notified().await;
    assert!(task.is_finished());
    Some(task)
}

#[tokio::test]
async fn queued_cancel_still_stops_a_finished_scan_before_any_new_lifecycle_command() {
    let barrier = Arc::new(StopBarrier::default());
    barrier.release.notify_one();
    let runner: Arc<dyn Runner> = barrier.clone();
    let tester: Arc<dyn StrategyTester> = Arc::new(CancelOnlyTester(AtomicUsize::new(0)));
    let mut task = completed_scan_task().await;
    retire_finished_scan(&mut task, &BackendCmd::RefreshStatus);
    assert!(cancel_scan_and_stop(
        &mut task,
        &tester,
        &runner,
        &AtomicBool::new(false),
        &AtomicBool::new(false),
        &std::sync::Mutex::new(()),
    )
    .await
    .unwrap());
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn old_queued_cancel_cannot_stop_a_new_manual_start_or_engine_mode() {
    let barrier = Arc::new(StopBarrier::default());
    let runner: Arc<dyn Runner> = barrier.clone();
    let tester: Arc<dyn StrategyTester> = Arc::new(CancelOnlyTester(AtomicUsize::new(0)));
    for command in [
        BackendCmd::Start("manual".into()),
        BackendCmd::SetEngineMode(EngineMode::Zapret2),
        BackendCmd::SetZapret2Enabled(false),
    ] {
        let mut task = completed_scan_task().await;
        retire_finished_scan(&mut task, &command);
        assert!(!cancel_scan_and_stop(
            &mut task,
            &tester,
            &runner,
            &AtomicBool::new(false),
            &AtomicBool::new(false),
            &std::sync::Mutex::new(()),
        )
        .await
        .unwrap());
    }
    assert_eq!(barrier.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failed_cancel_cleanup_is_reported_and_releases_scan_busy_state() {
    let runner: Arc<dyn Runner> = Arc::new(StopBarrier {
        fail: true,
        ..Default::default()
    });
    let running = AtomicBool::new(true);
    let error = finish_scan(
        &runner,
        &running,
        &AtomicBool::new(true),
        &std::sync::Mutex::new(()),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("did not stop"));
    assert!(!running.load(Ordering::SeqCst));
}

fn strategy(id: &str, engine: EngineKind) -> Strategy {
    Strategy {
        engine,
        id: id.into(),
        display_name: id.into(),
        category: Category::Mixed,
        description: String::new(),
        winws_args: Vec::new(),
        requires_lists: Vec::new(),
    }
}

struct Catalog(Vec<Strategy>);

impl StrategyCatalog for Catalog {
    fn all(&self) -> Vec<Strategy> {
        self.0.clone()
    }

    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.0.iter().find(|strategy| strategy.id == id).cloned()
    }

    fn by_category(&self, category: Category) -> Vec<Strategy> {
        self.0
            .iter()
            .filter(|strategy| strategy.category == category)
            .cloned()
            .collect()
    }
}

#[test]
fn candidate_order_keeps_last_good_and_favorites_without_duplicates() {
    let catalog: Arc<dyn StrategyCatalog> = Arc::new(Catalog(vec![
        strategy("ordinary", EngineKind::Zapret1),
        strategy("zapret2:favorite", EngineKind::Zapret2),
        strategy("zapret2:remembered", EngineKind::Zapret2),
    ]));
    let candidates = ordered_candidates(
        &catalog,
        Some("zapret2:remembered"),
        &[
            "removed".into(),
            "zapret2:favorite".into(),
            "zapret2:remembered".into(),
            "zapret2:favorite".into(),
        ],
    );
    assert_eq!(
        candidates
            .iter()
            .map(|strategy| strategy.id.as_str())
            .collect::<Vec<_>>(),
        ["zapret2:remembered", "zapret2:favorite", "ordinary"]
    );
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::contracts::StrategyTestResult;
    use std::sync::Mutex;

    #[derive(Default)]
    struct StartState {
        active: Option<String>,
        operations: Vec<String>,
        fail_stop: bool,
    }

    #[derive(Default)]
    struct FallbackRunner(Mutex<StartState>);

    #[async_trait::async_trait]
    impl Runner for FallbackRunner {
        async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
            let mut state = self.0.lock().unwrap();
            assert!(state.active.is_none(), "failed candidate was not stopped");
            state.operations.push(format!("start:{}", strategy.id));
            state.active = Some(strategy.id.clone());
            anyhow::ensure!(strategy.id != "broken", "candidate startup failed");
            Ok(1)
        }

        async fn stop(&self) -> anyhow::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.operations.push("stop".into());
            anyhow::ensure!(!state.fail_stop, "candidate cleanup failed");
            state.active = None;
            Ok(())
        }

        async fn detect_running(&self) -> RuntimeStatus {
            let state = self.0.lock().unwrap();
            RuntimeStatus {
                active_strategy: state.active.clone(),
                running_mode: if state.active.is_some() {
                    RunningMode::UserProcess
                } else {
                    RunningMode::None
                },
                ..Default::default()
            }
        }
    }

    fn result(id: &str) -> StrategyTestResult {
        StrategyTestResult {
            id: id.into(),
            ok: 2,
            total: 3,
            improved: 2,
            ..Default::default()
        }
    }

    fn catalog() -> Arc<dyn StrategyCatalog> {
        Arc::new(Catalog(vec![
            strategy("broken", EngineKind::Zapret1),
            strategy("zapret2:healthy", EngineKind::Zapret2),
        ]))
    }

    fn reported_errors(received: &mut broadcast::Receiver<UiEvent>) -> Vec<String> {
        std::iter::from_fn(|| received.try_recv().ok())
            .filter_map(|event| match event {
                UiEvent::Error(error) => Some(error),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn winner_restart_failure_is_cleaned_up_before_next_ranked_candidate() {
        let observed = Arc::new(FallbackRunner::default());
        let runner: Arc<dyn Runner> = observed.clone();
        let (events, mut received) = broadcast::channel(16);
        let selected = start_test_winner(
            &runner,
            &catalog(),
            &[result("broken"), result("zapret2:healthy")],
            &AtomicBool::new(false),
            &events,
        )
        .await;
        assert_eq!(selected, "zapret2:healthy");
        let state = observed.0.lock().unwrap();
        assert_eq!(state.active.as_deref(), Some("zapret2:healthy"));
        assert_eq!(
            state.operations,
            ["start:broken", "stop", "start:zapret2:healthy"]
        );
        assert!(
            reported_errors(&mut received).is_empty(),
            "a failed candidate is not a failed scan"
        );
    }

    #[tokio::test]
    async fn cancelled_scan_does_not_restart_any_candidate() {
        let observed = Arc::new(FallbackRunner::default());
        let runner: Arc<dyn Runner> = observed.clone();
        let (events, _) = broadcast::channel(16);
        let selected = start_test_winner(
            &runner,
            &catalog(),
            &[result("zapret2:healthy")],
            &AtomicBool::new(true),
            &events,
        )
        .await;
        assert!(selected.is_empty());
        assert!(observed.0.lock().unwrap().operations.is_empty());
    }

    #[tokio::test]
    async fn failed_cleanup_blocks_starting_another_engine() {
        let observed = Arc::new(FallbackRunner::default());
        observed.0.lock().unwrap().fail_stop = true;
        let runner: Arc<dyn Runner> = observed.clone();
        let (events, mut received) = broadcast::channel(16);
        let selected = start_test_winner(
            &runner,
            &catalog(),
            &[result("broken"), result("zapret2:healthy")],
            &AtomicBool::new(false),
            &events,
        )
        .await;
        assert!(selected.is_empty());
        assert_eq!(
            observed.0.lock().unwrap().operations,
            ["start:broken", "stop"]
        );
        let errors = reported_errors(&mut received);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("cleanup failed"));
    }

    #[tokio::test]
    async fn only_exhausting_all_restart_candidates_reports_failure() {
        let runner: Arc<dyn Runner> = Arc::new(FallbackRunner::default());
        let (events, mut received) = broadcast::channel(16);
        let selected = start_test_winner(
            &runner,
            &catalog(),
            &[result("broken")],
            &AtomicBool::new(false),
            &events,
        )
        .await;
        assert!(selected.is_empty());
        let errors = reported_errors(&mut received);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("startup failed"));
    }

    #[tokio::test]
    async fn missing_or_ineffective_results_are_never_started() {
        let observed = Arc::new(FallbackRunner::default());
        let runner: Arc<dyn Runner> = observed.clone();
        let (events, _) = broadcast::channel(16);
        let mut ineffective = result("broken");
        ineffective.baseline_ok = 2;
        ineffective.improved = 0;
        let selected = start_test_winner(
            &runner,
            &catalog(),
            &[result("removed"), ineffective, result("zapret2:healthy")],
            &AtomicBool::new(false),
            &events,
        )
        .await;
        assert_eq!(selected, "zapret2:healthy");
        assert_eq!(
            observed.0.lock().unwrap().operations,
            ["start:zapret2:healthy"]
        );
    }
}
