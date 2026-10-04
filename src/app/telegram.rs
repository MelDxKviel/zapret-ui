//! Telegram commands run independently of core downloads/tests. Tasks exist only
//! for a queued action; a mutex serializes start/stop/settings without a poll loop.
use super::MainWindow;
use crate::{
    config::AppConfig,
    contracts::{TelegramProxySettings, UiEvent},
    ports::TelegramProxy,
};
use slint::ComponentHandle;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, RwLock};

pub(super) fn apply_settings(ui: &MainWindow, settings: &TelegramProxySettings) {
    ui.set_telegram_port(settings.port.to_string().into());
    ui.set_telegram_secret(settings.secret.as_str().into());
    ui.set_telegram_dc_overrides(settings.dc_overrides.as_str().into());
    ui.set_telegram_fallback(settings.tcp_fallback);
    ui.set_telegram_timeout(settings.connect_timeout_secs.to_string().into());
    ui.set_telegram_limit(settings.max_connections.to_string().into());
    ui.set_telegram_link(settings.link().into());
}

#[derive(Clone)]
pub(super) struct Controller {
    proxy: Arc<dyn TelegramProxy>,
    config: Arc<RwLock<AppConfig>>,
    events: broadcast::Sender<UiEvent>,
    pending: Arc<Mutex<PendingActions>>,
    save_config: ConfigSave,
}

type ConfigSave = Arc<dyn Fn(&AppConfig) -> anyhow::Result<()> + Send + Sync>;

enum Action {
    Start,
    Startup,
    Stop,
    Save(TelegramProxySettings),
    Visible(bool),
    Autostart(bool),
}

#[derive(Default)]
struct PendingActions {
    actions: VecDeque<Action>,
    worker_running: bool,
}

impl Controller {
    pub(super) fn start_on_launch(&self) {
        // Default startup creates no Telegram task. If another settings action
        // owns the config, let the queued action check its final state instead.
        if let Ok(config) = self.config.try_read() {
            if !config.show_telegram_proxy || !config.telegram_autostart {
                return;
            }
        }
        self.dispatch(Action::Startup);
    }

    fn dispatch(&self, action: Action) {
        // Enqueue synchronously on the UI thread. Locking inside independent
        // spawned tasks only serializes execution: the multithread runtime can
        // acquire those locks in a different order from the user's clicks.
        let mut pending = self.pending.lock().unwrap();
        pending.actions.push_back(action);
        if pending.worker_running {
            return;
        }
        pending.worker_running = true;
        drop(pending);
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let action = {
                    let mut pending = this.pending.lock().unwrap();
                    match pending.actions.pop_front() {
                        Some(action) => action,
                        None => {
                            pending.worker_running = false;
                            break;
                        }
                    }
                };
                this.process(action).await;
            }
        });
    }

    async fn process(&self, action: Action) {
        let settings_action = matches!(&action, Action::Visible(_) | Action::Autostart(_));
        if let Err(error) = self.handle(action).await {
            tracing::warn!("Telegram operation: {error:#}");
            if settings_action {
                // Restore optimistic switches from the saved configuration.
                let config = self.config.read().await;
                self.publish_preferences(&config);
            }
            // Telegram failures must not reset the core's busy state. The UI
            // listener decides where to display the localizable error key.
            let _ = self.events.send(UiEvent::TelegramError(error.to_string()));
        }
    }

    fn publish_preferences(&self, config: &AppConfig) {
        let _ = self
            .events
            .send(UiEvent::TelegramVisibility(config.show_telegram_proxy));
        let _ = self
            .events
            .send(UiEvent::TelegramAutostart(config.telegram_autostart));
    }

    fn save(&self, config: &AppConfig) -> anyhow::Result<()> {
        (self.save_config)(config).map_err(|error| {
            tracing::warn!("Telegram config save: {error:#}");
            anyhow::anyhow!("telegram.error_save")
        })
    }

    async fn persist(&self, settings: TelegramProxySettings) -> anyhow::Result<()> {
        let mut config = self.config.write().await;
        let mut updated = config.clone();
        updated.telegram_proxy = settings;
        self.save(&updated)?;
        *config = updated;
        Ok(())
    }

    async fn start(&self) -> anyhow::Result<()> {
        let settings = {
            let config = self.config.read().await;
            anyhow::ensure!(config.show_telegram_proxy, "telegram.error_hidden");
            self.proxy.prepare_settings(config.telegram_proxy.clone())?
        };
        // Save the generated secret before binding. A failed save must never
        // leave a running proxy whose client link will be lost.
        self.persist(settings.clone()).await?;
        let events = self.events.clone();
        self.proxy
            .start(
                settings.clone(),
                Arc::new(move |status| {
                    let _ = events.send(UiEvent::TelegramStatus(status));
                }),
            )
            .await?;
        let _ = self.events.send(UiEvent::TelegramSettings(settings));
        Ok(())
    }

    async fn handle(&self, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Start => self.start().await?,
            Action::Startup => {
                let enabled = {
                    let config = self.config.read().await;
                    config.show_telegram_proxy && config.telegram_autostart
                };
                if enabled {
                    self.start().await?;
                }
            }
            Action::Stop => {
                self.proxy.stop().await?;
                let _ = self
                    .events
                    .send(UiEvent::TelegramStatus(Default::default()));
                let settings = self.config.read().await.telegram_proxy.clone();
                let _ = self.events.send(UiEvent::TelegramSettings(settings));
            }
            Action::Save(settings) => {
                anyhow::ensure!(!self.proxy.is_running().await, "telegram.error_running");
                let settings = self.proxy.prepare_settings(settings)?;
                self.persist(settings.clone()).await?;
                let _ = self.events.send(UiEvent::TelegramSettings(settings));
            }
            Action::Visible(visible) => {
                let mut config = self.config.write().await;
                let mut updated = config.clone();
                updated.show_telegram_proxy = visible;
                if !visible {
                    updated.telegram_autostart = false;
                }
                self.save(&updated)?;
                *config = updated;
                self.publish_preferences(&config);
                drop(config);
                if !visible {
                    self.proxy.stop().await?;
                    let _ = self
                        .events
                        .send(UiEvent::TelegramStatus(Default::default()));
                }
            }
            Action::Autostart(enabled) => {
                let mut config = self.config.write().await;
                anyhow::ensure!(
                    !enabled || config.show_telegram_proxy,
                    "telegram.error_hidden"
                );
                let mut updated = config.clone();
                updated.telegram_autostart = enabled;
                if enabled {
                    updated.telegram_proxy = self.proxy.prepare_settings(updated.telegram_proxy)?;
                }
                self.save(&updated)?;
                *config = updated;
                self.publish_preferences(&config);
                let _ = self
                    .events
                    .send(UiEvent::TelegramSettings(config.telegram_proxy.clone()));
            }
        }
        Ok(())
    }
}

pub(super) fn bind(
    ui: &MainWindow,
    proxy: Arc<dyn TelegramProxy>,
    config: Arc<RwLock<AppConfig>>,
    events: broadcast::Sender<UiEvent>,
) -> Controller {
    if let Ok(config) = config.try_read() {
        ui.set_show_telegram_proxy(config.show_telegram_proxy);
        ui.set_telegram_autostart(config.telegram_autostart);
        apply_settings(ui, &config.telegram_proxy);
    }
    let controller = Controller {
        proxy,
        config,
        events,
        pending: Arc::new(Mutex::new(PendingActions::default())),
        save_config: Arc::new(AppConfig::save),
    };
    let c = controller.clone();
    ui.on_telegram_start(move || c.dispatch(Action::Start));
    let c = controller.clone();
    ui.on_telegram_stop(move || c.dispatch(Action::Stop));
    let c = controller.clone();
    ui.on_set_telegram_visible(move |visible| c.dispatch(Action::Visible(visible)));
    let c = controller.clone();
    ui.on_set_telegram_autostart(move |enabled| c.dispatch(Action::Autostart(enabled)));
    let weak = ui.as_weak();
    let save_controller = controller.clone();
    ui.on_telegram_save(move |port, secret, dc, fallback, timeout, limit| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let parse = || -> anyhow::Result<TelegramProxySettings> {
            Ok(TelegramProxySettings {
                port: port
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("telegram.error_port"))?,
                secret: secret.to_string(),
                dc_overrides: dc.to_string(),
                tcp_fallback: fallback,
                connect_timeout_secs: timeout
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("telegram.error_timeout"))?,
                max_connections: limit
                    .trim()
                    .parse()
                    .map_err(|_| anyhow::anyhow!("telegram.error_limit"))?,
            })
        };
        match parse() {
            Ok(settings) => save_controller.dispatch(Action::Save(settings)),
            Err(error) => {
                ui.set_telegram_error(error.to_string().into());
                ui.set_telegram_busy(false);
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_telegram_connect(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_telegram_running()
                && !ui.get_telegram_link().is_empty()
                && !super::winexec::try_open_external(&ui.get_telegram_link())
            {
                ui.set_telegram_error("telegram.error_client".into());
            }
        }
    });
    controller
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::TelegramStatusCb;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::{
        sync::{mpsc, Notify},
        time::{timeout, Duration},
    };

    struct RejectingProxy {
        prepared: mpsc::UnboundedSender<u16>,
        first_action_entered: Notify,
        release_first_action: Notify,
        calls: AtomicUsize,
    }

    #[derive(Default)]
    struct RecordingProxy {
        running: AtomicBool,
        starts: AtomicUsize,
        stops: AtomicUsize,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl TelegramProxy for RecordingProxy {
        fn prepare_settings(
            &self,
            settings: TelegramProxySettings,
        ) -> anyhow::Result<TelegramProxySettings> {
            self.order.lock().unwrap().push("prepare");
            crate::telegram::LocalTelegramProxy::default().prepare_settings(settings)
        }

        async fn start(
            &self,
            settings: TelegramProxySettings,
            on_status: TelegramStatusCb,
        ) -> anyhow::Result<()> {
            assert!(!settings.link().is_empty());
            self.order.lock().unwrap().push("start");
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.running.store(true, Ordering::SeqCst);
            on_status(crate::contracts::TelegramProxyStatus {
                running: true,
                ..Default::default()
            });
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            self.order.lock().unwrap().push("stop");
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }

        async fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }
    }

    struct Harness {
        controller: Controller,
        proxy: Arc<RecordingProxy>,
        events: broadcast::Receiver<UiEvent>,
        saves: Arc<Mutex<Vec<AppConfig>>>,
        fail_save: Arc<AtomicBool>,
    }

    impl Harness {
        fn new(config: AppConfig) -> Self {
            let proxy = Arc::new(RecordingProxy::default());
            let (events, receiver) = broadcast::channel(256);
            let saves = Arc::new(Mutex::new(Vec::new()));
            let fail_save = Arc::new(AtomicBool::new(false));
            let saved = saves.clone();
            let save_failure = fail_save.clone();
            let order = proxy.order.clone();
            let save_config: ConfigSave = Arc::new(move |config| {
                order.lock().unwrap().push("save");
                anyhow::ensure!(!save_failure.load(Ordering::SeqCst), "test save failure");
                saved.lock().unwrap().push(config.clone());
                Ok(())
            });
            let controller = Controller {
                proxy: proxy.clone(),
                config: Arc::new(RwLock::new(config)),
                events,
                pending: Arc::new(Mutex::new(PendingActions::default())),
                save_config,
            };
            Self {
                controller,
                proxy,
                events: receiver,
                saves,
                fail_save,
            }
        }

        fn assert_preferences_and_telegram_error(&mut self, visible: bool, enabled: bool) {
            assert!(matches!(
                self.events.try_recv().unwrap(),
                UiEvent::TelegramVisibility(value) if value == visible
            ));
            assert!(matches!(
                self.events.try_recv().unwrap(),
                UiEvent::TelegramAutostart(value) if value == enabled
            ));
            assert!(matches!(
                self.events.try_recv().unwrap(),
                UiEvent::TelegramError(_)
            ));
            assert!(self.events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn telegram_autostart_prepares_and_saves_before_app_launch_starts_proxy() {
        let mut h = Harness::new(AppConfig::default());
        h.controller.handle(Action::Autostart(true)).await.unwrap();
        let config = h.controller.config.read().await.clone();
        assert!(config.telegram_autostart);
        assert_eq!(config.telegram_proxy.secret.len(), 32);
        assert_eq!(
            h.saves.lock().unwrap().as_slice(),
            std::slice::from_ref(&config)
        );
        assert_eq!(
            h.proxy.order.lock().unwrap().as_slice(),
            &["prepare", "save"]
        );
        assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
        while h.events.try_recv().is_ok() {}

        h.controller.start_on_launch();
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(h.events.recv().await.unwrap(), UiEvent::TelegramSettings(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.controller.config.read().await.telegram_proxy.secret,
            config.telegram_proxy.secret
        );
        assert_eq!(
            h.proxy.order.lock().unwrap().as_slice(),
            &["prepare", "save", "prepare", "save", "start"]
        );
    }

    #[tokio::test]
    async fn telegram_startup_requires_its_own_opt_in_and_visible_entry() {
        for (visible, telegram_autostart) in [(true, false), (false, true), (false, false)] {
            let h = Harness::new(AppConfig {
                autostart: true,
                show_telegram_proxy: visible,
                telegram_autostart,
                ..Default::default()
            });
            h.controller.start_on_launch();
            assert!(!h.controller.pending.lock().unwrap().worker_running);
            h.controller.handle(Action::Startup).await.unwrap();
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
            assert!(h.saves.lock().unwrap().is_empty());
            assert!(h.proxy.order.lock().unwrap().is_empty());
        }
        let h = Harness::new(AppConfig::default());
        h.controller.handle(Action::Start).await.unwrap();
        assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 1);
        assert!(!h.controller.config.read().await.telegram_autostart);
    }

    #[tokio::test]
    async fn telegram_app_launch_defers_opt_in_check_while_config_is_locked() {
        let mut h = Harness::new(AppConfig::default());
        let mut config = h.controller.config.write().await;
        config.telegram_autostart = true;
        h.controller.start_on_launch();
        assert!(h.controller.pending.lock().unwrap().worker_running);
        drop(config);
        timeout(Duration::from_secs(2), async {
            loop {
                if matches!(h.events.recv().await.unwrap(), UiEvent::TelegramSettings(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn telegram_autostart_rejects_hidden_proxy_and_restores_switches() {
        let config = AppConfig {
            show_telegram_proxy: false,
            ..Default::default()
        };
        let mut h = Harness::new(config.clone());
        h.controller.process(Action::Autostart(true)).await;
        assert_eq!(*h.controller.config.read().await, config);
        assert!(h.saves.lock().unwrap().is_empty());
        h.assert_preferences_and_telegram_error(false, false);
    }

    #[tokio::test]
    async fn telegram_autostart_save_failure_restores_flags_and_preserves_app_autostart() {
        for autostart in [false, true] {
            let config = AppConfig {
                autostart,
                ..Default::default()
            };
            let mut h = Harness::new(config.clone());
            h.fail_save.store(true, Ordering::SeqCst);
            h.controller.process(Action::Autostart(true)).await;
            assert_eq!(*h.controller.config.read().await, config);
            assert!(h.saves.lock().unwrap().is_empty());
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
            h.assert_preferences_and_telegram_error(true, false);
        }
    }

    #[tokio::test]
    async fn telegram_autostart_is_independent_of_windows_startup_and_does_not_stop_active_proxy() {
        for autostart in [false, true] {
            let h = Harness::new(AppConfig {
                autostart,
                ..Default::default()
            });
            h.controller.handle(Action::Autostart(true)).await.unwrap();
            assert_eq!(h.controller.config.read().await.autostart, autostart);
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
            h.controller.handle(Action::Startup).await.unwrap();
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 1);
            h.controller.handle(Action::Autostart(false)).await.unwrap();
            let config = h.controller.config.read().await.clone();
            assert_eq!(config.autostart, autostart);
            assert!(!config.telegram_autostart);
            assert!(h.proxy.is_running().await);
            assert_eq!(h.proxy.stops.load(Ordering::SeqCst), 0);
            h.controller.handle(Action::Startup).await.unwrap();
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn hiding_telegram_disables_its_startup_stops_proxy_and_preserves_windows_startup() {
        for autostart in [false, true] {
            let h = Harness::new(AppConfig {
                autostart,
                telegram_autostart: true,
                ..Default::default()
            });
            h.proxy.running.store(true, Ordering::SeqCst);
            h.controller.handle(Action::Visible(false)).await.unwrap();
            let config = h.controller.config.read().await.clone();
            assert!(!config.show_telegram_proxy);
            assert!(!config.telegram_autostart);
            assert_eq!(config.autostart, autostart);
            assert!(!h.proxy.is_running().await);
            assert_eq!(h.proxy.stops.load(Ordering::SeqCst), 1);
            assert_eq!(h.proxy.order.lock().unwrap().as_slice(), &["save", "stop"]);
            h.controller.handle(Action::Visible(true)).await.unwrap();
            h.controller.handle(Action::Startup).await.unwrap();
            assert!(!h.controller.config.read().await.telegram_autostart);
            assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn hiding_telegram_save_failure_restores_startup_flags_without_stopping_proxy() {
        let config = AppConfig {
            telegram_autostart: true,
            ..Default::default()
        };
        let mut h = Harness::new(config.clone());
        h.proxy.running.store(true, Ordering::SeqCst);
        h.fail_save.store(true, Ordering::SeqCst);
        h.controller.process(Action::Visible(false)).await;
        assert_eq!(*h.controller.config.read().await, config);
        assert!(h.proxy.is_running().await);
        assert_eq!(h.proxy.stops.load(Ordering::SeqCst), 0);
        h.assert_preferences_and_telegram_error(true, true);
    }

    #[tokio::test]
    async fn telegram_startup_does_not_bind_when_settings_cannot_be_saved() {
        let mut h = Harness::new(AppConfig {
            telegram_autostart: true,
            ..Default::default()
        });
        h.fail_save.store(true, Ordering::SeqCst);
        h.controller.process(Action::Startup).await;
        assert_eq!(h.proxy.starts.load(Ordering::SeqCst), 0);
        assert!(h.saves.lock().unwrap().is_empty());
        assert!(matches!(
            h.events.try_recv().unwrap(),
            UiEvent::TelegramError(_)
        ));
    }

    #[async_trait::async_trait]
    impl TelegramProxy for RejectingProxy {
        fn prepare_settings(
            &self,
            settings: TelegramProxySettings,
        ) -> anyhow::Result<TelegramProxySettings> {
            self.prepared.send(settings.port).unwrap();
            // Exercise dispatch/validation without writing the user's config.
            Err(anyhow::anyhow!("telegram.error_secret"))
        }

        async fn start(&self, _: TelegramProxySettings, _: TelegramStatusCb) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn stop(&self) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn is_running(&self) -> bool {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.first_action_entered.notify_one();
                self.release_first_action.notified().await;
            }
            false
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dispatched_actions_keep_click_order_and_resume_after_errors_and_idle() {
        timeout(Duration::from_secs(5), async {
            let (prepared, mut observed) = mpsc::unbounded_channel();
            let proxy = Arc::new(RejectingProxy {
                prepared,
                first_action_entered: Notify::new(),
                release_first_action: Notify::new(),
                calls: AtomicUsize::new(0),
            });
            let (events, _) = broadcast::channel(256);
            let controller = Controller {
                proxy: proxy.clone(),
                config: Arc::new(RwLock::new(AppConfig::default())),
                events,
                pending: Arc::new(Mutex::new(PendingActions::default())),
                save_config: Arc::new(|_| unreachable!("rejected settings must not be saved")),
            };
            let save = |port| {
                controller.dispatch(Action::Save(TelegramProxySettings {
                    port,
                    ..Default::default()
                }));
            };
            save(1);
            proxy.first_action_entered.notified().await;
            for port in 2..=128 {
                save(port);
            }
            proxy.release_first_action.notify_one();
            for port in 1..=128 {
                assert_eq!(observed.recv().await.unwrap(), port);
            }
            // An empty queue must stop its worker, while a later click starts a
            // new one. Wait until the first burst has finished its last error.
            while controller.pending.lock().unwrap().worker_running {
                tokio::task::yield_now().await;
            }
            save(129);
            assert_eq!(observed.recv().await.unwrap(), 129);
        })
        .await
        .unwrap();
    }
}
