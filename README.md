# Yandex Station TTS

Rust workspace для переноса локального TTS daemon из `~/nanobot_workspace/yandex_tts`. Сейчас создан каркас проекта; бинарники пока завершаются с ошибкой и не отправляют команды на станцию. План реализации — [docs/tasks.md](docs/tasks.md).

## Компоненты

- `service/` — будущий `yandex-ttsd`: авторизация, Glagol WSS, локальный Unix socket и user service systemd.
- `cli/` — будущий `yandex-tts`: отправка команд daemon без доступа к токенам.
- `.env.example` — шаблон настроек daemon; реальные значения не хранить в репозитории.

Проверка каркаса: `cargo check --workspace`. После реализации: `cargo build --release --workspace`.

## Планируемое использование

```bash
yandex-tts say "Свет на кухне выключен"
```

CLI будет передавать запрос запущенному сервису по Unix socket, который отправит TTS на станцию.

## systemd

```bash
systemctl --user status yandex-ttsd
systemctl --user restart yandex-ttsd
journalctl --user -u yandex-ttsd -f
```

После реализации сервис будет хранить состояние соединения независимо от вызывающих его приложений.

## Назначение

Проект рассчитан на использование как локальный TTS backend для агентов, автоматизаций и других CLI/tools.
