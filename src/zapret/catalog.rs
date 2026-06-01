use crate::contracts::{Category, GameFilterMode, Strategy};
use crate::ports::StrategyCatalog;
use crate::zapret::batparse;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileFingerprint {
    path: String,
    len: u64,
    modified_ns: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogFingerprint {
    game_filter: GameFilterMode,
    bats: Vec<FileFingerprint>,
}

#[derive(Clone, Debug)]
struct CachedScan {
    fingerprint: CatalogFingerprint,
    strategies: Vec<Strategy>,
}

/// Strategy catalog backed by the `.bat` preset files inside an installed zapret directory.
/// If nothing is installed yet, `all()` returns an empty list.
#[derive(Clone, Debug)]
pub struct LocalStrategyCatalog {
    install_dir: PathBuf,
    cache: Arc<Mutex<Option<CachedScan>>>,
}

impl LocalStrategyCatalog {
    pub fn new(install_dir: PathBuf) -> Self {
        Self {
            install_dir,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn fingerprint(&self, gf: GameFilterMode) -> CatalogFingerprint {
        let mut bats = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.install_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("bat"))
                    .unwrap_or(false)
                {
                    continue;
                }
                if let Ok(meta) = entry.metadata() {
                    let modified_ns = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    bats.push(FileFingerprint {
                        path: path.to_string_lossy().to_lowercase(),
                        len: meta.len(),
                        modified_ns,
                    });
                }
            }
        }
        bats.sort_by(|a, b| a.path.cmp(&b.path));
        CatalogFingerprint {
            game_filter: gf,
            bats,
        }
    }

    fn scan_uncached(&self, gf: GameFilterMode) -> Vec<Strategy> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.install_dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("bat"))
                    .unwrap_or(false)
                {
                    if let Some(s) = batparse::strategy_from_bat(&path, &self.install_dir, gf) {
                        out.push(s);
                    }
                }
            }
        }
        // Stable, friendly order: plain "general" first, then alphabetical.
        out.sort_by(|a, b| {
            let rank = |s: &Strategy| {
                if s.id.eq_ignore_ascii_case("general") {
                    0
                } else {
                    1
                }
            };
            rank(a).cmp(&rank(b)).then_with(|| {
                a.display_name
                    .to_lowercase()
                    .cmp(&b.display_name.to_lowercase())
            })
        });
        out
    }

    fn scan(&self) -> Vec<Strategy> {
        // Re-read game filter state before fingerprinting so toggles invalidate
        // cached args and take effect on the next start.
        let gf = batparse::read_game_filter(&self.install_dir);
        let fingerprint = self.fingerprint(gf);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = cache.as_ref() {
            if cached.fingerprint == fingerprint {
                return cached.strategies.clone();
            }
        }

        let strategies = self.scan_uncached(gf);
        *cache = Some(CachedScan {
            fingerprint,
            strategies: strategies.clone(),
        });
        strategies
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
        self.scan()
            .into_iter()
            .filter(|s| s.category == c)
            .collect()
    }
}
