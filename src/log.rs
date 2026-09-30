use std::path::PathBuf;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Registry};

/// A burst of process output may overrun the UI channel. Resume at the oldest
/// retained line after lag instead of permanently terminating the forwarder.
pub async fn forward_logs(
    mut rx: tokio::sync::broadcast::Receiver<String>,
    events: tokio::sync::broadcast::Sender<crate::contracts::UiEvent>,
) {
    loop {
        match rx.recv().await {
            Ok(line) => {
                let _ = events.send(crate::contracts::UiEvent::LogLine(line));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[derive(Clone)]
pub struct UiWriter {
    file_writer: tracing_appender::non_blocking::NonBlocking,
    tx: tokio::sync::broadcast::Sender<String>,
}

impl std::io::Write for UiWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let res = self.file_writer.write(buf);
        if let Ok(s) = std::str::from_utf8(buf) {
            let clean = s.trim_end_matches(['\r', '\n']).to_string();
            if !clean.is_empty() {
                let _ = self.tx.send(clean);
            }
        }
        res
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file_writer.flush()
    }
}

impl<'a> fmt::writer::MakeWriter<'a> for UiWriter {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

pub fn init_logging(
    tx: tokio::sync::broadcast::Sender<String>,
) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let appdata = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    let log_dir = appdata.join("zapret-ui").join("logs");
    std::fs::create_dir_all(&log_dir)?;

    let file_appender = tracing_appender::rolling::never(&log_dir, "app.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let ui_writer = UiWriter {
        file_writer: non_blocking,
        tx,
    };

    // Timestamps in local wall-clock time: the Logs page slices HH:MM:SS
    // straight out of the line, so it must not be UTC. If the local offset
    // can't be determined, fall back to the default UTC timer.
    match fmt::time::OffsetTime::local_rfc_3339() {
        Ok(timer) => {
            let subscriber = Registry::default()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
                .with(
                    fmt::layer()
                        .with_writer(ui_writer)
                        .with_ansi(false)
                        .with_timer(timer),
                );
            tracing::subscriber::set_global_default(subscriber)?;
        }
        Err(_) => {
            let subscriber = Registry::default()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
                .with(fmt::layer().with_writer(ui_writer).with_ansi(false));
            tracing::subscriber::set_global_default(subscriber)?;
        }
    }

    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::forward_logs;
    use crate::contracts::UiEvent;
    use tokio::sync::broadcast;

    #[tokio::test]
    async fn log_forwarder_recovers_after_lag_and_closes_cleanly() {
        let (logs, rx) = broadcast::channel(2);
        let (events, mut received) = broadcast::channel(4);
        for line in ["dropped", "retained 1", "retained 2"] {
            logs.send(line.to_string()).unwrap();
        }
        let task = tokio::spawn(forward_logs(rx, events));
        for expected in ["retained 1", "retained 2"] {
            match received.recv().await.unwrap() {
                UiEvent::LogLine(line) => assert_eq!(line, expected),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        logs.send("new output".into()).unwrap();
        assert!(
            matches!(received.recv().await.unwrap(), UiEvent::LogLine(line) if line == "new output")
        );
        drop(logs);
        task.await.unwrap();
    }
}
