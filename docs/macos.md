# macOS Apple Silicon

Требования: Mac с M1/M2/M3/M4 или новее, **macOS 14 Sonoma или новее**, учётная запись с возможностью подтвердить права администратора. Rosetta не нужна.

Ядро на Mac — [Flowseal/zapret-mac-discord-youtube](https://github.com/Flowseal/zapret-mac-discord-youtube). Приложение загружает `ZapretMac-macOS-universal.zip` из последнего релиза, извлекает `Payload` с `utunws` и читает стратегии из `strategies.tsv`. Windows продолжает использовать `zapret-discord-youtube` и `winws.exe`.

## Сборка на Mac

Установите Command Line Tools (дождитесь окончания установки):

```sh
xcode-select --install
```

Если Rust ещё не установлен, установите его через [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

В обычном терминале Apple Silicon, без Rosetta:

```sh
git clone --branch codex/macos-apple-silicon https://github.com/MelDxKviel/zapret-ui.git
cd zapret-ui
cargo test --locked
sh scripts/build-macos.sh
open "dist/Zapret UI.app"
```

Скрипт собирает `aarch64-apple-darwin`, создаёт и локально подписывает `dist/Zapret UI.app`, проверяет подпись и упаковывает `dist/zapret-ui-macos-arm64.zip`. Версия Rust берётся из `rust-toolchain.toml`. Первая сборка Slint/Skia может занять заметное время. Не запускайте GUI через `sudo`: правила ядра исключают трафик root, что мешает тестеру проверять обход.

Готовый архив также создаёт workflow **macOS Apple Silicon** в GitHub Actions этой ветки, artifact `zapret-ui-macos-arm64`. Для загруженной сборки macOS может потребовать «Открыть всё равно» в настройках конфиденциальности и безопасности: подпись локальная, приложение не нотарифицировано Apple.

Для проверки только интерфейса, без загрузки ядра и изменения сети:

```sh
cargo run --example ui_only
```

## Проверка обхода

1. Закройте оригинальное приложение ZapretMac, если оно есть: оба интерфейса управляют **одной** службой Flowseal. Отключите туннельные VPN. Настройте DNS, например 1.1.1.1 или 8.8.8.8 (рекомендация upstream).
2. Нажмите установку ядра в zapret-ui. Должны появиться версия ядра и список стратегий, включая `general (SIMPLE FAKE)`.
3. Переключитесь в расширенный режим, выберите стратегию и запустите обход. Подтвердите системный запрос администратора. Интерфейс должен показать системную службу, PID и время работы.
4. Проверьте YouTube и Discord, включая голосовой канал. Затем «Остановить»: состояние должно смениться на остановленное. Проверьте, что обычный интернет продолжает работать.
5. Запустите снова, закройте и откройте GUI: он должен обнаружить службу. Закрытие GUI **не останавливает обход**. Запущенная служба стартует и после перезагрузки; явная остановка отключает её до следующего запуска.
6. Запустите тест стратегий или подбор в простом режиме. Системные запросы могут повторяться при смене стратегии. Отмена запроса должна выводить ошибку, а не показывать успешный запуск. Полный тест останавливает кандидатов, простой режим оставляет рабочий вариант запущенным.
7. Измените IPSet (`none` / `loaded` / `any`) и перезапустите обход. Убедитесь, что пользовательские списки сохраняются после обновления ядра.

macOS всегда запускает ядро через LaunchDaemon: кнопки обычного запуска и установки службы используют один механизм. Отдельного непривилегированного режима процесса и GameFilter в этом ядре нет. Обновление самого GUI пока ручное — `git pull`, повторная сборка и замена всего `.app`; автоматическое обновление Windows EXE на Mac отключено. Автозапуск GUI при входе — отдельная настройка: включайте её уже после переноса `.app` в постоянную папку.

## Пути и диагностика

* Настройки GUI: `~/Library/Application Support/zapret-ui/config.toml`.
* Загруженное ядро: `~/Library/Application Support/zapret-ui/zapret`.
* Списки и выбор стратегии upstream: `~/Library/Application Support/ZapretMac/`.
* Защищённая копия ядра: `/Library/Application Support/ZapretMac/`.
* Служба: `/Library/LaunchDaemons/io.github.flowseal.zapretmac.plist`.
* Логи GUI: `~/Library/Application Support/zapret-ui/logs/app.log`.

```sh
launchctl print system/io.github.flowseal.zapretmac
tail -n 80 "/Library/Application Support/ZapretMac/engine.log"
tail -n 80 "/Library/Application Support/ZapretMac/zapret.log"
```

Если GUI недоступен, штатная аварийная остановка Flowseal:

```sh
sudo /bin/sh "/Library/Application Support/ZapretMac/stop.sh"
```

Она останавливает службу и очищает её PF-правила, возвращает сохранённую настройку TCP. Удаление `.app` само по себе службу не останавливает. Кнопка удаления службы сначала останавливает ядро, затем удаляет LaunchDaemon; пользовательские списки сохраняются.

Upstream поддерживает домашние каталоги `/Users/<имя>`. Нестандартные домашние пути и символы, небезопасные для его sed/XML-шаблонов, отклоняются с ошибкой. Проверки CI собирают приложение и запускают тесты без изменения сетевых правил; работу обхода у вашего провайдера нужно проверить на реальном Mac.
