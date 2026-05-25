#![allow(dead_code)]

use std::rc::Rc;

use zapret_ui::contracts::*;
use zapret_ui::ports::*;

slint::include_modules!();

// --- Mock implementations of all ports ---

struct MockInstaller;

#[async_trait::async_trait]
impl Installer for MockInstaller {
    async fn is_installed(&self) -> bool { true }
    async fn installed_version(&self) -> Option<String> { Some("v1.0.0-mock".to_string()) }
    async fn latest_version(&self) -> anyhow::Result<String> { Ok("v1.1.0-mock".to_string()) }
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()> {
        on_progress(InstallStage::Resolving, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        on_progress(InstallStage::Downloading, 50, Some(100));
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        on_progress(InstallStage::Downloading, 100, Some(100));
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        on_progress(InstallStage::Extracting, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        on_progress(InstallStage::Verifying, 0, None);
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        on_progress(InstallStage::Done, 1, Some(1));
        Ok(())
    }
}

struct MockRunner;

#[async_trait::async_trait]
impl Runner for MockRunner {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32> {
        tracing::info!("Mock starting strategy: {}", strategy.display_name);
        Ok(12345)
    }
    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("Mock stopping process");
        Ok(())
    }
    async fn detect_running(&self) -> RuntimeStatus {
        RuntimeStatus {
            installed: true,
            installed_version: Some("v1.0.0-mock".to_string()),
            running_mode: RunningMode::None,
            active_strategy: None,
            winws_pid: None,
            service_installed: false,
            uptime_secs: None,
        }
    }
}

struct MockServiceCtl;

#[async_trait::async_trait]
impl ServiceCtl for MockServiceCtl {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()> {
        tracing::info!("Mock installing service for strategy: {}", strategy.display_name);
        Ok(())
    }
    async fn remove(&self) -> anyhow::Result<()> {
        tracing::info!("Mock removing service");
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        tracing::info!("Mock starting service");
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        tracing::info!("Mock stopping service");
        Ok(())
    }
    async fn status(&self) -> anyhow::Result<RunningMode> {
        Ok(RunningMode::None)
    }
    async fn is_installed(&self) -> bool {
        false
    }
}

struct MockCatalog;

impl MockCatalog {
    fn sample() -> Vec<Strategy> {
        ["general", "general (ALT)", "general (ALT2)"]
            .iter()
            .map(|name| Strategy {
                id: name.to_string(),
                display_name: name.to_string(),
                category: Category::Mixed,
                description: "Mock preset for UI-only preview".to_string(),
                winws_args: vec!["--wf-tcp=80,443".to_string()],
                requires_lists: vec![],
            })
            .collect()
    }
}

impl StrategyCatalog for MockCatalog {
    fn all(&self) -> Vec<Strategy> {
        Self::sample()
    }
    fn by_id(&self, id: &str) -> Option<Strategy> {
        self.all().into_iter().find(|s| s.id == id)
    }
    fn by_category(&self, c: Category) -> Vec<Strategy> {
        self.all().into_iter().filter(|s| s.category == c).collect()
    }
}

fn main() -> anyhow::Result<()> {
    // Simple console logging for the example
    tracing_subscriber::fmt::init();

    let ui = MainWindow::new()?;

    // Window/taskbar icon, decoded from the bundled .ico.
    {
        use image::ImageReader;
        use std::io::Cursor;
        const ICON_BYTES: &[u8] = include_bytes!("../assets/icon.ico");
        if let Ok(img) = ImageReader::with_format(Cursor::new(ICON_BYTES), image::ImageFormat::Ico).decode() {
            let img = img.into_rgba8();
            let (w, h) = (img.width(), img.height());
            let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&img, w, h);
            ui.set_app_icon(slint::Image::from_rgba8(buf));
        }
    }

    // i18n: back the `I18n.t` callback with the JSON catalogs so text renders.
    // The Settings → Language control flips `I18n.lang` itself, so switching
    // works in the preview without a persistence backend.
    ui.global::<I18n>().on_t(|lang, key| zapret_ui::i18n::tr(lang.as_str(), key.as_str()).into());
    ui.global::<I18n>().set_lang("ru".into());
    ui.on_set_language(|code| println!("UI: Set language: {}", code));

    // Populate strategies. Favorites are kept in a shared set and the model is
    // rebuilt (favorites first) whenever the star is toggled.
    let catalog = MockCatalog;
    let favorites = Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let rebuild_strategies = {
        let favorites = favorites.clone();
        let ui_weak = ui.as_weak();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let favs = favorites.borrow();
                let q = ui.get_strategies_query().to_string().trim().to_lowercase();
                let mut list: Vec<Strategy> = MockCatalog
                    .all()
                    .into_iter()
                    .filter(|s| {
                        q.is_empty()
                            || format!("{} {} {}", s.id, s.display_name, s.description)
                                .to_lowercase()
                                .contains(&q)
                    })
                    .collect();
                list.sort_by_key(|s| if favs.iter().any(|f| f == &s.id) { 0 } else { 1 });
                let items: Vec<StrategyItem> = list
                    .iter()
                    .map(|s| {
                        let (pretty, alt) = zapret_ui::contracts::split_alt(&s.id);
                        StrategyItem {
                            id: s.id.as_str().into(),
                            display_name: s.display_name.as_str().into(),
                            category: format!("{:?}", s.category).into(),
                            description: s.description.as_str().into(),
                            pretty: pretty.into(),
                            alt: alt.into(),
                            favorite: favs.iter().any(|f| f == &s.id),
                        }
                    })
                    .collect();
                ui.set_strategies(Rc::new(slint::VecModel::from(items)).into());
            }
        }
    };
    let _ = &catalog;
    rebuild_strategies();
    {
        let favorites = favorites.clone();
        let rebuild_strategies = rebuild_strategies.clone();
        ui.on_toggle_favorite(move |id| {
            println!("UI: Toggle favorite: {}", id);
            let id = id.to_string();
            {
                let mut favs = favorites.borrow_mut();
                if let Some(pos) = favs.iter().position(|x| *x == id) {
                    favs.remove(pos);
                } else {
                    favs.push(id);
                }
            }
            rebuild_strategies();
        });
    }

    // Search: re-filter the mock list (mirrors the real backend contract).
    {
        let rebuild_strategies = rebuild_strategies.clone();
        ui.on_strategies_search(move |q| {
            println!("UI: Search strategies: {}", q);
            rebuild_strategies();
        });
    }

    // Logs callbacks (mock): echo the contract so the preview behaves.
    {
        let ui_weak = ui.as_weak();
        ui.on_logs_query_changed(move |grep, level| {
            println!("UI: Logs filter changed: grep={} level={}", grep, level);
            let _ = ui_weak.upgrade();
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_logs_clear_clicked(move || {
            println!("UI: Logs clear clicked");
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_log_lines(Rc::new(slint::VecModel::from(Vec::<LogLineItem>::new())).into());
                ui.set_log_text("".into());
            }
        });
    }
    ui.on_open_log_file_clicked(|| println!("UI: Open log file clicked"));
    ui.on_open_url_clicked(|url| println!("UI: Open URL: {}", url));

    // Set initial status
    ui.set_status_installed(true);
    ui.set_status_installed_version("v1.0.0-mock".into());
    ui.set_status_running_mode("None".into());

    // Wire up some callbacks with simple logging. Start/Stop simulate the real
    // backend round-trip with a short delay so the power-button spinner +
    // transitional label are previewable (the window clears `is_busy` once the
    // mock "status" lands).
    {
        let ui_weak = ui.as_weak();
        ui.on_start_clicked(move |strat_id| {
            println!("UI: Start clicked with strategy: {}", strat_id);
            let ui_weak = ui_weak.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_running_mode("UserProcess".into());
                    ui.set_status_winws_pid(4242);
                    ui.set_is_busy(false);
                }
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_stop_clicked(move || {
            println!("UI: Stop clicked");
            let ui_weak = ui_weak.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_running_mode("None".into());
                    ui.set_status_winws_pid(0);
                    ui.set_is_busy(false);
                }
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_service_start_clicked(move || {
            println!("UI: Service start clicked");
            let ui_weak = ui_weak.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_running_mode("WindowsService".into());
                    ui.set_is_busy(false);
                }
            });
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_service_stop_clicked(move || {
            println!("UI: Service stop clicked");
            let ui_weak = ui_weak.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(1400), move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_status_running_mode("None".into());
                    ui.set_is_busy(false);
                }
            });
        });
    }
    ui.on_install_clicked(|| {
        println!("UI: Install clicked");
    });
    ui.on_update_clicked(|| {
        println!("UI: Update clicked");
    });
    ui.on_strategy_selected(|strat_id| {
        println!("UI: Strategy selected: {}", strat_id);
    });
    ui.on_service_install_clicked(|strat_id| {
        println!("UI: Install service for strategy: {}", strat_id);
    });
    ui.on_service_remove_clicked(|| {
        println!("UI: Service remove clicked");
    });
    ui.on_open_folder_clicked(|| {
        println!("UI: Open folder clicked");
    });
    ui.on_refresh_status_clicked(|| {
        println!("UI: Refresh status clicked");
    });

    // Strategy tester (mock): fill the results table immediately so the page can
    // be previewed without a real winws/test backend.
    {
        let ui_weak = ui.as_weak();
        ui.on_test_start_clicked(move || {
            println!("UI: Test strategies clicked");
            if let Some(ui) = ui_weak.upgrade() {
                let mk = |id: &str, pretty: &str, alt: &str, ok: i32, total: i32, latency: i32, rank: i32, best: bool| TestResultItem {
                    id: id.into(),
                    display_name: id.into(),
                    pretty: pretty.into(),
                    alt: alt.into(),
                    ok,
                    total,
                    latency,
                    rank,
                    is_best: best,
                    favorite: false,
                };
                let rows = vec![
                    mk("general (ALT2)", "general", "ALT2", 12, 12, 184, 1, true),
                    mk("general", "general", "", 10, 12, 203, 2, false),
                    mk("general (ALT)", "general", "ALT", 7, 12, 311, 3, false),
                    mk("general (SIMPLE FAKE)", "general", "SIMPLE FAKE", 0, 12, 0, 4, false),
                ];
                ui.set_test_results(Rc::new(slint::VecModel::from(rows)).into());
                ui.set_test_best_id("general (ALT2)".into());
                ui.set_test_total(4);
                ui.set_test_current(4);
                ui.set_test_running(false);
            }
        });
    }
    ui.on_test_cancel_clicked(|| {
        println!("UI: Test cancel clicked");
    });
    ui.on_test_use_strategy(|id| {
        println!("UI: Use tested strategy: {}", id);
    });

    // DPI bypass tuning (mock): seed initial state and echo the callbacks.
    ui.set_game_filter("off".into());
    ui.set_ipset_mode("loaded".into());
    ui.set_ipset_lines(2048);
    {
        let ui_weak = ui.as_weak();
        ui.on_set_game_filter(move |slug| {
            println!("UI: Set game filter: {}", slug);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_game_filter(slug);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_set_ipset_mode(move |slug| {
            println!("UI: Set ipset mode: {}", slug);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_ipset_mode(slug);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_update_ipset_clicked(move || {
            println!("UI: Update ipset list clicked");
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_ipset_ok(true);
                ui.set_ipset_msg("Updated — 2048 IP entries loaded".into());
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_update_hosts_clicked(move || {
            println!("UI: Update hosts file clicked");
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_hosts_ok(true);
                ui.set_hosts_msg("Out of date — review the entries and update your hosts file".into());
                // Demo the review modal with sample content.
                ui.set_hosts_content(
                    "# zapret hosts\n127.0.0.1 localhost\n\n# YouTube\n0.0.0.0 example.googlevideo.com\n0.0.0.0 r1---sn-example.googlevideo.com\n# Discord\n0.0.0.0 example.discord.com\n".into(),
                );
                ui.set_hosts_path("C:\\Windows\\System32\\drivers\\etc\\hosts".into());
                ui.set_hosts_dir("C:\\Windows\\System32\\drivers\\etc".into());
                ui.set_hosts_modal_open(true);
            }
        });
    }
    ui.on_copy_to_clipboard(|text| {
        println!("UI: Copy to clipboard ({} chars)", text.len());
    });

    // Notifications toggle (mock): seed on and echo the callback.
    ui.set_notifications(true);
    ui.on_set_notifications(|on| println!("UI: Set notifications: {}", on));

    // Admin gating preview. Flip to `true` to preview the normal (elevated)
    // state with the banner hidden and the admin-only buttons enabled.
    ui.set_is_admin(false);
    ui.on_restart_as_admin(|| println!("UI: Restart as administrator clicked"));

    // Mock log lines
    let mk = |no: i32, ts: &str, lvl: &str, msg: &str| LogLineItem {
        line_no: no,
        timestamp: ts.into(),
        level: lvl.into(),
        message: msg.into(),
    };
    let log_lines = vec![
        mk(1, "2026-05-23T16:14:34.808277Z", "INFO", "zapret-ui started in UI-only mode"),
        mk(2, "2026-05-23T16:14:34.812000Z", "INFO", "Mock installer ready, version v1.0.0-mock"),
        mk(3, "2026-05-23T16:14:34.815000Z", "INFO", "3 strategies loaded from catalog"),
    ];
    // The LogsPage renders the selectable terminal from `log_text`, so seed it too
    // (not just `log_lines`) — otherwise the preview terminal stays empty.
    let log_text: String = log_lines
        .iter()
        .map(|l| format!("{} {} {}", l.timestamp, l.level, l.message))
        .collect::<Vec<_>>()
        .join("\n");
    ui.set_log_lines(Rc::new(slint::VecModel::from(log_lines)).into());
    ui.set_log_text(log_text.into());

    // App version for the stats strip / about page.
    ui.set_app_version(concat!("v", env!("CARGO_PKG_VERSION")).into());

    ui.run()?;
    Ok(())
}
