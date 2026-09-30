# План переноса на Rust

Источник поведения: `~/nanobot_workspace/yandex_tts` (`yandex_stationd/`, `tests/`, `systemd/`). Этот репозиторий — целевое место разработки; Python нужен только для изучения и сравнения со старой реализацией, не для работы готовых бинарников.

## Контракт совместимости

- `yandex-ttsd` держит авторизацию и соединение со станцией, `yandex-tts` обращается к нему через Unix stream socket. Формат: один JSON-объект на строку, один JSON-ответ на запрос.
- Путь по умолчанию: `$XDG_RUNTIME_DIR/yandex-stationd.sock`, если переменная не задана — `/run/user/<uid>/yandex-stationd.sock`; переопределение через `SOCKET_PATH`.
- `{"action":"ping"}` → `{"ok":true,"connected":true|false}`.
- `{"action":"say","text":"Привет"}` → `{"ok":true}` только после подтверждения команды станцией; ошибки: `invalid_json`, `invalid_request`, `unknown_action`, `message_too_large`, `timeout`, `station_not_connected`, `internal_error`.
- Строки с пустым текстом и лишними полями отклоняются. Максимальный размер строки запроса — 65536 байт; таймаут обработки — 10 секунд. Команды с неопределённым результатом не переотправляются автоматически.
- `yandex-tts say "Привет"` — целевой CLI; daemon не передаёт ему токены.

## Этапы

### 1. Зафиксировать поведение исходника и зависимости

- [x] Сверить `tests/` исходника с описанным выше протоколом: в том числе ответы на ошибки, разрыв соединения, многократные запросы по одному сокету и shutdown. (Семантика `tests/test_server.py` перенесена в Rust-тесты `service/tests/integration.rs`.)
- [x] Выбрать Rust crates для реализованной части и закрепить версии и `Cargo.lock`: `tokio` (async runtime, Unix sockets, сигналы), `serde_json` (JSON Lines), `libc` (uid), `log`; CLI без дополнительных зависимостей. WebSocket/TLS/mDNS-крейты будут выбраны на этапах 3–4. Границы: `protocol` (общий контракт и клиент), `service` (`config`, `station`, `server`), `cli`.
- [x] Составить тестовые fixtures без реальных токенов и станции; не переносить `.env`, `.venv`, `*.egg-info` или кеши из workspace. (Реальных токенов нет; станция эмулируется `MockStation` в тестах.)

**Готово, когда:** проект собирается без Python, контракт API покрыт проверками на тестовом Unix socket.

### 2. Локальный API и CLI

- [x] Перенести `server.py`: JSON Lines, валидация, лимиты, таймауты, конкурентные клиенты, безопасное создание сокета с правами `0600` и удаление только собственного stale socket.
- [x] Реализовать `yandex-tts say <text>` и `ping`: соединение с сокетом, чтение одного ответа, понятные ошибки и ненулевой код выхода при отказе; совместимость с `SOCKET_PATH`.
- [x] Проверить CLI и сервер совместно на тестовом обработчике станции без сети. (Сервер проверяется с `MockStation` через общий `protocol::Client`; отдельные тесты в `cli/tests/cli.rs` запускают сам бинарник `yandex-tts` против mock-daemon — коды выхода, stdout/stderr, строгая проверка формы ответа. Реальный Glagol-бэкенд пока заменён `NotConnectedStation`: `ping` → `connected:false`, `say` → `station_not_connected`.)

**Готово, когда:** CLI получает `ping` и `say` через локальный сокет, ошибки не маскируются под успех.

### 3. Авторизация и обнаружение станции

- [x] Перенести `auth.py`: обмен `YANDEX_X_TOKEN` на Music token и запрос `https://quasar.yandex.net/glagol/token` по `device_id`/`platform`, кеширование с учётом срока жизни, обновление после отклонения токена. (Крейт `auth` (`yandex-tts-auth`): async `YandexAuth` на `reqwest` (rustls), срок жизни из `expires_at`/`expires_in`/JWT `exp`, 60-секундный запас, `invalidate_device_token` и общий refresh для параллельных запросов. Встроенные `client_id`/`client_secret` Music OAuth удалены: `YandexAuth::new`/`with_options` принимают их явно из конфигурации daemon (`YANDEX_MUSIC_CLIENT_ID`/`YANDEX_MUSIC_CLIENT_SECRET`), встроенного фолбэка нет.)
- [ ] Подключить `YandexAuth` в daemon: пока daemon остаётся заглушкой (`NotConnectedStation`), при его реализации нужно читать `YANDEX_MUSIC_CLIENT_ID`/`YANDEX_MUSIC_CLIENT_SECRET` из окружения (`EnvironmentFile` в systemd/.env), валидировать их на старте и не передавать в CLI. До этого этапа `.env.example` описывает целевые переменные, но фактической загрузки в daemon ещё нет.
- [x] Обработать 401/403/429 (включая `Retry-After`) и сбои сети без утечки секретов в логах и ошибках. Тестировать на локальном HTTP mock. (`AuthError` с вариантами для 401/403/429/HTTP/сети; `Retry-After` как секунды или HTTP-date; тесты `auth/tests/auth.rs` на локальном tokio-сервере без сети и реальных токенов.)
- [x] Перенести `discovery.py`: `_yandexio._tcp.local.`, извлечение host/port/device_id/platform, выбор единственной станции либо по `YANDEX_DEVICE_ID`, ручная конфигурация всех четырёх полей. (Модуль `service::discovery` на крейте `mdns-sd`: разбор TXT (ключи без учёта регистра, `device_id`/`deviceid`), IPv4-адрес с фолбэком на hostname без точки в конце, валидация порта 1–65535, дедупликация по device_id; `select_station` — единственная станция либо по ID, иначе ошибки «No Yandex Stations found»/«Multiple Stations found; specify device_id»/«Station … not found»; `station_config` — ручной режим только при всех четырёх `YANDEX_STATION_HOST`/`YANDEX_STATION_PORT`/`YANDEX_DEVICE_ID`/`YANDEX_PLATFORM`, иначе mDNS-поиск с таймаутом 5 с. Тесты на фикстурах `ServiceInfo` без сети и реальной станции.)
- [ ] Задокументировать загрузку переменных окружения и их значения по умолчанию; токен хранится только у daemon. (Целевые переменные описаны в README и `.env.example`, включая параметры станции и ручной режим из `station_config`; фактической загрузки окружения в daemon ещё нет — она появится при подключении `YandexAuth` и конфигурации станции на этапе 4.)

**Готово, когда:** конфигурация станции работает вручную и через mDNS, повторные команды не вызывают авторизацию заново.

### 4. Glagol и управление соединением

- [x] Перенести `glagol.py`: локальный WSS станции, envelope `conversationToken`/UUID `id`/`sentTime`/`payload`, сопоставление ответа по `requestId`, heartbeat, таймауты и закрытие pending-запросов. (Модуль `service::glagol` на `tokio-tungstenite` + `rustls`: фоновый читатель с mpsc-каналом записи, dispatch по `requestId` (конкурентные запросы, ответы не по порядку), heartbeat-пинги и Pong-ответы, таймаут с очисткой pending, close code 4000 как `GlagolError::InvalidToken`, TLS станции отдельно от auth HTTP (`GlagolTls::SystemRoots` / явный `AcceptSelfSigned` для самоподписанного сертификата локальной станции). Тесты на mock WebSocket через in-memory duplex: конверт `say`, конкурентность/порядок ответов, таймаут, close, 4000, malformed/unmatched, heartbeat, невалидный URI, send без connect.)
- [x] Перенести `say()` через `serverAction` → `update_form` → `personal_assistant.scenarios.quasar.iot.repeat_phrase` со слотом `phrase_to_repeat`; проверять ответ станции до отправки `ok` клиенту. (`GlagolClient::say` возвращает ответ станции, сопоставленный по `requestId` — как в Python; это подтверждение корреляции, а не доказательство воспроизведения аудио. Сам `ok` клиенту отправляет daemon после проверки ответа; daemon пока заглушка, `say` возвращает `station_not_connected`, так что проверка и `ok` появятся только при подключении `GlagolClient` к server на этом же этапе.)
- [ ] Перенести `connection.py`: постоянное соединение, reconnect с backoff и jitter, сброс токена при close code 4000, обновление истёкшего токена, восстановление после рестарта станции/сети; без повторной отправки `say` с неизвестным результатом.
- [ ] Проверить протокол и сбои на локальном mock WebSocket; отдельно провести ручной тест с реальной станцией.

**Готово, когда:** серия `say` использует одно соединение, а daemon самостоятельно восстанавливается после обрыва.

### 5. Эксплуатация и переключение

- [ ] Добавить user unit systemd для Rust-бинарника, `EnvironmentFile=%h/.config/yandex-stationd/.env`, перезапуск при сбое и корректное завершение по SIGTERM/SIGINT.
- [ ] Убедиться, что при остановке прекращается приём запросов, закрываются клиенты/WSS и удаляется собственный сокет; не логировать токены и текст TTS на INFO.
- [ ] Обновить README: сборка release, размещение бинарников, настройка `.env`, запуск unit и проверка CLI; сверить миграцию пути сокета со старым Python daemon (одновременно слушать один путь они не могут).
- [ ] Запустить `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` и smoke test с реальной станцией без Python в runtime.

**Готово, когда:** CLI и user service работают после отключения Python-окружения исходного проекта.

## Позже

- [ ] Поддержка нескольких станций и параметра `station` в `say` после определения политики выбора по умолчанию.
- [ ] Дополнительные команды `status`, `volume`, `stop` и т. п. с отдельными тестами совместимости.
