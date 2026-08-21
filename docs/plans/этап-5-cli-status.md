# План Этапа 5 — CLI статуса (`run` / `status`, файл состояния)

Ветка: `этап-5/cli-status`. Исполнителю: никаких git-коммитов — commit/push/PR
делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/shutdown.rs` / `tests/signals.rs`.

## 1. Цель и критерий приёмки

CLI переводится на подкоманды: `supervisor-rs run <config>` — демон (текущее
поведение), `supervisor-rs status` — показать актуальное состояние
супервизируемых процессов: имя, состояние (running / restarting / stopping /
stopped), PID, restart-count, uptime. Обмен — через файл состояния: демон
периодически пишет снапшот, `status` его читает.

Критерий приёмки (уточнение формулировки TECHNICAL_PLAN): `supervisor-rs status`
на запущенном демоне печатает по строке на процесс с именем, состоянием, PID,
числом рестартов и uptime; на незапущенном демоне (нет файла, либо файл
остался от умершего демона) — внятная ошибка в stderr и exit-код 1. Данные не
старше `STATE_WRITE_INTERVAL` (1 с) на момент записи.

**Три решения приняты пользователем — не пересматривать:**

1. **Канал — файл состояния, не unix-socket.** Поэтому условие пересмотра из
   Этапа 3 НЕ срабатывает: цикл остаётся поллинговым, `src/signal.rs`
   (глобальный `AtomicI32` + опрос в `run()`) не трогается. Self-pipe/signalfd
   в этом этапе не вводить.
2. **CLI — чистый переход на подкоманды, без обратно-совместимого алиаса.**
   Старая форма «единственный позиционный аргумент = путь к конфигу»
   упраздняется. Ломаются: `src/main.rs` (разбор аргументов), `tests/cli.rs`
   (все три теста переписываются), хелперы запуска бинарника в
   `tests/signals.rs` и `tests/tree.rs`, критерий приёмки Этапа 1 в
   `docs/TECHNICAL_PLAN.md` (актуализировать: форма изменена в Этапе 5 по
   решению пользователя), команда запуска в `README.md`.
3. **Разбор аргументов — ручной по `std::env::args`.** Никаких clap и других
   CLI-крейтов; новых зависимостей под CLI нет.

## 2. Подготовительный шаг: рефактор `Supervised` (техдолг Этапа 4)

Триггер техдолга «`child` и `pgid` — два `Option`, обязанных быть
согласованными» сработал: этап трогает `Supervised` под нужды `status`.
Рефактор — **первый шаг, отдельным коммитом, до любого кода состояния**, чтобы
новая функциональность ложилась на чистую структуру.

```rust
/// A live (or not-yet-reaped) child together with its process group.
/// Merged into one struct so `child.is_some() ⟺ pgid.is_some()` is expressed
/// by the type instead of being an invariant every mutation must re-prove.
struct Running {
    child: std::process::Child,
    /// pgid == leader pid, set right after spawn (the child is a session
    /// leader via pre_exec setsid). Exists exactly while the leader is alive
    /// or an unreaped zombie — the window in which the kernel cannot recycle
    /// the pgid, so `killpg` on it is safe. The struct is dropped only after
    /// the leader is reaped.
    pgid: Pid,
}

struct Supervised<'a> {
    config: &'a ProcessConfig,
    running: Option<Running>,   // was: child: Option<Child> + pgid: Option<Pid>
    stop: StopPhase,
    poll_errors: u32,
    restart_count: u32,
    started_at: Instant,
    next_restart_at: Option<Instant>,
    backoff: Backoff,
    done: bool,
}
```

Точки правки (механически, без изменения семантики):

- `new()` и ветка респавна в `tick()`: `running: Some(Running { child, pgid })`.
- `poll_child` — новая сигнатура `fn poll_child(running: &mut Running, name:
  &str) -> PollOutcome`; внутри pgid больше не `Option`, sweep безусловен.
  **Порядок peek → killpg-sweep → reap сохранить дословно** — он load-bearing,
  комментарии о зомби-лидере не терять.
- Ветка `PollOutcome::Exited`: `proc.running = None` **только после**
  `try_wait()` реапнул лидера (как сейчас `child = None; pgid = None`).
- `begin_shutdown`, `escalate_to_kill`, ветка эскалации по дедлайну в `tick()`,
  `handle_poll_error`: `if let Some(running) = &proc.running` /
  `proc.running = None`. Мёртвые ветки `None => true` при `child.is_some()` —
  удалить: их устранение и есть смысл рефактора.
- Юнит `poll_error_gives_up_and_kills_group_after_max` (литеральный конструктор
  `Supervised`) — обновить под `running`.

Инвариант в новых терминах: `running.is_some()` ⇔ «killpg безопасен»;
`running` обнуляется только после реапинга лидера. Никогда не восстанавливать
pgid из `child.id()` задним числом.

### Сработавшие триггеры остального техдолга Этапа 4

Рефактор правит `begin_shutdown` и `tick()` (в т.ч. строки вокруг дедлайнов) и
ветку респавна — срабатывают два триггера. Тесты добавить **до** рефактора, на
текущем коде (они обязаны быть зелёными и до, и после):

- **«Параллельность гашения не проверена»** → тест
  `shutdown_deadlines_are_per_process_not_shared` (§7.3).
- **«Рестарт после sweep не проверяет новый pgid»** → тест
  `restarted_instance_leads_its_own_fresh_group` (§7.3).

Не сработали (не тащить): «три копии `wait_for_pid`» — четвёртой копии не
появляется, новые e2e-тесты делают handshake по файлу состояния, а не по
pid-файлам (§7.5); «`wait_until_gone` и EPERM» — фундаментальное ограничение,
записано; известные ограничения 1–5 Этапа 4 — принятое поведение, не «чинить».

## 3. Файл состояния: формат, путь, атомарность

### Формат — TOML

Обоснование: сериализатор уже в зависимостях (`serde` + `toml`,
`toml::to_string` входит в дефолтные фичи) — ноль новых крейтов; формат
человекочитаем и совпадает с форматом конфига проекта. Отвергнуто: JSON
(потребовал бы `serde_json` — новая зависимость ради ничего), самописный
line-based формат (парсер руками + escaping имён — хуже бесплатного serde).

Схема (ключи kebab-case — в стиле `stop-grace-secs`):

```toml
version = 1
daemon-pid = 12345
written-at-unix-secs = 1755772800

[[process]]
name = "web"
state = "running"        # running | restarting | stopping | stopped
pid = 4242               # absent unless running/stopping
restart-count = 0
uptime-secs = 42         # absent unless running/stopping
```

- `version` — на будущее; `status` отказывается читать незнакомую версию.
- `daemon-pid` — ключ к различению «демон жив» / «файл устарел» (§5).
- `written-at-unix-secs` — информационное поле для человека/отладки
  (`SystemTime::now()` при записи); **решения по нему не принимаются** и тесты
  его значение не проверяют, поэтому wall-clock здесь не ломает детерминизм.
- `uptime-secs` считается в момент снапшота как
  `clock.now().saturating_duration_since(started_at).as_secs()` — на
  инъектируемых часах, детерминированно для `FakeClock`.

### Путь по умолчанию и переопределение

- Дефолт: `$XDG_RUNTIME_DIR/supervisor-rs/state.toml`; если `XDG_RUNTIME_DIR`
  не задан — `/tmp/supervisor-rs-<uid>/state.toml`. Директория создаётся при
  записи (`DirBuilder` с mode `0o700` через `std::os::unix::fs::DirBuilderExt`
  — предсказуемый путь в общем `/tmp` не должен быть перехватываем чужим uid;
  если директория уже существует и не наша, запись честно провалится и уйдёт в
  warn).
- Переопределение: флаг `--state-file <path>` у обеих подкоманд. Env-переменную
  не вводить — один механизм достаточен.
- uid: добавить фичу `"user"` к уже имеющемуся крейту `nix` в `Cargo.toml`
  (это фича существующей зависимости, не новый крейт; комментарий в Cargo.toml
  дополнить «Этап 5: uid для дефолтного пути файла состояния») и звать
  `nix::unistd::Uid::current()`. Если по факту компиляции окажется, что нужный
  элемент не за этой фичей — допустимый запасной вариант
  `unsafe { nix::libc::getuid() }` (getuid не может отказать), но сначала фича.
- Нет прав / не пишется: демон **продолжает работать** — супервизия первична,
  status вторичен. Каждая неудачная запись — `tracing::warn!`; частота
  ограничена самим интервалом записи (1 запись/с), лог не заливается. Отдельной
  проверки при старте не делать (единый код-путь).

### Атомарность записи — обязательна

`status` не должен прочитать полузаписанный файл. Запись: сериализовать в
строку → записать во временный файл `state.toml.tmp` **в той же директории**
(rename атомарен только внутри одной файловой системы) → `fs::rename` поверх
`state.toml`. POSIX-rename атомарно подменяет имя: читатель видит либо старый
файл целиком, либо новый целиком. Имя tmp фиксированное, не рандомное: писатель
один (демон), а `tempfile` — dev-dependency и в продакшен-код не тянется.
fsync не делаем: защищаемся от читателя-наблюдателя, а не от потери файла при
падении ядра.

### Жизненный цикл файла

- Первая запись — сразу на первой итерации `run()` (см. §4): `status` работает
  практически немедленно после старта демона.
- Периодическая запись — раз в `STATE_WRITE_INTERVAL` (1 с). Обоснование: для
  человеческого CLI свежесть в 1 с достаточна, а писать на каждом тике — 20
  записей/с бессмысленного IO. Запись по событиям (изменение состояния) в v1
  не делаем — периодической достаточно, меньше кода; staleness ≤ 1 с
  задокументировать.
- При штатном завершении демона (все `done`, включая shutdown по сигналу) —
  файл **удаляется** (best-effort, в конце `run()`). «Нет файла» = «демон не
  запущен» — самый частый случай различается тривиально.
- При аварийной смерти демона (SIGKILL, паника) файл остаётся — `status`
  распознаёт это по `daemon-pid` (§5).

## 4. Новый модуль `src/state.rs` и интеграция в `supervise.rs`

### `src/state.rs`

```rust
use serde::{Deserialize, Serialize};

pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct StateSnapshot {
    pub version: u32,
    pub daemon_pid: u32,
    pub written_at_unix_secs: u64,
    #[serde(default)]
    pub process: Vec<ProcessState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub struct ProcessState {
    pub name: String,
    pub state: ProcState,
    /// ГРАБЛЯ toml-сериализации: `toml::to_string` падает на `None` без
    /// skip_serializing_if — TOML не умеет null. Оба Option-поля обязаны
    /// нести этот атрибут, иначе первая же запись restarting-процесса упадёт.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    pub restart_count: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub uptime_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcState {
    Running,
    Restarting,
    Stopping,
    Stopped,
}

/// Default path; the pure *_from variant exists so tests can cover the XDG
/// fallback without mutating process-global env (env races the parallel test
/// runner).
pub fn default_path() -> PathBuf;               // тонкая обёртка
pub fn default_path_from(xdg_runtime_dir: Option<&str>) -> PathBuf;  // чистая

/// tmp-in-same-dir + rename; creates the parent dir (mode 0700) if missing.
pub fn write_atomic(path: &Path, snapshot: &StateSnapshot) -> std::io::Result<()>;

#[derive(Debug)]
pub enum ReadError {
    NotFound,
    Io(std::io::Error),
    Parse(toml::de::Error),
    UnsupportedVersion(u32),
}
// + impl Display / Error в стиле ConfigError

pub fn read(path: &Path) -> Result<StateSnapshot, ReadError>;

/// Best-effort removal; NotFound is fine, other errors are logged at warn.
pub fn remove(path: &Path);

/// kill(pid, 0). EPERM counts as alive — same convention (and the same
/// pid-reuse caveat) as wait_until_gone in the tests; recorded in Этап 4 debt.
pub fn daemon_alive(pid: u32) -> bool;
```

`src/lib.rs`: добавить `pub mod state;` и `pub mod cli;` (§6).

### Интеграция в `SupervisorLoop` (`src/supervise.rs`)

```rust
/// How often the daemon rewrites the state snapshot. 1 s: fresh enough for a
/// human-facing status CLI, 20× cheaper than writing every 50 ms tick.
pub const STATE_WRITE_INTERVAL: Duration = Duration::from_secs(1);

struct StateWriter {
    path: PathBuf,
    /// None until the first write — the first maybe_write fires immediately,
    /// so `status` works right after the daemon starts.
    next_write_at: Option<Instant>,
}

impl<'a, C: Clock> SupervisorLoop<'a, C> {
    /// Opt-in: tests that do not care about the state file keep the old
    /// two-argument construction unchanged.
    pub fn with_state_file(mut self, path: PathBuf) -> Self;

    /// Builds the snapshot from live supervision state. Pure read; pub so
    /// in-process tests can assert on it without touching the filesystem.
    pub fn snapshot(&self) -> crate::state::StateSnapshot;

    /// Writes the snapshot if the interval elapsed on the injected clock.
    /// Driven by run(); pub because in-process tests drive tick() directly.
    pub fn maybe_write_state(&mut self);
}
```

Вывод состояния процесса в `snapshot()` — ровно эта логика:

```rust
let (state, pid, uptime_secs) = match (&proc.running, proc.stop, proc.done) {
    (Some(running), StopPhase::Idle, _) => (
        ProcState::Running,
        Some(running.child.id()),
        Some(uptime(&self.clock, proc.started_at)),
    ),
    (Some(running), _, _) => (          // Terminating | Killing
        ProcState::Stopping,
        Some(running.child.id()),
        Some(uptime(&self.clock, proc.started_at)),
    ),
    (None, _, false) if proc.next_restart_at.is_some() => (ProcState::Restarting, None, None),
    _ => (ProcState::Stopped, None, None),  // done, включая сдачу по poll-errors
};
```

`daemon_pid` — `std::process::id()`; `written_at_unix_secs` — `SystemTime::now()`
внутри `snapshot()`.

В `run()` — единственные две правки:

```rust
loop {
    /* сигналы, any_active, tick() — без изменений */
    self.tick();
    self.maybe_write_state();       // после tick, до sleep
    self.clock.sleep(TICK);
}
// после выхода из цикла:
if let Some(writer) = &self.state_writer {
    crate::state::remove(&writer.path);
}
```

`maybe_write_state`: интервал считается по `self.clock.now()` (инъектируемые
часы — обе стороны интервала проверяемы на `FakeClock`); ошибка записи —
`tracing::warn!` и продолжить. `tick()` не трогается вовсе — файл состояния не
его забота, инвариант «tick свободен от сайд-каналов» сохранён; блокирующих
sleep не появляется (запись — обычный короткий IO, как логи).

Известное ограничение (задокументировать в TECHNICAL_PLAN): процессы, чей
первый spawn провалился, в `procs` не попадают (best-effort семантика Этапа 2)
— и в `status` их нет. Отражены exit-кодом 1 демона и логом; не чинить в этом
этапе.

## 5. Семантика `status`

Порядок проверок (в `main.rs`, поверх `state::read`):

1. `ReadError::NotFound` → stderr:
   `error: supervisor is not running (no state file at <path>)` → exit 1.
2. `UnsupportedVersion(v)` → stderr:
   `error: state file <path> has unsupported version <v> (expected 1)` → exit 1.
3. `Io`/`Parse` → stderr: `error: failed to read state file <path>: <err>` →
   exit 1. (Благодаря атомарному rename парс-ошибка означает реальную порчу, а
   не гонку с писателем.)
4. Файл прочитан, `!daemon_alive(daemon_pid)` → stderr:
   `error: supervisor is not running (state file <path> is stale: daemon pid <N> is gone)`
   → exit 1. Файл при этом **не удалять**: `status` — read-only команда, чужие
   файлы не трогает.
5. Иначе — таблица в stdout, exit 0.

Вывод (`println!`, не `tracing` — это результат команды, а не лог демона):

```
NAME                 STATE          PID  RESTARTS  UPTIME
web                  running      12345         0  42s
worker               restarting       -         3       -
```

Форматирование: `{:<20} {:<12} {:>8} {:>9} {:>7}`; отсутствующие pid/uptime —
`-`; uptime — просто `{}s` (секунды; человекочитаемые h/m — не в этом этапе).
Тесты проверяют вхождение подстрок (имя, слово состояния, цифры pid), а не
ширины колонок.

## 6. CLI: `src/cli.rs` + переписанный `main.rs`

Разбор выносится в библиотечный модуль `src/cli.rs` (чистая функция —
юнит-тестируема и видна интеграционным тестам), `main.rs` остаётся тонкой
обвязкой IO/exit-кодов.

```rust
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run { config: PathBuf, state_file: Option<PathBuf> },
    Status { state_file: Option<PathBuf> },
    Help,
}

/// Manual parsing over the raw args (argv[0] already stripped). No CLI crate
/// by user decision: two subcommands and one flag do not justify a dependency.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);   // человекочитаемое сообщение

pub fn parse(args: &[String]) -> Result<Command, UsageError>;

/// The full help text (stdout for --help, appended to stderr on usage errors).
pub fn usage() -> &'static str;
```

Правила `parse` (предписывающе):

- `-h` / `--help` в любой позиции → `Command::Help` (побеждает всё).
- Первый позиционный аргумент — подкоманда: `run` | `status`; ничего /
  неизвестное слово / старая форма «сразу путь» → `UsageError`.
- `run`: ровно один позиционный `<config-path>` после подкоманды; ноль или два
  → `UsageError`.
- `status`: ноль позиционных; лишний → `UsageError`.
- `--state-file <path>` — у обеих подкоманд; флаг без значения → `UsageError`;
  повторный флаг → последний побеждает (не усложнять). Неизвестный флаг
  (`--*`) → `UsageError`.

Текст помощи (дословно, из `usage()`):

```
supervisor-rs — a minimal process supervisor (mini-systemd) for Unix

Usage:
  supervisor-rs run <config-path> [--state-file <path>]
  supervisor-rs status [--state-file <path>]

Subcommands:
  run      Start the supervisor daemon with the given TOML config.
  status   Show the state of the supervised processes of a running daemon.

Options:
  --state-file <path>  Path of the state snapshot the daemon writes and
                       status reads. Default: $XDG_RUNTIME_DIR/supervisor-rs/
                       state.toml, or /tmp/supervisor-rs-<uid>/state.toml when
                       XDG_RUNTIME_DIR is not set.
  -h, --help           Print this help.
```

`main.rs`:

- `Help` → `usage()` в **stdout**, exit 0.
- `UsageError` → `eprintln!("error: {msg}")` + `usage()` в stderr, exit 2.
- `Run { config, state_file }` — текущий сценарий: load config (ошибка → лог +
  exit 1) → `install_handlers` → `SupervisorLoop::new(...)
  .with_state_file(state_file.unwrap_or_else(state::default_path))` → `run()`
  → exit по `had_start_errors` (1) либо 0. Таблица приоритетов Этапа 3
  неизменна: usage → 2, config → 1, start errors → 1, иначе 0 (в т.ч. при
  остановке сигналом).
- `Status { state_file }` — §5; путь тот же `unwrap_or_else(default_path)`.
- Инициализацию `tracing` оставить в начале `main` (общая для обеих подкоманд;
  логи `status` при дефолтном `RUST_LOG=info` не мешают: свои сообщения он
  печатает через println/eprintln). Снять `// TODO(Этап 5)` и актуализировать
  doc-заголовок `main.rs` под подкоманды.

## 7. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность — по содержимому файла, поллинг с дедлайном вместо одиночных
assert, `wait_with_timeout` для ожидания бинарника, новые тесты на
сигналы/процессы прогнать 10 раз подряд.

**Правило этапа для e2e:** каждый тест, запускающий бинарник, передаёт
`--state-file` в свой tempdir. Без этого параллельные тесты молотят общий
дефолтный путь: последний писатель побеждает, а удаление при выходе одного
демона подставляет ножку другому. Это касается и **существующих**
`tests/signals.rs` / `tests/tree.rs` (их `start_supervisor` переходит на
`run <config> --state-file <tmp>`); семантика самих тестов не меняется.

### 7.1 Юниты `src/cli.rs` (mod tests)

- `parses_run_with_config`
- `parses_run_with_state_file`
- `parses_status_without_flags`
- `parses_status_with_state_file`
- `help_flag_wins_anywhere` (`["run", "cfg", "--help"]` → `Help`)
- `rejects_empty_args`
- `rejects_unknown_subcommand`
- `rejects_bare_config_path_without_subcommand` — фиксирует упразднение старой
  формы контрактом (путь как первый аргумент — ошибка usage, не алиас)
- `rejects_run_without_config`
- `rejects_run_with_two_configs`
- `rejects_status_with_positional`
- `rejects_state_file_without_value`
- `rejects_unknown_flag`

### 7.2 Юниты `src/state.rs` (mod tests; tempfile доступен — это тесты)

- `snapshot_roundtrips_through_toml` — to_string → from_str → PartialEq.
- `serializes_restarting_without_pid_and_uptime` — сериализация с `None` не
  падает и ключей нет в тексте (грабля toml-None из §4).
- `write_atomic_creates_parent_dir_and_file`
- `write_atomic_replaces_previous_content_and_leaves_no_tmp` — две записи
  подряд: читается вторая, `state.toml.tmp` не существует.
- `read_missing_file_is_not_found`
- `read_rejects_unsupported_version`
- `read_rejects_garbage` → `Parse`
- `default_path_prefers_xdg_runtime_dir` / `default_path_falls_back_to_tmp` —
  через чистую `default_path_from`, без мутации env.
- `daemon_alive_for_self_and_dead_pid` — `daemon_alive(std::process::id())` ==
  true; заспавнить `/usr/bin/env true`, `wait()` (реапнут нами — ESRCH
  детерминирован сразу, это прямой ребёнок), pid мёртв → false.

### 7.3 Триггерные тесты техдолга (в `tests/shutdown.rs`, ДО рефактора §2)

- `shutdown_deadlines_are_per_process_not_shared` — два глухих к TERM стаба
  (`DEAF_SCRIPT`, отдельные pid-файлы) с `stop_grace_secs` = 2 и 7 на
  `FakeClock`. `begin_shutdown(SIGTERM)`; advance(3 c) + tick → первый мёртв
  (`tick_until_done(0)`, `!is_alive`), второй жив (`is_alive`); advance(5 с) →
  `tick_until_done(1)`, второй мёртв. Доказывает и раздельность дедлайнов, и
  параллельность: общий shutdown уложился в max(grace), а не в сумму — главный
  аргумент против блокирующей `terminate_tree`, до сих пор не проверенный.
- `restarted_instance_leads_its_own_fresh_group` — стаб пишет `$$` в pid-файл
  и `exit 1`, `restart = "on-failure"`. Дождаться первого pid,
  `tick_until_restart_scheduled`, advance на задержку, tick → respawn;
  дождаться **второй** строки pid-файла (расширить хелпер:
  `wait_for_pid_line(path, n, timeout) -> i32` — обобщение существующего
  `wait_for_pid`, копию не плодить), затем поллингом с дедлайном
  `getpgid(pid2) == pid2` (setsid происходит в ребёнке после fork — не
  проверять одним assert). Прибраться: `begin_shutdown` + `escalate_to_kill` +
  `tick_until_done`.

### 7.4 In-process тесты снапшота — новый файл `tests/state.rs`

`FakeClock`, без реальных хендлеров; хелпер `cfg()`/`sh()` скопировать из
`tests/shutdown.rs` (дублирование между интеграционными крейтами осознанное —
см. преамбулы существующих тестов). Хелпер чтения:
поллить `state::read(path)` с дедлайном 5 с, любые `Err` — retry (атомарный
rename гарантирует, что «полфайла» не читается — этот поллинг заодно и
smoke-тест атомарности).

- `first_maybe_write_creates_state_file_immediately` — долгоживущий стаб,
  `with_state_file`, один `tick()` + `maybe_write_state()` → файл существует,
  в нём процесс `running`, `pid` = Some и жив (`kill(pid,0)`), `restart-count`
  = 0, `uptime-secs` = Some, `daemon-pid` == `std::process::id()`.
- `interval_throttles_writes_on_injected_clock` — после первой записи удалить
  файл руками; `maybe_write_state()` без advance → файла нет (интервал не
  вышел); `clock().advance(STATE_WRITE_INTERVAL)` + `maybe_write_state()` →
  файл появился. Обе стороны интервала на фейковых часах.
- `snapshot_reflects_restarting_after_crash` — стаб `exit 1`, `on-failure`;
  `tick_until_restart_scheduled` → `snapshot()`: state == Restarting, pid ==
  None, uptime == None, restart_count == 0 (счётчик растёт после respawn —
  зафиксировать текущую семантику).
- `snapshot_reflects_stopping_during_shutdown` — глухой стаб,
  `begin_shutdown(SIGTERM)` → `snapshot()`: Stopping, pid = Some; затем
  `escalate_to_kill` + `tick_until_done` → Stopped, pid = None.
- `snapshot_reflects_stopped_after_clean_exit` — `exit 0` + `never`, tick до
  done → Stopped.
- `run_removes_state_file_on_exit` — конфиг из одного `/usr/bin/env true`,
  `never`, `with_state_file`; вызвать `run()` (безопасно in-process: сигналов
  никто не шлёт, `FakeClock::sleep` не блокирует, цикл завершится сам по
  `done`) → файла нет после возврата.

### 7.5 e2e на реальном бинарнике — новый файл `tests/status.rs`

Хелперы `write_config`, `start_supervisor` (уже с формой
`run <config> --state-file <path>`), `wait_with_timeout` — скопировать из
`tests/signals.rs` (осознанное дублирование, пометить комментарием).
Handshake — по файлу состояния (`wait_for_state(path, pred, timeout)`), НЕ по
pid-файлам: четвёртая копия `wait_for_pid` не появляется, триггер техдолга не
срабатывает.

- `status_shows_running_process_and_detects_daemon_exit` — конфиг: один
  долгоживущий стаб (`sleep`-цикл), `restart = "never"`. Запустить демона;
  дождаться в файле состояния процесса в `running` (это же — «демон готов»);
  запустить `supervisor-rs status --state-file <path>` как подпроцесс: exit 0,
  stdout содержит имя процесса, `running` и pid из снапшота. SIGTERM демону,
  `wait_with_timeout`, exit-код 0; файл состояния отсутствует (удаление в
  `run()` происходит до выхода процесса — после `wait` это детерминированный
  одиночный assert); повторный `status` → exit 1, stderr содержит
  `not running`.
- `status_reports_stale_state_file_of_dead_daemon` — без демона: заспавнить
  `/usr/bin/env true`, `wait()` его (pid гарантированно мёртв и реапнут);
  собрать `StateSnapshot` с `daemon_pid` = этот pid и записать через
  `state::write_atomic` в tempdir; `status --state-file` → exit 1, stderr
  содержит `stale`. (Окно переиспользования pid ничтожно и даёт красное, не
  зелёное.)
- `status_errors_on_unsupported_version` — записать снапшот с `version = 99` →
  exit 1, stderr содержит `unsupported version`.

### 7.6 Переписанный `tests/cli.rs`

- `run_exits_success_when_all_processes_exit_cleanly` (бывший
  `exits_success_...`: `bin() run <cfg> --state-file <tmp>`)
- `run_exits_failure_when_a_process_fails_to_spawn` (код 1)
- `run_exits_failure_for_missing_config` (код 1 — ошибка конфига)
- `no_arguments_is_a_usage_error` (код 2, stderr содержит `Usage:`)
- `bare_config_path_is_a_usage_error` — старая форма мертва: `bin() <cfg>` →
  код 2 (е2е-фиксация решения пользователя)
- `unknown_subcommand_is_a_usage_error` (код 2)
- `help_prints_usage_to_stdout_and_exits_zero` (код 0, stdout содержит
  `Usage:`)
- `status_on_missing_state_file_exits_one` — `status --state-file
  <tmp>/absent.toml` → код 1, stderr содержит `not running` (быстрый smoke без
  демона; полный сценарий — в `tests/status.rs`)

### 7.7 Существующие тесты

`tests/signals.rs`, `tests/tree.rs` — только правка формы запуска в
`start_supervisor` (подкоманда `run` + `--state-file` в tempdir теста);
ожидаемое поведение не меняется. `tests/restart.rs`, `tests/spawn.rs`,
`tests/shutdown.rs` (кроме §7.3) — без изменений: конструктор
`SupervisorLoop::new` не менялся, `with_state_file` opt-in. Все 56 текущих
тестов обязаны остаться зелёными.

## 8. Порядок работ (шаг = один связный коммит)

1. **Триггерные тесты техдолга на текущем коде** (§7.3): 
   `shutdown_deadlines_are_per_process_not_shared`,
   `restarted_instance_leads_its_own_fresh_group`, хелпер `wait_for_pid_line`
   в `tests/shutdown.rs`. Прогнать 10 раз (`cargo test --test shutdown`).
2. **Рефактор `running: Option<Running>`** (§2) — `src/supervise.rs` + его
   юниты. Никаких новых фич; весь набор тестов зелёный без правки других
   файлов.
3. **`src/state.rs`** (§4): типы, `default_path(_from)`, `write_atomic`,
   `read`, `remove`, `daemon_alive` + юниты §7.2; `pub mod state;` в `lib.rs`;
   фича `"user"` у `nix` в `Cargo.toml`.
4. **Интеграция снапшота в `SupervisorLoop`** (§4): `StateWriter`,
   `with_state_file`, `snapshot()`, `maybe_write_state`, вызовы в `run()`,
   удаление при выходе; новый `tests/state.rs` (§7.4).
5. **CLI** (§6): `src/cli.rs` + юниты §7.1; переписать `main.rs`; переписать
   `tests/cli.rs` (§7.6); перевести `start_supervisor` в `tests/signals.rs` и
   `tests/tree.rs` на новую форму (§7.7).
6. **e2e `tests/status.rs`** (§7.5). Прогнать 10 раз подряд
   (`for i in $(seq 10); do cargo test --test status || break; done`), как и
   обновлённые `signals`/`tree`.
7. **Доки** (§9).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 9. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`:
  - раздел Этапа 5 переписать с «решить на месте» на принятые решения: файл
    состояния (решение пользователя; сигнальный механизм Этапа 3 не
    пересматривается — условие self-pipe не сработало), ручной разбор args
    (решение пользователя), чистый слом CLI без алиаса (решение пользователя),
    формат/путь/атомарность/интервал/удаление файла, семантика `status` и
    exit-код 1, ограничение «упавшие на старте процессы в status не видны»,
    staleness ≤ 1 с;
  - критерий приёмки Этапа 1: пометить, что форма «единственный позиционный
    аргумент» действовала до Этапа 5 и заменена подкомандой `run`;
  - раздел «Техдолг Этапа 4»: пометить закрытыми три пункта — объединение
    `child`/`pgid` (шаг 2), тест параллельности гашения и тест нового pgid
    после рестарта (шаг 1); у остальных двух явно указать «не сработал»;
  - модульная структура: добавить `state.rs`, `cli.rs`.
- `README.md`: «Быстрый старт» — `cargo run -- run examples/supervisor.toml`,
  пример `supervisor-rs status`; строку статуса этапов обновить.
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`: в «грабли» добавить —
  toml-сериализация падает на `None` без `skip_serializing_if`; атомарность
  файла состояния = tmp-in-same-dir + rename (tempfile в проде недоступен —
  dev-dep); handshake e2e-тестов через файл состояния вместо новой копии
  `wait_for_pid`; каждый e2e-запуск бинарника обязан изолировать
  `--state-file` в tempdir.
- `examples/supervisor.toml`: первая строка-комментарий («shape is finalised
  in Этап 1; illustrative for now») устарела — заменить на актуальную; полей
  конфига этап не добавляет.
- `src/main.rs`: снять `TODO(Этап 5)` (проверить `grep -rn "TODO(Этап 5)"
  src/` — должен опустеть), doc-заголовок — под подкоманды.
- Финальную редакцию формулировок делает основная сессия — здесь достаточно
  фактической точности.

## 10. Границы — что НЕ трогать

- `src/signal.rs` целиком: `AtomicI32`/`take_pending`/`install_handlers` — файл
  состояния не делает цикл событийным, условие пересмотра Этапа 3 не
  сработало. Self-pipe/signalfd не вводить.
- Порядок peek → killpg-sweep → reap и все комментарии-обоснования вокруг него;
  `Running` = «killpg безопасен», обнуляется только после реапинга.
- `tick()`: никакой записи состояния и никаких блокирующих sleep внутри
  (исключение `handle_poll_error` остаётся как есть); poll-loop с тиком 50 мс.
- API `Clock`/`FakeClock` не расширять (wall-clock таймштамп живёт в
  `snapshot()`, а не в Clock).
- Семантика restart policy / backoff / подавления рестартов; машина
  `StopPhase`; идемпотентность `begin_shutdown`; бюджет poll-ошибок.
- Таблица приоритетов exit-кодов Этапа 3: usage → 2, config → 1,
  `had_start_errors` → 1, иначе 0; `status` добавляет только свой «1 — демон
  не запущен / файл нечитаем», ничего не меняя для `run`.
- Подкоманды `stop` / `restart <name>` и полноценный control-socket — POST-MVP
  (`docs/POST_MVP_PLAN.md`, «Полноценный control-socket»), не реализовывать.
  Экспорт метрик, health-checks — там же.
- Зависимости: новых крейтов нет; единственная правка `Cargo.toml` — фича
  `"user"` у существующего `nix` (обоснование в §3). `tempfile` остаётся
  dev-only. Docker Compose не добавлять.
- Известные ограничения Этапа 4 (5 пунктов) — принятое поведение.

## 11. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` — зелёные;
   `cargo test --test status`, `--test shutdown`, `--test signals`,
   `--test tree` — зелёные 10 прогонов подряд.
2. Все тесты Этапов 1–4 проходят; изменения в них — только форма запуска
   бинарника (`run` + `--state-file`) и триггерные тесты §7.3.
3. `tests/status.rs::status_shows_running_process_and_detects_daemon_exit`
   доказывает критерий приёмки: `status` на живом демоне печатает
   имя/состояние/pid, после завершения демона — внятная ошибка и exit 1.
4. Старая CLI-форма мертва и зафиксирована тестом
   (`bare_config_path_is_a_usage_error`); exit-коды: usage 2, config 1,
   start-errors 1, штатно 0, `status` без демона 1.
5. Техдолг Этапа 4: `Running`-рефактор сделан (шаг 2), оба сработавших
   триггерных теста зелёные (шаг 1); `grep -rn "TODO(Этап 5)" src/` пуст.
6. Атомарность записи: `state.toml.tmp` не остаётся после записи; читатель
   никогда не видит частичный файл (поллинг-хелпер §7.4/§7.5 не ловит
   парс-ошибок за 10 прогонов).
7. Доки из §9 актуализированы.
