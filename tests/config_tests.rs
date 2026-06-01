#[path = "../src/contracts.rs"]
pub mod contracts;

#[path = "../src/config.rs"]
pub mod config;

#[path = "../src/state.rs"]
pub mod state;

use config::{AppConfig, Language, Theme};
use contracts::{RunningMode, RuntimeStatus};
use state::AppState;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_config_default() {
    let config = AppConfig::default();
    assert_eq!(config.last_strategy, None);
    assert!(!config.autostart);
    assert!(config.autoupdate_check);
    assert_eq!(config.install_dir_override, None);
    assert_eq!(config.theme, Theme::System);
    assert!(config.minimize_to_tray);
    assert!(!config.tray_notice_shown);
    assert_eq!(config.language, Language::Ru);
    assert!(config.favorites.is_empty());
    assert!(config.notifications_enabled);
    assert!(!config.autoengage);
}

#[test]
fn test_config_round_trip() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let config = AppConfig {
        last_strategy: Some("discord_alt4".to_string()),
        autostart: true,
        autoupdate_check: false,
        install_dir_override: Some(dir.path().to_path_buf()),
        theme: Theme::Dark,
        minimize_to_tray: true,
        ..AppConfig::default()
    };

    // Save config
    config.save_to_path(&config_path).unwrap();

    // Load config
    let loaded = AppConfig::load_from_path(&config_path);
    assert_eq!(config, loaded);
}

#[test]
fn test_config_fallback_on_corrupt() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backup_path = dir.path().join("config.toml.bak");

    // Write a corrupted file (invalid TOML syntax)
    fs::write(&config_path, "not a valid toml = [[{").unwrap();

    // Load from path should fallback to default config
    let loaded = AppConfig::load_from_path(&config_path);
    assert_eq!(loaded, AppConfig::default());

    // Verify backup exists and contains corrupted content
    assert!(backup_path.exists());
    let backup_content = fs::read_to_string(&backup_path).unwrap();
    assert_eq!(backup_content, "not a valid toml = [[{");

    // Verify a new default config file was saved to config_path
    assert!(config_path.exists());
    let new_content = fs::read_to_string(&config_path).unwrap();
    let parsed: AppConfig = toml::from_str(&new_content).unwrap();
    assert_eq!(parsed, AppConfig::default());
}

#[tokio::test]
async fn test_state_get_and_set_status() {
    let initial_status = RuntimeStatus {
        installed: false,
        installed_version: None,
        running_mode: RunningMode::None,
        active_strategy: None,
        winws_pid: None,
        service_installed: false,
        uptime_secs: None,
    };

    let state = AppState::new(initial_status.clone());

    // Get current status
    let current = state.get_status().await;
    assert!(!current.installed);

    // Update status using set_status
    let new_status = RuntimeStatus {
        installed: true,
        installed_version: Some("v1.0.0".to_string()),
        running_mode: RunningMode::UserProcess,
        active_strategy: Some("discord_alt4".to_string()),
        winws_pid: Some(1234),
        service_installed: false,
        uptime_secs: Some(42),
    };

    state.set_status(new_status.clone()).await;

    // Verify status was updated in state
    let updated = state.get_status().await;
    assert!(updated.installed);
    assert_eq!(updated.installed_version, Some("v1.0.0".to_string()));
    assert_eq!(updated.running_mode, RunningMode::UserProcess);
    assert_eq!(updated.active_strategy, Some("discord_alt4".to_string()));
    assert_eq!(updated.winws_pid, Some(1234));
}
