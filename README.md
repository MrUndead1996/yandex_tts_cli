# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Реализован первый этап: локальный Unix socket API (`ping`/`say`, JSON Lines) и CLI. Связь с реальной станцией (авторизация, Glagol WSS, mDNS) ещё не перенесена — до её появления `ping` всегда отвечает `connected:false`, а `say` — ошибкой `station_not_connected`. План — [docs/tasks.md](docs/tasks.md). Python в рантайме не используется.

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
