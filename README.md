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

Готовы Этапы 0–4: парсинг TOML-конфига, запуск процессов и супервизия с
restart policy (`always` / `on-failure` / `never`) и экспоненциальным backoff,
корректное завершение по SIGTERM/SIGINT, process-группы (`setsid`) и teardown
всего дерева потомков с эскалацией SIGTERM → `stop-grace-secs` → SIGKILL.
Впереди — CLI статуса (Этап 5); см. документацию.

## Быстрый старт

```bash
cargo check      # проверка компиляции
cargo build      # сборка
cargo test       # тесты

# запуск: путь к конфигу — единственный аргумент
cargo run -- examples/supervisor.toml
```

Внешних зависимостей нет — это CLI-демон, Docker/сервисы не требуются.

## Документация

- [`docs/PLAN.md`](docs/PLAN.md) — видение, архитектура, список этапов, «после MVP».
- [`docs/TECHNICAL_PLAN.md`](docs/TECHNICAL_PLAN.md) — стек и детальная разбивка по этапам.
- [`docs/POST_MVP_PLAN.md`](docs/POST_MVP_PLAN.md) — nice-to-have за рамками MVP.
- [`CLAUDE.md`](CLAUDE.md) — конвенции проекта и dev-пайплайн для AI-ассистента.

## Лицензия

MIT — см. [`LICENSE`](LICENSE).
