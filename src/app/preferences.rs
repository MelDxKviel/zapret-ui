//! Save UI preferences in click order without waiting behind core downloads.
//! A worker exists only while actions are pending; flush waits for all saves.
use crate::config::{AppConfig, Language, UiMode};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, RwLock};

pub(super) enum Action {
    Language(Language),
    UiMode(UiMode),
    SelectedStrategy(String),
}

type SaveConfig = Arc<dyn Fn(&AppConfig) -> anyhow::Result<()> + Send + Sync>;

#[derive(Clone)]
pub(super) struct Controller {
    config: Arc<RwLock<AppConfig>>,
    pending: Arc<Mutex<PendingActions>>,
    idle: Arc<Notify>,
    save: SaveConfig,
}

#[derive(Default)]
struct PendingActions {
    actions: VecDeque<Action>,
    worker_running: bool,
}

impl Controller {
    pub(super) fn new(config: Arc<RwLock<AppConfig>>) -> Self {
        Self::with_save(config, Arc::new(AppConfig::save))
    }

    fn with_save(config: Arc<RwLock<AppConfig>>, save: SaveConfig) -> Self {
        Self {
            config,
            pending: Arc::new(Mutex::new(PendingActions::default())),
            idle: Arc::new(Notify::new()),
            save,
        }
    }

    pub(super) fn dispatch(&self, action: Action) {
        // Enqueue before spawning so the runtime cannot reorder rapid clicks.
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
                            this.idle.notify_waiters();
                            break;
                        }
                    }
                };
                let mut config = this.config.write().await;
                match action {
                    Action::Language(language) => config.language = language,
                    Action::UiMode(mode) => config.ui_mode = mode,
                    Action::SelectedStrategy(id) => config.last_strategy = Some(id),
                }
                if let Err(error) = (this.save)(&config) {
                    tracing::warn!("Failed to persist UI preference: {error:#}");
                }
            }
        });
    }

    pub(super) async fn flush(&self) {
        loop {
            // Register before checking the queue: notify_waiters does not keep
            // a permit for a future that has not registered yet.
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.pending.lock().unwrap().worker_running {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preferences_save_in_click_order_and_flush_before_relaunch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let history = Arc::new(Mutex::new(Vec::new()));
        let snapshots = history.clone();
        let save_path = path.clone();
        let controller = Controller::with_save(
            config.clone(),
            Arc::new(move |config| {
                config.save_to_path(&save_path)?;
                snapshots.lock().unwrap().push((
                    config.language,
                    config.ui_mode,
                    config.last_strategy.clone(),
                ));
                Ok(())
            }),
        );
        controller.flush().await;

        // Hold the config lock until every click is enqueued. The worker may
        // start on either runtime thread, but cannot save or finish yet.
        let held = config.write().await;
        controller.dispatch(Action::Language(Language::En));
        controller.dispatch(Action::UiMode(UiMode::Advanced));
        controller.dispatch(Action::SelectedStrategy("first".into()));
        controller.dispatch(Action::SelectedStrategy("last".into()));
        controller.dispatch(Action::Language(Language::Ru));
        let mut flushed = Box::pin(controller.flush());
        assert!(futures_util::poll!(&mut flushed).is_pending());
        assert!(history.lock().unwrap().is_empty());
        drop(held);
        tokio::time::timeout(Duration::from_secs(5), flushed)
            .await
            .expect("all preference saves should finish before relaunch");

        assert_eq!(
            *history.lock().unwrap(),
            [
                (Language::En, UiMode::Simple, None),
                (Language::En, UiMode::Advanced, None),
                (Language::En, UiMode::Advanced, Some("first".into())),
                (Language::En, UiMode::Advanced, Some("last".into())),
                (Language::Ru, UiMode::Advanced, Some("last".into())),
            ]
        );
        assert_eq!(AppConfig::load_from_path(&path), *config.read().await);

        // A later action must start a fresh worker and support another flush.
        controller.dispatch(Action::UiMode(UiMode::Simple));
        tokio::time::timeout(Duration::from_secs(5), controller.flush())
            .await
            .unwrap();
        assert_eq!(AppConfig::load_from_path(&path).ui_mode, UiMode::Simple);
        assert_eq!(history.lock().unwrap().len(), 6);
    }
}
