# AGENTS.md

Read this file before anything else in the repo.

## What this is

A single-binary Windows GUI (Rust + Slint) wrapping
[`Flowseal/zapret-discord-youtube`](https://github.com/Flowseal/zapret-discord-youtube),
a DPI-bypass tool. The app downloads the zapret distribution, parses its `.bat`
presets into runnable `winws.exe` command lines, and runs the chosen strategy
either as a child process or a Windows service. Target platform is
**Windows 10/11 x64 only** (Win32 FFI + `windows-service`).

## Commands

```powershell
cargo build --release          # → target\release\zapret-ui.exe (single binary, no DLLs)
cargo test                     # run all tests
cargo test test_process_runner_lifecycle   # run one test by name
cargo run --example ui_only    # Slint UI with mock backends (no network/process/service)
```

`cargo run --example ui_only` is the fastest UI loop — every callback is
`println!` / mock data, no zapret install or admin rights. Some tests
(`process_tests.rs`) shell out to `rustc` to compile a stub `winws.exe`.
Service tests hit the real SCM only when elevated; otherwise they assert the
`NeedsElevation` path.

Do not commit `.bundle-ref/` (local upstream tree with `winws.exe` / WinDivert).

## Architecture

**Ports-and-adapters.** `src/ports.rs` defines seven traits — `Installer`,
`SelfUpdater`, `Runner`, `ServiceCtl`, `StrategyCatalog`, `StrategyTester`,
`Maintenance`. `src/contracts.rs` holds the shared types (`Strategy`,
`RuntimeStatus`, `BackendCmd`, `UiEvent`). Concrete adapters live under
`src/zapret/` (plus `src/selfupdate.rs` for the app binary itself).

**Orchestrator** is `src/app/mod.rs` (not a single `app.rs`). It owns
`Arc<dyn Trait>` handles and never depends on a concrete adapter —
`examples/ui_only.rs` swaps in mocks. Helpers extracted from the orchestrator:

- `src/app/ui_models.rs` — Slint model rebuilders (strategies / logs / tester)
- `src/app/winexec.rs` — `ShellExecuteW`, elevation relaunch, argv quoting

**Two-channel UI ↔ backend** (`src/app/mod.rs`):

- UI callbacks (`on_start_clicked`, …) `try_send` a `BackendCmd` on an mpsc
  channel. `run_backend_loop` consumes them on a tokio task.
- The backend emits `UiEvent`s on a tokio `broadcast` channel. A listener
  applies them to Slint properties via `slint::invoke_from_event_loop` (the
  only safe way to touch the UI from another thread).

Do not call Slint setters from backend tasks — go through a `UiEvent` + the
listener, or `invoke_from_event_loop`. The log buffer (`LOG_BUF`, `LOG_FILTER`)
is `thread_local!` on the Slint UI thread.

**Status flow.** Almost every `BackendCmd` ends with `runner.detect_running()`,
patches `service_installed` / `installed`, stores it in `AppState`, and
broadcasts `UiEvent::Status`. A 10-second safety-net timer also fires
`RefreshStatus`. `detect_running` prefers our spawned child handle, then a
running Windows service, then an owned `winws.exe`. Locally spawned uptime
uses a monotonic clock; fallback uptime comes from the OS so it survives
app restarts.

**Core update banner.** `UiEvent::UpdateAvailable` sets `has_update`. After a
successful core install (or a check that finds no newer version) emit
`UiEvent::UpToDate` so the Home “latest ↑” pill clears. The Slint
`AppStatus.update_available` binding also requires
`installed_version != latest_version`.

### Key adapters

- **`batparse.rs`** — parses a `.bat` preset: extract the `^`-continued
  `winws.exe` line, quote-aware tokenize, substitute `%BIN%` / `%LISTS%` /
  `%~dp0` / `%GameFilter*%`. Game-filter values come from
  `read_game_filter(install_dir)` (`utils\game_filter.enabled`).
  `ensure_user_lists` recreates `lists\*-user.txt` that `winws.exe` refuses
  to start without.
- **`maintenance.rs`** — in-app port of `service.bat` SETTINGS/UPDATES: game
  filter, IPSet filter, Update IPSet List, Update Hosts File. No admin.
  Surfaced as **DPI bypass tuning** on Settings; applies on next start /
  service reinstall.
- **`catalog.rs`** — strategies are discovered at runtime by scanning `.bat`
  files in the install dir. Empty catalog if nothing is installed.
- **`github.rs`** — never call `api.github.com` (DPI-blocked on the ISPs this
  tool targets). Version from `raw.githubusercontent.com/.../version.txt`,
  archive from `codeload.github.com`. Cached release on failure.
- **`installer.rs`** — download + extract to temp, promote a single root
  subdir, atomically swap into place (old dir → `zapret.old.<ts>` + rollback).
  Writes `version.txt` from `.service/version.txt` (or the release tag).
- **`tcp.rs`** — idempotent TCP-timestamps preflight (`netsh`, resolved via
  `GetSystemDirectoryW` so PATH cannot hijack an elevated process). Cached
  for the process lifetime.
- **`process.rs`** (`ProcessRunner`) — TCP preflight, then spawn `winws.exe`
  with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`, cwd `bin\`, stdout/stderr
  through `tracing` (target `winws`, stderr at WARN). Stop: `CTRL_BREAK_EVENT`
  then kill. Tests must call `.with_tcp_preflight(false)` — `netsh set global`
  needs admin and debug builds are unelevated. Do not hard-fail process-mode
  start if the SET fails; query first and warn (upstream `service.bat`
  `:tcp_enable` never aborts the launch).
- **`tester.rs`** (`ConnectivityTester`) — port of `utils/test zapret.ps1`.
  Reuses the shared `Runner`, waits `INIT_WAIT`, probes HTTPS targets
  (`utils/targets.txt`, skip `PING:`; Discord/YouTube/Google/Cloudflare
  fallback). Score = reachable count, tie-break latency. Auto-selects the
  winner. Cancellable `AtomicBool`. Page: `ui/pages/tester.slint`.
- **`service.rs`** (`WindowsServiceCtl`) — SCM via `windows-service`, name
  `"zapret"`. Install stages into `%ProgramData%\zapret-ui\zapret`, locks
  ACLs, re-resolves the strategy against that copy. Deletes a pre-existing
  *owned* service first; refuses a same-named service that is not ours.
- **`elevation.rs`** — `check_elevation()` → `Err(anyhow!("NeedsElevation"))`
  when not admin.
- **`src/selfupdate.rs`** (`GithubSelfUpdater`) — updates **zapret-ui itself**,
  not the core. Latest tag from `releases.atom` (no `api.github.com`),
  downloads `zapret-ui.exe` + `.sha256`, verifies, Windows rename-self swap.
  `cleanup_old_binary()` at startup. After a successful swap the orchestrator
  calls `relaunch_after_update()` (`--relaunch`) and `process::exit(0)`.

### Elevation model

**Release builds** embed a `requireAdministrator` manifest (`build.rs` →
`embed_windows_resources`, profile-aware). **Dev builds stay `asInvoker`**
(`cargo run`, `ui_only`, tests) so the mock UI does not UAC on every launch.

When a `ServiceCtl` error string contains `"NeedsElevation"` (dev / unelevated
only), `app/mod.rs` calls `relaunch_elevated(...)`: `ShellExecuteW` + `runas`
with quoted args `--elevated-task=… [--strategy=…] --install-dir=… --result-file=…
--nonce=…`. `main.rs::parse_args` runs `run_elevated_task` against a fresh
`WindowsServiceCtl`, writes the nonce result file, and exits — no UI. The
parent awaits `wait_for_elevated_result`. Service-mode copies the install into
admin-only `%ProgramData%\zapret-ui\zapret` and points LocalSystem at *that*
path, never `%APPDATA%`.

### UI (`ui/`)

Slint compiled by `build.rs` (`slint_build::compile("ui/main_window.slint")`);
`slint::include_modules!()` generates the Rust bindings.

`tokens.slint` (palettes + `StrategyItem` / `AppStatus` / `LogLineItem`) →
`components/` → `pages/` → `main_window.slint`.

**Callback and property names in `main_window.slint` are a hand-maintained
contract with both `src/app/mod.rs` and `examples/ui_only.rs`.** Add/rename a
`callback` or `in-out property` → update `on_*` / `set_*` in both Rust files
or the build breaks. `DESIGN.md` is the design spec the UI was ported from.

`StatusDot` pulses only while `testing`. A permanent `active` pulse forced a
full-window redraw every frame for the whole bypass session.

### i18n

Every user-visible string is `I18n.t(I18n.lang, "some.key")`. `I18n` is the
global in `ui/i18n.slint`. `lang` is passed as the first argument so flipping
it re-renders every binding. The `t` callback is `src/i18n.rs` against
`src/locales/{ru,en}.json` (`include_str!`). A unit test asserts the two
catalogs have identical key sets — keep them in sync.

`app/mod.rs` registers `on_t` and seeds `I18n.lang` from
`AppConfig::language` (default `Ru`). Settings flips `I18n.lang` immediately
and fires `set_language` to persist. `examples/ui_only.rs` must register
`on_t` too or all text is blank. Backend-built status strings use
`crate::i18n::tr`.

### Slint 1.x gotchas (this project has hit all of these)

- Fonts import at compile time only (`ui/assets/fonts/`).
- No `oklch()` — hex literals only.
- No string `substring` / slicing — parse in Rust (`contracts::split_alt`)
  and pass parts as struct fields.
- Define-before-use for components / globals.

## Notes / traps

- Strategies are discovered at runtime by `LocalStrategyCatalog`. There is no
  hardcoded list (`src/zapret/strategies.rs` and `tools/extract_strategies.rs`
  are gone).
- Paths: config `%APPDATA%\zapret-ui\config.toml`, install
  `%APPDATA%\zapret-ui\zapret\` (`install_dir_override`), logs
  `%APPDATA%\zapret-ui\logs\app.log`. `AppConfig::load` self-heals a corrupt
  file by renaming it to `.toml.bak`.
- Logging (`src/log.rs`) tees `tracing` to the rolling file *and* the
  broadcast that feeds the Logs page. Timestamps are local RFC 3339.
  The Logs page shows `HH:MM:SS` and shortens `zapret_ui::…::module:` to the
  last segment (`ui_models::parse_log_line`).
- Single-instance: named mutex; a second launch focuses the existing window
  (`src/single_instance.rs`).
- Tests under `tests/` `#[path = "../src/..."]` include listed modules rather
  than `use zapret_ui::...`. A test file compiles only what it lists.
  `process_tests.rs` includes `src/zapret/mod.rs` (the whole adapter tree).
- CI (`.github/workflows/release.yml`) is `cargo test` + `cargo build --release`
  on `windows-2022`. Tag `v*` publishes `zapret-ui.exe`.
