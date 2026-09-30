# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Реализованы первый и второй этапы (локальный Unix socket API и CLI), перенос `auth.py` (крейт `auth/`), `discovery.py` (mDNS-обнаружение станции и ручная конфигурация, `service::discovery`), `glagol.py` (клиент Glagol WSS, `service::glagol`) и `connection.py` (connection manager с фоновым восстановлением, `service::connection`). Daemon (`yandex-ttsd`) теперь использует реальный `ConnectionManager`: креды читаются из окружения на старте, станция определяется вручную или по mDNS, `ping` отражает фактическую готовность соединения, а `say` возвращает успех только после коррелированного ответа станции. systemd user unit перенесён (`systemd/yandex-ttsd.service`, этап 5 в части unit; smoke test с реальной станцией остаётся ручной проверкой). Python в рантайме не используется.

## Компоненты

- `protocol/` — общий контракт: путь сокета, формат запросов/ответов, блокирующий клиент.
- `service/` — `yandex-ttsd`: Unix socket сервер (JSON Lines, лимит строки 65536 байт, таймаут 10 с, права `0600`, удаление только собственного stale socket), mDNS-обнаружение станции (`discovery`), клиент Glagol WSS (`glagol`), connection manager с фоновым reconnect/token refresh (`connection`).
- `cli/` — `yandex-tts`: отправка команд daemon без доступа к токенам.

## Сборка

```bash
cargo build --release --locked --workspace
```

Реалистичное место установки бинарников — `~/.local/bin` (user-инсталляция без root):

```bash
install -Dm755 target/release/yandex-ttsd ~/.local/bin/yandex-ttsd
install -Dm755 target/release/yandex-tts  ~/.local/bin/yandex-tts
```

## systemd (user unit)

Unit для Rust-бинарника лежит в `systemd/yandex-ttsd.service`. Проверка синтаксиса:

```bash
systemd-analyze verify systemd/yandex-ttsd.service
# «is not executable» до установки бинарника в ~/.local/bin — ожидаемо
```

### Шаг 1. Остановить старый Python daemon (для миграции)

Старый Python daemon и новый Rust daemon слушают **один и тот же путь сокета** — одновременно работать они не могут, поэтому старый сервис останавливается **до** любых действий с новым unit. Также это защищает существующие креды от случайной перезаписи на следующем шаге:

```bash
systemctl --user disable --now yandex-stationd   # старый Python unit (имя может отличаться)
# либо, если запускался вручную: kill <pid> и дождитесь завершения
```

Если старый unit был с `Restart=always`, сначала `systemctl --user disable yandex-stationd`, затем stop — иначе он перехватит сокет обратно.

### Шаг 2. Подготовить `.env` (без перезаписи существующих кредов)

**Если `~/.config/yandex-stationd/.env` уже существует** (например, остался от Python-daemon) — не перезаписывайте его. Сохраните файл как есть, только выровняйте права и допишите недостающие переменные:

```bash
test -e ~/.config/yandex-stationd/.env && chmod 600 ~/.config/yandex-stationd/.env
$EDITOR ~/.config/yandex-stationd/.env
```

В редакторе добавьте при отсутствии `YANDEX_MUSIC_CLIENT_ID` и `YANDEX_MUSIC_CLIENT_SECRET` (и убедитесь, что `YANDEX_X_TOKEN` непустой). Шаблон имён и значений — `.env.example` в репозитории; сами значения в терминал не вводятся и в команды не подставляются.

**Если файла нет** — создайте его из шаблона только при отсутствии (команда с `test -e … ||` не выполняется, если файл уже существует, поэтому перезапись невозможна):

```bash
mkdir -p ~/.config/yandex-stationd
test -e ~/.config/yandex-stationd/.env || install -m600 .env.example ~/.config/yandex-stationd/.env
$EDITOR ~/.config/yandex-stationd/.env
```

Обязательно к заполнению: `YANDEX_X_TOKEN`, `YANDEX_MUSIC_CLIENT_ID`, `YANDEX_MUSIC_CLIENT_SECRET`. OAuth `client_id`/`client_secret` **не встроены** в Rust-daemon (в отличие от старого Python-варианта, где они были зашиты в код): если в старом `.env`/окружении их не было, задайте явно. В обоих случаях итоговые права файла — `0600`; реальные значения не попадают ни в репозиторий, ни в команды в истории шелла.

### Шаг 3. Установить и запустить unit

```bash
mkdir -p ~/.config/systemd/user
install -m644 systemd/yandex-ttsd.service ~/.config/systemd/user/
systemctl --user daemon-reload
```

Если старый Python-daemon ещё работает — вернитесь к шагу 1. Запуск (однократно; `--now` и включает, и стартует, отдельный `start` не нужен):

```bash
systemctl --user enable --now yandex-ttsd
journalctl --user -u yandex-ttsd -f
```

Свойства unit:

- `Restart=on-failure` (`RestartSec=2s`) — перезапуск при сбое; чистый `exit 0` не рестартует.
- `EnvironmentFile=%h/.config/yandex-stationd/.env` — креды и конфигурация станции; формат — `KEY=value`, как в `.env.example`. Парсер systemd строгий: без `export`, кавычки поддерживаются.
- `Wants=network-online.target` + `After=network-online.target` — попытка упорядочить старт после сети. В user-менеджере этот таргет существует не всегда (зависит от дистрибутива и настроек системного менеджера), поэтому упорядочивание может не срабатывать; это допустимо — daemon восстанавливает соединение со станцией в фоне, так что задержка лишь оптимизирует первый `ping`, а не корректность.
- `TimeoutStopSec=15s`, `KillMode=mixed` — при `SIGTERM` daemon перестаёт принимать, завершает запросы в работе (лимит — таймаут запроса 10 с), удаляет сокет и закрывает WSS; 15 с покрывают дренаж с запасом.

При переустановке unit после правки `systemd/yandex-ttsd.service` повторите только `install -m644 …` + `daemon-reload` + `systemctl --user restart yandex-ttsd`; `.env` при этом не затрагивается.

### Откат на Python-вариант

```bash
systemctl --user disable --now yandex-ttsd
systemctl --user start yandex-stationd   # при необходимости верните автозапуск: enable
```

Откат не меняет `.env`; права `0600` сохраняются. Переиспользуется он лишь частично: общий у обоих daemon — `YANDEX_X_TOKEN` и настройки станции/сокета (`YANDEX_DEVICE_ID`, `YANDEX_PLATFORM`, `YANDEX_STATION_HOST`, `YANDEX_STATION_PORT`, `SOCKET_PATH`). OAuth `YANDEX_MUSIC_CLIENT_ID`/`YANDEX_MUSIC_CLIENT_SECRET` старому Python-daemon не нужны (там они встроены) — их можно не удалять, но полная взаимозаменяемость конфигурации не предполагается. Бинарники Rust при откате можно не удалять.

### Секреты

- `.env` с реальными токенами храните только в `~/.config/yandex-stationd/.env` с правами `0600`; не копируйте его в репозиторий и не передавайте содержимое CLI/агентам.
- Существующий `.env` не перезаписывается и не печатается (шаг 2): права выравниваются `chmod 600`, значения правятся в редакторе; значения переменных не подставляются в команды.
- `~/.config/systemd/user/yandex-ttsd.service` и этот репозиторий не должны содержать реальных токенов.

## Использование

```bash
# терминал 1: если сервис ещё не запущен, запускайте через systemd,
# чтобы daemon получил переменные из EnvironmentFile
systemctl --user start yandex-ttsd

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

Покрыты протокол (валидация, лимиты, ошибки), многократные запросы, конкурентные клиенты, права сокета, замена stale socket, shutdown и интеграция CLI-пути с сервером на mock-станции. Интеграция daemon (`service/tests/daemon_integration.rs`) проверяет полный стек — сервер поверх `ConnectionManager` с mock auth/mock Glagol: `ping` отражает готовность, `say` успешен только после коррелированного ответа, явный отказ станции и обрыв соединения дают `station_not_connected` без повторной отправки, ошибки после закрытия manager. Конфигурация (`daemon`) покрыта юнит-тестами: пропуск/пустое значение переменной — ошибка до создания сокета, имя переменной в ошибке без значения, `Debug` без секретов. Клиент Glagol проверяется на mock WebSocket через in-memory duplex (конкурентные запросы, ответы не по порядку, таймаут, закрытие, код 4000, malformed/unmatched ответы, heartbeat, конверт `say`) — без сети, реальной станции и Python. Connection manager проверяется на mock auth и in-memory mock-соединении (подключение, обрыв/восстановление, 4000→invalidate→новый токен, ротация токена, `Retry-After`, отсутствие повторной отправки, backoff/джиттер-политика, пробуждение ждущих при закрытии, отсутствие токена в `Debug`). Продакшен-биндинги (`YandexAuth` на reqwest, `GlagolWsDialer` на tokio-tungstenite + rustls) поверх `ConnectionManager` и Unix socket сервера покрыты офлайн smoke-тестом `service/tests/production_smoke.rs`: локальный mock-HTTP для auth, mock-станция по `wss://` с самоподписанным TLS (тестовые фикстуры), проверка конверта/авторизации, восстановление после обрыва, close 4000 → новый токен, отсутствие replay, shutdown — без внешней сети, реальных секретов и станции. Ручной smoke с реальной станцией остаётся отдельной проверкой.

CI: GitHub Actions запускает `cargo test --workspace --locked` на каждый pull request (`.github/workflows/tests.yml`).

## Назначение

Проект рассчитан на использование как локальный TTS backend для агентов, автоматизаций и других CLI/tools.
