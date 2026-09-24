//! Telegram commands run independently of core downloads/tests. Tasks exist only
//! for a user action; a mutex serializes start/stop/settings without a poll loop.
use super::MainWindow;
use crate::{
    config::AppConfig,
    contracts::{TelegramProxySettings, UiEvent},
    ports::TelegramProxy,
};
use slint::ComponentHandle;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};

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
struct Controller {
    proxy: Arc<dyn TelegramProxy>,
    config: Arc<RwLock<AppConfig>>,
    events: broadcast::Sender<UiEvent>,
    operation: Arc<Mutex<()>>,
}

enum Action {
    Start,
    Stop,
    Save(TelegramProxySettings),
    Visible(bool),
}

impl Controller {
    fn dispatch(&self, action: Action) {
        let this = self.clone();
        tokio::spawn(async move {
            let _operation = this.operation.lock().await;
            let visibility_action = matches!(&action, Action::Visible(_));
            if let Err(error) = this.handle(action).await {
                tracing::warn!("Telegram operation: {error:#}");
                if visibility_action {
                    // This action lives on Settings, where the proxy page is
                    // hidden. Use the app's visible error surface on save failure.
                    let language = this.config.read().await.language;
                    let _ = this.events.send(UiEvent::Error(crate::i18n::tr(
                        crate::i18n::code(language),
                        &error.to_string(),
                    )));
                } else {
                    let _ = this.events.send(UiEvent::TelegramError(error.to_string()));
                }
            }
        });
    }

    async fn persist(&self, settings: TelegramProxySettings) -> anyhow::Result<()> {
        let mut config = self.config.write().await;
        let mut updated = config.clone();
        updated.telegram_proxy = settings;
        updated
            .save()
            .map_err(|_| anyhow::anyhow!("telegram.error_save"))?;
        *config = updated;
        Ok(())
    }

    async fn handle(&self, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Start => {
                let settings = {
                    let config = self.config.read().await;
                    anyhow::ensure!(config.show_telegram_proxy, "telegram.error_hidden");
                    self.proxy.prepare_settings(config.telegram_proxy.clone())?
                };
                // Save the generated secret before binding. A failed save must
                // never leave a running proxy whose client link will be lost.
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
                if updated.save().is_err() {
                    let _ = self
                        .events
                        .send(UiEvent::TelegramVisibility(config.show_telegram_proxy));
                    return Err(anyhow::anyhow!("telegram.error_save"));
                }
                *config = updated;
                drop(config);
                if !visible {
                    self.proxy.stop().await?;
                    let _ = self
                        .events
                        .send(UiEvent::TelegramStatus(Default::default()));
                }
                let _ = self.events.send(UiEvent::TelegramVisibility(visible));
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
) {
    if let Ok(config) = config.try_read() {
        ui.set_show_telegram_proxy(config.show_telegram_proxy);
        apply_settings(ui, &config.telegram_proxy);
    }
    let controller = Controller {
        proxy,
        config,
        events,
        operation: Arc::new(Mutex::new(())),
    };
    let c = controller.clone();
    ui.on_telegram_start(move || c.dispatch(Action::Start));
    let c = controller.clone();
    ui.on_telegram_stop(move || c.dispatch(Action::Stop));
    let c = controller.clone();
    ui.on_set_telegram_visible(move |visible| c.dispatch(Action::Visible(visible)));
    let weak = ui.as_weak();
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
            Ok(settings) => controller.dispatch(Action::Save(settings)),
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
}
