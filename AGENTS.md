# AGENTS.md

Read this file before anything else in the repo.

## What this is

A native desktop GUI written in Rust + Slint, maintained as **one application
for Windows and macOS** in this repository. The interface, business logic,
configuration, translations, logging, Telegram proxy and release version are
shared. Only core formats, core lifecycle and OS integration differ:

| Target | Core | Execution | Release artifacts |
| --- | --- | --- | --- |
| Windows 10/11 x64 | [`Flowseal/zapret-discord-youtube`](https://github.com/Flowseal/zapret-discord-youtube), `.bat` strategies + `winws.exe` | Child process or Windows SCM service | `zapret-ui.exe` |
| macOS 14+ Apple Silicon | [`Flowseal/zapret-mac-discord-youtube`](https://github.com/Flowseal/zapret-mac-discord-youtube), TSV strategies + `utunws` | System launchd service | `zapret-ui-macos-arm64.zip` and `.dmg` |

Each release contains both targets at the same version, plus a `.sha256` for
each artifact. The GUI downloads the appropriate core at runtime; neither core
is embedded in the app. Intel Mac and Linux are not supported targets.

## Cross-platform change rules

- Implement features and fixes in shared code by default. A change to the UI,
  orchestration, settings, logging, Telegram or update policy must apply to both
  supported platforms. Do not create separate Windows/Mac copies of these layers
  or maintain a separate macOS feature branch as a permanent product fork.
- Use `src/ports.rs` and `src/contracts.rs` for common behavior. Keep `cfg` gates
  and native APIs at adapter boundaries (`src/zapret/`, `src/platform/macos/`
  and Windows desktop helpers). Share parsing, validation, networking and data
  handling when semantics match; keep core formats and privilege/lifecycle
  differences explicit. Do not force Windows assumptions onto the Mac core.
- Keep one Slint component tree under `ui/`, one set of callbacks, and the same
  RU/EN catalogs. Gate genuinely unavailable capabilities and use translated
  platform overrides instead of duplicating pages. Keep `examples/ui_only.rs`
  working on both platforms.
- Check both native CI jobs before merging or releasing. Run the relevant tests
  locally on the available OS, and state when the other OS has only CI coverage
  or is still unverified. A passing Mac build does not validate Windows FFI,
  and a passing Windows build does not validate launchd or AppKit integration.
- Keep Windows-only dependencies and APIs target-gated, preserve Windows asset
  names used by self-update, and keep versioning shared. An incomplete platform
  build must never publish a partial release. See `docs/RELEASING.md`.
- Keep background work bounded: no busy polling, idle redraw loops or eagerly
  started optional services. Stop timers while hidden where appropriate and
  keep the GUI responsive while either platform's core is working.

### macOS adapter

On macOS the core is `Flowseal/zapret-mac-discord-youtube`, downloaded as the
`ZapretMac-macOS-universal.zip` release asset (Atom feed, no GitHub API).
`src/zapret/macos_bundle.rs` handles its Payload format and data-path validation;
`src/zapret/macos/` provides the TSV catalog and launchd runner/service adapters.
Desktop helpers in `src/platform/macos/` replace Windows-only modules through
`cfg_attr`. `RunningMode::SystemService` means SCM on Windows or launchd on Mac;
the legacy Slint wire value remains `WindowsService` on both platforms.

The Mac GUI must run unelevated: upstream PF excludes root traffic, including
connectivity probes. Root operations use `osascript` authorization and upstream
install/stop scripts. There is no user-process mode or GameFilter on Mac.
Upstream owns `/Library/Application Support/ZapretMac` and its launchd label;
user lists stay in `~/Library/Application Support/ZapretMac` across core updates.
Detect its child through `proc_listpids`/`proc_pidpath` and read PPID/uptime with
a targeted `ps` query. `sysinfo` cannot read these fields after utunws drops UID,
so using it here falsely reports a healthy engine as stopped.
Do not run networking/service integration tests on CI. `cargo test` must remain
unprivileged and offline; Windows SCM/process tests are platform-gated.
Build on Apple Silicon with `sh scripts/build-macos.sh`; see `docs/macos.md`.
App self-update is manual on Mac to preserve the signed `.app` bundle; core
update remains supported. Keep translated `macos.*` overrides in both catalogs.
Slint and slint-build are pinned together to 1.17.1. The tray uses Slint's native
`SystemTrayIcon`; keep menu dispatch within Slint. A separate tray-icon/muda
event handler can consume Slint text-edit context-menu actions. Multiple muda
versions also duplicate Objective-C classes and break macOS release LTO;
verify the dependency tree when upgrading GUI dependencies.
Keep `profile.release.build-override.strip = false`: macOS 27 can reject stripped
proc-macro dylibs (Rust #157750), surfaced as E0463 during release compilation.

## Commands

Shared development checks on either supported OS (the first fetch requires
network access; subsequent commands are offline with the lockfile enforced):

```sh
cargo fetch --locked
cargo fmt --all -- --check
cargo clippy --frozen --all-targets --all-features -- -D warnings
cargo test --frozen --all-targets --all-features
cargo run --locked --example ui_only
```

Native release packaging:

```powershell
# Windows, x86_64-pc-windows-msvc
cargo build --locked --release          # target\release\zapret-ui.exe
```

```sh
# macOS, aarch64-apple-darwin
sh scripts/build-macos.sh               # dist/*.app, *.zip, *.dmg, *.sha256
```

`cargo run --example ui_only` is the fastest UI loop: mock backends, no core
install, network changes, service operations or admin rights. Runtime tests
must be unprivileged and must not contact external services by default; local
loopback tests are allowed. Explicit live networking/service checks belong on
a developer machine, never in CI. Windows process tests compile a stub
`winws.exe` with `rustc`, and Windows SCM tests are platform-gated; unelevated
service tests assert the `NeedsElevation` path. Use a normal terminal for tests.

Do not commit `.bundle-ref/` (local upstream tree with `winws.exe` / WinDivert).

## Architecture

**Single library.** `src/lib.rs` exports the application and shared modules.
`src/main.rs` is the executable entrypoint and imports `zapret_ui`; do not
redeclare library modules in the binary. Generated Slint types are included
once in `src/app/mod.rs` and reused by `examples/ui_only.rs`. Share these types
instead of compiling a second copy of the UI for mocks.

**Shared infrastructure.** `src/app_dirs.rs` defines the per-user app-data root
used by settings, downloaded core, logs and instance state; do not duplicate OS
path resolution. Upstream ZapretMac data remains in its own directory.
`src/download.rs` handles common bounded streaming downloads, progress and
SHA-256; core installation and app self-update reuse it. Keep core-specific
validation and platform-specific installation outside this transfer helper.
`src/release_feed.rs` parses Atom release entry links for both GUI and Mac core
updates; do not duplicate feed parsing or infer a release from note/title text.

**Ports-and-adapters.** `src/ports.rs` defines eight traits — `Installer`,
`SelfUpdater`, `Runner`, `ServiceCtl`, `StrategyCatalog`, `StrategyTester`,
`Maintenance`, `TelegramProxy`. `src/contracts.rs` holds the shared types (`Strategy`,
`RuntimeStatus`, `BackendCmd`, `UiEvent`). Concrete adapters live under
`src/zapret/` (plus `src/selfupdate.rs` for the app binary itself).

**Orchestrator** is `src/app/mod.rs` (not a single `app.rs`). It owns
`Arc<dyn Trait>` handles and never depends on a concrete adapter —
`examples/ui_only.rs` swaps in mocks. Helpers extracted from the orchestrator:

- `src/app/ui_models.rs` — Slint model rebuilders (strategies / logs / tester)
- `src/app/winexec.rs` — Windows-only `ShellExecuteW`, elevation relaunch, argv quoting

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
`RefreshStatus`. On Windows, `detect_running` prefers our spawned child handle,
then a running Windows service, then an owned `winws.exe`. On macOS, it resolves
the owned launchd service and validates its `utunws` child. Locally spawned
uptime uses a monotonic clock; service/fallback uptime comes from the OS so it
survives app restarts.

**Core update banner.** `UiEvent::UpdateAvailable` sets `has_update`. After a
successful core install (or a check that finds no newer version) emit
`UiEvent::UpToDate` so the Home “latest ↑” pill clears. The Slint
`AppStatus.update_available` binding also requires
`installed_version != latest_version`.

### Core adapters and shared services

- **`batparse.rs`** (Windows) — parses a `.bat` preset: extract the `^`-continued
  `winws.exe` line, quote-aware tokenize, substitute `%BIN%` / `%LISTS%` /
  `%~dp0` / `%GameFilter*%`. Game-filter values come from
  `read_game_filter(install_dir)` (`utils\game_filter.enabled`).
  `ensure_user_lists` recreates `lists\*-user.txt` that `winws.exe` refuses
  to start without.
- **`maintenance.rs`** — shared maintenance interface with platform-specific
  operations. Windows ports `service.bat` SETTINGS/UPDATES: game
  filter, IPSet filter, Update IPSet List, Update Hosts File. The Windows hosts
  update uses the existing one-shot UAC helper, preserves unrelated entries,
  and leaves a backup next to the system hosts file.
  macOS supports the upstream IPSet/user lists; GameFilter is unavailable and
  hosts changes are reviewed manually. Surfaced as **DPI bypass tuning** on
  Settings; core tuning applies on next start / service reinstall.
- **`catalog.rs`** (Windows) / **`macos/catalog.rs`** — discover strategies at
  runtime from installed `.bat` presets / `strategies.tsv`. Empty catalog if
  nothing is installed. Never hardcode a cross-platform strategy list.
- **`github.rs`** — never call `api.github.com` (DPI-blocked on the ISPs this
  tool targets). Windows core version comes from
  `raw.githubusercontent.com/.../version.txt`, archive from `codeload.github.com`.
  Mac core releases use `releases.atom` + the upstream universal ZIP asset.
  Cached release on failure.
- **`installer.rs`** — shared download/extraction and rollback lifecycle;
  platform-specific validation/promote steps normalize each core distribution.
  Atomically swap into place (old dir → `zapret.old.<ts>` + rollback).
  Writes `version.txt` from `.service/version.txt` or the release tag.
- **`macos_bundle.rs`**, **`macos/runtime.rs`**, **`macos/service.rs`** — validate
  Payload and user data, then invoke upstream install/stop scripts through
  authorization. Restore PF/TCP state on stop/failure; retain user lists.
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
  not the core. Candidates from `releases.atom` (no `api.github.com`); bare tags
  and incomplete releases are skipped until the target's artifact + valid
  `.sha256` are available (`zapret-ui.exe` on Windows, ARM64 ZIP on Mac).
  Windows downloads/verifies the exe and performs a rename-self swap, with
  `cleanup_old_binary()` at startup. After a successful swap the orchestrator
  calls `relaunch_after_update()` (`--relaunch`) and `process::exit(0)`.
  Mac update detection opens the release for manual replacement of the whole
  signed `.app`; never replace just its executable.

### Telegram proxy

`src/telegram/` is a native, local-only MTProto → Telegram WSS bridge inspired
by Flowseal/tg-ws-proxy, not a bundled Python subprocess. `LocalTelegramProxy`
implements the `TelegramProxy` port. It is stopped on app launch by default; no
listener, TLS context, connection pool, polling or task is created until Start.
An explicit opt-in `telegram_autostart` preference starts it when the app opens,
independently of the core and the app's separate login/startup preference.
Hiding Telegram disables its startup preference as well as stopping the proxy.
Stop joins the listener and every connection task. Keep it independent of the
zapret core, strategy testing and elevation.

`src/app/telegram.rs` serializes user actions with an on-demand task + mutex,
outside the core command queue so a download cannot block Stop. UI updates still
go through `UiEvent`. Settings are saved before Start; edit only while stopped.
Hiding the blue entry in the sidebar footer, immediately above the status pill,
also stops the proxy. Its page is instantiated only while selected
(`ui/pages/telegram.slint`); keep mocks in `ui_only` synced.

`protocol.rs` implements MTProxy SHA256/AES-256-CTR key derivation and stream
translation, retaining MTProto payload encryption. `transport.rs` sends complete
transport packets as WS binary messages, validates TLS names even for IP
overrides, retries official domains, and optionally falls back to direct TCP.
Buffers/connection count are bounded. Never log secrets or proxy links. No CF
relay domains, public listen addresses, certificate bypass or keepalive pool.

Create Telegram listeners and outbound connections through `TcpSocket`, which
sets non-inheritable Windows handles atomically. Direct Mio-backed
`TcpListener::bind` / `TcpStream::connect` can leak sockets into a spawned winws
or helper process and keep the proxy port occupied after Stop. Do not work around
that Windows handle leak with `SO_REUSEADDR`. On Unix, enable `SO_REUSEADDR` on
the listener (as Tokio/Mio normally does) so TIME_WAIT does not block a restart;
never enable `SO_REUSEPORT`. A live exact-address or wildcard listener must
still prevent another proxy from binding the port.

`cargo test --lib telegram` runs local protocol/lifecycle/bridge tests. The
ignored `live_telegram_wss_mtproto_roundtrip` test sends an unauthenticated
req_pq_multi to Telegram (no account or client config), and must be explicitly
opted into with `-- --ignored` when network access is available.

### Elevation model

**Windows release builds** embed a `requireAdministrator` manifest (`build.rs` →
`embed_windows_resources`, profile-aware). **Dev builds stay `asInvoker`**
(`cargo run`, `ui_only`, tests) so the mock UI does not UAC on every launch.

When a `ServiceCtl` error string contains `"NeedsElevation"` (dev / unelevated
only, Windows), `app/mod.rs` calls `relaunch_elevated(...)`: `ShellExecuteW` + `runas`
with quoted args `--elevated-task=… [--strategy=…] --install-dir=… --result-file=…
--nonce=…`. `main.rs::parse_args` runs `run_elevated_task` against a fresh
`WindowsServiceCtl`, writes the nonce result file, and exits — no UI. The
parent awaits `wait_for_elevated_result`. Service-mode copies the install into
admin-only `%ProgramData%\zapret-ui\zapret` and points LocalSystem at *that*
path, never `%APPDATA%`.

**macOS GUI always stays unelevated**, including release builds and probes.
Only the core's privileged operations use the system authorization dialog and
root-owned upstream installation. Do not reuse the Windows relaunch-elevated
path on Mac or run the GUI/tester with `sudo`.

### UI (`ui/`)

Slint compiled by `build.rs` (`slint_build::compile("ui/main_window.slint")`);
`slint::include_modules!()` generates the Rust bindings.

`tokens.slint` (palettes + `StrategyItem` / `AppStatus` / `LogLineItem`) →
`components/` → `pages/` → `main_window.slint`.

Keep `std-widgets` `Palette.color-scheme` synchronized with `ThemePalette`.
The native TextEdit used for logs/hosts otherwise retains the OS theme and
can render unreadable text when the app theme differs.
On macOS, initialize the winit backend with `with_transparent(false)` before
creating windows and synchronize the native window theme through `winenv`.
Slint's transparent default can leave the system title bar empty/transparent.

**Callback and property names in `main_window.slint` are a hand-maintained
contract with both `src/app/mod.rs` and `examples/ui_only.rs`.** Add/rename a
`callback` or `in-out property` → update `on_*` / `set_*` in both Rust files
or the build breaks. `DESIGN.md` is the design spec the UI was ported from.

`StatusDot` pulses only while `testing`. A permanent `active` pulse forced a
full-window redraw every frame for the whole bypass session.

The native tray/menu-bar wrapper lives in `src/tray.rs`. Tray callbacks run on
the UI thread without polling. Do not replace Slint's global native-menu event
handler: the Logs and hosts TextEdit context menus need it for Copy/Paste.
The uptime timer stops while the main window is hidden and resyncs on reopening.

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

- Strategies are discovered at runtime by the target's `LocalStrategyCatalog`
  (`.bat` on Windows, TSV on Mac). There is no hardcoded list
  (`src/zapret/strategies.rs` and `tools/extract_strategies.rs` are gone).
- Per-user app data lives in `%APPDATA%\zapret-ui\` on Windows and
  `~/Library/Application Support/zapret-ui/` on Mac: `config.toml`, downloaded
  core in `zapret/` (`install_dir_override`), and `logs/app.log`. Mac upstream
  lists live separately in `~/Library/Application Support/ZapretMac/`.
  `AppConfig::load` self-heals a corrupt file by renaming it to `.toml.bak`.
- Logging (`src/log.rs`) tees `tracing` to the rolling file *and* the
  broadcast that feeds the Logs page. Timestamps are local RFC 3339.
  The Logs page shows `HH:MM:SS` and shortens `zapret_ui::…::module:` to the
  last segment (`ui_models::parse_log_line`).
- Single-instance: named mutex on Windows (`src/single_instance.rs`), advisory
  file lock and activation socket on macOS (`src/platform/macos/single_instance.rs`).
  A second process launch focuses the existing window.
- Tests import production code from `zapret_ui` by default. Do not duplicate
  the full module tree using `#[path]`: this creates separate global state and
  causes tests to compile/run the same modules more than once. A narrowly scoped
  `#[path]` is allowed for a pure adapter deliberately tested on the other OS
  (for example, the Windows `.bat` catalog fixtures on Mac). Native process/SCM
  tests remain Windows-gated; pure Mac bundle/catalog tests can run on Windows.
- CI (`.github/workflows/release.yml`) uses one native matrix: Windows x64 on
  `windows-2022`, Apple Silicon on `macos-15`. Both run formatting, clippy,
  offline tests and native packaging for PRs, supported branch pushes and manual
  runs. Only a pushed `v*` tag publishes: both jobs must pass, then all EXE/ZIP/DMG
  assets and checksums are uploaded to a draft, verified and published together.
  Mac builds use an ad-hoc signature by default. See `docs/RELEASING.md`.
