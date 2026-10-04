use zapret_ui::contracts::{AuthorizationCancelled, Category, RuntimeStatus, Strategy};
use zapret_ui::ports::{Runner, StrategyCatalog, StrategyTester};
use zapret_ui::zapret::macos_bundle;
// Exercise the macOS catalog on either host, without networking or elevation.
pub mod contracts {
    pub use zapret_ui::contracts::*;
}
pub mod ports {
    pub use zapret_ui::ports::*;
}
pub mod zapret {
    pub use zapret_ui::zapret::macos_bundle;
}
#[path = "../src/zapret/macos/catalog.rs"]
mod mac_catalog;

fn fixture_payload(path: &std::path::Path) {
    for name in [
        "bin/utunws",
        "bin/ACTIVE_DISCORD_UDP.bin",
        "bin/quic_initial_www_google_com.bin",
        "bin/stun.bin",
        "bin/stun2.bin",
        "bin/tls_clienthello_4pda_to.bin",
        "bin/tls_clienthello_max_ru.bin",
        "bin/tls_clienthello_www_google_com.bin",
        "install.sh",
        "run.sh",
        "stop.sh",
        "restart.sh",
        "watchdog.sh",
        "test-strategies.sh",
        "update-app.sh",
        "strategies.tsv",
        "strategies/general-simple-fake.conf.in",
        "io.github.flowseal.zapretmac.plist.in",
        "ipset-none.txt",
        "ipset-any.txt",
    ] {
        let file = path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "fixture").unwrap();
    }
    std::fs::create_dir_all(path.join("default-lists")).unwrap();
    for name in macos_bundle::DEFAULT_LISTS {
        std::fs::write(path.join("default-lists").join(name), "fixture").unwrap();
    }
}

#[test]
fn incomplete_core_is_rejected_before_replacing_the_installed_version() {
    for missing in [
        "default-lists/list-general-user.txt",
        "default-lists/ipset-exclude-user.txt",
        "bin/tls_clienthello_max_ru.bin",
        "bin/stun2.bin",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("ZapretMac.app/Contents/Resources/Payload");
        fixture_payload(&payload);
        std::fs::remove_file(payload.join(missing)).unwrap();
        assert!(!macos_bundle::valid_payload(&payload), "{missing}");
        assert!(macos_bundle::promote_payload(temp.path()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn failed_install_cleans_up_in_the_same_shell_and_keeps_its_error() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("core ' with spaces");
    std::fs::create_dir(&source).unwrap();
    let data = temp.path().join("user ' data");
    let marker = source.join("cleaned");
    std::fs::write(
        source.join("stop.sh"),
        format!(
            "printf cleaned > {}\nexit 0\n",
            macos_bundle::shell_quote(marker.to_str().unwrap())
        ),
    )
    .unwrap();
    for exit_code in [23, 0] {
        let _ = std::fs::remove_file(&marker);
        std::fs::write(
            source.join("install.sh"),
            format!(
                "test \"$1\" = {} || exit 24\ntest \"$2\" = {} || exit 25\nexit {exit_code}\n",
                macos_bundle::shell_quote(source.to_str().unwrap()),
                macos_bundle::shell_quote(data.to_str().unwrap()),
            ),
        )
        .unwrap();
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &macos_bundle::install_command(&source, &data)])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(exit_code));
        assert_eq!(marker.exists(), exit_code != 0);
    }
}

#[cfg(unix)]
#[test]
fn user_data_rejects_symlinks_without_overwriting_their_targets() {
    use std::os::unix::fs::symlink;
    for linked in [
        "lists",
        "lists/list-general-user.txt",
        "selected-strategy",
        "ipset-mode",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let payload = temp.path().join("payload");
        fixture_payload(&payload);
        let data = temp.path().join("data");
        macos_bundle::initialize_user_data(&payload, &data).unwrap();
        let link = data.join(linked);
        let outside = temp.path().join("outside");
        if link.is_dir() {
            std::fs::remove_dir_all(&link).unwrap();
            std::fs::create_dir(&outside).unwrap();
        } else {
            std::fs::remove_file(&link).unwrap();
            std::fs::write(&outside, "leave untouched").unwrap();
        }
        symlink(&outside, &link).unwrap();
        assert!(
            macos_bundle::initialize_user_data(&payload, &data).is_err(),
            "{linked}"
        );
        if outside.is_file() {
            assert_eq!(
                std::fs::read_to_string(&outside).unwrap(),
                "leave untouched"
            );
        }
    }
}

#[test]
fn promotes_only_core_from_ditto_archive_with_resource_forks() {
    for prefix in [
        "ZapretMac.app/Contents/Resources/Payload",
        "Contents/Resources/Payload",
    ] {
        let temp = tempfile::tempdir().unwrap();
        fixture_payload(&temp.path().join(prefix));
        std::fs::create_dir(temp.path().join("__MACOSX")).unwrap();
        macos_bundle::promote_payload(temp.path()).unwrap();
        assert!(macos_bundle::valid_payload(temp.path()));
        assert!(!temp.path().join("__MACOSX").exists());
        assert!(!temp.path().join("ZapretMac.app").exists());
        assert!(!temp.path().join("Contents").exists());
    }
    let broken = tempfile::tempdir().unwrap();
    assert!(macos_bundle::promote_payload(broken.path()).is_err());
}

#[test]
fn catalog_requires_valid_ids_and_real_configs_and_refreshes() {
    let temp = tempfile::tempdir().unwrap();
    let catalog = mac_catalog::LocalStrategyCatalog::new(temp.path().into());
    assert!(catalog.all().is_empty());
    std::fs::create_dir(temp.path().join("strategies")).unwrap();
    std::fs::write(
        temp.path().join("strategies/general-alt2.conf.in"),
        "--filter-tcp=443",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("strategies.tsv"),
        "general-alt2\tgeneral (ALT2)\n../evil\tevil\ngeneral\tmissing\ngeneral-alt2\tduplicate\n",
    )
    .unwrap();
    let strategies = catalog.all();
    assert_eq!(strategies.len(), 1);
    assert_eq!(strategies[0].id, "general-alt2");
    assert_eq!(strategies[0].display_name, "general (ALT2)");
    assert!(strategies[0].winws_args.is_empty());
    assert_eq!(catalog.by_category(Category::Mixed).len(), 1);
    std::fs::remove_file(temp.path().join("strategies/general-alt2.conf.in")).unwrap();
    assert!(catalog.by_id("general-alt2").is_none());
}

struct Declined(std::sync::atomic::AtomicU32);
#[async_trait::async_trait]
impl Runner for Declined {
    async fn start(&self, _: &Strategy) -> anyhow::Result<u32> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(AuthorizationCancelled.into())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn detect_running(&self) -> RuntimeStatus {
        RuntimeStatus::default()
    }
}
#[tokio::test]
async fn declining_authorization_aborts_test_and_auto_engage_without_more_prompts() {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    let temp = tempfile::tempdir().unwrap();
    let runner = Arc::new(Declined(AtomicU32::new(0)));
    let tester =
        zapret_ui::zapret::tester::ConnectivityTester::new(runner.clone(), temp.path().into());
    let candidates = || {
        (0..3)
            .map(|i| Strategy {
                id: format!("general-alt{i}"),
                display_name: "test".into(),
                category: Category::Mixed,
                description: String::new(),
                winws_args: vec![],
                requires_lists: vec![],
            })
            .collect()
    };
    assert!(tester
        .test_all(candidates(), Box::new(|_| {}), Box::new(|_, _, _| {}))
        .await
        .is_err());
    assert_eq!(runner.0.load(Ordering::SeqCst), 1);
    assert!(tester
        .auto_engage(candidates(), Box::new(|_, _, _| {}))
        .await
        .is_err());
    assert_eq!(runner.0.load(Ordering::SeqCst), 2);
}
