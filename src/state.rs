use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use crate::contracts::RuntimeStatus;

/// Thread-safe application state that monitors and broadcasts runtime status changes.
#[derive(Clone, Debug)]
pub struct AppState {
    status: Arc<RwLock<RuntimeStatus>>,
    tx: broadcast::Sender<RuntimeStatus>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(RuntimeStatus::default())
    }
}

impl AppState {
    /// Creates a new AppState with the given initial runtime status.
    pub fn new(initial_status: RuntimeStatus) -> Self {
        let (tx, _) = broadcast::channel(128);
        Self {
            status: Arc::new(RwLock::new(initial_status)),
            tx,
        }
    }

    /// Retrieves the current runtime status.
    pub async fn get_status(&self) -> RuntimeStatus {
        self.status.read().await.clone()
    }

    /// Sets the runtime status to a new value and broadcasts the change.
    pub async fn set_status(&self, new_status: RuntimeStatus) {
        let mut lock = self.status.write().await;
        *lock = new_status.clone();
        let _ = self.tx.send(new_status);
    }

    /// Updates the runtime status in place and broadcasts the change.
    pub async fn update_status<F>(&self, f: F)
    where
        F: FnOnce(&mut RuntimeStatus),
    {
        let mut lock = self.status.write().await;
        f(&mut *lock);
        let _ = self.tx.send(lock.clone());
    }

    /// Subscribes to runtime status updates.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeStatus> {
        self.tx.subscribe()
    }
}
