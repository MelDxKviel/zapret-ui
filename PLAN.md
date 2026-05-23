# zapret-ui — Brief for Antigravity

Этот документ — самодостаточное ТЗ. Выполни всё сам, разбив работу между своими агентами. Внешней помощи не требуется.

## 1. Цели проекта

- Single-binary `.exe` под Windows x86_64.
- Удобный UI поверх `github.com/Flowseal/zapret-discord-youtube`, **полностью повторяющий `service.bat`**: все стратегии + `service_install` / `service_remove` / `service_start` / `service_stop` / `service_status`.
- Сам качает `zapret-discord-youtube` с GitHub, если его нет.
- Подхватывает уже установленную копию (как Windows-сервис или распакованную в папку).
- Автообновление zapret из релизов того же репо.
- Логи, прогресс установки, статус-индикатор, тёмная/светлая тема.

## 2. Стек (фиксированный)

| Тема | Выбор |
|---|---|
| Язык | Rust 1.80+ stable |
| UI | Slint 1.8+ (Rust API, embedded backend: `backend-winit` + `renderer-skia`) |
| Async | tokio (rt-multi-thread) |
| HTTP | `reqwest` с `rustls-tls` (без OpenSSL) |
| GitHub API | сырой reqwest на `/releases/latest` |
| Архивы | `zip` |
| Процессы | `tokio::process` + `sysinfo` + `windows-service` |
| Конфиг | `serde` + `toml` + `directories` в `%APPDATA%\zapret-ui\` |
| Логи | `tracing` + `tracing-subscriber` + `tracing-appender` |
| Эмбеддинг | `rust-embed` |
| Elevation | UAC re-launch себя через shell verb `runas` |
| CI | GitHub Actions, runner `windows-2022` |

## 3. Архитектура

```
            ┌────────────────────────────────┐
            │            Slint UI            │
            │  (main_window.slint, pages/*)  │
            └──────────────▲─────────────────┘
                           │ callbacks / properties
            ┌──────────────┴─────────────────┐
            │       App / Orchestrator       │
            │      (src/app.rs, main.rs)     │
            └──┬───────┬────────┬────────┬───┘
   BackendCmd │       │UiEvent │        │
   (mpsc)    ▼       ▲        ▼        ▼
        ┌────────┐ ┌─────────┐ ┌────────┐ ┌────────┐
        │Installer│ │Process │ │Strategy│ │Config  │
        │/Updater │ │/Service│ │ Catalog│ │/State  │
        └────────┘ └─────────┘ └────────┘ └────────┘
                  GitHub releases │ winws.exe / Windows Service
                                  ▼
                      %APPDATA%\zapret-ui\zapret\
```

Жёсткое правило: **UI никогда не вызывает backend напрямую**. Только через `mpsc<BackendCmd>` (UI → backend) и `broadcast<UiEvent>` (backend → UI).

## 4. Фаза 0 — Контракты (один агент, остальные ждут)

Этот шаг блокирует параллельную работу. Один агент пишет ровно эти файлы и больше ничего. После этого `contracts.rs` и `ports.rs` — **read-only** для всех остальных.

### `src/contracts.rs`

```rust
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Strategy {
    pub id: &'static str,                          // "discord_alt4"
    pub display_name: &'static str,                // "Discord — ALT 4"
    pub category: Category,
    pub description: &'static str,
    pub winws_args: &'static [&'static str],       // готовый argv для winws.exe
    pub requires_lists: &'static [&'static str],   // ["list-discord.txt", ...]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Category { Discord, Youtube, Mixed, Mgts, Rostelecom, Mts, Beeline, Other }

#[derive(Clone, Debug)]
pub enum BackendCmd {
    Install,
    CheckUpdate,
    Update,
    Start(String /* strategy_id */),
    Stop,
    ServiceInstall(String /* strategy_id */),
    ServiceRemove,
    ServiceStart,
    ServiceStop,
    RefreshStatus,
    OpenInstallFolder,
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    Status(RuntimeStatus),
    DownloadProgress { bytes: u64, total: Option<u64> },
    InstallProgress(InstallStage),
    LogLine(String),
    UpdateAvailable { current: String, latest: String, url: String },
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeStatus {
    pub installed: bool,
    pub installed_version: Option<String>,
    pub running_mode: RunningMode,
    pub active_strategy: Option<String>,
    pub winws_pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunningMode { #[default] None, UserProcess, WindowsService }

#[derive(Clone, Debug)]
pub enum InstallStage { Resolving, Downloading, Extracting, Verifying, Done }
```

### `src/ports.rs`

```rust
#[async_trait::async_trait]
pub trait Installer: Send + Sync {
    async fn is_installed(&self) -> bool;
    async fn installed_version(&self) -> Option<String>;
    async fn latest_version(&self) -> anyhow::Result<String>;
    async fn install_or_update(&self, on_progress: ProgressCb) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    async fn start(&self, strategy: &Strategy) -> anyhow::Result<u32>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn detect_running(&self) -> RuntimeStatus;
}

#[async_trait::async_trait]
pub trait ServiceCtl: Send + Sync {
    async fn install(&self, strategy: &Strategy) -> anyhow::Result<()>;
    async fn remove(&self) -> anyhow::Result<()>;
    async fn start(&self) -> anyhow::Result<()>;
    async fn stop(&self) -> anyhow::Result<()>;
    async fn status(&self) -> anyhow::Result<RunningMode>;
}

pub trait StrategyCatalog: Send + Sync {
    fn all(&self) -> &'static [Strategy];
    fn by_id(&self, id: &str) -> Option<&'static Strategy>;
    fn by_category(&self, c: Category) -> Vec<&'static Strategy>;
}
```

### Также в Фазе 0
- `Cargo.toml` (см. §10) с полным списком зависимостей.
- `src/main.rs` с `fn main() { println!("stub"); }`.
- `src/zapret/mod.rs` с `pub use` модулей.
- Пустые файлы модулей с `todo!()` чтобы крейт компилировался.

Только после `cargo check` без ошибок — стартует Фаза 1.

## 5. Фаза 1 — параллельные агенты

Правило: **каждый файл принадлежит одному агенту**. Пересечения только через read-only `contracts.rs` / `ports.rs`. Каждый агент пишет юнит-тесты под свой модуль.

### Agent A — Strategy Catalog
- Скачать `service.bat` из `main` ветки `Flowseal/zapret-discord-youtube`.
- Распарсить блоки между метками `:label` ... `goto :eof`, найти `winws.exe` запуски, нормализовать argv в массив.
- Сгруппировать по категориям (Discord / YouTube / Mixed / MGTS / Ростелеком / МТС / Билайн / Other).
- Сгенерировать `src/zapret/strategies.rs` как `pub const STRATEGIES: &[Strategy] = &[ ... ];`.
- Реализовать `StrategyCatalog` в `src/zapret/catalog.rs`.
- Покрытие — минимум: Discord general/alt/alt2-6/mgts/voice, YouTube general/alt-alt6, Combined, региональные пресеты что есть в .bat.
- Тест: `assert!(STRATEGIES.len() >= 20)` + проверка уникальности `id`.
- **Owns:** `src/zapret/strategies.rs`, `src/zapret/catalog.rs`, `tools/extract_strategies.rs`.

### Agent B — Installer / Updater
- GET `https://api.github.com/repos/Flowseal/zapret-discord-youtube/releases/latest`, парсить `tag_name` и `assets[].browser_download_url` (взять zip-ассет).
- Потоковая загрузка с эмиссией `DownloadProgress`.
- Распаковка в `%APPDATA%\zapret-ui\zapret\`. Атомарно: распаковать в `.tmp`, потом rename.
- Писать `version.txt` рядом.
- Сравнение версий — `semver` если ассет семверный, иначе string-eq тегов.
- Детект существующей установки в стандартных местах: дефолтный путь, рядом с `.exe`, путь из конфига.
- Кэш ответа GitHub на 6 часов (rate limit 60/час).
- Тесты с `mockito`.
- **Owns:** `src/zapret/installer.rs`, `src/zapret/updater.rs`, `src/zapret/github.rs`, `src/zapret/paths.rs`.

### Agent C — Process & Service Manager
- `Runner`: `tokio::process::Command` для `winws.exe` со скрытым окном (`CREATE_NO_WINDOW` через `CommandExt::creation_flags`), захват stdout/stderr построчно → `LogLine`. `cwd` = папка установки zapret (важно для `list-*.txt`).
- Детект уже запущенного: `sysinfo` по имени процесса + проверка Windows-сервиса `zapret` через `windows-service`.
- `ServiceCtl`: install/remove/start/stop/status через crate `windows-service` (не shell-out на `sc.exe`).
- Если операция требует admin и текущий процесс не elevated — возвращать `Err(NeedsElevation)`. Оркестратор перезапустит с `--elevated-task=...`.
- Чистая остановка: SIGBREAK → TerminateProcess fallback.
- Интеграционный тест с заглушечным `winws.exe` (echo-loop под `tests/fixtures/`).
- **Owns:** `src/zapret/process.rs`, `src/zapret/service.rs`, `src/zapret/elevation.rs`.

### Agent D — Config & State
- `AppConfig { last_strategy: Option<String>, autostart: bool, autoupdate_check: bool, install_dir_override: Option<PathBuf>, theme: Theme, minimize_to_tray: bool }`.
- Хранение в `%APPDATA%\zapret-ui\config.toml`.
- Атомарная запись через `tempfile` + rename.
- При битом файле — дефолты + бэкап старого как `.bak`.
- `AppState` — `Arc<RwLock<...>>` с рассылкой изменений через `tokio::sync::broadcast`.
- Тесты: round-trip конфига, fallback при повреждённом файле.
- **Owns:** `src/config.rs`, `src/state.rs`.

### Agent E — Slint UI
- Страницы:
  1. **Home** — большая кнопка Start/Stop, активная стратегия, статус (Not installed / Installed / Running user / Running service), кнопки «Install» и «Update available».
  2. **Strategies** — сайдбар категорий, карточки стратегий с фильтром поиска, кнопки «Use» и «Install as service».
  3. **Settings** — autostart with Windows, autoupdate toggle, путь установки, тема, mute tray.
  4. **Logs** — стрим строк, кнопка «Open log file», кнопка «Clear».
  5. **About** — версия UI, версия zapret, ссылки.
- Поверхность контактов с Rust:
  - Callbacks: `start-clicked`, `stop-clicked`, `strategy-selected(id)`, `install-clicked`, `update-clicked`, `set-strategy-as-service(id)`, `service-remove-clicked`, `open-folder-clicked`.
  - Properties: `status`, `active-strategy-id`, `strategies-model` (ListModel), `log-lines` (ListModel), `progress` (0..1), `theme`, `is-busy`.
- Дизайн-токены в `ui/tokens.slint`. Тёмная/светлая темы.
- Должен запускаться через `cargo run --example ui_only` с моковыми данными (мок-данные подаст оркестратор).
- **Owns:** `ui/main_window.slint`, `ui/pages/*.slint`, `ui/components/*.slint`, `ui/tokens.slint`, `assets/icons/*`.

### Agent F — Orchestrator / Glue
- `main.rs`: tokio runtime, инициализация tracing (файл `%APPDATA%\zapret-ui\logs\app.log` + канал в UI).
- `App::new(installer, runner, service_ctl, catalog, config, state)`.
- Каналы: `mpsc::channel::<BackendCmd>(64)`, `broadcast::channel::<UiEvent>(256)`.
- Привязка Slint callbacks → отправка `BackendCmd` в канал.
- Подписка UI на `UiEvent` через `slint::invoke_from_event_loop` (Slint event loop != tokio runtime).
- Системный трей через `tray-icon`, поведение «закрыть = свернуть в трей».
- Single-instance через named mutex; второй запуск делает focus в первое окно.
- Парсинг `--elevated-task=<name> [--strategy=<id>]` для UAC re-launch.
- `examples/ui_only.rs` — UI с моковыми реализациями портов.
- **Owns:** `src/main.rs`, `src/app.rs`, `src/tray.rs`, `src/single_instance.rs`, `src/log.rs`, `examples/ui_only.rs`.

### Agent G — Build / Packaging / CI
- Модерирует `Cargo.toml` (см. §10).
- `build.rs`: `winres` (иконка + версия + manifest), `slint_build::compile`.
- `assets/app.manifest`: `requestedExecutionLevel level="asInvoker"` — **НЕ** `requireAdministrator`. Admin запрашиваем только для service-ops через UAC re-launch.
- `[profile.release]`: `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `opt-level = "z"`.
- GitHub Actions workflow на `windows-2022`: build, тесты, артефакт `zapret-ui.exe`, релиз по тегу `v*`.
- `rust-toolchain.toml` фиксирует stable.
- Проверка финального бинаря через `dumpbin /dependents` — никаких внешних DLL кроме системных.
- **Owns:** `Cargo.toml`, `Cargo.lock`, `build.rs`, `assets/app.manifest`, `assets/icon.ico`, `.github/workflows/release.yml`, `rust-toolchain.toml`.

### (опц.) Agent H — Self-update самого `zapret-ui`
- Релизы UI на GitHub → проверка, скачивание, замена бинаря через crate `self-replace` или `self_update`, рестарт.
- Включается флагом в конфиге.
- **Owns:** `src/self_update.rs`.

## 6. Карта владения файлами

```
src/
├── main.rs                    F
├── app.rs                     F
├── contracts.rs               0  (read-only после Фазы 0)
├── ports.rs                   0  (read-only после Фазы 0)
├── config.rs                  D
├── state.rs                   D
├── log.rs                     F
├── tray.rs                    F
├── single_instance.rs         F
├── self_update.rs             H (опц.)
└── zapret/
    ├── mod.rs                 0  (только pub use)
    ├── catalog.rs             A
    ├── strategies.rs          A  (генерится)
    ├── installer.rs           B
    ├── updater.rs             B
    ├── github.rs              B
    ├── paths.rs               B
    ├── process.rs             C
    ├── service.rs             C
    └── elevation.rs           C
ui/                            E
assets/                        G  (E кладёт свои иконки в assets/icons)
build.rs                       G
Cargo.toml                     G
.github/workflows/             G
tools/extract_strategies.rs    A
tests/                         каждый агент свои в tests/<area>_*.rs
```

## 7. Фазы

- **Фаза 0** — один агент пишет контракты и скелет. После `cargo check` запускается Фаза 1.
- **Фаза 1** — A, B, C, D, E, F, G работают параллельно. Каждый модуль компилируется самостоятельно благодаря portам.
- **Фаза 2** — интеграция в `App`, ручной smoke-тест на реальной машине, фикс interface drift.
- **Фаза 3** — упаковка, иконка, релиз через CI.

## 8. Контрольный лист функционала service.bat

Agent A покрывает (минимум):
- [ ] Discord: general, alt, alt2, alt3, alt4, alt5, alt6, mgts, ipv4/ipv6 варианты
- [ ] YouTube: general, alt, alt2, alt3, alt4, alt5, alt6
- [ ] Discord Voice (UDP)
- [ ] Combined (Discord + YouTube) — пресет по умолчанию
- [ ] Региональные: МГТС, Ростелеком, МТС, Билайн (что есть в .bat)

Agent C повторяет:
- [ ] `service_install <strategy>` — регистрация Windows-сервиса `zapret` с argv стратегии
- [ ] `service_remove`
- [ ] `service_start` / `service_stop`
- [ ] `service_status` → `RUNNING` / `STOPPED` / отсутствует
- [ ] Прямой запуск без сервиса

## 9. Риски и заранее закрытые вопросы

- **Admin rights:** манифест `asInvoker`. Elevation — только пер-операция через UAC re-launch с `--elevated-task=...`.
- **Antivirus false positives** на `winws.exe`: задокументировать в About, ничего не обфусцировать.
- **Изменения в `service.bat` upstream:** парсер устойчив к перестановкам (ищем по `:label`, не по номерам строк). Fallback — встроенный snapshot последней известной версии в `rust-embed`.
- **GitHub rate limit:** анонимные 60/час. Кэш ответа в `config.toml` на 6 часов.
- **Slint event loop ≠ tokio runtime:** в Slint из tokio только через `slint::invoke_from_event_loop`.
- **Single binary:** только embedded backend Slint. Финальная проверка — `dumpbin /dependents`, кроме системных DLL ничего быть не должно.
- **Списки (`list-*.txt`):** живут в распакованном zapret. `cwd` процесса `winws.exe` = папка установки.

## 10. `Cargo.toml`

```toml
[package]
name = "zapret-ui"
version = "0.1.0"
edition = "2021"

[dependencies]
slint = { version = "1.8", features = ["backend-winit", "renderer-skia"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "sync", "fs", "io-util", "time"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "stream", "json"] }
zip = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
directories = "5"
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-appender = "0.2"
async-trait = "0.1"
windows-service = "0.7"
sysinfo = "0.32"
semver = "1"
tray-icon = "0.19"
rust-embed = "8"
tempfile = "3"

[build-dependencies]
slint-build = "1.8"
winres = "0.1"

[profile.release]
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
opt-level = "z"
```

## 11. Definition of Done

- `cargo build --release` собирает один `.exe` без внешних DLL.
- На чистой Windows-машине: `.exe` запускается, скачивает zapret, активирует выбранную стратегию, ставит/снимает сервис, переживает рестарт.
- При запуске на машине с уже установленным zapret или запущенным сервисом — корректно подхватывает состояние.
- Все юнит- и интеграционные тесты зелёные в GitHub Actions.
- Релиз `v0.1.0` опубликован через CI.
