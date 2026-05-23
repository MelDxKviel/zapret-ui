#[path = "../src/contracts.rs"]
pub mod contracts;

#[path = "../src/config.rs"]
pub mod config;

#[path = "../src/state.rs"]
pub mod state;

use std::fs;
use tempfile::tempdir;
use config::{AppConfig, Theme};
use contracts::{RuntimeStatus, RunningMode};
use state::AppState;

#[test]
fn test_config_default() {
    let config = AppConfig::default();
    assert_eq!(config.last_strategy, None);
    assert_eq!(config.autostart, false);
    assert_eq!(config.autoupdate_check, true);
    assert_eq!(config.install_dir_override, None);
    assert_eq!(config.theme, Theme::System);
    assert_eq!(config.minimize_to_tray, false);
}

#[test]
fn test_config_round_trip() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    let mut config = AppConfig::default();
    config.last_strategy = Some("discord_alt4".to_string());
    config.autostart = true;
    config.autoupdate_check = false;
    config.install_dir_override = Some(dir.path().to_path_buf());
    config.theme = Theme::Dark;
    config.minimize_to_tray = true;

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
async fn test_state_updates_and_broadcast() {
    let initial_status = RuntimeStatus {
        installed: false,
        installed_version: None,
        running_mode: RunningMode::None,
        active_strategy: None,
        winws_pid: None,
    };
    
    let state = AppState::new(initial_status.clone());
    let mut rx = state.subscribe();

    // Get current status
    let current = state.get_status().await;
    assert_eq!(current.installed, false);

    // Update status using set_status
    let new_status = RuntimeStatus {
        installed: true,
        installed_version: Some("v1.0.0".to_string()),
        running_mode: RunningMode::UserProcess,
        active_strategy: Some("discord_alt4".to_string()),
        winws_pid: Some(1234),
    };
    
    state.set_status(new_status.clone()).await;

    // Verify status was updated in state
    let updated = state.get_status().await;
    assert_eq!(updated.installed, true);
    assert_eq!(updated.installed_version, Some("v1.0.0".to_string()));
    assert_eq!(updated.running_mode, RunningMode::UserProcess);
    assert_eq!(updated.active_strategy, Some("discord_alt4".to_string()));
    assert_eq!(updated.winws_pid, Some(1234));

    // Verify broadcast received the change
    let rx_status = rx.recv().await.unwrap();
    assert_eq!(rx_status.installed, true);
    assert_eq!(rx_status.winws_pid, Some(1234));

    // Update status in place using update_status
    state.update_status(|s| {
        s.winws_pid = Some(5678);
    }).await;

    // Verify status updated
    let updated2 = state.get_status().await;
    assert_eq!(updated2.winws_pid, Some(5678));

    // Verify broadcast received second update
    let rx_status2 = rx.recv().await.unwrap();
    assert_eq!(rx_status2.winws_pid, Some(5678));
}
