//! Select installed cores at the port boundary; the app never knows an adapter.
use crate::contracts::{Category, EngineKind, EngineMode, InstallStage, Strategy};
use crate::ports::{Installer, ProgressCb, StrategyCatalog};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};

#[derive(Default)]
struct Selection(AtomicU8);
impl Selection {
    fn set(&self, mode: EngineMode) {
        self.0.store(
            match mode {
                EngineMode::Zapret1 => 0,
                EngineMode::Zapret2 => 1,
                EngineMode::Auto => 2,
            },
            Ordering::SeqCst,
        );
    }
    fn get(&self) -> EngineMode {
        match self.0.load(Ordering::SeqCst) {
            1 => EngineMode::Zapret2,
            2 => EngineMode::Auto,
            _ => EngineMode::Zapret1,
        }
    }
}

pub struct EngineInstaller {
    first: Arc<dyn Installer>,
    second: Arc<dyn Installer>,
    selection: Selection,
}
impl EngineInstaller {
    pub fn new(first: Arc<dyn Installer>, second: Arc<dyn Installer>, mode: EngineMode) -> Self {
        let selection = Selection::default();
        selection.set(mode);
        Self {
            first,
            second,
            selection,
        }
    }
}

#[async_trait::async_trait]
impl Installer for EngineInstaller {
    fn set_engine_mode(&self, mode: EngineMode) {
        self.selection.set(mode);
    }
    async fn is_installed(&self) -> bool {
        match self.selection.get() {
            EngineMode::Zapret1 => self.first.is_installed().await,
            EngineMode::Zapret2 => self.second.is_installed().await,
            EngineMode::Auto => self.first.is_installed().await && self.second.is_installed().await,
        }
    }
    async fn installed_version(&self) -> Option<String> {
        match self.selection.get() {
            EngineMode::Zapret1 => self.first.installed_version().await,
            EngineMode::Zapret2 => self.second.installed_version().await,
            EngineMode::Auto => Some(format!(
                "zapret1: {} · zapret2: {}",
                self.first
                    .installed_version()
                    .await
                    .unwrap_or_else(|| "—".into()),
                self.second
                    .installed_version()
                    .await
                    .unwrap_or_else(|| "—".into())
            )),
        }
    }
    async fn latest_version(&self) -> anyhow::Result<String> {
        match self.selection.get() {
            EngineMode::Zapret1 => self.first.latest_version().await,
            EngineMode::Zapret2 => self.second.latest_version().await,
            EngineMode::Auto => Ok(format!(
                "zapret1: {} · zapret2: {}",
                self.first.latest_version().await?,
                self.second.latest_version().await?
            )),
        }
    }
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()> {
        let mode = self.selection.get();
        let progress: Arc<dyn Fn(InstallStage, u64, Option<u64>) + Send + Sync> =
            Arc::from(on_progress);
        for (engine, installer) in [
            (EngineKind::Zapret1, &self.first),
            (EngineKind::Zapret2, &self.second),
        ] {
            if mode.includes(engine) {
                let progress = progress.clone();
                installer
                    .install_or_update(Box::new(move |stage, bytes, total| {
                        // Only the whole selected installation can release the UI lock.
                        if !matches!(stage, InstallStage::Done) {
                            progress(stage, bytes, total);
                        }
                    }))
                    .await
                    .map_err(|e| e.context(format!("{} installation failed", engine.slug())))?;
            }
        }
        progress(InstallStage::Done, 0, None);
        Ok(())
    }
}

pub struct EngineCatalog {
    first: Arc<dyn StrategyCatalog>,
    second: Arc<dyn StrategyCatalog>,
    selection: Selection,
}
impl EngineCatalog {
    pub fn new(
        first: Arc<dyn StrategyCatalog>,
        second: Arc<dyn StrategyCatalog>,
        mode: EngineMode,
    ) -> Self {
        let selection = Selection::default();
        selection.set(mode);
        Self {
            first,
            second,
            selection,
        }
    }
}
impl StrategyCatalog for EngineCatalog {
    fn set_engine_mode(&self, mode: EngineMode) {
        self.selection.set(mode);
    }
    fn all(&self) -> Vec<Strategy> {
        let mode = self.selection.get();
        let mut out = Vec::new();
        if mode.includes(EngineKind::Zapret1) {
            out.extend(self.first.all());
        }
        if mode.includes(EngineKind::Zapret2) {
            out.extend(self.second.all());
        }
        out
    }
    // Resolve the active/previous preset even when the user has changed modes.
    fn by_id(&self, id: &str) -> Option<Strategy> {
        match EngineKind::for_strategy_id(id) {
            EngineKind::Zapret1 => self.first.by_id(id),
            EngineKind::Zapret2 => self.second.by_id(id),
        }
    }
    fn by_category(&self, category: Category) -> Vec<Strategy> {
        self.all()
            .into_iter()
            .filter(|s| s.category == category)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Catalog(EngineKind);
    impl StrategyCatalog for Catalog {
        fn all(&self) -> Vec<Strategy> {
            vec![Strategy {
                id: if self.0 == EngineKind::Zapret2 {
                    "zapret2:split"
                } else {
                    "general"
                }
                .into(),
                engine: self.0,
                display_name: "fixture".into(),
                category: Category::Mixed,
                description: String::new(),
                winws_args: vec![],
                requires_lists: vec![],
            }]
        }
        fn by_id(&self, id: &str) -> Option<Strategy> {
            self.all().into_iter().find(|s| s.id == id)
        }
        fn by_category(&self, category: Category) -> Vec<Strategy> {
            self.all()
                .into_iter()
                .filter(|s| s.category == category)
                .collect()
        }
    }
    #[test]
    fn selection_filters_candidates_and_keeps_active_id_resolvable() {
        let catalog = EngineCatalog::new(
            Arc::new(Catalog(EngineKind::Zapret1)),
            Arc::new(Catalog(EngineKind::Zapret2)),
            EngineMode::Zapret1,
        );
        assert_eq!(catalog.all().len(), 1);
        assert_eq!(catalog.all()[0].engine, EngineKind::Zapret1);
        catalog.set_engine_mode(EngineMode::Zapret2);
        assert_eq!(catalog.all()[0].engine, EngineKind::Zapret2);
        assert!(catalog.by_id("general").is_some());
        catalog.set_engine_mode(EngineMode::Auto);
        assert_eq!(catalog.all().len(), 2);
        assert_eq!(catalog.by_category(Category::Mixed).len(), 2);
        assert!(catalog.by_category(Category::Discord).is_empty());
    }
    struct Install {
        name: &'static str,
        installed: bool,
        updates: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait::async_trait]
    impl Installer for Install {
        async fn is_installed(&self) -> bool {
            self.installed
        }
        async fn installed_version(&self) -> Option<String> {
            self.installed.then(|| self.name.into())
        }
        async fn latest_version(&self) -> anyhow::Result<String> {
            Ok(self.name.into())
        }
        async fn install_or_update(&self, progress: ProgressCb) -> anyhow::Result<()> {
            self.updates.lock().unwrap().push(self.name);
            progress(InstallStage::Done, 0, None);
            Ok(())
        }
    }
    #[tokio::test]
    async fn installation_and_versions_follow_selected_mode() {
        let calls = Arc::new(Mutex::new(vec![]));
        let installer = EngineInstaller::new(
            Arc::new(Install {
                name: "one",
                installed: true,
                updates: calls.clone(),
            }),
            Arc::new(Install {
                name: "two",
                installed: false,
                updates: calls.clone(),
            }),
            EngineMode::Zapret1,
        );
        assert!(installer.is_installed().await);
        installer.set_engine_mode(EngineMode::Zapret2);
        assert!(!installer.is_installed().await);
        installer
            .install_or_update(Box::new(|_, _, _| {}))
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["two"]);
        calls.lock().unwrap().clear();
        installer.set_engine_mode(EngineMode::Auto);
        assert!(!installer.is_installed().await);
        assert_eq!(
            installer.latest_version().await.unwrap(),
            "zapret1: one · zapret2: two"
        );
        let done = Arc::new(Mutex::new(0));
        let count = done.clone();
        installer
            .install_or_update(Box::new(move |stage, _, _| {
                if matches!(stage, InstallStage::Done) {
                    *count.lock().unwrap() += 1;
                }
            }))
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["one", "two"]);
        assert_eq!(*done.lock().unwrap(), 1);
    }
}
