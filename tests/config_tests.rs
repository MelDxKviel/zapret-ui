use std::fs;
use tempfile::tempdir;
use zapret_ui::config::{AppConfig, Language, Theme};
use zapret_ui::contracts::{self, RunningMode, RuntimeStatus};
use zapret_ui::state::AppState;

#[test]
fn retired_simple_dashboard_preference_keeps_other_saved_settings() {
    let expected = AppConfig {
        language: Language::En,
        last_strategy: Some("general (ALT2)".into()),
        simple_last_good: Some("general (ALT2)".into()),
        ..Default::default()
    };
    let legacy = format!(
        "ui_mode = \"Simple\"\n{}",
        toml::to_string(&expected).unwrap()
    );
    let restored: AppConfig = toml::from_str(&legacy).unwrap();
    assert_eq!(restored, expected);
    assert!(!toml::to_string(&restored).unwrap().contains("ui_mode"));
}

#[test]
fn telegram_relay_defaults_on_and_preserves_explicit_opt_out_and_custom_domains() {
    let legacy_text = toml::to_string(&AppConfig::default())
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with("relay_enabled =") && !line.starts_with("relay_domains ="))
        .collect::<Vec<_>>()
        .join("\n");
    let legacy: AppConfig = toml::from_str(&legacy_text).unwrap();
    assert!(legacy.telegram_proxy.relay_enabled);
    assert!(legacy.telegram_proxy.relay_domains.is_empty());
    let mut opted_out = legacy;
    opted_out.telegram_proxy.relay_enabled = false;
    opted_out.telegram_proxy.relay_domains = "relay.example.org".into();
    let restored: AppConfig = toml::from_str(&toml::to_string(&opted_out).unwrap()).unwrap();
    assert_eq!(restored, opted_out);
}

#[test]
fn test_config_default() {
    let config = AppConfig::default();
    assert!(!config.zapret2_enabled);
    assert_eq!(
        config.effective_engine_mode(),
        contracts::EngineMode::Zapret1
    );
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
    assert!(config.show_telegram_proxy);
    assert!(!config.telegram_autostart);
    assert!(config.telegram_proxy.secret.is_empty());
}

#[test]
fn telegram_settings_persist_and_old_configs_keep_defaults() {
    let config = AppConfig {
        show_telegram_proxy: false,
        telegram_autostart: true,
        telegram_proxy: contracts::TelegramProxySettings {
            port: 2443,
            secret: "00112233445566778899aabbccddeeff".into(),
            tcp_fallback: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let text = toml::to_string(&config).unwrap();
    assert_eq!(toml::from_str::<AppConfig>(&text).unwrap(), config);
    let mut old = toml::Value::try_from(config).unwrap();
    old.as_table_mut().unwrap().remove("telegram_proxy");
    old.as_table_mut().unwrap().remove("show_telegram_proxy");
    old.as_table_mut().unwrap().remove("telegram_autostart");
    let loaded: AppConfig = old.try_into().unwrap();
    assert!(loaded.show_telegram_proxy);
    assert!(!loaded.telegram_autostart);
    assert_eq!(
        loaded.telegram_proxy,
        contracts::TelegramProxySettings::default()
    );
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
        active_engine: None,
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
        active_engine: None,
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

#[test]
fn config_recovery_preserves_source_when_backup_is_unavailable() {
    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let backup_path = dir.path().join("config.toml.bak");
    let corrupt = "not a valid toml = [[{";
    fs::write(&config_path, corrupt).unwrap();
    // A directory at the backup destination cannot be removed as a file.
    fs::create_dir(&backup_path).unwrap();

    assert_eq!(
        AppConfig::load_from_path(&config_path),
        AppConfig::default()
    );
    assert_eq!(fs::read_to_string(config_path).unwrap(), corrupt);
    assert!(backup_path.is_dir());
}

#[test]
fn corrupt_config_diagnostics_do_not_log_proxy_secrets() {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let dir = tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let secret = "00112233445566778899aabbccddeeff";
    // An unterminated secret makes TOML's Display include the secret source line.
    fs::write(
        &config_path,
        format!("[telegram_proxy]\nsecret = \"{secret}"),
    )
    .unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || LogWriter(sink.clone()))
        .finish();
    // A single global subscriber keeps callsite interest stable while the other
    // config tests log concurrently; a thread-local subscriber can lose events
    // when tracing rebuilds its interest cache on another test thread.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    assert_eq!(
        AppConfig::load_from_path(&config_path),
        AppConfig::default()
    );
    let logs = String::from_utf8(captured.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Failed to parse config file"));
    assert!(!logs.contains(secret));
}

#[test]
fn engine_preferences_roundtrip_and_preserve_legacy_configuration() {
    use contracts::{EngineKind, EngineMode, StrategyTestResult};
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut config = AppConfig {
        last_strategy: Some("general (ALT2)".into()),
        simple_last_good: Some("general (ALT2)".into()),
        favorites: vec!["general (ALT2)".into()],
        install_dir_override: Some(dir.path().join("legacy-core")),
        theme: Theme::Dark,
        language: Language::En,
        telegram_autostart: true,
        ..Default::default()
    };
    let mut legacy = toml::Value::try_from(&config).unwrap();
    for key in [
        "zapret2_enabled",
        "engine_mode",
        "zapret2_install_dir_override",
        "strategy_test_results",
    ] {
        legacy.as_table_mut().unwrap().remove(key);
    }
    fs::write(&path, toml::to_string(&legacy).unwrap()).unwrap();
    let loaded = AppConfig::load_from_path(&path);
    assert_eq!(loaded, config);
    assert_eq!(loaded.engine_mode, EngineMode::Zapret1);
    assert!(!loaded.zapret2_enabled);
    assert_ne!(
        loaded.engine_install_dir(EngineKind::Zapret2),
        loaded.install_dir()
    );
    config.engine_mode = EngineMode::Auto;
    config.zapret2_enabled = true;
    config.zapret2_install_dir_override = Some(dir.path().join("second-core"));
    config.strategy_test_results = vec![StrategyTestResult {
        id: "zapret2:multisplit".into(),
        engine: EngineKind::Zapret2,
        baseline_ok: 1,
        improved: 2,
        ok: 3,
        total: 3,
        avg_latency_ms: 42,
        rank: 1,
        ..Default::default()
    }];
    config.save_to_path(&path).unwrap();
    assert_eq!(AppConfig::load_from_path(&path), config);
    assert!(!path.with_extension("toml.bak").exists());
}

#[test]
fn zapret2_beta_defaults_off_without_erasing_saved_preferences() {
    use contracts::{EngineKind, EngineMode};
    let directory = tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let saved = AppConfig {
        engine_mode: EngineMode::Auto,
        last_strategy: Some("zapret2:multisplit".into()),
        simple_last_good: Some("zapret2:multisplit".into()),
        favorites: vec!["general".into(), "zapret2:user-custom".into()],
        zapret2_install_dir_override: Some(directory.path().join("beta-core")),
        ..Default::default()
    };
    let mut old = toml::Value::try_from(&saved).unwrap();
    old.as_table_mut().unwrap().remove("zapret2_enabled");
    fs::write(&path, toml::to_string(&old).unwrap()).unwrap();
    let mut loaded = AppConfig::load_from_path(&path);
    assert_eq!(loaded, saved);
    assert!(!loaded.zapret2_enabled);
    assert_eq!(loaded.engine_mode, EngineMode::Auto);
    assert_eq!(loaded.effective_engine_mode(), EngineMode::Zapret1);
    assert_eq!(
        loaded.engine_install_dir(EngineKind::Zapret2),
        directory.path().join("beta-core")
    );
    loaded.save_to_path(&path).unwrap();
    assert_eq!(AppConfig::load_from_path(&path), saved);

    loaded.zapret2_enabled = true;
    loaded.save_to_path(&path).unwrap();
    let enabled = AppConfig::load_from_path(&path);
    assert_eq!(enabled, loaded);
    assert!(enabled.zapret2_enabled);
    assert_eq!(enabled.engine_mode, EngineMode::Auto);
    assert_eq!(enabled.effective_engine_mode(), EngineMode::Zapret1);
    assert_eq!(enabled.favorites, saved.favorites);
    assert_eq!(enabled.last_strategy, saved.last_strategy);
}

#[test]
fn only_an_explicit_enabled_zapret2_selection_changes_the_default_engine() {
    use contracts::EngineMode;
    for engine_mode in [EngineMode::Zapret1, EngineMode::Zapret2, EngineMode::Auto] {
        let mut config = AppConfig {
            engine_mode,
            ..Default::default()
        };
        assert_eq!(config.effective_engine_mode(), EngineMode::Zapret1);
        assert_eq!(config.engine_mode, engine_mode);
        config.zapret2_enabled = true;
        let expected = if engine_mode == EngineMode::Zapret2 {
            EngineMode::Zapret2
        } else {
            EngineMode::Zapret1
        };
        assert_eq!(config.effective_engine_mode(), expected);
        config.zapret2_enabled = false;
        assert_eq!(config.effective_engine_mode(), EngineMode::Zapret1);
        assert_eq!(config.engine_mode, engine_mode);
    }
}

#[test]
fn legacy_strategy_defaults_to_zapret1() {
    let strategy: contracts::Strategy = serde_json::from_str(
        r#"{
        "id":"general", "display_name":"general", "category":"Mixed", "description":"",
        "winws_args":[], "requires_lists":[]
    }"#,
    )
    .unwrap();
    assert_eq!(strategy.engine, contracts::EngineKind::Zapret1);
}

#[cfg(windows)]
#[test]
fn engine_directories_reject_same_or_nested_installations() {
    let dir = tempdir().unwrap();
    let mut config = AppConfig {
        install_dir_override: Some(dir.path().join("one")),
        zapret2_install_dir_override: Some(dir.path().join("two")),
        ..Default::default()
    };
    config.validate_engine_directories().unwrap();
    for path in [
        dir.path().join("one"),
        dir.path().join("one/child"),
        dir.path().to_path_buf(),
    ] {
        config.zapret2_install_dir_override = Some(path);
        assert!(
            config.validate_engine_directories().is_err(),
            "overlapping installation was accepted: {:?}",
            config.zapret2_install_dir_override
        );
    }
}

#[cfg(windows)]
#[test]
fn missing_engine_paths_resolve_their_existing_parent_aliases() {
    let dir = tempfile::Builder::new()
        .prefix("zapret engine directories ")
        .tempdir()
        .unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    let first = canonical.join("one");
    assert!(!first.exists());
    for second in [
        dir.path().join("one"),
        dir.path().join("one/child"),
        dir.path().to_path_buf(),
    ] {
        let config = AppConfig {
            install_dir_override: Some(first.clone()),
            zapret2_install_dir_override: Some(second.clone()),
            ..Default::default()
        };
        assert!(
            config.validate_engine_directories().is_err(),
            "overlapping installation was accepted: {first:?}, {second:?}"
        );
    }
    let separate = AppConfig {
        install_dir_override: Some(first),
        zapret2_install_dir_override: Some(dir.path().join("two")),
        ..Default::default()
    };
    separate.validate_engine_directories().unwrap();
}

#[cfg(windows)]
#[test]
fn zapret2_work_directory_cannot_overlap_zapret1() {
    let dir = tempdir().unwrap();
    let mut config = AppConfig {
        install_dir_override: Some(dir.path().join("one")),
        zapret2_install_dir_override: Some(dir.path().join("two")),
        ..Default::default()
    };
    config.validate_engine_directories().unwrap();
    for path in [
        dir.path().join("two-work"),
        dir.path().join("two-work/child"),
    ] {
        config.install_dir_override = Some(path);
        assert!(config.validate_engine_directories().is_err());
    }
}
