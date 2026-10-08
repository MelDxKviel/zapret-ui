//! Deterministic dual-engine tests: no WinDivert, privilege or external network.
use super::*;
use crate::contracts::{Category, EngineKind, RunningMode, RuntimeStatus};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

#[derive(Default)]
struct MockState {
    active: Option<Strategy>,
    events: Vec<String>,
    starts: BTreeMap<String, usize>,
    fail_starts: BTreeSet<(String, usize)>,
    stops: usize,
    fail_stop: Option<usize>,
    exit_on_start: bool,
}

#[derive(Default)]
struct MockRunner(Mutex<MockState>);

#[async_trait::async_trait]
impl Runner for MockRunner {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
        let mut state = self.0.lock().unwrap();
        assert!(state.active.is_none(), "two engines must never overlap");
        state.events.push(format!("start:{}", strategy.id));
        let count = state.starts.entry(strategy.id.clone()).or_default();
        *count += 1;
        let count = *count;
        if !state.exit_on_start {
            state.active = Some(strategy.clone());
        }
        // Model failures after spawning, when the caller still owes cleanup.
        if state.fail_starts.contains(&(strategy.id.clone(), count)) {
            anyhow::bail!("mock start failed for {}", strategy.id);
        }
        Ok(1)
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let mut state = self.0.lock().unwrap();
        state.events.push("stop".into());
        state.stops += 1;
        if state.fail_stop == Some(state.stops) {
            anyhow::bail!("mock stop failed");
        }
        state.active = None;
        Ok(())
    }

    async fn detect_running(&self) -> RuntimeStatus {
        let state = self.0.lock().unwrap();
        RuntimeStatus {
            running_mode: if state.active.is_some() {
                RunningMode::UserProcess
            } else {
                RunningMode::None
            },
            active_strategy: state.active.as_ref().map(|strategy| strategy.id.clone()),
            active_engine: state.active.as_ref().map(|strategy| strategy.engine),
            ..Default::default()
        }
    }
}

fn candidate(id: &str, engine: EngineKind) -> Strategy {
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

type Samples<'a> = &'a [(&'a str, &'a [Option<u32>])];

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut bytes = [0; 1024];
        let read = socket.read(&mut bytes).await.unwrap();
        assert!(read > 0 && request.len() + read <= 4096);
        request.extend_from_slice(&bytes[..read]);
    }
    String::from_utf8(request).unwrap()
}

fn fixture(samples: Samples<'_>) -> (ConnectivityTester, Arc<MockRunner>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("utils")).unwrap();
    std::fs::write(
        tmp.path().join("utils/targets.txt"),
        "a = \"http://127.0.0.1/a\"\nb = \"http://127.0.0.1/b\"\nc = \"http://127.0.0.1/c\"\n",
    )
    .unwrap();
    let samples: BTreeMap<String, Vec<Option<u32>>> = samples
        .iter()
        .map(|(id, probes)| (id.to_string(), probes.to_vec()))
        .collect();
    let runner = Arc::new(MockRunner::default());
    let observed = runner.clone();
    let tester = ConnectivityTester {
        settle_wait: Duration::ZERO,
        probe_override: Some(Arc::new(move || {
            let mut state = observed.0.lock().unwrap();
            let id = state
                .active
                .as_ref()
                .map(|strategy| strategy.id.clone())
                .unwrap_or_default();
            state.events.push(format!("probe:{id}"));
            samples.get(&id).cloned().unwrap_or_else(|| vec![None; 3])
        })),
        ..ConnectivityTester::new(runner.clone(), tmp.path().into())
    };
    (tester, runner, tmp)
}

#[tokio::test]
async fn automatic_scan_tests_both_engines_and_keeps_best_instead_of_first_success() {
    let (tester, runner, _tmp) = fixture(&[
        ("", &[Some(10), None, None]),
        ("first", &[Some(30), Some(40), None]),
        ("zapret2:best", &[Some(20), Some(25), Some(30)]),
    ]);
    let result = tester
        .auto_engage(
            vec![
                candidate("first", EngineKind::Zapret1),
                candidate("zapret2:best", EngineKind::Zapret2),
            ],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::Engaged(id) if id == "zapret2:best"));
    let results = tester.last_results();
    assert_eq!(results[0].engine, EngineKind::Zapret2);
    assert_eq!((results[0].baseline_ok, results[0].improved), (1, 2));
    assert_eq!((results[0].avg_latency_ms, results[0].rank), (25, 1));
    assert_eq!(results[1].id, "first");
    let state = runner.0.lock().unwrap();
    assert_eq!(state.active.as_ref().unwrap().id, "zapret2:best");
    let baseline = state
        .events
        .iter()
        .position(|event| event == "probe:")
        .unwrap();
    let first = state
        .events
        .iter()
        .position(|event| event == "start:first")
        .unwrap();
    assert!(baseline < first);
}

#[tokio::test]
async fn unchanged_partial_baseline_is_not_mistaken_for_working_bypass() {
    let (tester, runner, _tmp) = fixture(&[
        ("", &[Some(10), Some(10), None]),
        ("ineffective", &[Some(1), Some(1), None]),
        ("trades-endpoints", &[Some(1), None, Some(1)]),
    ]);
    let result = tester
        .auto_engage(
            vec![
                candidate("ineffective", EngineKind::Zapret1),
                candidate("trades-endpoints", EngineKind::Zapret2),
            ],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::NoneWorking));
    assert!(runner.0.lock().unwrap().active.is_none());
    let results = tester.last_results();
    assert!(results.iter().all(|result| !result.eligible()));
    let trade = results
        .iter()
        .find(|result| result.id == "trades-endpoints")
        .unwrap();
    assert_eq!((trade.improved, trade.regressed), (1, 1));
}

#[tokio::test]
async fn full_baseline_allows_a_nonregressing_strategy_and_retains_tie_priority() {
    let (tester, _runner, _tmp) = fixture(&[
        ("", &[Some(10); 3]),
        ("remembered", &[Some(20); 3]),
        ("other", &[Some(20); 3]),
    ]);
    let result = tester
        .auto_engage(
            vec![
                candidate("remembered", EngineKind::Zapret2),
                candidate("other", EngineKind::Zapret1),
            ],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::Engaged(id) if id == "remembered"));
    assert!(tester
        .last_results()
        .iter()
        .all(|result| result.improved == 0));
}

#[tokio::test]
async fn failed_start_is_cleaned_up_before_testing_the_other_engine() {
    let (tester, runner, _tmp) = fixture(&[("healthy", &[Some(20); 3])]);
    runner
        .0
        .lock()
        .unwrap()
        .fail_starts
        .insert(("broken".into(), 1));
    let result = tester
        .auto_engage(
            vec![
                candidate("broken", EngineKind::Zapret2),
                candidate("healthy", EngineKind::Zapret1),
            ],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::Engaged(id) if id == "healthy"));
    assert_eq!(tester.last_results()[1].ok, 0);
    assert_eq!(
        runner.0.lock().unwrap().active.as_ref().unwrap().id,
        "healthy"
    );
}

#[tokio::test]
async fn failed_winner_restart_falls_back_to_next_eligible_candidate() {
    let (tester, runner, _tmp) = fixture(&[
        ("best", &[Some(10); 3]),
        ("second", &[Some(20), Some(20), None]),
    ]);
    runner
        .0
        .lock()
        .unwrap()
        .fail_starts
        .insert(("best".into(), 2));
    let result = tester
        .auto_engage(
            vec![
                candidate("best", EngineKind::Zapret2),
                candidate("second", EngineKind::Zapret1),
            ],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::Engaged(id) if id == "second"));
    assert_eq!(
        runner.0.lock().unwrap().active.as_ref().unwrap().id,
        "second"
    );
}

#[tokio::test]
async fn cancelling_scan_restores_the_previously_running_engine() {
    let (tester, runner, _tmp) = fixture(&[("prior", &[Some(20); 3])]);
    let prior = candidate("prior", EngineKind::Zapret1);
    runner.0.lock().unwrap().active = Some(prior.clone());
    let cancel = tester.cancel.clone();
    let results = tester
        .test_all(
            vec![prior, candidate("next", EngineKind::Zapret2)],
            Box::new(|_| {}),
            Box::new(move |index, _, _| {
                if index == 2 {
                    cancel.store(true, Ordering::SeqCst);
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        runner.0.lock().unwrap().active.as_ref().unwrap().id,
        "prior"
    );
    assert!(!runner.0.lock().unwrap().starts.contains_key("next"));
}

#[tokio::test]
async fn cancelling_during_a_probe_discards_partial_score_and_restores_prior() {
    let (mut tester, runner, _tmp) = fixture(&[]);
    let prior = candidate("prior", EngineKind::Zapret1);
    runner.0.lock().unwrap().active = Some(prior.clone());
    let cancel = tester.cancel.clone();
    let observed = runner.clone();
    tester.probe_override = Some(Arc::new(move || {
        if observed.0.lock().unwrap().active.is_some() {
            cancel.store(true, Ordering::SeqCst);
        }
        vec![Some(10); 3]
    }));
    let result = tester
        .auto_engage(vec![prior], Box::new(|_, _, _| {}))
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::Cancelled));
    assert!(tester.last_results().is_empty());
    assert_eq!(
        runner.0.lock().unwrap().active.as_ref().unwrap().id,
        "prior"
    );
}

#[tokio::test]
async fn stop_error_never_starts_next_engine_and_restores_prior_after_cleanup() {
    let (tester, runner, _tmp) = fixture(&[("prior", &[Some(20); 3])]);
    let prior = candidate("prior", EngineKind::Zapret1);
    {
        let mut state = runner.0.lock().unwrap();
        state.active = Some(prior.clone());
        // Initial stop succeeds; releasing the first candidate fails once.
        state.fail_stop = Some(2);
    }
    let error = tester
        .auto_engage(
            vec![prior, candidate("other", EngineKind::Zapret2)],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stop failed"));
    let state = runner.0.lock().unwrap();
    assert_eq!(state.active.as_ref().unwrap().id, "prior");
    assert!(!state.starts.contains_key("other"));
}

#[tokio::test]
async fn process_that_exited_does_not_receive_a_successful_baseline_score() {
    let (tester, runner, _tmp) = fixture(&[("", &[Some(10); 3])]);
    runner.0.lock().unwrap().exit_on_start = true;
    let result = tester
        .auto_engage(
            vec![candidate("crashed", EngineKind::Zapret2)],
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert!(matches!(result, AutoEngageOutcome::NoneWorking));
    assert_eq!(tester.last_results()[0].ok, 0);
}

#[tokio::test]
async fn successful_explicit_scan_returns_ranking_and_leaves_start_to_orchestrator() {
    let (tester, runner, _tmp) = fixture(&[("only", &[Some(12); 3])]);
    let results = tester
        .test_all(
            vec![candidate("only", EngineKind::Zapret2)],
            Box::new(|_| {}),
            Box::new(|_, _, _| {}),
        )
        .await
        .unwrap();
    assert_eq!(results[0].improved, 3);
    assert!(runner.0.lock().unwrap().active.is_none());
    assert_eq!(tester.last_results(), results);
}

#[tokio::test]
async fn baseline_progress_does_not_claim_an_engine_is_starting() {
    let (tester, runner, _tmp) = fixture(&[("zapret2:only", &[Some(12); 3])]);
    let progress = Arc::new(Mutex::new(Vec::new()));
    let captured = progress.clone();
    let observed = runner.clone();
    tester
        .test_all(
            vec![candidate("zapret2:only", EngineKind::Zapret2)],
            Box::new(|_| {}),
            Box::new(move |index, _, id| {
                if index > 0 {
                    assert!(observed
                        .0
                        .lock()
                        .unwrap()
                        .events
                        .iter()
                        .any(|event| event == "probe:"));
                }
                captured.lock().unwrap().push((index, id.to_owned()));
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        *progress.lock().unwrap(),
        [(0, String::new()), (1, "zapret2:only".into())]
    );
}

#[tokio::test]
async fn real_loopback_probes_keep_endpoint_identity_and_accept_http_error_responses() {
    use tokio::io::AsyncWriteExt;

    let (mut tester, _runner, _tmp) = fixture(&[]);
    tester.probe_override = None;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            if read_request(&mut socket).await.contains("/response ") {
                socket
                    .write_all(
                        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
            // The other request is closed before receiving any HTTP response.
        }
    });
    let samples = tester
        .probe_endpoints(
            &[
                format!("http://{address}/closed"),
                format!("http://{address}/response"),
            ],
            &AtomicBool::new(false),
        )
        .await
        .unwrap();
    server.await.unwrap();
    assert!(samples[0].is_none());
    assert!(samples[1].is_some());
}

#[tokio::test]
async fn cancelling_real_loopback_probe_joins_tasks_and_closes_connections() {
    use tokio::io::AsyncReadExt;

    let (mut tester, _runner, _tmp) = fixture(&[]);
    tester.probe_override = None;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let server_cancel = cancel.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let mut request = [0; 1024];
        server_cancel.store(true, Ordering::SeqCst);
        let read = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut request))
            .await
            .expect("cancelled HTTP task must release the socket");
        assert!(matches!(read, Ok(0) | Err(_)));
    });
    let samples = tokio::time::timeout(
        Duration::from_secs(2),
        tester.probe_endpoints(&[format!("http://{address}/pending")], &cancel),
    )
    .await
    .unwrap()
    .unwrap();
    server.await.unwrap();
    assert_eq!(samples, vec![None]);
}
