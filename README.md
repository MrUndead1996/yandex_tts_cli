# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Реализованы первый и второй этапы (локальный Unix socket API и CLI), перенос `auth.py` (крейт `auth/`), `discovery.py` (mDNS-обнаружение станции и ручная конфигурация, `service::discovery`), `glagol.py` (клиент Glagol WSS, `service::glagol`) и `connection.py` (connection manager с фоновым восстановлением, `service::connection`). Daemon (`yandex-ttsd`) теперь использует реальный `ConnectionManager`: креды читаются из окружения на старте, станция определяется вручную или по mDNS, `ping` отражает фактическую готовность соединения, а `say` возвращает успех только после коррелированного ответа станции. systemd unit ещё не перенесён (этап 5). Python в рантайме не используется.

## Компоненты

- `protocol/` — общий контракт: путь сокета, формат запросов/ответов, блокирующий клиент.
- `service/` — `yandex-ttsd`: Unix socket сервер (JSON Lines, лимит строки 65536 байт, таймаут 10 с, права `0600`, удаление только собственного stale socket), mDNS-обнаружение станции (`discovery`), клиент Glagol WSS (`glagol`), connection manager с фоновым reconnect/token refresh (`connection`).
- `cli/` — `yandex-tts`: отправка команд daemon без доступа к токенам.

## Сборка

```bash
cargo build --release --workspace
```

## Использование

```bash
# терминал 1 (нужны YANDEX_X_TOKEN, YANDEX_MUSIC_CLIENT_ID, YANDEX_MUSIC_CLIENT_SECRET;
# без них daemon завершается с ошибкой до создания сокета)
./target/release/yandex-ttsd

# терминал 2
./target/release/yandex-tts ping     # печатает {"connected":true|false,"ok":true} — фактическая готовность станции
./target/release/yandex-tts say "Привет"
# при успехе ничего не печатает (код выхода 0); иначе
# yandex-tts: daemon error: station_not_connected  (код выхода 1)
```

Путь сокета: `SOCKET_PATH`, иначе `$XDG_RUNTIME_DIR/yandex-stationd.sock`, иначе `/run/user/<uid>/yandex-stationd.sock`. `say` возвращает успех только после подтверждения команды станцией (коррелированный ответ без явного отказа); команды с неизвестным исходом (обрыв, таймаут) не переотправляются и возвращают `station_not_connected`. Ожидание готовности ограничено таймаутом запроса (10 с).

## Переменные окружения (только для daemon)

Крейт `auth` больше не содержит встроенных OAuth-кредов Yandex Music: `YandexAuth::new`/`with_options` принимают `client_id`/`client_secret` явно. Их нужно задавать в конфигурации daemon:

- `YANDEX_X_TOKEN` — x-token Яндекса (секрет daemon).
- `YANDEX_MUSIC_CLIENT_ID`, `YANDEX_MUSIC_CLIENT_SECRET` — OAuth-креды клиента Yandex Music для обмена x-token на Music token; обязательны, встроенного фолбэка нет.

Они никогда не передаются CLI и не попадают в логи или тексты ошибок. Daemon читает их из окружения при старте и завершается с ошибкой до создания сокета, если что-то из `YANDEX_X_TOKEN`/`YANDEX_MUSIC_CLIENT_ID`/`YANDEX_MUSIC_CLIENT_SECRET` отсутствует или пусто. Парсер `.env` не встроен: задавать переменные можно любым способом (export, systemd `EnvironmentFile` и т. п.).

## Обнаружение станции

Модуль `service::discovery` (крейт `mdns-sd`, чистый Rust без системных зависимостей) ищет станции по mDNS `_yandexio._tcp.local.` (таймаут 5 с). Из рекламации извлекаются IPv4-адрес (при его отсутствии — hostname без завершающей точки), порт, `device_id` (TXT-ключ `deviceId`, без учёта регистра, также принимается `deviceid`) и `platform`; неполные рекламации и порт вне 1–65535 игнорируются. Выбор:

- если задан `YANDEX_DEVICE_ID` — станция с этим ID, иначе ошибка `Station "…" not found`;
- без ID — ровно одна найденная станция; ошибки: `No Yandex Stations found`, `Multiple Stations found; specify device_id`.

Ручной режим: если заданы **все четыре** переменные `YANDEX_STATION_HOST`, `YANDEX_STATION_PORT`, `YANDEX_DEVICE_ID`, `YANDEX_PLATFORM`, mDNS не используется; порт обязан быть числом 1–65535, иначе ошибка `YANDEX_STATION_PORT must be a valid port`. Примеры — в `.env.example`. Daemon выполняет `station_config` через `spawn_blocking`, чтобы mDNS-поиск (до 5 с) не блокировал async-рантайм.

## Glagol WSS (библиотечный API)

Модуль `service::glagol` — постоянное WSS-соединение со станцией: конверт `conversationToken`/UUID `id`/`sentTime`/`payload`, фоновый читатель, сопоставляющий ответы по `requestId` (в том числе конкурентные запросы и ответы не по порядку), heartbeat-пинги с контролем ответа (`ping_timeout`, по умолчанию 20 с — неответивший peer считается отключённым), таймаут запроса (по умолчанию 10 с) и очистка pending-запросов при разрыве. `connect`/`close` сериализованы, отмена запроса не оставляет висячих pending-записей. `GlagolClient::say` отправляет `serverAction` → `update_form` → `personal_assistant.scenarios.quasar.iot.repeat_phrase` со слотом `phrase_to_repeat` и возвращает ответ станции, сопоставленный по `requestId`, — как в Python; это корреляция ответа, а не доказательство воспроизведения аудио.

- Закрытие соединения с кодом 4000 различается особо: `GlagolError::InvalidToken` (станция отвергла device token), прочие закрытия — `GlagolError::Closed`.
- TLS (`GlagolTls`) касается только WSS станции и никогда — HTTP авторизации Яндекса: `SystemRoots` проверяет сертификат по системным корням; `AcceptSelfSigned` — явный opt-in для локальной станции с самоподписанным сертификатом.
- Ошибки и `Debug` не содержат device token, текстов запросов и TTS.

## Connection manager (библиотечный API)

Модуль `service::connection` — порт `connection.py`: `ConnectionManager` владеет device token и Glagol-клиентом и держит соединение доступным при сетевых и токен-сбоях. Фоновое восстановление стартует без живой сети; reconnect с экспоненциальным backoff (удвоение с капом) и джиттером до 20% (инъекционен для тестов); `Retry-After` от auth 429 соблюдается; close code 4000 сбрасывает device token и переподключается с новым; периодический рефреш-зонд замечает ротацию токена. Команда с неизвестным исходом (обрыв/таймаут при отправке) возвращается вызывающему как ошибка и лишь помечает соединение на пересборку — автоматической повторной отправки нет. Закрытие (`close`) останавливает retry-цикл, закрывает сокет и будит всех ждущих; `send_within`/`say_within` позволяют daemon ограничить ожидание готовности таймаутом запроса.

- Auth и Glagol-транспорт инъекционны: трейты `TokenSource` и `GlagolDialer`; прод-биндинги — `YandexAuth` и `GlagolWsDialer` над `GlagolClient`. Тесты используют mock auth и in-memory mock-соединение — без сети и реальной станции.
- `ConnectionManager` реализует `service::station::Station` (`connected`/`say`); daemon (`main` через `daemon`) использует его напрямую. Коррелированный ответ станции с явным отказом (`status` вне ok/success/accepted/done/ack, поле `error`/`errorCode`) не считается успехом и отвечает клиенту `station_not_connected`.
- `Debug` и ошибки не содержат device token и TTS-текста; креды daemon (`Credentials`) полностью скрыты в `Debug`.

## Подключение daemon (main)

`main` читает креды (`daemon::Credentials::from_env`), определяет станцию (`daemon::resolve_station` через `spawn_blocking`), строит `ConnectionManager<YandexAuth, GlagolWsDialer>` (`daemon::build_manager`, интервалы как в Python: 1 c retry / 30 c кап / 30 c refresh) и запускает восстановление в фоне — API сокета не ждёт станцию. WSS станции набирается с `GlagolTls::AcceptSelfSigned` (самоподписанный сертификат локальной станции); авторизация Яндекса идёт по обычному HTTPS и этот режим TLS не затрагивает. При SIGTERM/SIGINT сервер перестаёт принимать, простаивающие соединения закрываются сразу, обработчикам с запросом в работе даётся до `timeout` (10 с) завершить текущий запрос, затем оставшиеся обрываются, сокет удаляется и закрывается соединение со станцией (`manager.close`). Если станцию найти не удалось или связаться с сокетом не вышло, daemon завершается с ненулевым кодом, закрыв фоновой manager.

## Тесты

```bash
cargo test --workspace
```

Покрыты протокол (валидация, лимиты, ошибки), многократные запросы, конкурентные клиенты, права сокета, замена stale socket, shutdown и интеграция CLI-пути с сервером на mock-станции. Интеграция daemon (`service/tests/daemon_integration.rs`) проверяет полный стек — сервер поверх `ConnectionManager` с mock auth/mock Glagol: `ping` отражает готовность, `say` успешен только после коррелированного ответа, явный отказ станции и обрыв соединения дают `station_not_connected` без повторной отправки, ошибки после закрытия manager. Конфигурация (`daemon`) покрыта юнит-тестами: пропуск/пустое значение переменной — ошибка до создания сокета, имя переменной в ошибке без значения, `Debug` без секретов. Клиент Glagol проверяется на mock WebSocket через in-memory duplex (конкурентные запросы, ответы не по порядку, таймаут, закрытие, код 4000, malformed/unmatched ответы, heartbeat, конверт `say`) — без сети, реальной станции и Python. Connection manager проверяется на mock auth и in-memory mock-соединении (подключение, обрыв/восстановление, 4000→invalidate→новый токен, ротация токена, `Retry-After`, отсутствие повторной отправки, backoff/джиттер-политика, пробуждение ждущих при закрытии, отсутствие токена в `Debug`).

CI: GitHub Actions запускает `cargo test --workspace --locked` на каждый pull request (`.github/workflows/tests.yml`).

## systemd

Пока не перенесён (этап 5 в docs/tasks.md).

## Назначение

Проект рассчитан на использование как локальный TTS backend для агентов, автоматизаций и других CLI/tools.
