use std::path::PathBuf;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter, Registry};

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

pub fn init_logging(tx: tokio::sync::broadcast::Sender<String>) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
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

    let subscriber = Registry::default()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(ui_writer).with_ansi(false));

    tracing::subscriber::set_global_default(subscriber)?;

    Ok(guard)
}
