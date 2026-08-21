# supervisor-rs

Минимальный супервизор процессов (мини-systemd) для Unix, на Rust.

Демон читает конфиг со списком процессов, стартует их, следит и перезапускает по
policy (`always` / `on-failure` / `never`), а при собственном завершении
корректно гасит **всё дерево** дочерних процессов: SIGTERM → таймаут → SIGKILL,
через process-группы (`setsid`/`killpg`). Реапит супервизор только своих прямых
детей: зомби внуков принадлежат init и супервизору недоступны в принципе.

Учебный pet-проект. Первый Rust в портфеле — тема (сигналы, process groups,
`waitid`/`killpg`) выбрана как практика системного программирования, где уместен
низкоуровневый контроль над libc при memory-safety.

## Статус

Готовы Этапы 0–6: парсинг TOML-конфига, запуск процессов и супервизия с
restart policy (`always` / `on-failure` / `never`) и экспоненциальным backoff,
корректное завершение по SIGTERM/SIGINT, process-группы (`setsid`) и teardown
всего дерева потомков с эскалацией SIGTERM → `stop-grace-secs` → SIGKILL,
CLI с подкомандами `run` / `status` поверх файла состояния (MVP, Этапы 1–5), и
управляющий control-socket с подкомандами `start` / `stop` / `restart <name>`
на работающем демоне (Этап 6, первый пост-MVP). Что дальше — в
[`docs/POST_MVP_PLAN.md`](docs/POST_MVP_PLAN.md).

## Быстрый старт

```bash
cargo check      # проверка компиляции
cargo build      # сборка
cargo test       # тесты

# запуск демона
cargo run -- run examples/supervisor.toml

# состояние супервизируемых процессов (из другого терминала)
cargo run -- status

# управление процессом на работающем демоне (из другого терминала)
cargo run -- stop web
cargo run -- start web
cargo run -- restart web
```

`status` печатает по строке на процесс:

```
NAME                 STATE             PID  RESTARTS  UPTIME
web                  running        172692         0      1s
worker               running        172693         0      1s
```

Если демон не запущен — внятная ошибка в stderr и код возврата 1.

`stop web` останавливает процесс `web` и держит его остановленным — restart
policy для него подавляется, даже `always` не воскресит; в `status` он виден
как `stopped`. `start web` возвращает его в супервизию с новым PID. `restart
web` принудительно пересоздаёт работающий процесс, независимо от policy. Все
три отвечают одной строкой (`ok`/`ok: <текст>`/`error: <текст>`) и завершаются
кодом 0 или 1 — команда лишь взводит операцию (сигнал послан / рестарт
запланирован), а не ждёт её завершения; прогресс виден через `status`.

Демон и `status` обмениваются данными через файл состояния:
`$XDG_RUNTIME_DIR/supervisor-rs/state.toml`, а при незаданном
`XDG_RUNTIME_DIR` — `/tmp/supervisor-rs-<uid>/state.toml`. Путь
переопределяется флагом `--state-file <path>` у `run`/`status`. Демон
переписывает снапшот раз в секунду, поэтому `status` показывает состояние
не старше 1 с; при штатной остановке файл удаляется.

Управляющий control-socket живёт в том же каталоге:
`$XDG_RUNTIME_DIR/supervisor-rs/control.sock` (тот же фолбэк на `/tmp`), путь
переопределяется флагом `--control-socket <path>` у `run`/`start`/`stop`/
`restart`. При штатной остановке демона файл сокета удаляется.

Полный список аргументов — `cargo run -- --help`.

Внешних зависимостей нет — это CLI-демон, Docker/сервисы не требуются.

## Документация

- [`docs/PLAN.md`](docs/PLAN.md) — видение, архитектура, список этапов, «после MVP».
- [`docs/TECHNICAL_PLAN.md`](docs/TECHNICAL_PLAN.md) — стек и детальная разбивка по этапам.
- [`docs/POST_MVP_PLAN.md`](docs/POST_MVP_PLAN.md) — nice-to-have за рамками MVP.
- [`CLAUDE.md`](CLAUDE.md) — конвенции проекта и dev-пайплайн для AI-ассистента.

## Лицензия

MIT — см. [`LICENSE`](LICENSE).
