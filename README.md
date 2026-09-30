# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Реализованы первый и второй этапы (локальный Unix socket API и CLI) и перенос `auth.py`: обмен x-token на Music token и получение device token `quasar.yandex.net/glagol/token` с кешированием и обработкой 401/403/429 (крейт `auth/`). Связь со станцией (Glagol WSS, mDNS) ещё не перенесена — до её появления `ping` всегда отвечает `connected:false`, а `say` — ошибкой `station_not_connected`. План — [docs/tasks.md](docs/tasks.md). Python в рантайме не используется.

## Компоненты

- `protocol/` — общий контракт: путь сокета, формат запросов/ответов, блокирующий клиент.
- `service/` — `yandex-ttsd`: Unix socket сервер (JSON Lines, лимит строки 65536 байт, таймаут 10 с, права `0600`, удаление только собственного stale socket).
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

## Тесты

```bash
cargo test --workspace
```

Покрыты протокол (валидация, лимиты, ошибки), многократные запросы, конкурентные клиенты, права сокета, замена stale socket, shutdown и интеграция CLI-пути с сервером на mock-станции — без сети и Python.

CI: GitHub Actions запускает `cargo test --workspace --locked` на каждый pull request (`.github/workflows/tests.yml`).

## systemd

Пока не перенесён (этап 5 в docs/tasks.md).

## Назначение

Проект рассчитан на использование как локальный TTS backend для агентов, автоматизаций и других CLI/tools.
