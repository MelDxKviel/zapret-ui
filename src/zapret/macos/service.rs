use crate::contracts::{RunningMode, Strategy};
use crate::ports::{ServiceCtl, StrategyCatalog};
use crate::zapret::{catalog::LocalStrategyCatalog, macos_bundle, process};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct MacServiceCtl {
    install_dir: PathBuf,
}
impl MacServiceCtl {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }
}
pub async fn install_service_protected(install: &Path, id: &str) -> Result<()> {
    let strategy = LocalStrategyCatalog::new(install.to_owned())
        .by_id(id)
        .context("Unknown ZapretMac strategy")?;
    process::start_strategy(install, &strategy).await?;
    Ok(())
}
#[async_trait::async_trait]
impl ServiceCtl for MacServiceCtl {
    async fn install(&self, strategy: &Strategy) -> Result<()> {
        process::start_strategy(&self.install_dir, strategy).await?;
        Ok(())
    }
    async fn remove(&self) -> Result<()> {
        process::remove_service().await
    }
    async fn start(&self) -> Result<()> {
        let id = std::fs::read_to_string(macos_bundle::user_data_dir()?.join("selected-strategy"))?;
        install_service_protected(&self.install_dir, id.trim()).await
    }
    async fn stop(&self) -> Result<()> {
        process::stop_engine().await
    }
    async fn status(&self) -> Result<RunningMode> {
        Ok(if process::engine_process().await.is_some() {
            RunningMode::SystemService
        } else {
            RunningMode::None
        })
    }
    async fn is_installed(&self) -> bool {
        process::registered()
    }
}
