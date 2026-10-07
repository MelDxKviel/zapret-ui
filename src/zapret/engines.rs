//! Windows engine lifecycle coordination. One lock protects both process and SCM
//! actions: an interceptor is never started until every previous one has stopped.
use crate::contracts::{EngineKind, RunningMode, RuntimeStatus, Strategy};
use crate::ports::{Runner, ServiceCtl};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct EngineRuntime {
    pub runner: Arc<dyn Runner>,
    pub service: Arc<dyn ServiceCtl>,
}

impl EngineRuntime {
    pub fn new(zapret1_dir: PathBuf, zapret2_dir: PathBuf) -> Self {
        use super::{process::ProcessRunner, service::WindowsServiceCtl};
        let zapret1 = Arc::new(ProcessRunner::new(zapret1_dir.clone()));
        let zapret2 = Arc::new(ProcessRunner::new_for_engine(
            zapret2_dir.clone(),
            EngineKind::Zapret2,
        ));
        let service1 = Arc::new(
            WindowsServiceCtl::new(zapret1_dir.clone()).with_owned_dir(zapret2_dir.clone()),
        );
        let service2 = Arc::new(
            WindowsServiceCtl::new_for_engine(zapret2_dir, EngineKind::Zapret2)
                .with_owned_dir(zapret1_dir),
        );
        Self::from_adapters(zapret1, zapret2, service1, service2)
    }

    /// Adapters share the same owned SCM slot. Keeping this constructor generic
    /// allows lifecycle ordering/failure tests without touching Windows services.
    pub fn from_adapters(
        zapret1: Arc<dyn Runner>,
        zapret2: Arc<dyn Runner>,
        service1: Arc<dyn ServiceCtl>,
        service2: Arc<dyn ServiceCtl>,
    ) -> Self {
        let shared = Arc::new(Shared {
            runners: [zapret1, zapret2],
            services: [service1, service2],
            action: Mutex::new(()),
            last_started: Mutex::new([None, None]),
        });
        Self {
            runner: Arc::new(MultiEngineRunner {
                shared: shared.clone(),
            }),
            service: Arc::new(MultiEngineServiceCtl { shared }),
        }
    }
}

fn index(engine: EngineKind) -> usize {
    match engine {
        EngineKind::Zapret1 => 0,
        EngineKind::Zapret2 => 1,
    }
}

struct Shared {
    runners: [Arc<dyn Runner>; 2],
    services: [Arc<dyn ServiceCtl>; 2],
    action: Mutex<()>,
    last_started: Mutex<[Option<Strategy>; 2]>,
}

impl Shared {
    async fn stop_processes(&self) -> anyhow::Result<()> {
        for runner in &self.runners {
            runner.stop().await?;
        }
        for runner in &self.runners {
            let status = runner.detect_running().await;
            anyhow::ensure!(
                status.running_mode != RunningMode::UserProcess,
                "Previous engine is still running; refusing to start another interceptor"
            );
        }
        Ok(())
    }

    async fn stop_all(&self) -> anyhow::Result<()> {
        // Either service adapter can inspect the one shared SCM slot and both
        // protected directories. Queries fail closed; an SCM error is not Stop.
        let service = &self.services[0];
        if service.status().await? != RunningMode::None {
            service.stop().await?;
            anyhow::ensure!(
                service.status().await? == RunningMode::None,
                "Previous service is still running; refusing to start another interceptor"
            );
        }
        self.stop_processes().await?;
        for runner in &self.runners {
            anyhow::ensure!(
                runner.detect_running().await.running_mode == RunningMode::None,
                "Previous engine has not stopped; refusing to start another interceptor"
            );
        }
        Ok(())
    }
}

pub struct MultiEngineRunner {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl Runner for MultiEngineRunner {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
        let _guard = self.shared.action.lock().await;
        let previous_service = self.shared.services[0].status().await? != RunningMode::None;
        let mut previous_strategy = None;
        for (i, runner) in self.shared.runners.iter().enumerate() {
            let status = runner.detect_running().await;
            if status.running_mode == RunningMode::UserProcess {
                let remembered = self.shared.last_started.lock().await[i].clone();
                previous_strategy = remembered.filter(|previous| {
                    status.active_strategy.as_deref() == Some(previous.id.as_str())
                });
                break;
            }
        }
        self.shared.stop_all().await?;
        match self.shared.runners[index(strategy.engine)]
            .start(strategy)
            .await
        {
            Ok(pid) => {
                self.shared.last_started.lock().await[index(strategy.engine)] =
                    Some(strategy.clone());
                Ok(pid)
            }
            Err(start_error) => {
                // A manual switch should not discard a working bypass merely
                // because the new binary/arguments failed. Explicit Stop clears
                // this fallback, so tester candidates are still independent.
                self.shared.stop_all().await.map_err(|cleanup| {
                    anyhow::anyhow!(
                        "{start_error:#}; failed to stop rejected candidate: {cleanup:#}"
                    )
                })?;
                let restored = if previous_service {
                    self.shared.services[0].start().await
                } else if let Some(previous) = previous_strategy {
                    self.shared.runners[index(previous.engine)]
                        .start(&previous)
                        .await
                        .map(|_| ())
                } else {
                    Ok(())
                };
                if let Err(restore_error) = restored {
                    anyhow::bail!(
                        "{start_error:#}; previous engine could not be restored: {restore_error:#}"
                    );
                }
                Err(start_error)
            }
        }
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        self.shared.stop_all().await?;
        *self.shared.last_started.lock().await = [None, None];
        Ok(())
    }

    async fn detect_running(&self) -> RuntimeStatus {
        let _guard = self.shared.action.lock().await;
        let first = self.shared.runners[0].detect_running().await;
        let second = self.shared.runners[1].detect_running().await;
        if first.running_mode != RunningMode::None {
            first
        } else if second.running_mode != RunningMode::None {
            second
        } else {
            RuntimeStatus {
                installed: first.installed || second.installed,
                ..first
            }
        }
    }
}

struct MultiEngineServiceCtl {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl ServiceCtl for MultiEngineServiceCtl {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        self.shared.stop_all().await?;
        self.shared.services[index(strategy.engine)]
            .install(strategy)
            .await
    }

    async fn install_protected(&self, strategy: &Strategy) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        // The service adapter snapshots the existing registration/running state
        // before stopping it, so failed replacement can restore that state.
        self.shared.stop_processes().await?;
        self.shared.services[index(strategy.engine)]
            .install_protected(strategy)
            .await
    }

    async fn start(&self) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        self.shared.stop_processes().await?;
        // Start the already registered image; never choose an engine implicitly.
        self.shared.services[0].start().await
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        self.shared.services[0].stop().await
    }

    async fn remove(&self) -> anyhow::Result<()> {
        let _guard = self.shared.action.lock().await;
        self.shared.services[0].remove().await
    }

    async fn status(&self) -> anyhow::Result<RunningMode> {
        let _guard = self.shared.action.lock().await;
        self.shared.services[0].status().await
    }

    async fn is_installed(&self) -> bool {
        let _guard = self.shared.action.lock().await;
        self.shared.services[0].is_installed().await
    }

    async fn installed_engine(&self) -> Option<EngineKind> {
        let _guard = self.shared.action.lock().await;
        self.shared.services[0].installed_engine().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::Category;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    struct MockRunner {
        engine: EngineKind,
        running: AtomicBool,
        fail_stop: AtomicBool,
        refuse_stop: AtomicBool,
        fail_start: AtomicBool,
        active_id: StdMutex<Option<String>>,
        events: Arc<StdMutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Runner for MockRunner {
        async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
            assert_eq!(strategy.engine, self.engine);
            self.events
                .lock()
                .unwrap()
                .push(format!("start:{:?}", self.engine));
            anyhow::ensure!(!self.fail_start.load(Ordering::SeqCst), "spawn failed");
            self.running.store(true, Ordering::SeqCst);
            *self.active_id.lock().unwrap() = Some(strategy.id.clone());
            Ok(42)
        }
        async fn stop(&self) -> anyhow::Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("stop:{:?}", self.engine));
            anyhow::ensure!(!self.fail_stop.load(Ordering::SeqCst), "stop failed");
            if !self.refuse_stop.load(Ordering::SeqCst) {
                self.running.store(false, Ordering::SeqCst);
                *self.active_id.lock().unwrap() = None;
            }
            Ok(())
        }
        async fn detect_running(&self) -> RuntimeStatus {
            let running = self.running.load(Ordering::SeqCst);
            RuntimeStatus {
                running_mode: if running {
                    RunningMode::UserProcess
                } else {
                    RunningMode::None
                },
                active_engine: running.then_some(self.engine),
                active_strategy: self.active_id.lock().unwrap().clone(),
                installed: true,
                ..RuntimeStatus::default()
            }
        }
    }

    struct MockService {
        running: AtomicBool,
        fail_stop: AtomicBool,
        events: Arc<StdMutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl ServiceCtl for MockService {
        async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
            self.events
                .lock()
                .unwrap()
                .push(format!("install:{:?}", strategy.engine));
            Ok(())
        }
        async fn remove(&self) -> anyhow::Result<()> {
            self.stop().await
        }
        async fn start(&self) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("service:start".into());
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self) -> anyhow::Result<()> {
            self.events.lock().unwrap().push("service:stop".into());
            anyhow::ensure!(
                !self.fail_stop.load(Ordering::SeqCst),
                "service stop failed"
            );
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }
        async fn status(&self) -> anyhow::Result<RunningMode> {
            Ok(if self.running.load(Ordering::SeqCst) {
                RunningMode::SystemService
            } else {
                RunningMode::None
            })
        }
        async fn is_installed(&self) -> bool {
            true
        }
    }

    fn strategy(engine: EngineKind) -> Strategy {
        Strategy {
            engine,
            id: format!("{engine:?}:test"),
            display_name: "test".into(),
            category: Category::Other,
            description: String::new(),
            winws_args: vec![],
            requires_lists: vec![],
        }
    }

    struct Fixture {
        runtime: EngineRuntime,
        first: Arc<MockRunner>,
        second: Arc<MockRunner>,
        service: Arc<MockService>,
        events: Arc<StdMutex<Vec<String>>>,
    }
    fn fixture() -> Fixture {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let make = |engine| {
            Arc::new(MockRunner {
                engine,
                running: AtomicBool::new(false),
                fail_stop: AtomicBool::new(false),
                refuse_stop: AtomicBool::new(false),
                fail_start: AtomicBool::new(false),
                active_id: StdMutex::new(None),
                events: events.clone(),
            })
        };
        let first = make(EngineKind::Zapret1);
        let second = make(EngineKind::Zapret2);
        let service = Arc::new(MockService {
            running: AtomicBool::new(false),
            fail_stop: AtomicBool::new(false),
            events: events.clone(),
        });
        let runtime = EngineRuntime::from_adapters(
            first.clone(),
            second.clone(),
            service.clone(),
            service.clone(),
        );
        Fixture {
            runtime,
            first,
            second,
            service,
            events,
        }
    }

    #[tokio::test]
    async fn switch_stops_and_confirms_both_engines_before_starting_target() {
        let f = fixture();
        f.first.running.store(true, Ordering::SeqCst);
        f.runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .unwrap();
        assert_eq!(
            *f.events.lock().unwrap(),
            ["stop:Zapret1", "stop:Zapret2", "start:Zapret2"]
        );
        assert!(!f.first.running.load(Ordering::SeqCst));
        assert_eq!(
            f.runtime.runner.detect_running().await.active_engine,
            Some(EngineKind::Zapret2)
        );
        f.runtime
            .runner
            .start(&strategy(EngineKind::Zapret1))
            .await
            .unwrap();
        assert!(!f.second.running.load(Ordering::SeqCst));
        assert_eq!(
            f.runtime.runner.detect_running().await.active_engine,
            Some(EngineKind::Zapret1)
        );
    }

    #[tokio::test]
    async fn switch_aborts_when_stop_fails_or_does_not_actually_stop() {
        for refuse in [false, true] {
            let f = fixture();
            f.first.running.store(true, Ordering::SeqCst);
            f.first.fail_stop.store(!refuse, Ordering::SeqCst);
            f.first.refuse_stop.store(refuse, Ordering::SeqCst);
            assert!(f
                .runtime
                .runner
                .start(&strategy(EngineKind::Zapret2))
                .await
                .is_err());
            assert!(!f.second.running.load(Ordering::SeqCst));
            assert!(!f
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("start:")));
        }
    }

    #[tokio::test]
    async fn service_is_stopped_before_a_process_start_and_errors_abort_switch() {
        let f = fixture();
        f.service.running.store(true, Ordering::SeqCst);
        f.service.fail_stop.store(true, Ordering::SeqCst);
        assert!(f
            .runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .is_err());
        assert_eq!(*f.events.lock().unwrap(), ["service:stop"]);
        f.service.fail_stop.store(false, Ordering::SeqCst);
        f.events.lock().unwrap().clear();
        f.runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .unwrap();
        assert_eq!(
            *f.events.lock().unwrap(),
            [
                "service:stop",
                "stop:Zapret1",
                "stop:Zapret2",
                "start:Zapret2"
            ]
        );
    }

    #[tokio::test]
    async fn failed_spawn_leaves_no_candidate_reported_running() {
        let f = fixture();
        f.first.running.store(true, Ordering::SeqCst);
        f.second.fail_start.store(true, Ordering::SeqCst);
        assert!(f
            .runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .is_err());
        assert_eq!(
            f.runtime.runner.detect_running().await.running_mode,
            RunningMode::None
        );
    }

    #[tokio::test]
    async fn failed_manual_switch_restores_previous_process_but_explicit_stop_clears_it() {
        let f = fixture();
        f.runtime
            .runner
            .start(&strategy(EngineKind::Zapret1))
            .await
            .unwrap();
        f.second.fail_start.store(true, Ordering::SeqCst);
        assert!(f
            .runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .is_err());
        assert_eq!(
            f.runtime.runner.detect_running().await.active_engine,
            Some(EngineKind::Zapret1)
        );
        assert!(!f.second.running.load(Ordering::SeqCst));
        f.runtime.runner.stop().await.unwrap();
        assert!(f
            .runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .is_err());
        assert_eq!(
            f.runtime.runner.detect_running().await.running_mode,
            RunningMode::None
        );
    }

    #[tokio::test]
    async fn failed_process_switch_restores_previously_running_service() {
        let f = fixture();
        f.service.running.store(true, Ordering::SeqCst);
        f.second.fail_start.store(true, Ordering::SeqCst);
        assert!(f
            .runtime
            .runner
            .start(&strategy(EngineKind::Zapret2))
            .await
            .is_err());
        assert!(f.service.running.load(Ordering::SeqCst));
        assert!(!f.first.running.load(Ordering::SeqCst));
        assert!(!f.second.running.load(Ordering::SeqCst));
        assert_eq!(
            f.events.lock().unwrap().last().map(String::as_str),
            Some("service:start")
        );
    }

    #[tokio::test]
    async fn service_install_and_start_stop_user_processes_first() {
        let f = fixture();
        f.second.running.store(true, Ordering::SeqCst);
        f.runtime
            .service
            .install(&strategy(EngineKind::Zapret2))
            .await
            .unwrap();
        assert_eq!(
            *f.events.lock().unwrap(),
            ["stop:Zapret1", "stop:Zapret2", "install:Zapret2"]
        );
        f.events.lock().unwrap().clear();
        f.runtime.service.start().await.unwrap();
        assert_eq!(
            *f.events.lock().unwrap(),
            ["stop:Zapret1", "stop:Zapret2", "service:start"]
        );
    }

    #[tokio::test]
    async fn protected_install_leaves_prior_service_running_until_adapter_snapshots_it() {
        let f = fixture();
        f.service.running.store(true, Ordering::SeqCst);
        f.runtime
            .service
            .install_protected(&strategy(EngineKind::Zapret2))
            .await
            .unwrap();
        assert!(f.service.running.load(Ordering::SeqCst));
        assert_eq!(
            *f.events.lock().unwrap(),
            ["stop:Zapret1", "stop:Zapret2", "install:Zapret2"]
        );
    }
}
