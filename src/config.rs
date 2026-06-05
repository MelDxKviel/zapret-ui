use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Light,
    Dark,
    #[default]
    System,
}

impl Theme {
    /// Slug exchanged with the Slint UI ("dark" | "light" | "system").
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }
    pub fn from_slug(s: &str) -> Self {
        match s {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::System,
        }
    }
}

/// Dashboard presentation mode. `Simple` shows the one-button power dial that
/// auto-picks a working strategy; `Advanced` shows the full control dashboard.
/// Defaults to `Simple` so a first-time user has nothing to configure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiMode {
    #[default]
    Simple,
    Advanced,
}

impl UiMode {
    /// Slug exchanged with the Slint UI ("simple" | "advanced").
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Advanced => "advanced",
        }
    }
    pub fn from_slug(s: &str) -> Self {
        match s {
            "advanced" => Self::Advanced,
            _ => Self::Simple,
        }
    }
}

/// UI language. Defaults to Russian, switchable on the Settings page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    Ru,
    En,
}

impl Language {
    /// Map a Slint language code ("ru" | "en") back to the enum.
    pub fn from_code(code: &str) -> Self {
        match code {
            "en" => Self::En,
            _ => Self::Ru,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub last_strategy: Option<String>,
    pub autostart: bool,
    pub autoupdate_check: bool,
    pub install_dir_override: Option<PathBuf>,
    pub theme: Theme,
    /// Hide to the system tray instead of quitting on window close. Defaults to
    /// on; `#[serde(default = ...)]` keeps older configs loadable.
    #[serde(default = "default_true")]
    pub minimize_to_tray: bool,
    /// Whether the one-time "still running in the tray" toast has been shown.
    /// Set the first time the window is hidden to tray, so the hint never repeats.
    #[serde(default)]
    pub tray_notice_shown: bool,
    /// UI language. `#[serde(default)]` keeps configs written before this field
    /// was added loadable (they fall back to the default, Russian).
    #[serde(default)]
    pub language: Language,
    /// Strategy ids the user has starred as favorites. Surfaced on the Strategies
    /// and Tester pages (favorites float to the top of the list). `#[serde(default)]`
    /// keeps older configs loadable.
    #[serde(default)]
    pub favorites: Vec<String>,
    /// Show a Windows toast when the bypass starts or stops. Defaults to on;
    /// `#[serde(default = ...)]` keeps older configs loadable (and defaulting to on).
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    /// Automatically start the last-used strategy as a user process when the app
    /// launches. `#[serde(default)]` keeps older configs loadable.
    #[serde(default)]
    pub autoengage: bool,
    /// Dashboard presentation mode (simple one-button dial vs. full dashboard).
    /// `#[serde(default)]` keeps older configs loadable (defaulting to Simple).
    #[serde(default)]
    pub ui_mode: UiMode,
    /// Simple-mode memory: the strategy a full auto-engage scan last proved
    /// working. When set, the dial fast-starts it (and verifies in the
    /// background) instead of re-scanning; a failed background verify clears it,
    /// so the next engage runs a full scan. `None` until the first scan succeeds.
    #[serde(default)]
    pub simple_last_good: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The per-user default zapret install dir, `%APPDATA%\zapret-ui\zapret`.
/// `None` only if the OS user directories can't be resolved. Canonical home for
/// the default-path logic that callers reach via [`AppConfig::install_dir`].
pub fn default_install_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.config_dir().join("zapret-ui").join("zapret"))
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_strategy: None,
            autostart: false,
            autoupdate_check: true,
            install_dir_override: None,
            theme: Theme::default(),
            minimize_to_tray: true,
            tray_notice_shown: false,
            language: Language::default(),
            favorites: Vec::new(),
            notifications_enabled: true,
            autoengage: false,
            ui_mode: UiMode::default(),
            simple_last_good: None,
        }
    }
}

impl AppConfig {
    /// The effective zapret install dir: the explicit `install_dir_override` if
    /// set, else the per-user default ([`default_install_dir`]). Falls back to an
    /// empty path only if the OS user directories can't be resolved (effectively
    /// never on Windows). Single source of truth for every caller that needs the
    /// install dir (app.rs, main.rs).
    pub fn install_dir(&self) -> PathBuf {
        self.install_dir_override
            .clone()
            .or_else(default_install_dir)
            .unwrap_or_default()
    }

    /// Returns the default config path under `%APPDATA%\zapret-ui\config.toml`
    pub fn default_config_path() -> anyhow::Result<PathBuf> {
        let base_dirs = directories::BaseDirs::new()
            .ok_or_else(|| anyhow::anyhow!("Failed to retrieve user directories"))?;
        Ok(base_dirs.config_dir().join("zapret-ui").join("config.toml"))
    }

    /// Loads the configuration from the specified path.
    /// If the file does not exist, it creates it with default values.
    /// If the file is corrupted, it moves the corrupt file to `<path>.bak`, saves a new default config, and returns default values.
    pub fn load_from_path(path: &Path) -> Self {
        if !path.exists() {
            let default_config = Self::default();
            if let Err(e) = default_config.save_to_path(path) {
                tracing::warn!("Failed to save default config to {:?}: {}", path, e);
            }
            return default_config;
        }

        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str::<Self>(&content) {
                Ok(config) => config,
                Err(e) => {
                    tracing::error!(
                            "Failed to parse config file: {}. Corrupted file will be backed up and replaced with defaults.",
                            e
                        );

                    let mut backup_path = path.to_path_buf();
                    backup_path.set_extension("toml.bak");

                    if backup_path.exists() {
                        let _ = std::fs::remove_file(&backup_path);
                    }

                    if let Err(err) = std::fs::rename(path, &backup_path) {
                        tracing::error!(
                            "Failed to rename corrupted config to {:?}: {}",
                            backup_path,
                            err
                        );
                    } else {
                        tracing::info!("Corrupted config backed up to {:?}", backup_path);
                    }

                    let default_config = Self::default();
                    if let Err(err) = default_config.save_to_path(path) {
                        tracing::error!("Failed to save default config after corruption: {}", err);
                    }
                    default_config
                }
            },
            Err(e) => {
                tracing::error!(
                    "Failed to read config file at {:?}: {}. Returning default config.",
                    path,
                    e
                );
                Self::default()
            }
        }
    }

    /// Loads the configuration from the default path.
    pub fn load() -> Self {
        match Self::default_config_path() {
            Ok(path) => Self::load_from_path(&path),
            Err(e) => {
                tracing::error!(
                    "Failed to get default config path: {}. Returning default config.",
                    e
                );
                Self::default()
            }
        }
    }

    /// Saves the configuration to the specified path atomically.
    pub fn save_to_path(&self, path: &Path) -> anyhow::Result<()> {
        let content = toml::to_string_pretty(self)?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("No parent directory for config path"))?;
        let mut temp_file = tempfile::NamedTempFile::new_in(parent)?;

        use std::io::Write;
        temp_file.write_all(content.as_bytes())?;
        temp_file.flush()?;

        temp_file.persist(path)?;
        Ok(())
    }

    /// Saves the configuration to the default path atomically.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::default_config_path()?;
        self.save_to_path(&path)
    }
}
