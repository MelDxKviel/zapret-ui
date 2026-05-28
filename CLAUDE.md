# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-binary Windows GUI (Rust + Slint) wrapping [`bol-van/zapret2`](https://github.com/bol-van/zapret2) — the upstream DPI-bypass engine — packaged for Windows via [`bol-van/zapret-win-bundle`](https://github.com/bol-van/zapret-win-bundle). The app downloads the bundle, ships a curated set of zapret2 strategies as Rust constants, and runs the chosen strategy either as a child process or a Windows service. Target platform is Windows 10/11 x64; the library half of the crate is partitioned so a future cross-platform port (nfqws2 + systemd) can slot in without re-architecting the orchestrator.

> The pre-`zapret-2` branch wrapped [`Flowseal/zapret-discord-youtube`](https://github.com/Flowseal/zapret-discord-youtube), a port that packaged the original `winws.exe` as `.bat` presets. That model is gone: `.bat` parsing, the runtime `LocalStrategyCatalog`, the game-filter / IPSet / hosts-merge knobs from `service.bat` — all removed. See `docs/MIGRATION.md` (TBD) for the rationale.

## Commands

```powershell
cargo build --release          # → target\release\zapret-ui.exe (single binary, no DLLs)
cargo test                     # run all tests
cargo test test_process_runner_lifecycle   # run one test by name
cargo run --example ui_only    # launch the Slint UI with mock backends (no real network/process/service)
cargo check --target x86_64-unknown-linux-gnu --lib   # cross-target sanity (CI runs this too)
```

`cargo run --example ui_only` is the fastest way to iterate on UI changes — it wires every callback to `println!`/mock data, so no bundle install or admin rights are needed. `tests/process_tests.rs` shells out to `rustc` to compile a stub `winws2.exe`, and the service tests exercise the real SCM only when run elevated (otherwise they assert the `NeedsElevation` path). The whole `tests/process_tests.rs` file is gated to `#[cfg(target_os = "windows")]` so the Linux library check stays clean.

## Architecture

**Ports-and-adapters.** `src/ports.rs` defines six traits — `Installer`, `Runner`, `ServiceCtl`, `StrategyCatalog`, `StrategyTester`, `DpiTuning`. `src/contracts.rs` holds the shared data types (`Strategy`, `RuntimeStatus`, `BackendCmd`, `UiEvent`, `DpiTuningState`, `HostlistInfo`). The `src/zapret/` modules are the concrete adapters. `src/app.rs` is the orchestrator that owns `Arc<dyn Trait>` handles and never depends on a concrete adapter — so `examples/ui_only.rs` swaps in mocks trivially.

**Two-channel message passing between UI and backend** (`src/app.rs`):
- UI callbacks (`on_start_clicked`, etc.) `try_send` a `BackendCmd` over an mpsc channel. `run_backend_loop` consumes them on a tokio task.
- The backend emits `UiEvent`s over a tokio `broadcast` channel. A listener task receives them and applies them to Slint properties via `slint::invoke_from_event_loop` (the only safe way to touch the UI from another thread).

Do not call Slint setters directly from backend tasks — always go through a `UiEvent` + the listener, or `invoke_from_event_loop`. The log buffer (`LOG_BUF`, `LOG_FILTER`) is `thread_local!` and lives on the Slint UI thread because both the event listener's closures and the UI callbacks run there.

**Status flow.** Almost every `BackendCmd` ends by calling `runner.detect_running()`, patching `service_installed`/`installed`, storing it in `AppState`, and broadcasting `UiEvent::Status`. A 5-second timer also fires `RefreshStatus`. `detect_running` prefers our own spawned child handle, then a running Windows service, then any `winws2.exe` by name; uptime is read from the OS process so it survives app restarts.

### Key adapters (`src/zapret/`)

- **`strategies.rs`** — the heart of strategy handling after the zapret2 migration. `builtin_strategies()` returns a static array of `StrategyDef`s grounded in the upstream `preset2_example.cmd` / `preset2_wireguard.cmd`. Each `StrategyDef::build` is a `fn(&Path) -> Vec<String>` that resolves the install dir into a complete `winws2.exe` argv — rebuilt on demand so a user-process install (`%APPDATA%`) and a service install (`%ProgramData%`) get correctly-rooted paths without caching either. `BuiltinCatalog` is the `StrategyCatalog` impl; `check_required_files` reports missing inputs before winws2 dies on startup. (No `.bat` files involved — that whole layer is gone.)
- **`winbundle.rs`** (`WinBundleSource`) — deliberately avoids `api.github.com`, which is DPI-blocked on the ISPs this tool targets. Resolves the latest snapshot from `commits/master.atom` (parsing the first `<entry>`'s commit SHA + `<updated>`) and downloads the archive from `codeload.github.com`. Caches in `bundle_cache.json` for offline fallback. The synthesised version string is `master@<sha7> (YYYY-MM-DD)` — non-semver, so `crate::zapret::updater::is_update_available` falls through to string compare, which is exactly what we want.
- **`installer.rs`** — downloads + extracts to a temp dir, locates the `zapret-winws/` subtree inside the bundle (one level under `<repo>-<branch>/`), promotes its contents to a fresh `promoted/` dir, then atomically swaps into place (renaming the old install to `zapret.old.<ts>` with rollback). Drops everything else in the bundle (`arm64/`, `blockcheck/`, `cygwin/`, etc.) — we don't ship it.
- **`maintenance.rs`** (`ZapretDpiTuning`) — the slimmed-down DPI-tuning port after the zapret2 migration: refresh the curated hostlists in `<install>/files/` (currently just `list-youtube.txt` from the upstream bundle), and clear the Discord cache. The Flowseal-port game-filter / IPSet / hosts-merge concepts went away with `.bat` parsing (winws2 on Windows doesn't expose any of them).
- **`process.rs`** (`ProcessRunner`) — spawns `winws2.exe` with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`, working dir = install root (where `WinDivert64.sys`, `WinDivert.dll` and `cygwin1.dll` live next to the exe), piping stdout/stderr into `UiEvent::LogLine`. Stops via `CTRL_BREAK_EVENT` then kill.
- **`tester.rs`** (`ConnectivityTester`) — for each strategy it reuses the shared `Runner` to start winws2, waits `INIT_WAIT` (6 s, bumped from 4 s — WinDivert + Lua-script init takes longer than the old `winws`), then probes a built-in Discord/YouTube/Google/Cloudflare list concurrently via `reqwest`. An optional `utils/targets.txt` overrides the defaults (both Flowseal's legacy `Key = "value"` form and bare-URL lines are accepted). Scores by reachable-endpoint count, tie-broken by average latency; ranks best-first and the `TestStrategies` handler in `app.rs` auto-selects + persists the winner. Streams `UiEvent::TestStarted/TestProgress/TestResult/TestComplete`; cancellable via an `AtomicBool`. Surfaced as the **Tester** page (`ui/pages/tester.slint`).
- **`service.rs`** (`WindowsServiceCtl`) — SCM operations via `windows-service`. Service name is `"zapret"`. Install deletes any pre-existing service first; the ownership check accepts both `winws2.exe` (current) and `winws.exe` (legacy from an earlier zapret-ui build) so the upgrade replaces rather than rejects.
- **`elevation.rs`** — `check_elevation()` returns `Err(anyhow!("NeedsElevation"))` when not admin.
- **`src/selfupdate.rs`** (`GithubSelfUpdater`, top-level — *not* under `zapret/`, since it updates **zapret-ui itself**, not the zapret core) — the `SelfUpdater` port. Resolves the latest release tag from the repo's `releases.atom` feed (again avoiding `api.github.com`), downloads `zapret-ui.exe` + its `.sha256` from `github.com/.../releases/download/<tag>/`, verifies the checksum, then swaps the running binary via the Windows rename-self trick (rename current → `*.exe.old`, move new into place, roll back on failure). `cleanup_old_binary()` (called from `main.rs` at startup) removes the leftover `.old`. After a successful swap `app.rs` calls `relaunch_after_update()` (plain spawn with `--relaunch`, no elevation) and `process::exit(0)`. Driven by `BackendCmd::CheckSelfUpdate`/`SelfUpdate` and surfaced via `UiEvent::AppUpdateAvailable`/`AppUpToDate`/`AppUpdateProgress`/`AppUpdateError`. UI: a dismissible accent `UpdateBanner` on the dashboard, a row in About, and a check/update row in Settings → Updates. The startup check (gated by the shared `autoupdate_check` config flag) covers both the bundle and the app.

### Elevation model (important)

The UI runs **unelevated**. Only service operations need admin. When a `ServiceCtl` call returns an error whose string contains `"NeedsElevation"`, `app.rs` calls `relaunch_elevated(task, strategy, install_dir)`, which re-launches *this same exe* via `ShellExecuteW` with the `runas` verb and **correctly quoted** args: `--elevated-task=<task> [--strategy=<id>] --install-dir=<dir> --result-file=<path> --nonce=<n>`. `main.rs::parse_args` detects those flags, runs `run_elevated_task` against a fresh `WindowsServiceCtl`, writes its outcome to the nonce result file, and exits — it never shows the UI. The unelevated parent awaits that result file (`wait_for_elevated_result`) so it can surface the helper's real success/error. For **service-mode**, `run_elevated_task` first copies the install into the admin-only `%ProgramData%\zapret-ui\zapret` and locks its ACLs (`service::prepare_protected_dir`), then registers the `LocalSystem` service to run `winws2.exe` from *there* — never from the user-writable `%APPDATA%` dir. The recursive copy is unfiltered so `WinDivert64.sys`, `WinDivert.dll`, `cygwin1.dll` and the `lua/` + `files/` + `windivert.filter/` payloads all come along.

### Multi-platform partitioning (Phase 8 of the zapret2 migration)

The crate is split into a cross-platform library and a Windows-only binary so a future nfqws2/systemd port has somewhere to land:

- **Cross-platform (`#[cfg]`-free):** `contracts`, `ports`, `config`, `i18n`, `state`, `log`, `zapret::{installer, updater, winbundle, paths, strategies, tester}`, `selfupdate`.
- **Windows-only (gated in `lib.rs` and `zapret/mod.rs`):** `notify`, `tray`, `single_instance`, `winicon`, `winenv`, `zapret::{process, service, elevation, maintenance}`.
- **`Cargo.toml`:** Windows-only deps (`windows-service`, `tray-icon`, `clipboard-win`, `tauri-winrt-notification`, `winresource`) live under `[target.'cfg(target_os = "windows")'.dependencies]` so a Linux toolchain isn't asked to download them.
- **`src/main.rs`:** the binary as a whole is Windows-only by design — `compile_error!` fires on any non-Windows target with an explicit message ("the cross-platform port is tracked separately"). The `windows_subsystem = "windows"` crate attribute is gated on `target_os = "windows"` so it doesn't surface on Linux either.
- **CI:** `.github/workflows/release.yml` adds a `check-linux` job that runs `cargo check --lib --target x86_64-unknown-linux-gnu` on Ubuntu after installing `fontconfig`/`xkbcommon` (Slint's Linux deps). If anyone leaks a Win32 import into a cross-platform module, this job catches it.
- **`tests/process_tests.rs`** is gated to `#[cfg(target_os = "windows")]` at the file level — those tests exercise the SCM and sysinfo's Windows backend.

A local clone of `bol-van/zapret-win-bundle` is convenient as a reference (the upstream `.cmd` files document the exact winws2 flag combinations); add it at `.bundle-ref/` — it's already in `.gitignore`.

### UI (`ui/`)

Slint compiled by `build.rs` (`slint_build::compile("ui/main_window.slint")`); `slint::include_modules!()` generates the Rust bindings. Structure: `tokens.slint` (theme palettes, exported structs `StrategyItem`/`AppStatus`/`LogLineItem`/`HostlistInfoItem`), `components/`, `pages/`, `main_window.slint` (the `MainWindow` component + all callbacks/properties).

**Callback and property names in `main_window.slint` are a hand-maintained contract with both `src/app.rs` and `examples/ui_only.rs`.** Adding/renaming a `callback` or `in-out property` means updating the `on_*`/`set_*` calls in both Rust files or the build breaks. `DESIGN.md` is the design spec the UI was ported from.

### i18n (runtime translations)

Every user-visible string is written as `I18n.t(I18n.lang, "some.key")` — `I18n` is the global singleton in `ui/i18n.slint` (re-exported from `main_window.slint`). `lang` is threaded through as the first argument *purely* to make each binding depend on it, so flipping the language re-renders every translated string with no extra plumbing. The `t` callback is implemented in Rust (`src/i18n.rs`) and looks the key up in the flat JSON catalogs `src/locales/{ru,en}.json` (embedded via `include_str!`). A unit test asserts `ru.json` and `en.json` have identical key sets — keep them in sync when adding strings.

`app.rs` registers the callback (`ui.global::<I18n>().on_t(...)`) and seeds `I18n.lang` from `AppConfig::language` (default `Ru`); the Settings → Language control flips `I18n.lang` itself (instant re-render) and fires `set_language` so Rust persists it. `examples/ui_only.rs` registers the same callback. The few status strings the backend builds (hostlist refresh / Discord cache) are also localized via `crate::i18n::tr` using the config language. `ui_only.rs` must register `on_t` too or all text renders blank.

### Slint 1.x gotchas (this project has hit all of these)

- Fonts are imported at compile time only; bundled `.ttf`s live in `ui/assets/fonts/` and are referenced from Slint.
- No `oklch()` — colors must be hex literals (the original design's oklch values were sampled to hex).
- No string `substring`/slicing — parse strings in Rust (e.g. `contracts::split_alt`) and pass the parts in as struct fields.
- Define-before-use ordering applies to components/globals.

## Notes / traps

- Strategies are a **fixed Rust constant** (`crate::zapret::strategies::BUILTIN`). To add or tweak one, edit `src/zapret/strategies.rs` — there's no `.bat` to drop in. The `StrategyDef::build` fn must produce absolute argv elements (paths via `Path::display()` rather than hand-built strings) so a service install (rooted at `%ProgramData%`) and a user-process install (`%APPDATA%`) both resolve their `lua/` and `files/` references correctly.
- Paths: config `%APPDATA%\zapret-ui\config.toml`, install dir `%APPDATA%\zapret-ui\zapret\` (overridable via `install_dir_override`), logs `%APPDATA%\zapret-ui\logs\app.log`. `AppConfig::load` self-heals a corrupt config by backing it up to `.toml.bak`.
- `AppConfig::migrate_unknown_last_strategy` is the zapret-2 migration shim: on launch, `main.rs` resets `last_strategy` if it doesn't match any current builtin id (covers configs from the pre-zapret-2 build that referenced Flowseal-style `"general (ALT2)"` ids).
- Logging (`src/log.rs`) tees `tracing` output to both the rolling log file and the broadcast channel that feeds the in-app Logs page.
- Single-instance is enforced with a named mutex; a second launch focuses the existing window (`src/single_instance.rs`).
- Tests under `tests/` pull in source modules via `#[path = "../src/..."]` includes rather than `use zapret_ui::...`, so a test file compiles only the modules it lists.
- CI (`.github/workflows/release.yml`) runs `cargo test` + `cargo build --release` on `windows-2022`, plus the cross-target `check-linux` job on `ubuntu-latest`; tagging `v*` publishes `zapret-ui.exe` to a GitHub Release.
