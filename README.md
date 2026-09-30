# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Реализованы первый и второй этапы (локальный Unix socket API и CLI), перенос `auth.py` (крейт `auth/`), `discovery.py` (mDNS-обнаружение станции и ручная конфигурация, `service::discovery`) и `glagol.py` (клиент Glagol WSS, `service::glagol`). Клиент Glagol пока не подключён к daemon: до появления connection manager (этап 4 в docs/tasks.md) `ping` отвечает `connected:false`, а `say` — ошибкой `station_not_connected`. Python в рантайме не используется.

## Компоненты

- `protocol/` — общий контракт: путь сокета, формат запросов/ответов, блокирующий клиент.
- `service/` — `yandex-ttsd`: Unix socket сервер (JSON Lines, лимит строки 65536 байт, таймаут 10 с, права `0600`, удаление только собственного stale socket), mDNS-обнаружение станции (`discovery`), клиент Glagol WSS (`glagol`).
- `cli/` — `yandex-tts`: отправка команд daemon без доступа к токенам.

## Сборка

```bash
cargo build --release --workspace
```

## Использование

```bash
# терминал 1
./target/release/yandex-ttsd

# терминал 2
./target/release/yandex-tts ping     # {"connected":false,"ok":true} — станции нет
./target/release/yandex-tts say "Привет"
# yandex-tts: daemon error: station_not_connected  (код выхода 1)
```

Путь сокета: `SOCKET_PATH`, иначе `$XDG_RUNTIME_DIR/yandex-stationd.sock`, иначе `/run/user/<uid>/yandex-stationd.sock`. `say` возвращает успех только после подтверждения команды станцией; пока бэкенд-заглушка (`NotConnectedStation`) активна, успеха не бывает.

## Переменные окружения (только для daemon)

Крейт `auth` больше не содержит встроенных OAuth-кредов Yandex Music: `YandexAuth::new`/`with_options` принимают `client_id`/`client_secret` явно. Их нужно задавать в конфигурации daemon:

- `YANDEX_X_TOKEN` — x-token Яндекса (секрет daemon).
- `YANDEX_MUSIC_CLIENT_ID`, `YANDEX_MUSIC_CLIENT_SECRET` — OAuth-креды клиента Yandex Music для обмена x-token на Music token; обязательны, встроенного фолбэка нет.

Они никогда не передаются CLI и не попадают в логи или тексты ошибок. Внимание: daemon пока является заглушкой и не читает эти переменные — фактическая загрузка появится при подключении `YandexAuth` на этапах 3–4 (см. docs/tasks.md).

## Обнаружение станции

Модуль `service::discovery` (крейт `mdns-sd`, чистый Rust без системных зависимостей) ищет станции по mDNS `_yandexio._tcp.local.` (таймаут 5 с). Из рекламации извлекаются IPv4-адрес (при его отсутствии — hostname без завершающей точки), порт, `device_id` (TXT-ключ `deviceId`, без учёта регистра, также принимается `deviceid`) и `platform`; неполные рекламации и порт вне 1–65535 игнорируются. Выбор:

- если задан `YANDEX_DEVICE_ID` — станция с этим ID, иначе ошибка `Station "…" not found`;
- без ID — ровно одна найденная станция; ошибки: `No Yandex Stations found`, `Multiple Stations found; specify device_id`.

Ручной режим: если заданы **все четыре** переменные `YANDEX_STATION_HOST`, `YANDEX_STATION_PORT`, `YANDEX_DEVICE_ID`, `YANDEX_PLATFORM`, mDNS не используется; порт обязан быть числом 1–65535, иначе ошибка `YANDEX_STATION_PORT must be a valid port`. Примеры — в `.env.example`. До интеграции с daemon (этап 4) фактической загрузки этих переменных нет; тесты покрытия — на фикстурах `ServiceInfo` без сети.

## Glagol WSS (библиотечный API)

Модуль `service::glagol` — постоянное WSS-соединение со станцией: конверт `conversationToken`/UUID `id`/`sentTime`/`payload`, фоновый читатель, сопоставляющий ответы по `requestId` (в том числе конкурентные запросы и ответы не по порядку), heartbeat-пинги с контролем ответа (`ping_timeout`, по умолчанию 20 с — неответивший peer считается отключённым), таймаут запроса (по умолчанию 10 с) и очистка pending-запросов при разрыве. `connect`/`close` сериализованы, отмена запроса не оставляет висячих pending-записей. `GlagolClient::say` отправляет `serverAction` → `update_form` → `personal_assistant.scenarios.quasar.iot.repeat_phrase` со слотом `phrase_to_repeat` и возвращает ответ станции, сопоставленный по `requestId`, — как в Python; это корреляция ответа, а не доказательство воспроизведения аудио.

- Закрытие соединения с кодом 4000 различается особо: `GlagolError::InvalidToken` (станция отвергла device token), прочие закрытия — `GlagolError::Closed`.
- TLS (`GlagolTls`) касается только WSS станции и никогда — HTTP авторизации Яндекса: `SystemRoots` проверяет сертификат по системным корням; `AcceptSelfSigned` — явный opt-in для локальной станции с самоподписанным сертификатом.
- Ошибки и `Debug` не содержат device token, текстов запросов и TTS.
- Reconnect, обновление токена и интеграция с daemon не реализованы (этап 4).

## Тесты

```bash
cargo test --workspace
```

Покрыты протокол (валидация, лимиты, ошибки), многократные запросы, конкурентные клиенты, права сокета, замена stale socket, shutdown и интеграция CLI-пути с сервером на mock-станции. Клиент Glagol проверяется на mock WebSocket через in-memory duplex (конкурентные запросы, ответы не по порядку, таймаут, закрытие, код 4000, malformed/unmatched ответы, heartbeat, конверт `say`) — без сети, реальной станции и Python.

CI: GitHub Actions запускает `cargo test --workspace --locked` на каждый pull request (`.github/workflows/tests.yml`).

## systemd

Пока не перенесён (этап 5 в docs/tasks.md).

## Назначение

Проект рассчитан на использование как локальный TTS backend для агентов, автоматизаций и других CLI/tools.
