//! Strategy connectivity tester — the in-app equivalent of Flowseal's
//! `utils/test zapret.ps1`.
//!
//! For every strategy it (1) starts winws with that preset via the shared
//! [`Runner`], (2) waits for the desync engine to initialise, (3) probes a set
//! of normally-blocked HTTPS endpoints, and (4) scores the strategy by how many
//! endpoints became reachable (tie-broken by average latency). The strategy
//! with the highest score wins and is reported back so the UI can auto-select
//! it.
//!
//! Endpoints come from the installed `utils/targets.txt` (same file the upstream
//! script reads); if it is missing we fall back to a built-in Discord/YouTube/
//! Google/Cloudflare list. `PING:` ICMP-only entries are ignored — we only do
//! TLS/HTTP reachability, which is what actually exercises the DPI bypass.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::contracts::{AutoEngageOutcome, Strategy, StrategyTestResult};
use crate::ports::{Runner, StrategyTester, TestProgressCb, TestResultCb};

/// How long to let winws settle before probing (matches the upstream 5s wait).
const INIT_WAIT: Duration = Duration::from_secs(4);
/// Per-endpoint request timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Quick verification and macOS auto-engage threshold. Windows full scans use
/// per-endpoint improvements against a measured baseline instead.
const AUTO_ENGAGE_MIN_PCT: u32 = 50;

/// Built-in endpoints used when `utils/targets.txt` is absent. Kept in sync
/// with the upstream defaults (HTTPS targets only).
const DEFAULT_TARGETS: &[&str] = &[
    "https://discord.com",
    "https://gateway.discord.gg",
    "https://cdn.discordapp.com",
    "https://updates.discord.com",
    "https://www.youtube.com",
    "https://youtu.be",
    "https://i.ytimg.com",
    "https://redirector.googlevideo.com",
    "https://www.google.com",
    "https://www.gstatic.com",
    "https://www.cloudflare.com",
    "https://cdnjs.cloudflare.com",
];

pub struct ConnectivityTester {
    runner: Arc<dyn Runner>,
    install_dir: PathBuf,
    cancel: Arc<AtomicBool>,
    #[cfg(windows)]
    results: std::sync::Mutex<Vec<StrategyTestResult>>,
    #[cfg(windows)]
    settle_wait: Duration,
    #[cfg(all(test, windows))]
    probe_override: Option<MockProbe>,
}

#[cfg(all(test, windows))]
type MockProbe = Arc<dyn Fn() -> Vec<Option<u32>> + Send + Sync>;

impl ConnectivityTester {
    pub fn new(runner: Arc<dyn Runner>, install_dir: PathBuf) -> Self {
        Self {
            runner,
            install_dir,
            cancel: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            results: std::sync::Mutex::new(Vec::new()),
            #[cfg(windows)]
            settle_wait: INIT_WAIT,
            #[cfg(all(test, windows))]
            probe_override: None,
        }
    }

    /// Load HTTPS endpoints from `utils/targets.txt`, falling back to defaults.
    fn load_targets(&self) -> Vec<String> {
        let path = self.install_dir.join("utils").join("targets.txt");
        let mut out = Vec::new();
        if let Ok(content) = std::fs::read_to_string(&path) {
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                // Format: `KeyName = "value"`
                if let Some((_, rhs)) = line.split_once('=') {
                    let val = rhs.trim().trim_matches('"').trim();
                    if (val.starts_with("http://") || val.starts_with("https://"))
                        && out.len() < 64
                        && !out.iter().any(|url| url == val)
                    {
                        out.push(val.to_string());
                    }
                    // `PING:` ICMP-only entries are intentionally skipped.
                }
            }
        }
        if out.is_empty() {
            out = DEFAULT_TARGETS.iter().map(|s| s.to_string()).collect();
        }
        out
    }

    /// Probe every target concurrently; return (ok_count, avg_latency_ms).
    async fn probe(&self, targets: &[String], cancel: &AtomicBool) -> (u32, u32) {
        match self.probe_endpoints(targets, cancel).await {
            Ok(endpoints) => summarize(&endpoints),
            Err(error) => {
                tracing::warn!("tester: probe failed: {error:#}");
                (0, 0)
            }
        }
    }

    /// Keep endpoint identity: two equal totals can hide a newly broken service.
    async fn probe_endpoints(
        &self,
        targets: &[String],
        cancel: &AtomicBool,
    ) -> anyhow::Result<Vec<Option<u32>>> {
        #[cfg(all(test, windows))]
        if let Some(probe) = &self.probe_override {
            return Ok(probe());
        }
        // A fresh client per strategy so connections aren't reused across the
        // winws restart that happens between strategies.
        let client = reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .pool_max_idle_per_host(0)
            .no_proxy()
            .build()?;

        let mut set = tokio::task::JoinSet::new();
        for (index, url) in targets.iter().enumerate() {
            let client = client.clone();
            // Own the url so the spawned ('static) task doesn't borrow `targets`.
            let url = url.clone();
            set.spawn(async move {
                let started = Instant::now();
                // Any HTTP response means the TLS handshake completed through the
                // DPI — the status code itself doesn't matter for reachability.
                let latency = match client.get(&url).send().await {
                    Ok(_) => Some(started.elapsed().as_millis() as u32),
                    Err(_) => None,
                };
                (index, latency)
            });
        }

        let mut endpoints = vec![None; targets.len()];
        loop {
            tokio::select! {
                _ = wait_cancelled(cancel) => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    break;
                }
                res = set.join_next() => {
                    let Some(res) = res else {
                        break;
                    };
                    if let Ok((index, latency)) = res {
                        endpoints[index] = latency;
                    }
                }
            }
        }
        Ok(endpoints)
    }
}

#[cfg(windows)]
impl ConnectivityTester {
    /// Service restoration belongs to the orchestrator: restarting a process
    /// cannot reproduce a service's protected installation or startup policy.
    async fn prior_strategy(&self, candidates: &[Strategy]) -> anyhow::Result<Option<Strategy>> {
        let status = self.runner.detect_running().await;
        if status.running_mode != crate::contracts::RunningMode::UserProcess {
            return Ok(None);
        }
        let prior = candidates.iter().find(|candidate| {
            status.active_strategy.as_deref() == Some(candidate.id.as_str())
                && status
                    .active_engine
                    .is_none_or(|engine| engine == candidate.engine)
        });
        match prior {
            Some(strategy) => Ok(Some(strategy.clone())),
            None => anyhow::bail!(
                "Cannot restore the active strategy after testing; stop the running core first"
            ),
        }
    }

    async fn restore_prior(&self, prior: Option<&Strategy>) -> anyhow::Result<()> {
        self.runner.stop().await?;
        if let Some(strategy) = prior {
            if let Err(error) = self.runner.start(strategy).await {
                let error = error.context(format!(
                    "Failed to restore previous strategy {}",
                    strategy.id
                ));
                return match self.runner.stop().await {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(error.context(format!(
                        "Failed to stop the unsuccessful restoration: {cleanup:#}"
                    ))),
                };
            }
        }
        Ok(())
    }

    async fn recover_error(&self, error: anyhow::Error, prior: Option<&Strategy>) -> anyhow::Error {
        match self.restore_prior(prior).await {
            Ok(()) => error,
            Err(cleanup) => error.context(format!("Scan cleanup failed: {cleanup:#}")),
        }
    }

    async fn scan_windows(
        &self,
        strategies: &[Strategy],
        on_each: TestResultCb,
        on_progress: &TestProgressCb,
    ) -> anyhow::Result<Vec<StrategyTestResult>> {
        let targets = self.load_targets();
        let mut baseline = None;
        let mut results = Vec::new();
        for (index, strategy) in strategies.iter().enumerate() {
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            if baseline.is_none() {
                // Index zero means the no-bypass baseline, not the first
                // candidate. Never label an engine as starting during baseline.
                on_progress(0, strategies.len() as u32, "");
            } else {
                on_progress(index as u32 + 1, strategies.len() as u32, &strategy.id);
            }
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            // This barrier is mandatory across both engines and after failed
            // starts; never attach a second driver when stop has failed.
            self.runner.stop().await?;
            if baseline.is_none() {
                if !wait_cancellable(self.settle_wait, &self.cancel).await {
                    break;
                }
                baseline = Some(self.probe_endpoints(&targets, &self.cancel).await?);
                if self.cancel.load(Ordering::SeqCst) {
                    break;
                }
                tracing::info!(
                    "Strategy baseline without bypass: {}/{} reachable",
                    summarize(baseline.as_ref().unwrap()).0,
                    targets.len()
                );
                on_progress(index as u32 + 1, strategies.len() as u32, &strategy.id);
                if self.cancel.load(Ordering::SeqCst) {
                    break;
                }
            }
            let baseline = baseline.as_ref().unwrap();
            let endpoints = match self.runner.start(strategy).await {
                Ok(_) => {
                    if !wait_cancellable(self.settle_wait, &self.cancel).await {
                        break;
                    }
                    if self.runner.detect_running().await.running_mode
                        == crate::contracts::RunningMode::None
                    {
                        tracing::warn!("Strategy {} exited before probing", strategy.id);
                        vec![None; targets.len()]
                    } else {
                        self.probe_endpoints(&targets, &self.cancel).await?
                    }
                }
                Err(error) => {
                    if authorization_cancelled(&error) {
                        return Err(error);
                    }
                    tracing::warn!("Failed to start strategy {}: {error:#}", strategy.id);
                    vec![None; targets.len()]
                }
            };
            self.runner.stop().await?;
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            let result = score(strategy, baseline, &endpoints);
            tracing::info!(
                "{}: {}/{} reachable, +{} / -{} against baseline, avg {} ms",
                strategy.id,
                result.ok,
                result.total,
                result.improved,
                result.regressed,
                result.avg_latency_ms
            );
            on_each(result.clone());
            results.push(result);
            *self.results.lock().unwrap() = results.clone();
        }
        self.runner.stop().await?;
        rank_results(&mut results);
        *self.results.lock().unwrap() = results.clone();
        Ok(results)
    }
}

fn summarize(endpoints: &[Option<u32>]) -> (u32, u32) {
    let count = endpoints.iter().flatten().count() as u32;
    let sum: u64 = endpoints.iter().flatten().map(|ms| *ms as u64).sum();
    (count, (sum / u64::from(count.max(1))) as u32)
}

#[cfg(windows)]
fn authorization_cancelled(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::contracts::AuthorizationCancelled>()
        .is_some()
}

#[cfg(windows)]
fn score(
    strategy: &Strategy,
    baseline: &[Option<u32>],
    endpoints: &[Option<u32>],
) -> StrategyTestResult {
    let (ok, avg_latency_ms) = summarize(endpoints);
    let improved = baseline
        .iter()
        .zip(endpoints)
        .filter(|(before, after)| before.is_none() && after.is_some())
        .count() as u32;
    let regressed = baseline
        .iter()
        .zip(endpoints)
        .filter(|(before, after)| before.is_some() && after.is_none())
        .count() as u32;
    StrategyTestResult {
        engine: strategy.engine,
        id: strategy.id.clone(),
        display_name: strategy.display_name.clone(),
        baseline_ok: summarize(baseline).0,
        improved,
        regressed,
        ok,
        total: endpoints.len() as u32,
        avg_latency_ms,
        rank: 0,
    }
}

#[cfg(windows)]
fn rank_results(results: &mut [StrategyTestResult]) {
    // Stable sorting preserves last-known-good priority for identical scores.
    results.sort_by(|a, b| {
        b.eligible()
            .cmp(&a.eligible())
            .then_with(|| {
                let a_net = i64::from(a.improved) - i64::from(a.regressed);
                let b_net = i64::from(b.improved) - i64::from(b.regressed);
                b_net.cmp(&a_net)
            })
            .then_with(|| b.ok.cmp(&a.ok))
            .then_with(|| a.regressed.cmp(&b.regressed))
            .then_with(|| a.avg_latency_ms.cmp(&b.avg_latency_ms))
    });
    for (index, result) in results.iter_mut().enumerate() {
        result.rank = index as u32 + 1;
    }
}

#[async_trait::async_trait]
impl StrategyTester for ConnectivityTester {
    #[cfg(windows)]
    fn last_results(&self) -> Vec<StrategyTestResult> {
        self.results.lock().unwrap().clone()
    }

    #[cfg(windows)]
    async fn test_all(
        &self,
        strategies: Vec<Strategy>,
        on_each: TestResultCb,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<Vec<StrategyTestResult>> {
        self.cancel.store(false, Ordering::SeqCst);
        self.results.lock().unwrap().clear();
        let prior = self.prior_strategy(&strategies).await?;
        let scan = self.scan_windows(&strategies, on_each, &on_progress).await;
        match scan {
            Ok(results) => {
                if self.cancel.load(Ordering::SeqCst) {
                    self.restore_prior(prior.as_ref()).await?;
                }
                Ok(results)
            }
            Err(error) => Err(self.recover_error(error, prior.as_ref()).await),
        }
    }

    #[cfg(not(windows))]
    async fn test_all(
        &self,
        strategies: Vec<Strategy>,
        on_each: TestResultCb,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<Vec<StrategyTestResult>> {
        self.cancel.store(false, Ordering::SeqCst);

        let targets = self.load_targets();
        let total = strategies.len() as u32;
        tracing::info!(
            "Strategy test starting: {} strategies × {} endpoints",
            total,
            targets.len()
        );

        let mut results: Vec<StrategyTestResult> = Vec::new();

        for (i, strategy) in strategies.iter().enumerate() {
            if self.cancel.load(Ordering::SeqCst) {
                tracing::info!("Strategy test cancelled by user");
                break;
            }

            let index = i as u32 + 1;
            on_progress(index, total, &strategy.id);
            if self.cancel.load(Ordering::SeqCst) {
                break;
            }
            tracing::info!("[{index}/{total}] testing strategy: {}", strategy.id);

            // Clean slate, then start this preset.
            self.runner.stop().await?;
            if let Err(e) = self.runner.start(strategy).await {
                if e.downcast_ref::<crate::contracts::AuthorizationCancelled>()
                    .is_some()
                {
                    return Err(e);
                }
                tracing::warn!("[{index}/{total}] failed to start {}: {e}", strategy.id);
                let result = StrategyTestResult {
                    id: strategy.id.clone(),
                    display_name: strategy.display_name.clone(),
                    engine: strategy.engine,
                    ok: 0,
                    total: targets.len() as u32,
                    avg_latency_ms: 0,
                    rank: 0,
                    ..Default::default()
                };
                on_each(result.clone());
                results.push(result);
                continue;
            }

            // Let the desync engine settle (honour cancellation while we wait).
            let waited = wait_cancellable(INIT_WAIT, &self.cancel).await;
            if !waited {
                self.runner.stop().await?;
                tracing::info!("Strategy test cancelled by user");
                break;
            }

            let (ok, avg_latency_ms) = self.probe(&targets, &self.cancel).await;
            self.runner.stop().await?;

            tracing::info!(
                "[{index}/{total}] {} → {}/{} reachable, avg {} ms",
                strategy.id,
                ok,
                targets.len(),
                avg_latency_ms
            );

            let result = StrategyTestResult {
                id: strategy.id.clone(),
                display_name: strategy.display_name.clone(),
                engine: strategy.engine,
                ok,
                total: targets.len() as u32,
                avg_latency_ms,
                rank: 0,
                ..Default::default()
            };
            on_each(result.clone());
            results.push(result);
        }

        // Make sure nothing is left running after a test.
        self.runner.stop().await?;

        // Rank: most endpoints reachable first, ties broken by lower latency.
        results.sort_by(|a, b| {
            b.ok.cmp(&a.ok).then_with(|| {
                let al = if a.avg_latency_ms == 0 {
                    u32::MAX
                } else {
                    a.avg_latency_ms
                };
                let bl = if b.avg_latency_ms == 0 {
                    u32::MAX
                } else {
                    b.avg_latency_ms
                };
                al.cmp(&bl)
            })
        });
        for (i, r) in results.iter_mut().enumerate() {
            r.rank = i as u32 + 1;
        }

        if let Some(best) = results.first() {
            if best.ok > 0 {
                tracing::info!("Best strategy: {} ({}/{})", best.id, best.ok, best.total);
            }
        }

        Ok(results)
    }

    #[cfg(windows)]
    async fn auto_engage(
        &self,
        candidates: Vec<Strategy>,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<AutoEngageOutcome> {
        self.cancel.store(false, Ordering::SeqCst);
        self.results.lock().unwrap().clear();
        let prior = self.prior_strategy(&candidates).await?;
        let outcome = async {
            let results = self
                .scan_windows(&candidates, Box::new(|_| {}), &on_progress)
                .await?;
            if self.cancel.load(Ordering::SeqCst) {
                return Ok(AutoEngageOutcome::Cancelled);
            }
            for result in results.iter().filter(|result| result.eligible()) {
                let strategy = candidates
                    .iter()
                    .find(|candidate| {
                        candidate.id == result.id && candidate.engine == result.engine
                    })
                    .expect("scan results belong to the supplied candidates");
                // A previously tested candidate may fail on restart. Always
                // release it before considering the next ranked candidate.
                on_progress(
                    candidates.len() as u32,
                    candidates.len() as u32,
                    &strategy.id,
                );
                self.runner.stop().await?;
                if self.cancel.load(Ordering::SeqCst) {
                    return Ok(AutoEngageOutcome::Cancelled);
                }
                if let Err(error) = self.runner.start(strategy).await {
                    if authorization_cancelled(&error) {
                        return Err(error);
                    }
                    tracing::warn!("Failed to restart candidate {}: {error:#}", strategy.id);
                    continue;
                }
                if !wait_cancellable(self.settle_wait, &self.cancel).await {
                    return Ok(AutoEngageOutcome::Cancelled);
                }
                if self.runner.detect_running().await.running_mode
                    != crate::contracts::RunningMode::None
                {
                    tracing::info!(
                        "Auto-engage selected {}: {}/{} reachable, +{} / -{} against baseline",
                        strategy.id,
                        result.ok,
                        result.total,
                        result.improved,
                        result.regressed
                    );
                    return Ok(AutoEngageOutcome::Engaged(strategy.id.clone()));
                }
                tracing::warn!("Candidate {} exited during restart", strategy.id);
            }
            Ok(AutoEngageOutcome::NoneWorking)
        }
        .await;
        match outcome {
            Ok(AutoEngageOutcome::Engaged(id)) => Ok(AutoEngageOutcome::Engaged(id)),
            Ok(other) => {
                self.restore_prior(prior.as_ref()).await?;
                Ok(other)
            }
            Err(error) => Err(self.recover_error(error, prior.as_ref()).await),
        }
    }

    #[cfg(not(windows))]
    async fn auto_engage(
        &self,
        candidates: Vec<Strategy>,
        on_progress: TestProgressCb,
    ) -> anyhow::Result<AutoEngageOutcome> {
        self.cancel.store(false, Ordering::SeqCst);

        let targets = self.load_targets();
        let total = candidates.len() as u32;
        // Minimum reachable endpoints to accept a candidate (floored majority).
        let need = ((targets.len() as u32 * AUTO_ENGAGE_MIN_PCT) / 100).max(1);
        tracing::info!(
            "Auto-engage starting: {} candidate(s), need ≥{}/{} endpoints",
            total,
            need,
            targets.len()
        );

        // Best-scoring candidate so far — a fallback when nothing meets the
        // threshold but something is at least partially reachable.
        let mut best: Option<(u32, usize)> = None;

        for (i, strategy) in candidates.iter().enumerate() {
            if self.cancel.load(Ordering::SeqCst) {
                self.runner.stop().await?;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            let index = i as u32 + 1;
            on_progress(index, total, &strategy.id);
            if self.cancel.load(Ordering::SeqCst) {
                self.runner.stop().await?;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            tracing::info!("[{index}/{total}] auto-engage trying: {}", strategy.id);

            self.runner.stop().await?;
            if let Err(e) = self.runner.start(strategy).await {
                if e.downcast_ref::<crate::contracts::AuthorizationCancelled>()
                    .is_some()
                {
                    return Err(e);
                }
                tracing::warn!("[{index}/{total}] failed to start {}: {e}", strategy.id);
                continue;
            }
            // Let the desync engine settle (honouring cancellation).
            if !wait_cancellable(INIT_WAIT, &self.cancel).await {
                self.runner.stop().await?;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            let (ok, avg) = self.probe(&targets, &self.cancel).await;
            if self.cancel.load(Ordering::SeqCst) {
                self.runner.stop().await?;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            tracing::info!(
                "[{index}/{total}] {} → {}/{} reachable, avg {} ms",
                strategy.id,
                ok,
                targets.len(),
                avg
            );

            if ok >= need {
                // Good enough — leave winws running with this strategy.
                tracing::info!(
                    "Auto-engage selected: {} ({}/{})",
                    strategy.id,
                    ok,
                    targets.len()
                );
                return Ok(AutoEngageOutcome::Engaged(strategy.id.clone()));
            }
            if best.is_none_or(|(bok, _)| ok > bok) {
                best = Some((ok, i));
            }
            self.runner.stop().await?;
        }

        // Nothing crossed the threshold. If a candidate had partial reachability,
        // fall back to the best one (better than leaving the user with nothing).
        if let Some((bok, idx)) = best {
            if bok > 0 {
                let strategy = &candidates[idx];
                self.runner.stop().await?;
                match self.runner.start(strategy).await {
                    Ok(_) => {
                        tracing::info!(
                            "Auto-engage fell back to best candidate: {} ({}/{})",
                            strategy.id,
                            bok,
                            targets.len()
                        );
                        return Ok(AutoEngageOutcome::Engaged(strategy.id.clone()));
                    }
                    Err(e)
                        if e.downcast_ref::<crate::contracts::AuthorizationCancelled>()
                            .is_some() =>
                    {
                        return Err(e);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to restart best candidate {}: {e:#}", strategy.id)
                    }
                }
            }
        }

        self.runner.stop().await?;
        Ok(AutoEngageOutcome::NoneWorking)
    }

    async fn verify(&self) -> anyhow::Result<bool> {
        // The bypass is already running the strategy under test — we don't touch
        // the runner, just let it settle, then probe the same endpoints.
        // A delayed background verification may overlap a newly started scan.
        // It must neither clear nor consume that scan's cancellation flag.
        let verification_cancel = AtomicBool::new(false);
        let targets = self.load_targets();
        tokio::time::sleep(INIT_WAIT).await;
        let (ok, avg) = self.probe(&targets, &verification_cancel).await;
        let need = ((targets.len() as u32 * AUTO_ENGAGE_MIN_PCT) / 100).max(1);
        tracing::info!(
            "Background verify: {}/{} reachable (need {}), avg {} ms",
            ok,
            targets.len(),
            need,
            avg
        );
        Ok(ok >= need)
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

async fn wait_cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Sleep for `dur`, returning `false` early if cancellation is requested.
async fn wait_cancellable(dur: Duration, cancel: &AtomicBool) -> bool {
    let started = Instant::now();
    while started.elapsed() < dur {
        if cancel.load(Ordering::SeqCst) {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !cancel.load(Ordering::SeqCst)
}

#[cfg(all(test, windows))]
#[path = "tester_tests.rs"]
mod windows_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{Category, RuntimeStatus};
    use futures_util::FutureExt;
    use std::sync::atomic::AtomicUsize;

    struct StopFailingRunner {
        starts: AtomicUsize,
    }

    struct CancelAuthorizationRunner {
        starts: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Runner for CancelAuthorizationRunner {
        async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Err(crate::contracts::AuthorizationCancelled.into())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn detect_running(&self) -> RuntimeStatus {
            RuntimeStatus::default()
        }
    }

    struct NeverStartRunner;

    #[async_trait::async_trait]
    impl Runner for NeverStartRunner {
        async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
            panic!("cancelled scan must not start winws")
        }

        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn detect_running(&self) -> RuntimeStatus {
            RuntimeStatus::default()
        }
    }

    #[async_trait::async_trait]
    impl Runner for StopFailingRunner {
        async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(1)
        }

        async fn stop(&self) -> anyhow::Result<()> {
            anyhow::bail!("previous winws could not be stopped")
        }

        async fn detect_running(&self) -> RuntimeStatus {
            RuntimeStatus::default()
        }
    }

    fn strategy() -> Strategy {
        Strategy {
            engine: Default::default(),
            id: "test".into(),
            display_name: "test".into(),
            category: Category::Other,
            description: String::new(),
            winws_args: Vec::new(),
            requires_lists: Vec::new(),
        }
    }

    fn tester(runner: Arc<dyn Runner>, install_dir: PathBuf) -> ConnectivityTester {
        let tester = ConnectivityTester::new(runner, install_dir);
        #[cfg(windows)]
        let tester = ConnectivityTester {
            settle_wait: Duration::ZERO,
            probe_override: Some(Arc::new(|| vec![None; DEFAULT_TARGETS.len()])),
            ..tester
        };
        tester
    }

    #[tokio::test]
    async fn cancelled_authorization_stops_scans_without_prompting_for_the_next_strategy() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(CancelAuthorizationRunner {
            starts: AtomicUsize::new(0),
        });
        let tester = tester(runner.clone(), tmp.path().to_path_buf());
        let error = tester
            .test_all(
                vec![strategy(), strategy()],
                Box::new(|_| panic!("cancelled authorization must not report a score")),
                Box::new(|_, _, _| {}),
            )
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<crate::contracts::AuthorizationCancelled>()
            .is_some());
        assert_eq!(runner.starts.load(Ordering::SeqCst), 1);
        let error = tester
            .auto_engage(vec![strategy(), strategy()], Box::new(|_, _, _| {}))
            .await
            .unwrap_err();
        assert!(error
            .downcast_ref::<crate::contracts::AuthorizationCancelled>()
            .is_some());
        assert_eq!(runner.starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_all_does_not_start_or_score_after_stop_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(StopFailingRunner {
            starts: AtomicUsize::new(0),
        });
        let tester = tester(runner.clone(), tmp.path().to_path_buf());
        let results = Arc::new(AtomicUsize::new(0));
        let observed = results.clone();
        let error = tester
            .test_all(
                vec![strategy()],
                Box::new(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                }),
                Box::new(|_, _, _| {}),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("could not be stopped"));
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
        assert_eq!(results.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_engage_does_not_start_after_stop_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(StopFailingRunner {
            starts: AtomicUsize::new(0),
        });
        let tester = tester(runner.clone(), tmp.path().to_path_buf());
        let error = tester
            .auto_engage(vec![strategy()], Box::new(|_, _, _| {}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("could not be stopped"));
        assert_eq!(runner.starts.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn background_verify_does_not_clear_scan_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(StopFailingRunner {
            starts: AtomicUsize::new(0),
        });
        let tester = tester(runner, tmp.path().to_path_buf());
        tester.cancel();
        // Poll until the initial sleep, then drop before any network probe.
        assert!(tester.verify().now_or_never().is_none());
        assert!(tester.cancel.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_from_progress_prevents_the_first_start() {
        let tmp = tempfile::tempdir().unwrap();
        let tester = Arc::new(tester(Arc::new(NeverStartRunner), tmp.path().to_path_buf()));
        let progress_tester = tester.clone();
        let results = tester
            .test_all(
                vec![strategy()],
                Box::new(|_| panic!("cancelled scan must not report a score")),
                Box::new(move |_, _, _| progress_tester.cancel()),
            )
            .await
            .unwrap();
        assert!(results.is_empty());

        let progress_tester = tester.clone();
        let outcome = tester
            .auto_engage(
                vec![strategy()],
                Box::new(move |_, _, _| progress_tester.cancel()),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, AutoEngageOutcome::Cancelled));
    }
}
