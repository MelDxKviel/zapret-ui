//! Interface counters only on the visible page; HTTPS checks only on request
//! or after explicitly enabling the page's 15-second automatic checks.
use super::{MainWindow, MonitorRound, MonitorTarget, TrafficSample};
use slint::ComponentHandle;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use sysinfo::Networks;
use tracing_subscriber::fmt::{format::Writer, time::FormatTime};

const ENDPOINTS: [(&str, &str, &str); 5] = [
    (
        "YouTube",
        "www.youtube.com",
        "https://www.youtube.com/generate_204",
    ),
    (
        "Discord",
        "discord.com",
        "https://discord.com/api/v9/gateway",
    ),
    ("Telegram", "web.telegram.org", "https://web.telegram.org/"),
    (
        "Google",
        "www.google.com",
        "https://www.google.com/generate_204",
    ),
    (
        "Cloudflare",
        "www.cloudflare.com",
        "https://www.cloudflare.com/cdn-cgi/trace",
    ),
];
const PROBES: u32 = 3;

#[derive(Default)]
struct Sampler {
    networks: Networks,
    selected: String,
    last: Option<(Instant, u64, u64)>,
    history: VecDeque<(Instant, f32, f32)>,
}

#[derive(Default)]
struct Snapshot {
    names: Vec<String>,
    selected: String,
    ip: String,
    rx: String,
    tx: String,
    rx_total: String,
    tx_total: String,
    errors: u64,
    history: Vec<(f32, f32, f32)>,
}

fn clock() -> String {
    let mut value = String::new();
    let mut writer = Writer::new(&mut value);
    if let Ok(timer) = tracing_subscriber::fmt::time::OffsetTime::local_rfc_3339() {
        let _ = timer.format_time(&mut writer);
        value.get(11..19).unwrap_or_default().to_owned()
    } else {
        let _ = tracing_subscriber::fmt::time::SystemTime.format_time(&mut writer);
        format!("{} UTC", value.get(11..19).unwrap_or_default())
    }
}

fn bytes(value: u64) -> String {
    if value >= 1_073_741_824 {
        format!("{:.2} GiB", value as f64 / 1_073_741_824.0)
    } else {
        format!("{:.1} MiB", value as f64 / 1_048_576.0)
    }
}

fn rates(
    previous: Option<(Instant, u64, u64)>,
    now: Instant,
    rx: u64,
    tx: u64,
) -> Option<(f32, f32)> {
    let (time, old_rx, old_tx) = previous?;
    let elapsed = now.duration_since(time).as_secs_f64();
    // Do not average hidden time or turn reset OS counters into huge spikes.
    if !(0.25..=9.0).contains(&elapsed) {
        return None;
    }
    Some((
        (rx.checked_sub(old_rx)? as f64 * 8.0 / elapsed / 1_000_000.0) as f32,
        (tx.checked_sub(old_tx)? as f64 * 8.0 / elapsed / 1_000_000.0) as f32,
    ))
}

impl Sampler {
    fn sample(&mut self, selected: String) -> Snapshot {
        self.networks.refresh(true);
        let mut names = self
            .networks
            .iter()
            .filter(|(_, data)| {
                data.ip_networks()
                    .iter()
                    .any(|ip| !ip.addr.is_loopback() && !ip.addr.is_unspecified())
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        names.sort();
        let selected = if names.contains(&selected) {
            selected
        } else {
            names
                .iter()
                .max_by_key(|name| {
                    self.networks[*name]
                        .total_received()
                        .saturating_add(self.networks[*name].total_transmitted())
                })
                .cloned()
                .unwrap_or_default()
        };
        if self.selected != selected {
            self.last = None;
            self.history.clear();
            self.selected = selected.clone();
        }
        let mut result = Snapshot {
            names,
            selected,
            rx: "—".into(),
            tx: "—".into(),
            rx_total: "—".into(),
            tx_total: "—".into(),
            ..Default::default()
        };
        if let Some(data) = self.networks.get(&result.selected) {
            let rx = data.total_received();
            let tx = data.total_transmitted();
            result.ip = data
                .ip_networks()
                .iter()
                .filter(|ip| ip.addr.is_ipv4() && !ip.addr.is_loopback())
                .map(|ip| ip.addr.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            result.errors = data
                .total_errors_on_received()
                .saturating_add(data.total_errors_on_transmitted());
            result.rx_total = bytes(rx);
            result.tx_total = bytes(tx);
            let now = Instant::now();
            if let Some((rx, tx)) = rates(self.last, now, rx, tx) {
                result.rx = format!("{rx:.2}");
                result.tx = format!("{tx:.2}");
                if self.history.len() == 36 {
                    self.history.pop_front();
                }
                self.history.push_back((now, rx, tx));
            } else {
                self.history.clear();
            }
            self.last = Some((now, rx, tx));
        } else {
            self.last = None;
            self.history.clear();
        }
        // Position samples by elapsed time, including ordinary timer jitter.
        // The newest point is age zero; hidden gaps already reset the history.
        if let Some((newest, _, _)) = self.history.back() {
            result.history = self
                .history
                .iter()
                .map(|(time, rx, tx)| (newest.duration_since(*time).as_secs_f32(), *rx, *tx))
                .collect();
        }
        result
    }
}

#[derive(Default)]
struct ProbeResult {
    successes: u32,
    sum_ms: u64,
    min_ms: Option<u32>,
    max_ms: u32,
    detail: String,
    samples: Vec<i32>,
}

#[derive(Default)]
struct ProbeHistory {
    context: String,
    rounds: VecDeque<(String, i32, i32, i32)>,
}

impl ProbeResult {
    fn http_response(&mut self, status: u16) {
        if !(200..300).contains(&status) {
            self.detail = format!("HTTP {status}");
        }
    }
    fn push(&mut self, result: Result<u32, String>) {
        match result {
            Ok(ms) => {
                self.successes += 1;
                self.sum_ms += u64::from(ms);
                self.min_ms = Some(self.min_ms.map_or(ms, |old| old.min(ms)));
                self.max_ms = self.max_ms.max(ms);
                self.samples.push(1);
            }
            Err(detail) => {
                self.detail = detail;
                self.samples.push(-1);
            }
        }
    }
    fn target(self, name: &str, host: &str) -> MonitorTarget {
        let attempts = self.samples.len() as u32;
        MonitorTarget {
            name: name.into(),
            host: host.into(),
            state: if self.successes == 0 {
                "error"
            } else if self.successes < attempts {
                "partial"
            } else {
                "ok"
            }
            .into(),
            latency: if self.successes == 0 {
                0
            } else {
                (self.sum_ms / u64::from(self.successes)) as i32
            },
            jitter: self.min_ms.map_or(0, |min| self.max_ms.saturating_sub(min)) as i32,
            loss: ((attempts - self.successes) * 100)
                .checked_div(attempts)
                .unwrap_or_default() as i32,
            successes: self.successes as i32,
            detail: self.detail.into(),
            samples: std::rc::Rc::new(slint::VecModel::from(self.samples)).into(),
        }
    }
}

pub(super) fn bind(ui: &MainWindow) {
    let sampler = Arc::new(Mutex::new(Sampler::default()));
    let sampling = Arc::new(AtomicBool::new(false));
    let weak = ui.as_weak();
    ui.on_monitor_sample(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_current_page() != "monitor"
            || !ui.get_window_visible()
            || sampling.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let selected = ui.get_monitor_interface().to_string();
        let sampler = sampler.clone();
        let sampling = sampling.clone();
        let weak = weak.clone();
        tokio::spawn(async move {
            let result =
                tokio::task::spawn_blocking(move || sampler.lock().unwrap().sample(selected)).await;
            if let Ok(s) = result {
                let sampled_at = clock();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if !ui.get_monitor_interface().is_empty()
                        && ui.get_monitor_interface() != s.selected
                        && s.names
                            .iter()
                            .any(|n| n == ui.get_monitor_interface().as_str())
                    {
                        return;
                    }
                    ui.set_monitor_interfaces(
                        std::rc::Rc::new(slint::VecModel::from(
                            s.names
                                .into_iter()
                                .map(Into::into)
                                .collect::<Vec<slint::SharedString>>(),
                        ))
                        .into(),
                    );
                    ui.set_monitor_interface(s.selected.into());
                    ui.set_monitor_ip(s.ip.into());
                    ui.set_monitor_rx(s.rx.into());
                    ui.set_monitor_tx(s.tx.into());
                    ui.set_monitor_rx_total(s.rx_total.into());
                    ui.set_monitor_tx_total(s.tx_total.into());
                    ui.set_monitor_interface_errors(s.errors.min(i32::MAX as u64) as i32);
                    ui.set_monitor_scale(
                        s.history
                            .iter()
                            .fold(1.0_f32, |scale, &(_, rx, tx)| scale.max(rx).max(tx))
                            .ceil(),
                    );
                    let points = s
                        .history
                        .into_iter()
                        .map(|(age, rx, tx)| TrafficSample { rx, tx, age })
                        .collect::<Vec<_>>();
                    ui.set_monitor_history(std::rc::Rc::new(slint::VecModel::from(points)).into());
                    ui.set_monitor_sampled_at(sampled_at.into());
                });
            }
            sampling.store(false, Ordering::Release);
        });
    });
    let weak = ui.as_weak();
    ui.on_monitor_select_interface(move |name| {
        if let Some(ui) = weak.upgrade() {
            ui.set_monitor_interface(name);
            ui.set_monitor_rx("—".into());
            ui.set_monitor_tx("—".into());
            ui.set_monitor_rx_total("—".into());
            ui.set_monitor_tx_total("—".into());
            ui.set_monitor_ip("".into());
            ui.set_monitor_history(Default::default());
            ui.invoke_monitor_sample();
        }
    });

    let rounds = Arc::new(Mutex::new(ProbeHistory::default()));
    let weak = ui.as_weak();
    ui.on_monitor_probe(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_monitor_checking() {
            return;
        }
        ui.set_monitor_checking(true);
        let context = ui.get_monitor_context();
        let rounds = rounds.clone();
        let weak = weak.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::limited(3))
                .user_agent(concat!("zapret-ui/", env!("CARGO_PKG_VERSION")))
                .build();
            let results = futures_util::future::join_all(ENDPOINTS.map(|(name, host, url)| {
                let client = &client;
                async move {
                    let mut result = ProbeResult::default();
                    for index in 0..PROBES {
                        if index > 0 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        let start = Instant::now();
                        let sample = match client {
                            Ok(client) => match client.get(url).send().await {
                                Ok(response) => {
                                    // An authenticated HTTP error still proves
                                    // reachability; show the code separately.
                                    result.http_response(response.status().as_u16());
                                    Ok(start.elapsed().as_millis().min(i32::MAX as u128) as u32)
                                }
                                Err(error) if error.is_timeout() => Err("monitor.timeout".into()),
                                Err(_) => Err("monitor.connect_error".into()),
                            },
                            Err(_) => Err("monitor.connect_error".into()),
                        };
                        result.push(sample);
                    }
                    (name, host, result)
                }
            }))
            .await;
            let checked_at = clock();
            let total = ENDPOINTS.len() as i32 * PROBES as i32;
            let successful = results
                .iter()
                .map(|(_, _, r)| r.successes as i32)
                .sum::<i32>();
            let sum_ms = results.iter().map(|(_, _, r)| r.sum_ms).sum::<u64>();
            let latency = if successful > 0 {
                (sum_ms / successful as u64) as i32
            } else {
                0
            };
            let passed = results
                .iter()
                .filter(|(_, _, r)| r.successes == PROBES)
                .count() as i32;
            let history = {
                let mut rounds = rounds.lock().unwrap();
                if rounds.context != context.as_str() {
                    rounds.rounds.clear();
                    rounds.context = context.to_string();
                }
                if rounds.rounds.len() == 8 {
                    rounds.rounds.pop_front();
                }
                rounds
                    .rounds
                    .push_back((checked_at.clone(), successful, total, latency));
                rounds.rounds.iter().rev().cloned().collect::<Vec<_>>()
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let targets = results
                    .into_iter()
                    .map(|(name, host, result)| result.target(name, host))
                    .collect::<Vec<_>>();
                ui.set_monitor_targets(std::rc::Rc::new(slint::VecModel::from(targets)).into());
                ui.set_monitor_passed(passed);
                ui.set_monitor_latency(latency);
                ui.set_monitor_loss((total - successful) * 100 / total);
                let history = history
                    .into_iter()
                    .map(|(time, passed, total, latency)| MonitorRound {
                        time: time.into(),
                        passed,
                        total,
                        latency,
                    })
                    .collect::<Vec<_>>();
                ui.set_monitor_rounds(std::rc::Rc::new(slint::VecModel::from(history)).into());
                ui.set_monitor_checked_at(checked_at.into());
                ui.set_monitor_checked_context(context);
                ui.set_monitor_checking(false);
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_rates_use_elapsed_time_and_reject_resets_and_hidden_gaps() {
        let start = Instant::now();
        assert_eq!(
            rates(
                Some((start, 1000, 2000)),
                start + Duration::from_secs(2),
                2_001_000,
                502_000
            ),
            Some((8.0, 2.0))
        );
        assert!(rates(
            Some((start, 1000, 2000)),
            start + Duration::from_secs(2),
            10,
            20
        )
        .is_none());
        assert!(rates(
            Some((start, 0, 0)),
            start + Duration::from_secs(20),
            100,
            200
        )
        .is_none());
        assert!(rates(None, start, 100, 200).is_none());
    }
    #[test]
    fn https_failures_do_not_become_zero_latency_or_packet_loss() {
        let mut result = ProbeResult::default();
        result.push(Ok(40));
        result.push(Err("monitor.timeout".into()));
        result.push(Ok(80));
        let target = result.target("Test", "localhost");
        assert_eq!(target.state, "partial");
        assert_eq!(target.latency, 60);
        assert_eq!(target.jitter, 40);
        assert_eq!(target.loss, 33);
        assert_eq!(target.successes, 2);
        assert_eq!(target.detail, "monitor.timeout");
    }

    #[test]
    fn authenticated_http_error_is_reachable_and_keeps_the_status_code() {
        let mut result = ProbeResult::default();
        for _ in 0..3 {
            result.http_response(403);
            result.push(Ok(30));
        }
        let target = result.target("Test", "localhost");
        assert_eq!(target.state, "ok");
        assert_eq!(target.loss, 0);
        assert_eq!(target.latency, 30);
        assert_eq!(target.detail, "HTTP 403");
    }
}
