# zapret-ui

Удобный графический интерфейс для обхода DPI-блокировок на Windows. Discord, YouTube и другие сервисы снова работают — без командной строки и возни с `.bat`-файлами.

![Rust](https://img.shields.io/badge/Rust-stable-000000?logo=rust&logoColor=white)
![Slint](https://img.shields.io/badge/UI-Slint-2379F4)
![Windows](https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-0078D6?logo=windows&logoColor=white)
[![CI](https://github.com/meldxkviel/zapret-ui/actions/workflows/release.yml/badge.svg)](https://github.com/meldxkviel/zapret-ui/actions/workflows/release.yml)
[![Release](https://img.shields.io/github/v/release/meldxkviel/zapret-ui?logo=github&logoColor=white)](https://github.com/meldxkviel/zapret-ui/releases/latest)
[![License](https://img.shields.io/badge/License-MIT-green)](LICENSE)

## Что это

Графическая оболочка над апстрим-движком [`bol-van/zapret2`](https://github.com/bol-van/zapret2) (через сборку Windows-бинарников [`bol-van/zapret-win-bundle`](https://github.com/bol-van/zapret-win-bundle)). Приложение само качает дистрибутив с `winws2.exe` и WinDivert, держит курируемый набор готовых пресетов в коде и запускает выбранную стратегию одной кнопкой. Один `.exe`, никаких дополнительных DLL.

## Возможности

- ⬇️ **Авто-загрузка bundle** с Windows-сборкой zapret2 прямо из приложения, если её ещё нет на ПК.
- 🎯 **Курируемые пресеты zapret2** в комплекте — General, YouTube TLS, YouTube QUIC, Discord/VoIP, WireGuard; добавляются обычной правкой Rust-кода.
- 🧪 **Автоподбор стратегии** — встроенный тест прогоняет пресеты по заблокированным сайтам и сам выбирает лучший.
- ▶️ **Запуск как процесс** (кнопка START) или **как служба Windows** с автозапуском при загрузке.
- 🔄 **Проверка и установка обновлений** bundle и самого zapret-ui в один клик.
- ⚙️ **Тонкая настройка**: обновление курируемых хост-листов, очистка кэша Discord.
- 📋 Живые логи `winws2.exe`, тёмная/светлая тема, русский и английский язык, сворачивание в трей.

## Установка

1. Скачайте **`zapret-ui.exe`** из раздела [**Releases**](https://github.com/meldxkviel/zapret-ui/releases/latest).
2. Запустите. Установка не требуется — всё в одном файле.

> ⚠️ Запускайте **от имени администратора** — это необходимо: обходу нужен драйвер WinDivert, а тесту стратегий и работе со службой нужны права. Без них окно откроется, но запустить обход не получится (баннер вверху предложит перезапуск).

## Как пользоваться

1. На вкладке **Home** нажмите **Install zapret** (если ещё не установлен).
2. Откройте **Strategies** и нажмите **Select** на нужном пресете — либо запустите **тест** и дайте приложению подобрать лучший автоматически.
3. Нажмите **START** (запуск как процесс) или **Run as service** (как служба, переживёт перезагрузку).
4. Не заработало у вашего провайдера? Попробуйте следующий вариант `ALT` — у разных операторов помогают разные стратегии.

## Частые вопросы

**Антивирус ругается на `winws2.exe`.**
Ложное срабатывание: обход работает с сырыми сетевыми пакетами через WinDivert, и некоторые антивирусы считают это подозрительным. При необходимости добавьте папку установки в исключения. zapret-ui лишь оборачивает официальный дистрибутив.

**Почему приложение не использует GitHub API?**
У многих провайдеров `api.github.com` сам заблокирован DPI. Поэтому версия bundle берётся из atom-фида коммитов на `github.com`, а архив — с `codeload.github.com`: они доступны там, где API уже недоступен.

**Где хранятся данные?**
Конфиг и установленный bundle лежат в `%APPDATA%\zapret-ui\`, логи — в `%APPDATA%\zapret-ui\logs\`.

## Благодарности

zapret-ui — самостоятельная оболочка и **не входит в состав** перечисленных проектов; она лишь скачивает и запускает их на вашем компьютере. Все права на ядро обхода принадлежат их авторам:

- [**bol-van/zapret2**](https://github.com/bol-van/zapret2) — апстрим-движок обхода DPI (`winws2`, Lua-стратегии).
- [**bol-van/zapret-win-bundle**](https://github.com/bol-van/zapret-win-bundle) — Windows-сборка `winws2.exe` с WinDivert и Cygwin DLL.
- [**basil00/WinDivert**](https://github.com/basil00/WinDivert) — драйвер перехвата пакетов.

## Лицензия

Код zapret-ui распространяется под лицензией [MIT](LICENSE). Лицензии скачиваемых компонентов и их правообладатели описаны в [NOTICE.md](NOTICE.md).
