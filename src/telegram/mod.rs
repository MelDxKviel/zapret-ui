//! On-demand local MTProto → WSS proxy, inspired by Flowseal/tg-ws-proxy.
//! No Python/runtime downloads, preconnection pool, periodic probes or worker
//! thread. Uses the app's Tokio runtime; stopping joins all connection tasks.
mod protocol;
mod settings;
mod transport;

use crate::contracts::{TelegramProxySettings, TelegramProxyStatus};
use crate::ports::{TelegramProxy, TelegramStatusCb};
use anyhow::Result;
use std::net::Ipv4Addr;
use std::sync::Arc;
use tokio::{
    net::TcpListener,
    sync::{oneshot, Mutex},
    task::{JoinHandle, JoinSet},
};

#[derive(Default)]
pub struct LocalTelegramProxy {
    running: Mutex<Option<Running>>,
}

struct Running {
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        // Also safe if the owner is dropped without an explicit stop. Dropping
        // the listener task drops its JoinSet and aborts every child connection.
        self.task.abort();
    }
}

#[async_trait::async_trait]
impl TelegramProxy for LocalTelegramProxy {
    fn prepare_settings(&self, settings: TelegramProxySettings) -> Result<TelegramProxySettings> {
        settings::prepare(settings)
    }

    async fn start(
        &self,
        settings: TelegramProxySettings,
        on_status: TelegramStatusCb,
    ) -> Result<()> {
        let mut running = self.running.lock().await;
        if running.as_ref().is_some_and(|r| !r.task.is_finished()) {
            return Ok(());
        }
        running.take();
        let settings = settings::prepare(settings)?;
        let secret = settings::decode_secret(&settings.secret)?;
        // Construct TLS config only on Start, and share it across connections.
        let routes = Arc::new(transport::Routes::new(&settings)?);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port))
            .await
            .map_err(|e| {
                tracing::warn!("Telegram listener: {e}");
                anyhow::anyhow!("telegram.error_bind")
            })?;
        let (shutdown, mut stop) = oneshot::channel();
        on_status(TelegramProxyStatus {
            running: true,
            ..Default::default()
        });
        let task = tokio::spawn(async move {
            let mut clients: JoinSet<Result<()>> = JoinSet::new();
            let mut error = String::new();
            tracing::info!("Telegram proxy listening on 127.0.0.1:{}", settings.port);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop => { error.clear(); break; },
                    result = clients.join_next(), if !clients.is_empty() => {
                        match result {
                            Some(Ok(Err(e))) => {
                                // Telegram retries individual connections automatically.
                                // Keep transient failures in logs, without marking the
                                // whole running proxy as failed.
                                tracing::debug!("Telegram connection ended: {e:#}");
                            }
                            Some(Err(e)) => tracing::warn!("Telegram connection task: {e}"),
                            _ => {}
                        }
                        on_status(TelegramProxyStatus { running: true, connections: clients.len() as u32, error: error.clone() });
                    }
                    result = listener.accept(), if clients.len() < usize::from(settings.max_connections) => {
                        let (socket, _) = match result {
                            Ok(pair) => pair,
                            Err(e) => {
                                tracing::warn!("Telegram accept: {e}");
                                error = "telegram.error_listener".into();
                                break;
                            }
                        };
                        let routes = routes.clone();
                        clients.spawn(async move { transport::serve(socket, secret, routes).await });
                        on_status(TelegramProxyStatus { running: true, connections: clients.len() as u32, error: error.clone() });
                    }
                }
            }
            drop(listener);
            clients.abort_all();
            while clients.join_next().await.is_some() {}
            on_status(TelegramProxyStatus {
                running: false,
                connections: 0,
                error,
            });
            tracing::info!("Telegram proxy stopped");
        });
        *running = Some(Running {
            shutdown: Some(shutdown),
            task,
        });
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut running = self.running.lock().await;
        if let Some(mut active) = running.take() {
            if let Some(shutdown) = active.shutdown.take() {
                let _ = shutdown.send(());
            }
            let _ = (&mut active.task).await;
        }
        Ok(())
    }

    async fn is_running(&self) -> bool {
        self.running
            .lock()
            .await
            .as_ref()
            .is_some_and(|r| !r.task.is_finished())
    }
}

#[cfg(test)]
mod tests;
