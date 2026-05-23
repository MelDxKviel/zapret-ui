use std::path::PathBuf;
use crate::contracts::{Strategy, Category};
use crate::ports::StrategyCatalog;
use crate::zapret::batparse;

/// Strategy catalog backed by the `.bat` preset files inside an installed zapret directory.
/// If nothing is installed yet, `all()` returns an empty list.
#[derive(Clone, Debug)]
pub struct LocalStrategyCatalog {
    install_dir: PathBuf,
}

impl LocalStrategyCatalog {
    pub fn new(install_dir: PathBuf) -> Self {
        Self { install_dir }
    }

    fn scan(&self) -> Vec<Strategy> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.install_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e.eq_ignore_ascii_case("bat")).unwrap_or(false) {
                    if let Some(s) = batparse::strategy_from_bat(&path, &self.install_dir) {
                        out.push(s);
                    }
                }
            }
        }
        // Stable, friendly order: plain "general" first, then alphabetical.
        out.sort_by(|a, b| {
            let rank = |s: &Strategy| if s.id.eq_ignore_ascii_case("general") { 0 } else { 1 };
            rank(a).cmp(&rank(b)).then_with(|| a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()))
        });
        out
    }
}

impl StrategyCatalog for LocalStrategyCatalog {
    fn all(&self) -> Vec<Strategy> {
        self.scan()
    }

    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.scan().into_iter().find(|s| s.id == id)
    }

    fn by_category(&self, c: Category) -> Vec<Strategy> {
        self.scan().into_iter().filter(|s| s.category == c).collect()
    }
}
