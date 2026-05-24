# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A single-binary Windows GUI (Rust + Slint) wrapping [`Flowseal/zapret-discord-youtube`](https://github.com/Flowseal/zapret-discord-youtube), a DPI-bypass tool. The app downloads the zapret distribution, parses its `.bat` presets into runnable `winws.exe` command lines, and runs the chosen strategy either as a child process or a Windows service. Target platform is Windows 10/11 x64 only (much of the code uses Win32 FFI and `windows-service`).

## Commands

```powershell
cargo build --release          # → target\release\zapret-ui.exe (single binary, no DLLs)
cargo test                     # run all tests
cargo test test_process_runner_lifecycle   # run one test by name
cargo run --example ui_only    # launch the Slint UI with mock backends (no real network/process/service)
```

`cargo run --example ui_only` is the fastest way to iterate on UI changes — it wires every callback to `println!`/mock data, so no zapret install or admin rights are needed. Some tests (`process_tests.rs`) shell out to `rustc` to compile a stub `winws.exe`, and service tests exercise the real SCM only when run elevated (otherwise they assert the `NeedsElevation` path).

## Architecture

**Ports-and-adapters.** `src/ports.rs` defines four traits — `Installer`, `Runner`, `ServiceCtl`, `StrategyCatalog`. `src/contracts.rs` holds the shared data types (`Strategy`, `RuntimeStatus`, `BackendCmd`, `UiEvent`). The `src/zapret/` modules are the concrete adapters. `src/app.rs` is the orchestrator that owns `Arc<dyn Trait>` handles and never depends on a concrete adapter — so `examples/ui_only.rs` swaps in mocks trivially.

**Two-channel message passing between UI and backend** (`src/app.rs`):
- UI callbacks (`on_start_clicked`, etc.) `try_send` a `BackendCmd` over an mpsc channel. `run_backend_loop` consumes them on a tokio task.
- The backend emits `UiEvent`s over a tokio `broadcast` channel. A listener task receives them and applies them to Slint properties via `slint::invoke_from_event_loop` (the only safe way to touch the UI from another thread).

Do not call Slint setters directly from backend tasks — always go through a `UiEvent` + the listener, or `invoke_from_event_loop`. The log buffer (`LOG_BUF`, `LOG_FILTER`) is `thread_local!` and lives on the Slint UI thread because both the event listener's closures and the UI callbacks run there.

**Status flow.** Almost every `BackendCmd` ends by calling `runner.detect_running()`, patching `service_installed`/`installed`, storing it in `AppState`, and broadcasting `UiEvent::Status`. A 5-second timer also fires `RefreshStatus`. `detect_running` prefers our own spawned child handle, then a running Windows service, then any `winws.exe` by name; uptime is read from the OS process so it survives app restarts.

### Key adapters (`src/zapret/`)

- **`batparse.rs`** — the heart of strategy handling. Parses a `.bat` preset by extracting the `^`-continued `winws.exe` command line, tokenizing (quote-aware), and substituting batch vars (`%BIN%`, `%LISTS%`, `%~dp0`, `%GameFilter*%`) into absolute paths. `ensure_user_lists` recreates the `lists\*-user.txt` files that `winws.exe` refuses to start without.
- **`catalog.rs`** (`LocalStrategyCatalog`) — strategies are discovered **at runtime** by scanning `.bat` files in the install dir, *not* hardcoded. If nothing is installed, the catalog is empty.
- **`github.rs`** — deliberately avoids `api.github.com`, which is DPI-blocked on the ISPs this tool targets. Version comes from `raw.githubusercontent.com/.../version.txt` and the archive from `codeload.github.com`; both reachable when the API is not. Falls back to a cached release on failure.
- **`installer.rs`** — downloads + extracts to a temp dir, promotes a single root subdir, then atomically swaps it into place (renaming the old install to `zapret.old.<ts>` with rollback).
- **`process.rs`** (`ProcessRunner`) — spawns `winws.exe` with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP`, working dir `bin\`, piping stdout/stderr into `UiEvent::LogLine`. Stops via `CTRL_BREAK_EVENT` then kill.
- **`service.rs`** (`WindowsServiceCtl`) — SCM operations via `windows-service`. Service name is `"zapret"`. Install deletes any pre-existing service first.
- **`elevation.rs`** — `check_elevation()` returns `Err(anyhow!("NeedsElevation"))` when not admin.

### Elevation model (important)

The UI runs **unelevated**. Only service operations need admin. When a `ServiceCtl` call returns an error whose string contains `"NeedsElevation"`, `app.rs` calls `relaunch_elevated(task, strategy)`, which re-launches *this same exe* via `ShellExecuteW` with the `runas` verb and `--elevated-task=<task> [--strategy=<id>]`. `main.rs::parse_args` detects those flags, runs `run_elevated_task` against a fresh `WindowsServiceCtl`, and exits — it never shows the UI. So service actions briefly spawn a second, elevated process that does the SCM work and quits.

### UI (`ui/`)

Slint compiled by `build.rs` (`slint_build::compile("ui/main_window.slint")`); `slint::include_modules!()` generates the Rust bindings. Structure: `tokens.slint` (theme palettes, exported structs `StrategyItem`/`AppStatus`/`LogLineItem`), `components/`, `pages/`, `main_window.slint` (the `MainWindow` component + all callbacks/properties).

**Callback and property names in `main_window.slint` are a hand-maintained contract with both `src/app.rs` and `examples/ui_only.rs`.** Adding/renaming a `callback` or `in-out property` means updating the `on_*`/`set_*` calls in both Rust files or the build breaks. `DESIGN.md` is the design spec the UI was ported from.

### Slint 1.x gotchas (this project has hit all of these)

- Fonts are imported at compile time only; bundled `.ttf`s live in `ui/assets/fonts/` and are referenced from Slint.
- No `oklch()` — colors must be hex literals (the original design's oklch values were sampled to hex).
- No string `substring`/slicing — parse strings in Rust (e.g. `contracts::split_alt`) and pass the parts in as struct fields.
- Define-before-use ordering applies to components/globals.

## Notes / traps

- `src/zapret/strategies.rs` and `tools/extract_strategies.rs` are **legacy/dead**: the runtime uses `LocalStrategyCatalog` (`.bat` scanning), not the auto-generated `STRATEGIES` const. Don't wire new code to them.
- `src/self_update.rs` is a stub.
- Paths: config `%APPDATA%\zapret-ui\config.toml`, install dir `%APPDATA%\zapret-ui\zapret\` (overridable via `install_dir_override`), logs `%APPDATA%\zapret-ui\logs\app.log`. `AppConfig::load` self-heals a corrupt config by backing it up to `.toml.bak`.
- Logging (`src/log.rs`) tees `tracing` output to both the rolling log file and the broadcast channel that feeds the in-app Logs page.
- Single-instance is enforced with a named mutex; a second launch focuses the existing window (`src/single_instance.rs`).
- Tests under `tests/` pull in source modules via `#[path = "../src/..."]` includes rather than `use zapret_ui::...`, so a test file compiles only the modules it lists.
- CI (`.github/workflows/release.yml`) runs `cargo test` + `cargo build --release` on `windows-2022`; tagging `v*` publishes `zapret-ui.exe` to a GitHub Release.
