//! Offline orchestration regressions for selection and the cancellation boundary.
use super::*;
use crate::contracts::{Category, EngineKind, RuntimeStatus, Strategy};
use std::sync::atomic::AtomicUsize;
use tokio::sync::Notify;

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
async fn finish_successful_scan_closes_cancellation_window_without_stopping_winner() {
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
    // This is the same lock/check used by the command handler. A late cancel
    // cannot turn a completed successful scan into an unhandled cancellation.
    {
        let _completion = completion.lock().unwrap();
        if running.load(Ordering::SeqCst) {
            cancelled.store(true, Ordering::SeqCst);
        }
    }
    assert!(!cancelled.load(Ordering::SeqCst));
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
            matches!(received.try_recv().unwrap(), UiEvent::Error(error) if error.contains("startup failed"))
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
        assert!(matches!(received.try_recv().unwrap(), UiEvent::Error(_)));
        assert!(
            matches!(received.try_recv().unwrap(), UiEvent::Error(error) if error.contains("cleanup failed"))
        );
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
