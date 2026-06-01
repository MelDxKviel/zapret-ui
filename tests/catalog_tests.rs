#[path = "../src/contracts.rs"]
pub mod contracts;

#[path = "../src/ports.rs"]
pub mod ports;

pub mod zapret {
    #[path = "../../src/zapret/batparse.rs"]
    pub mod batparse;

    #[path = "../../src/zapret/catalog.rs"]
    pub mod catalog;
}

use ports::StrategyCatalog;
use tempfile::TempDir;
use zapret::catalog::LocalStrategyCatalog;

/// Build a throwaway install dir containing a couple of preset `.bat` files
/// shaped like the real zapret distribution, plus a service.bat that must be
/// ignored. Returns the `TempDir` guard (kept alive by the caller) so parallel
/// tests get isolated, auto-cleaned directories instead of a shared pid path.
fn make_fixture() -> TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    let winws = |name: &str| {
        format!(
            "@echo off\r\nset \"BIN=%~dp0bin\\\"\r\nset \"LISTS=%~dp0lists\\\"\r\n\
         start \"zapret: {name}\" /min \"%BIN%winws.exe\" --wf-tcp=80,443 \
         --hostlist=\"%LISTS%list-general.txt\" --dpi-desync=fake\r\n"
        )
    };

    std::fs::write(dir.join("general.bat"), winws("general")).unwrap();
    std::fs::write(dir.join("general (ALT).bat"), winws("alt")).unwrap();
    // service.bat must be skipped by the catalog.
    std::fs::write(
        dir.join("service.bat"),
        "@echo off\r\nrem control script\r\n",
    )
    .unwrap();
    tmp
}

#[test]
fn scans_bat_presets_and_skips_service() {
    let tmp = make_fixture();
    let catalog = LocalStrategyCatalog::new(tmp.path().to_path_buf());
    let all = catalog.all();

    assert_eq!(
        all.len(),
        2,
        "should find exactly the two preset .bat files, got {}",
        all.len()
    );
    assert!(
        all.iter().all(|s| !s.id.eq_ignore_ascii_case("service")),
        "service.bat must be skipped"
    );
    // "general" is ranked first.
    assert_eq!(all[0].id, "general");
}

#[test]
fn parses_resolved_args() {
    let tmp = make_fixture();
    let dir = tmp.path();
    let catalog = LocalStrategyCatalog::new(dir.to_path_buf());
    let s = catalog.by_id("general").expect("general should exist");

    assert_eq!(s.winws_args[0], "--wf-tcp=80,443");
    // %LISTS% resolved to an absolute path, no batch variables remain.
    assert!(s
        .winws_args
        .iter()
        .any(|a| a.contains("list-general.txt") && a.contains(&dir.display().to_string())));
    assert!(
        s.winws_args.iter().all(|a| !a.contains('%')),
        "no unresolved %vars%"
    );

    assert!(catalog.by_id("does-not-exist").is_none());
}

#[test]
fn empty_dir_yields_no_strategies() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = LocalStrategyCatalog::new(tmp.path().to_path_buf());
    assert!(
        catalog.all().is_empty(),
        "empty install dir should have no strategies"
    );
}
