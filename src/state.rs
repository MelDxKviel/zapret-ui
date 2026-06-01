use crate::contracts::RuntimeStatus;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Thread-safe store for the latest runtime status (see CLAUDE.md "Status flow").
#[derive(Clone, Debug)]
pub struct AppState {
    status: Arc<RwLock<RuntimeStatus>>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(RuntimeStatus::default())
    }
}

impl AppState {
    /// Creates a new AppState with the given initial runtime status.
    pub fn new(initial_status: RuntimeStatus) -> Self {
        Self {
            status: Arc::new(RwLock::new(initial_status)),
        }
    }

    /// Retrieves the current runtime status.
    pub async fn get_status(&self) -> RuntimeStatus {
        self.status.read().await.clone()
    }

    /// Stores a new runtime status.
    pub async fn set_status(&self, new_status: RuntimeStatus) {
        *self.status.write().await = new_status;
    }
}
