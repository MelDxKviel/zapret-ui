use std::path::{Path, PathBuf};
use serde::{Serialize, Deserialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Theme {
    Light,
    Dark,
    System,
}

impl Default for Theme {
    fn default() -> Self {
        Self::System
    }
}

/// UI language. Defaults to Russian, switchable on the Settings page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    Ru,
    En,
}

impl Default for Language {
    fn default() -> Self {
        Self::Ru
    }
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
    pub minimize_to_tray: bool,
    /// UI language. `#[serde(default)]` keeps configs written before this field
    /// was added loadable (they fall back to the default, Russian).
    #[serde(default)]
    pub language: Language,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_strategy: None,
            autostart: false,
            autoupdate_check: true,
            install_dir_override: None,
            theme: Theme::default(),
            minimize_to_tray: false,
            language: Language::default(),
        }
    }
}

impl AppConfig {
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
            Ok(content) => {
                match toml::from_str::<Self>(&content) {
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
                            tracing::error!("Failed to rename corrupted config to {:?}: {}", backup_path, err);
                        } else {
                            tracing::info!("Corrupted config backed up to {:?}", backup_path);
                        }

                        let default_config = Self::default();
                        if let Err(err) = default_config.save_to_path(path) {
                            tracing::error!("Failed to save default config after corruption: {}", err);
                        }
                        default_config
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to read config file at {:?}: {}. Returning default config.", path, e);
                Self::default()
            }
        }
    }

    /// Loads the configuration from the default path.
    pub fn load() -> Self {
        match Self::default_config_path() {
            Ok(path) => Self::load_from_path(&path),
            Err(e) => {
                tracing::error!("Failed to get default config path: {}. Returning default config.", e);
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

        let parent = path.parent().ok_or_else(|| anyhow::anyhow!("No parent directory for config path"))?;
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
