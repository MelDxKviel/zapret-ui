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
/// Auto-engage acceptance threshold: a candidate is considered "good enough" to
/// keep when at least this percentage of the target endpoints became reachable.
/// Without any bypass only a couple of the targets respond, so a simple majority
/// reliably distinguishes a working strategy from a non-working one.
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
}

impl ConnectivityTester {
    pub fn new(runner: Arc<dyn Runner>, install_dir: PathBuf) -> Self {
        Self {
            runner,
            install_dir,
            cancel: Arc::new(AtomicBool::new(false)),
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
                    if val.starts_with("http://") || val.starts_with("https://") {
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
    async fn probe(&self, targets: &[String]) -> (u32, u32) {
        // A fresh client per strategy so connections aren't reused across the
        // winws restart that happens between strategies.
        let client = match reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .pool_max_idle_per_host(0)
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("tester: failed to build http client: {e}");
                return (0, 0);
            }
        };

        let mut set = tokio::task::JoinSet::new();
        for url in targets {
            let client = client.clone();
            // Own the url so the spawned ('static) task doesn't borrow `targets`.
            let url = url.clone();
            set.spawn(async move {
                let started = Instant::now();
                // Any HTTP response means the TLS handshake completed through the
                // DPI — the status code itself doesn't matter for reachability.
                match client.get(&url).send().await {
                    Ok(_) => Some(started.elapsed().as_millis() as u32),
                    Err(_) => None,
                }
            });
        }

        let mut ok = 0u32;
        let mut latency_sum = 0u64;
        loop {
            tokio::select! {
                _ = wait_cancelled(&self.cancel) => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    break;
                }
                res = set.join_next() => {
                    let Some(res) = res else {
                        break;
                    };
                    if let Ok(Some(ms)) = res {
                        ok += 1;
                        latency_sum += ms as u64;
                    }
                }
            }
        }
        let avg = if ok > 0 {
            (latency_sum / ok as u64) as u32
        } else {
            0
        };
        (ok, avg)
    }
}

#[async_trait::async_trait]
impl StrategyTester for ConnectivityTester {
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
            tracing::info!("[{index}/{total}] testing strategy: {}", strategy.id);

            // Clean slate, then start this preset.
            let _ = self.runner.stop().await;
            if let Err(e) = self.runner.start(strategy).await {
                tracing::warn!("[{index}/{total}] failed to start {}: {e}", strategy.id);
                let result = StrategyTestResult {
                    id: strategy.id.clone(),
                    display_name: strategy.display_name.clone(),
                    ok: 0,
                    total: targets.len() as u32,
                    avg_latency_ms: 0,
                    rank: 0,
                };
                on_each(result.clone());
                results.push(result);
                continue;
            }

            // Let the desync engine settle (honour cancellation while we wait).
            let waited = wait_cancellable(INIT_WAIT, &self.cancel).await;
            if !waited {
                let _ = self.runner.stop().await;
                tracing::info!("Strategy test cancelled by user");
                break;
            }

            let (ok, avg_latency_ms) = self.probe(&targets).await;
            let _ = self.runner.stop().await;

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
                ok,
                total: targets.len() as u32,
                avg_latency_ms,
                rank: 0,
            };
            on_each(result.clone());
            results.push(result);
        }

        // Make sure nothing is left running after a test.
        let _ = self.runner.stop().await;

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
                let _ = self.runner.stop().await;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            let index = i as u32 + 1;
            on_progress(index, total, &strategy.id);
            tracing::info!("[{index}/{total}] auto-engage trying: {}", strategy.id);

            let _ = self.runner.stop().await;
            if let Err(e) = self.runner.start(strategy).await {
                tracing::warn!("[{index}/{total}] failed to start {}: {e}", strategy.id);
                continue;
            }
            // Let the desync engine settle (honouring cancellation).
            if !wait_cancellable(INIT_WAIT, &self.cancel).await {
                let _ = self.runner.stop().await;
                return Ok(AutoEngageOutcome::Cancelled);
            }
            let (ok, avg) = self.probe(&targets).await;
            if self.cancel.load(Ordering::SeqCst) {
                let _ = self.runner.stop().await;
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
            let _ = self.runner.stop().await;
        }

        // Nothing crossed the threshold. If a candidate had partial reachability,
        // fall back to the best one (better than leaving the user with nothing).
        if let Some((bok, idx)) = best {
            if bok > 0 {
                let strategy = &candidates[idx];
                let _ = self.runner.stop().await;
                if self.runner.start(strategy).await.is_ok() {
                    tracing::info!(
                        "Auto-engage fell back to best candidate: {} ({}/{})",
                        strategy.id,
                        bok,
                        targets.len()
                    );
                    return Ok(AutoEngageOutcome::Engaged(strategy.id.clone()));
                }
            }
        }

        let _ = self.runner.stop().await;
        Ok(AutoEngageOutcome::NoneWorking)
    }

    async fn verify(&self) -> anyhow::Result<bool> {
        // The bypass is already running the strategy under test — we don't touch
        // the runner, just let it settle, then probe the same endpoints.
        self.cancel.store(false, Ordering::SeqCst);
        let targets = self.load_targets();
        tokio::time::sleep(INIT_WAIT).await;
        let (ok, avg) = self.probe(&targets).await;
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
