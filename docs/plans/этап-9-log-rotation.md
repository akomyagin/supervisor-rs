# План Этапа 9 — Ротация логов (захват stdout/stderr через pipe, ротация по размеру)

Ветка: `этап-9/log-rotation` (уже создана). Исполнителю: никаких git-коммитов —
commit/push/PR делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/health.rs` / `tests/health_e2e.rs` /
`tests/reload.rs`.

## 1. Цель и критерий приёмки

Сейчас stdout/stderr супервизируемых процессов наследуются от супервизора
(`Command` не настраивает stdio — `Stdio::inherit()` по умолчанию, см.
`src/process.rs::spawn`): вывод детей мешается с `tracing`-логом демона и никак
не сохраняется. Этап 9 добавляет опциональный **захват вывода в файлы с
ротацией по размеру**, целиком внутри супервизора:

```toml
[[process]]
name = "web"
command = ["/usr/bin/myserver", "--port", "8080"]

[process.log]
stdout-path = "/var/log/web.stdout.log"
stderr-path = "/var/log/web.stderr.log"
max-size-bytes = 10485760   # default 10 MiB
keep = 5                     # default 5; ротированные — .1 (новейший) … .keep
```

- Без секции `[process.log]` поведение прежнее до байта: наследование
  дескрипторов, ноль новых механик (opt-in по прецеденту
  `[process.health-check]` Этапа 7).
- С секцией: сконфигурированный поток (stdout, stderr или оба, раздельные
  файлы) идёт через pipe в супервизор; отдельный поток ОС на каждый захваченный
  поток вывода читает pipe и дописывает в текущий файл; когда файл достигает
  `max-size-bytes`, выполняется сдвиг `<path>.{keep-1}`→`<path>.keep`, …,
  `<path>.1`→`<path>.2`, `<path>`→`<path>.1` и открывается новый пустой
  текущий файл. Хранится не более `keep` ротированных файлов плюс текущий.
- Проверка размера — непрерывно, в момент каждой записи; никаких сигналов,
  внешних инструментов и интервальных таймеров.
- Вывод форкнутых потомков (внуков), унаследовавших stdout лидера, попадает в
  тот же файл — pipe наследуется через `fork()` как обычный дескриптор.

Критерий приёмки: на живом демоне процесс с `[process.log]` и болтливым
stdout наполняет `stdout-path`; при достижении `max-size-bytes` появляется
`<path>.1`, при продолжении — `.2` и далее, но никогда больше `keep`
ротированных; содержимое не теряется на границах ротации (конкатенация
`.N….1 + текущий` содержит весь вывод); процесс без секции работает как
раньше. Рестарт процесса (policy/оператор/health/reload) продолжает писать в
те же файлы (append), SIGTERM демону — exit 0, лог-файлы остаются на диске.
Секция без единого пути или с совпадающими путями — ошибка загрузки конфига
(exit 1), не паника в рантайме.

**Отклонено: интеграция с внешним `logrotate` через SIGHUP** (упоминалась в
`POST_MVP_PLAN.md`). Причина: SIGHUP с Этапа 8 занят перезагрузкой конфига
(`src/signal.rs::RELOAD_PENDING`), второй смысл тому же сигналу давать нельзя
(правило «каждый качественно новый смысл сигнала — свой флаг» из SKILL.md
здесь не помогает: у logrotate-конвенции сигнал фиксирован — именно SIGHUP).
Ротация — полностью внутренняя, без сигналов вообще. Формулировку в
`POST_MVP_PLAN.md` снять при актуализации доков (§10).

## 2. Архитектурные решения — приняты пользователем, НЕ пересматривать

1. **Ротация — полностью в супервизоре**, без внешних инструментов и без
   сигналов. Опциональная секция `[process.log]` на процесс (прецедент
   `[process.health-check]`): раздельные файлы для stdout/stderr, лимит
   `max-size-bytes` (default 10 MiB), ретеншн `keep` последних файлов
   (`.1`…`.keep`, default 5), **без сжатия** (осознанно не в v1 — как триада
   health-проб в Этапе 7). Проверка размера — в момент записи, не по сигналу
   и не по интервалу.
2. **Механизм live-ротации — отдельный поток ОС на каждый захваченный поток
   вывода**, читающий из pipe, а не прямая запись ребёнка в файл. Это
   **первое использование потоков в проекте** (до сих пор — однопоточный
   поллинг-цикл на `FakeClock`). Обоснование, зафиксировать комментарием:
   ребёнок пишет в унаследованный дескриптор напрямую; переименуй супервизор
   файл под открытым дескриптором (`.log` → `.log.1`) — дескриптор продолжит
   писать в старый inode, новый `.log` останется пуст (переоткрывать stdout —
   не контракт ребёнка). Copy-truncate (скопировать + `ftruncate`) отвергнут:
   позиция записи открытого дескриптора не сбрасывается при truncate — первая
   же запись после ротации создала бы sparse-дыру размером в старый файл.
   Pipe решает оба: у файла ровно один писатель — поток-читатель супервизора,
   он и ротирует.

### 2.1 Решения плана (приняты здесь; утверждает основная сессия при ревью)

- **v1 ротирует только по размеру; ротация по времени — не делается** (в
  исходной формулировке POST_MVP было «по размеру и/или времени», пример
  пользователя несёт только `max-size-bytes`). Обоснование: time-based
  ротация потребовала бы таймера в потоке-читателе — второй, реальный таймер
  вне `Clock`/`FakeClock`, ненаблюдаемый в детерминированных тестах, — ради
  сценария «редко пишущий лог должен стареть по календарю», которого у
  учебного супервизора нет. Остаётся кандидатом в POST_MVP.
- **Хотя бы один из `stdout-path`/`stderr-path` обязателен** — секция без
  обоих бессмысленна, а «тихая» пустая секция маскировала бы опечатку в имени
  ключа (та же логика, что тест `rejects_snake_case_stop_grace`). Каждый путь
  опционален по отдельности: можно захватывать только stdout, только stderr
  или оба.
- **Все сконфигурированные лог-пути уникальны глобально** (в пределах секции
  и между процессами): два потока-писателя на один файл — несинхронизированная
  гонка ротации (оба переименовывают, оба считают размер). Повтор пути →
  `ConfigError::Invalid` при загрузке — прецедент уникальности имён процессов
  Этапа 8, тот же вариант ошибки, тот же exit 1.
- **`max-size-bytes` и `keep` — `NonZeroU64`/`NonZeroU32`**: ноль лимита
  означал бы ротацию на каждой записи, ноль keep — удаление лога сразу после
  ротации; оба — бессмыслица, отсекаемая бесплатно типом (прецедент
  `NonZero*` health-полей). «Захват без ротации» отдельным режимом не
  вводится — практический эквивалент задаётся большим `max-size-bytes`.
- **Состояние ротации (текущий размер, файловый хендл) живёт целиком внутри
  потока-читателя** — локальные переменные потока, супервизор их не видит.
  `Supervised` владеет только `JoinHandle`-ами для присоединения при реапинге.
  Синхронизировать нечего: размер файла и сдвиг `.N` — внутренняя механика
  одного писателя; единственный факт, нужный супервизору, — «поток завершился»
  (= EOF дочитан, всё записано), и его несёт сам `join`.
- **Потоки-читатели присоединяются в момент реапинга лидера** (в `tick()`,
  ветка `PollOutcome::Exited`, и в give-up пути `handle_poll_error`), а не при
  остановке демона. Три причины (§6.3): EOF к этому моменту гарантирован
  killpg-sweep'ом; респавн открывает читателя на **тот же путь** — join до
  респавна сохраняет инвариант «один писатель на файл»; тестам достаётся
  синхронная гарантия «pid реапнут ⟹ файл полон». Join **ограниченный**
  (поллинг `JoinHandle::is_finished()` с реальным дедлайном), не голый
  `join()` — чтобы потомок, сбежавший из process-группы, не подвесил демона
  навечно (§6.3).
- **Лог-файлы не удаляются при выходе демона** — в отличие от state-файла и
  сокета: те — служебные артефакты рантайма, лог — продукт для оператора.
- **Ошибки файлового IO у читателя никогда не останавливают чтение pipe**
  (drain-first, §5): вывод отбрасывается с warn-логом и попыткой
  восстановления, но pipe дочитывается до EOF всегда — остановка чтения либо
  блокирует ребёнка на полном буфере pipe, либо (при закрытом read-конце)
  убивает его SIGPIPE.

## 3. Конфиг: `src/config.rs` — секция `[process.log]`

По прецеденту `HealthCheckConfig`: сырая структура с serde + ручная
кросс-валидация в `load()`; бесплатная валидация типами (`NonZero*`).

```rust
/// Default rotation threshold for a captured log file, bytes (10 MiB).
pub const DEFAULT_LOG_MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024;
/// Default number of rotated files kept (`.1` … `.keep`), besides the current.
pub const DEFAULT_LOG_KEEP: u32 = 5;

/// Raw, as-parsed `[process.log]` section (Этап 9): capture the process's
/// stdout/stderr into files with size-based rotation, fully inside the
/// supervisor. Cross-field validation (at least one path, distinct paths)
/// happens in [`LogConfig::validate`], called by [`load`] — a bad section is
/// a config error at load time, never a runtime panic. `Clone` + `PartialEq`
/// keep the Этап 8 reload diff working: a changed log section is a config
/// change like any other and forces a restart.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LogConfig {
    /// Capture the child's stdout into this file. Optional: either stream can
    /// be captured on its own; an uncaptured stream stays inherited.
    #[serde(rename = "stdout-path", default)]
    pub stdout_path: Option<PathBuf>,
    /// Capture the child's stderr into this file.
    #[serde(rename = "stderr-path", default)]
    pub stderr_path: Option<PathBuf>,
    /// Rotate once the current file reaches this many bytes. NonZero: a zero
    /// limit would rotate on every write (free type-level validation, the
    /// `NonZero*` precedent of Этап 7).
    #[serde(rename = "max-size-bytes", default = "default_log_max_size")]
    pub max_size_bytes: NonZeroU64,
    /// How many rotated files to keep (`.1` newest … `.keep` oldest). NonZero:
    /// zero would delete output right after rotating it.
    #[serde(default = "default_log_keep")]
    pub keep: NonZeroU32,
}

impl LogConfig {
    /// Validates the section. Pure; unit-tested directly. Every violation is
    /// an `Err(String)` naming the problem, so [`load`] can turn it into a
    /// `ConfigError::Invalid` with the process name (the `HealthCheckConfig::probe`
    /// pattern).
    pub fn validate(&self) -> Result<(), String> {
        // 1) at least one of stdout-path / stderr-path;
        // 2) stdout-path != stderr-path (two writers on one file would race
        //    the rotation).
    }
}
```

- `ProcessConfig` получает поле **последним** (после `health_check`; правило
  «скаляры до подтаблиц» соблюдено, подтаблицы между собой — в порядке
  появления этапов):

```rust
    /// Optional stdout/stderr capture with size-based rotation (Этап 9). A
    /// `[process.log]` subtable; without it stdio is inherited, exactly as
    /// before.
    #[serde(default)]
    pub log: Option<LogConfig>,
```

  Derived `Clone`/`PartialEq` у `ProcessConfig` подхватывают поле
  автоматически — reload-diff Этапа 8 видит изменение секции как «поле
  конфига изменилось» → принудительный рестарт по существующей ветке
  `changed` в `apply_config`. Проверено чтением `plan_reload`
  (`src/supervise.rs:266` — сравнивает `ProcessConfig` целиком); **новых
  веток в reload не нужно**, только тест (§8.4).
- `load()` после существующих проверок (уникальность имён, health): для
  каждого `proc.log` — `validate()`, `Err(msg)` → `ConfigError::Invalid` с
  префиксом `process "<name>": ` (как health); затем глобальная уникальность
  путей: собрать все `stdout_path`/`stderr_path` всех процессов в
  `HashSet<&Path>`, повтор → `Invalid` с сообщением вида
  `duplicate log path "/var/log/x.log"` (решение §2.1). Doc-комментарий
  `ConfigError::Invalid` дополнить третьим примером (log-секция).
- Относительные пути не запрещаются и резолвятся против cwd демона (как
  `--state-file`); зафиксировать в doc-комментарии поля.

Механическая правка: **все литералы `ProcessConfig { … }` получают
`log: None`** — `grep -rn "ProcessConfig {" src tests`; сейчас это 39 мест в
10 файлах (`src/config.rs`, `src/process.rs`, `src/supervise.rs`,
`tests/commands.rs`, `tests/health.rs`, `tests/reload.rs`, `tests/restart.rs`,
`tests/shutdown.rs`, `tests/spawn.rs`, `tests/state.rs`). Плюс два
существующих теста расширяются новым вариантом поля (это их прямое
назначение — страховка полноты): `process_config_equality_notices_every_field`
(`src/config.rs`) и `plan_reload_notices_each_field` (`src/supervise.rs`)
получают пары «без секции / с секцией» и «секция / секция с другим
`max-size-bytes`».

## 4. Захват: `src/process.rs` — pipe и `Spawned`

### Сигнатура

`spawn` начинает возвращать, кроме ребёнка, read-концы pipe-ов:

```rust
/// A freshly spawned process together with the supervisor-side read ends of
/// its capture pipes (Этап 9). A field is `Some` exactly when the config's
/// `[process.log]` section names a path for that stream; without the section
/// both are `None` and stdio is inherited, exactly as before.
pub struct Spawned {
    pub child: Child,
    pub stdout_capture: Option<OwnedFd>,
    pub stderr_capture: Option<OwnedFd>,
}

pub fn spawn(cfg: &ProcessConfig) -> Result<Spawned, SpawnError>
```

`SpawnError` получает третий вариант (Display/Error в стиле существующих):

```rust
    /// Этап 9: creating a capture pipe failed before the process was spawned.
    CapturePipe {
        name: String,
        source: nix::errno::Errno,
    },
```

### Тело (внутри `spawn`, до `cmd.spawn()`)

```rust
let mut stdout_capture = None;
let mut stderr_capture = None;
if let Some(log) = &cfg.log {
    if log.stdout_path.is_some() {
        let (read, write) = nix::unistd::pipe().map_err(/* CapturePipe */)?;
        cmd.stdout(std::process::Stdio::from(write)); // write end MOVES in
        stdout_capture = Some(read);
    }
    if log.stderr_path.is_some() {
        let (read, write) = nix::unistd::pipe().map_err(/* CapturePipe */)?;
        cmd.stderr(std::process::Stdio::from(write));
        stderr_capture = Some(read);
    }
}
```

Group-механика Этапа 4 не трогается: pipe-ы создаются **до** `pre_exec`-хука,
`setsid` в ребёнке и порядок peek → sweep → reap не меняются ни на строку.

### КРИТИЧНАЯ грабля фд-гигиены — закрыть родительскую копию write-конца

`Command::spawn()` дублирует переданный `Stdio` в форкнутого ребёнка, но
исходный дескриптор остаётся у родителя, пока жив владелец. Если родительская
копия write-конца не закрыта, поток-читатель **никогда не увидит EOF** — даже
после смерти всего дерева ребёнка — и потоки будут копиться при каждом
респавне на всю жизнь демона.

Здесь это решается владением: `nix 0.29` возвращает пару `OwnedFd`;
write-конец **перемещается** в `Stdio::from(write)` внутрь `cmd`, а `cmd` —
локальная переменная `spawn()`, дропающаяся при выходе из функции (вместе с
хранимым `Stdio` и его fd). После возврата `spawn()` write-конец держит только
ребёнок. Обязательный комментарий у этого места: **не выносить `Command` за
пределы `spawn()` и не хранить `Stdio` где-либо** — иначе EOF не наступит
никогда; тест `spawn_with_log_reaches_eof_after_child_exit` (§8.2) ловит
регрессию (утёкший write-конец = вечно блокирующийся read = громко висящий
тест).

Используется именно `pipe()`, не `pipe2(O_CLOEXEC)`: `pipe2` за фичей `fs`
крейта `nix`, а `Cargo.toml` заморожен (§11). Это корректно, потому что
супервизор форкает строго однопоточно и последовательно: между `pipe()` и
`cmd.spawn()` чужой `fork()` невозможен, а write-конец закрыт до следующего
спавна. Косметическое следствие: долгоживущие read-концы (у потоков-читателей)
наследуются позже спавнящимися детьми и exec-пробами как лишние fd — на EOF
это не влияет (EOF определяется write-концами), записать в известные
ограничения (§11).

### Правка вызовов

`process::spawn` вызывается из: `src/supervise.rs` (`start_supervised`,
респавн-ветка `tick()`, юнит-тест give-up), `src/process.rs` (3 юнит-теста),
`tests/spawn.rs` (3 места). Тесты без log-секции берут `.child` из `Spawned`
механически; supervise — §6.

## 5. Ротация и поток-читатель: новый модуль `src/logs.rs`

`pub mod logs;` в `lib.rs`. Модуль ничего не знает о `SupervisorLoop` и
`Clock` (ротация по размеру — часов нет вообще): чистое ядро ротации +
функция запуска потока. Разделение — чтобы ядро юнит-тестировалось на
tempdir без процессов и потоков (прецедент `health.rs`: раннер отдельно от
расписания).

### 5.1 Чистое ядро: `RotatingFile`

```rust
/// One rotating log file: append-only writes with a size check on every write
/// (the user's decision: rotation is continuous, at write time — no signals,
/// no timers). All rotation state (current size, open handle) lives here,
/// inside the owning reader thread; the supervisor never sees it.
pub struct RotatingFile {
    path: PathBuf,
    max_size_bytes: u64,
    keep: u32,
    /// `None` until the first write, and again after a failed open; re-opened
    /// lazily so a transient FS error (full disk, missing dir) heals itself.
    file: Option<std::fs::File>,
    /// Size of the current file, tracked incrementally; initialised from
    /// metadata on open so a respawned instance appends and resumes the count.
    size: u64,
}

impl RotatingFile {
    /// Does not touch the filesystem; the first `write` opens the file.
    pub fn new(path: PathBuf, max_size_bytes: u64, keep: u32) -> Self;

    /// Appends `chunk`, rotating *after* the write once `size >=
    /// max_size_bytes`. An `Err` means the chunk was dropped; the caller logs
    /// and keeps going — see the drain-first policy.
    pub fn write(&mut self, chunk: &[u8]) -> std::io::Result<()>;
}
```

- **Открытие**: `OpenOptions::new().append(true).create(true)`, `size` — из
  `metadata().len()` (респавн и рестарт демона продолжают существующий файл,
  не truncate). Родительская директория не создаётся — её отсутствие это
  ошибка записи, обрабатываемая политикой ниже.
- **Ротация** (`fn rotate(&mut self)`): дропнуть `file`; затем
  `for i in (1..keep).rev() { rename("<path>.{i}", "<path>.{i+1}") }`
  (NotFound игнорируется — цепочка ещё не полна; на Unix `rename` молча
  замещает существующую цель, поэтому старейший `.keep` исчезает без
  отдельного `remove_file`); затем `rename(path, "<path>.1")`; следующий
  `write` лениво откроет новый пустой текущий. Ошибки rename — best-effort:
  warn-лог и продолжение (лучше переполненный файл, чем потерянный вывод).
- **Момент проверки — после записи** (`size >= max` → rotate). Запись
  безусловна, инвариант прост («после ротации текущий файл пуст»); цена —
  файл может превысить лимит на ≤ один чанк (`READ_BUF_BYTES`, 8 KiB против
  дефолтных 10 MiB — ничтожно). Rotate-before рассмотрен и отвергнут: он
  экономит эти байты, но добавляет ветку «первый чанк сам больше лимита»
  (писать всё равно надо) и не спасает целостность строк — чанки из pipe
  режутся по границам `read()`, не по `\n`, так что строка и так может
  разъехаться между `.1` и текущим (известное ограничение, §11).

### 5.2 Поток-читатель: `spawn_reader`

```rust
/// Read-buffer size for the capture pipe. One pipe-buffer chunk at most.
const READ_BUF_BYTES: usize = 8192;

/// Spawns the reader thread for one captured stream: blockingly reads the
/// pipe until EOF, appending everything to a `RotatingFile`. EOF arrives only
/// once EVERY holder of the pipe's write end is gone — the child and every
/// descendant that inherited the fd — which the Этап 4 killpg-sweep on leader
/// exit already guarantees to converge (§6.3). The thread therefore needs no
/// stop signal of its own: death of the tree is its shutdown protocol.
pub fn spawn_reader(
    fd: OwnedFd,
    path: PathBuf,
    max_size_bytes: u64,
    keep: u32,
    proc_name: String,
    stream: &'static str, // "stdout" | "stderr", for logs and the thread name
) -> std::io::Result<std::thread::JoinHandle<()>>
```

- `std::thread::Builder::new().name(format!("log-{proc_name}-{stream}"))` —
  имя потока попадает в отладчик/panic-сообщения.
- Тело: `let mut pipe = std::fs::File::from(fd);` (у `OwnedFd` есть
  `From`-конверсия; `File` даёт `Read`), буфер `[u8; READ_BUF_BYTES]`, цикл:
  - `Ok(0)` → EOF: единственный штатный выход, `debug!`-лог, конец потока;
  - `Ok(n)` → `rotating.write(&buf[..n])`, ошибка — по политике ниже;
  - `Err(Interrupted)` → `continue` (обработчики сигналов процесса ставятся с
    `SA_RESTART`, так что EINTR здесь скорее теоретичен — но ветка дешёвая);
  - другой `Err` от `read` → `error!`-лог, конец потока (pipe сломан — EOF не
    придёт, держать поток незачем).
- **Drain-first политика ошибок записи** (решение §2.1, зафиксировать
  комментарием): `Err` от `rotating.write` → warn-лог и **продолжение
  чтения**; чанк потерян, следующий `write` попробует переоткрыть файл
  (`file: None` после неудачи). Прекращать чтение нельзя: живой write-конец
  при остановленном читателе блокирует ребёнка, как только заполнится
  64 KiB-буфер pipe, а дроп read-конца при живом писателе доставляет ребёнку
  SIGPIPE — дефолтная диспозиция убивает его. Warn-лог — только на смену
  состояния ok→err (и info на восстановление), не на каждый чанк: полный диск
  не должен генерировать 20 строк лога в секунду.
- Паник внутри потока быть не должно (все IO-результаты обработаны); паника
  всё равно не уронит демона (панику потока поглощает `JoinHandle`), а
  bounded-join её увидит и залогирует.

## 6. Интеграция в `SupervisorLoop` (`src/supervise.rs`)

### 6.1 Состояние: `LogState` на `Supervised`

```rust
/// Handles of the capture reader threads for the *current* instance (Этап 9).
/// Present exactly when the instance was spawned with at least one capture
/// pipe. Unlike `health` (config-derived, stable across an instance's life),
/// this is instance-derived: armed at every (re)spawn, taken at reap. All
/// rotation state lives inside the threads (plan §2.1); the supervisor only
/// ever joins them.
struct LogState {
    stdout: Option<std::thread::JoinHandle<()>>,
    stderr: Option<std::thread::JoinHandle<()>>,
}
```

Поле `log: Option<LogState>` на `Supervised` — после `health`. В снапшот
(`snapshot()`) ничего не добавляется (§7).

### 6.2 `start_instance` — общий путь spawn + потоки

Спавн инстанса теперь двухфазный (процесс + потоки), и он нужен в двух местах
(`start_supervised` и респавн-ветка `tick()`) — извлечь общий хелпер, как
`start_supervised` в Этапе 8:

```rust
/// Spawns one process instance and, if capture is configured, its reader
/// threads — the shared construction of `start_supervised` and the respawn
/// branch of `tick()`. A reader thread that cannot be spawned (OS thread
/// exhaustion) fails the whole instance: the child is SIGKILLed and reaped
/// here, and the error surfaces as a SpawnError — a silently capture-less
/// (or worse, pipe-wedged) instance would violate the configured contract.
fn start_instance(config: &ProcessConfig) -> Result<(Running, Option<LogState>), process::SpawnError>
```

- `process::spawn(config)?` → `Spawned { child, stdout_capture, stderr_capture }`;
  `pgid = Pid::from_raw(child.id())` — как сейчас.
- Для каждого `Some(fd)` — `logs::spawn_reader(fd, path.clone(),
  log.max_size_bytes.get(), log.keep.get(), name, "stdout"/"stderr")`. Пути и
  лимиты — из `config.log` (`Some` гарантирован: fd существует только если
  путь был задан).
- Ошибка `Builder::spawn` (по сути OOM, экзотика): `error!`-лог,
  `signal_group(pgid, SIGKILL)` + `child.wait()` (реапнуть обязательно — как
  exec-проба Этапа 7 реапит убитого по таймауту), уже запущенный поток-близнец
  при этом получит EOF и завершится сам; вернуть
  `SpawnError::Spawn { name, source }` с io-ошибкой билдера. Дропать fd при
  живом ребёнке нельзя (SIGPIPE), оставлять открытым навсегда — нельзя
  (ребёнок заблокируется на полном буфере молча).
- `start_supervised` вызывает `start_instance` и кладёт `log` в литерал
  `Supervised { …, log, … }`; респавн-ветка `tick()` заменяет прямой
  `process::spawn(&proc.config)` на `start_instance(&proc.config)` и в
  `Ok`-рукаве дополнительно ставит `proc.log = log_state` (в `Err`-рукаве
  поведение прежнее — backoff-ретрай).

### 6.3 Присоединение потоков — в момент реапинга лидера

**Почему EOF гарантирован к реапингу.** EOF на pipe наступает, когда закрыт
*последний* write-конец — включая копии, унаследованные внуками через их
`fork()` (проставить на них CLOEXEC супервизор не может — это адресное
пространство чужого процесса). Существующий порядок Этапа 4 в `poll_child`
(peek → **killpg-SIGKILL-sweep всей группы** → reap) сводит все копии к нулю:
к моменту, когда `tick()` видит `PollOutcome::Exited`, вся группа уже под
SIGKILL. Никакого отдельного «стоп-сигнала» потоку не нужно — teardown Этапа 4
и есть его протокол завершения; поток просто дочитывает остаток буфера и
выходит. **Не городить новый механизм остановки потоков.**

**Почему join именно здесь, а не при выходе демона** (решение §2.1):

1. следующий инстанс после респавна открывает читателя на **тот же путь**;
   join до респавна — единственное, что сохраняет инвариант «у файла один
   писатель» (внутри одного `tick()` реапинг и респавн не совпадают: респавн —
   отдельная ветка следующего вызова `tick()`, так что join в ветке `Exited`
   всегда предшествует новым потокам);
2. тестам достаётся синхронная гарантия: «`Exited` обработан ⟹ файл
   содержит весь вывод инстанса, ротация применена» — не нужен новый
   примитив ожидания;
3. join почти мгновенен (EOF уже наступил или наступает — sweep отправлен
   микросекунды назад).

**Почему join ограниченный, а не голый `join()`.** «Почти мгновенен» имеет
одно исключение: потомок, сбежавший из process-группы (сам вызвал `setsid`),
недостижим для killpg — это существующее известное ограничение Этапа 4 («живой
орфан»). Такой орфан держит унаследованный write-конец, EOF не наступает, и
безусловный `join()` подвесил бы `tick()` — а с ним весь демон — навечно.
Поэтому:

```rust
/// How long, and in what steps, the reap path waits for the capture readers
/// to drain the pipe and finish. Real time, like REAP_RETRIES: what we wait
/// for is the kernel delivering SIGKILL to stragglers plus the reader's last
/// writes — not logical time, so a FakeClock must not skip it. On timeout the
/// handle is dropped (the thread is detached): the one way to get here is a
/// descendant that escaped the process group and pins the pipe open — the
/// Этап 4 "escaped setsid orphan" limitation, which must not wedge the loop.
const LOG_JOIN_RETRIES: u32 = 100;
const LOG_JOIN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Bounded join of the current instance's capture readers; call once the
/// leader has been reaped (EOF is then guaranteed to converge, see above).
fn join_log_readers(proc: &mut Supervised)
```

- Реализация: `let Some(log) = proc.log.take() else { return };` затем для
  обоих хендлов — общий реальный дедлайн (`LOG_JOIN_RETRIES ×
  LOG_JOIN_RETRY_DELAY` = 1 с суммарно, не на поток): поллинг
  `handle.is_finished()` с `thread::sleep(LOG_JOIN_RETRY_DELAY)`; финиш →
  `handle.join()` (уже не блокирует; `Err` паники — `error!`-лог); таймаут →
  `warn!`-лог с именем процесса/потока и дроп хендла (detach). Блокировка
  цикла до 1 с — четвёртое санкционированное исключение из «цикл не
  блокирует», того же класса, что `handle_poll_error` (реальный retry на
  give-up пути): нормальный случай — первая-вторая итерация.
- Детач — осознанный компромисс: сбежавший орфан, продолжающий писать,
  оставляет старого читателя жить; его вывод продолжает попадать в файл, но
  ротация старого и нового читателей не синхронизирована (§11). Альтернатива —
  вечный join — превращала бы известное ограничение «орфан выживает» в
  «демон повис», что строго хуже.

**Точки вызова** (все правки существующего кода — эти три плюс респавн из
§6.2):

1. `tick()`, ветка `PollOutcome::Exited` — сразу после `proc.running = None;
   proc.stop = StopPhase::Idle;`, до ветвления по `shutting_down`/`intent`:
   `join_log_readers(proc);`. Покрывает все пути: policy-рестарт, оператор,
   health, reload-remove (запись прюнится в конце того же `tick()` — уже с
   присоединёнными потоками), shutdown.
2. `handle_poll_error`, give-up путь — после `proc.running = None;`:
   лидер убит SIGKILL и best-effort реапнут, EOF сходится так же;
   `join_log_readers(proc);`.
3. Выхода `run()` это не касается: к моменту `!any_active()` каждый процесс
   прошёл через 1 или 2, потоков в полёте нет — отдельный teardown не нужен
   (зафиксировать комментарием у `run()`-эпилога не нужно, инвариант следует
   из точек 1–2).

### 6.4 Reload Этапа 8 — правок нет, только тест

Изменение `[process.log]` при неизменном прочем попадает в `changed` через
derived `PartialEq` (§3) → принудительный рестарт по существующей ветке
`apply_config`; новый инстанс в `start_instance` открывает pipe и потоки уже
по новому конфигу. `resurrect_removed` и `unchanged`-путь также не трогаются.
Убедиться тестом `changed_log_config_forces_restart` (§8.4), а не декларацией.

## 7. Наблюдаемость и state-файл — решения

- **Схема state-файла НЕ меняется, `version` остаётся 1** (прецедент Этапа 7:
  health-статус тоже не в снапшоте). Пути логов и состояние ротации в
  `status` не отражаются — лог наблюдаем самим лог-файлом.
- **Логи `tracing` — единственный канал наблюдения механики**: `debug!` на
  EOF/завершение читателя; `warn!` на ошибку записи (по смене состояния) и на
  detach по таймауту join; `error!` на ошибку `read` и на неудачу спавна
  потока; `info!` не добавляется (захват — не событие, ротация — рутина;
  прецедент «debug на рутинный успех пробы»).
- CLI (`src/cli.rs`), control-socket (`src/control.rs`), `main.rs` — ноль
  правок: ни флагов, ни команд, ни новых exit-кодов (невалидная log-секция —
  существующий класс `ConfigError::Invalid` → exit 1).

## 8. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность по содержимому файла, поллинг с дедлайном вместо sleep, изоляция
`--state-file` и `--control-socket` в tempdir у каждого e2e-запуска, новые
тесты на процессы/потоки прогнать 10 раз подряд.

**Паттерн наблюдения лог-файла — двухрежимный** (первое отступление от «весь
супервизор — один поток на `FakeClock`», зафиксировать в преамбуле
`tests/logs.rs`):

- **процесс жив** → данные пишутся из другого потока асинхронно к главному
  циклу; единственный корректный ассерт — **поллинг файла с реальным
  дедлайном** (хелпер `wait_for_log(path, pred, timeout)` — тот же паттерн,
  что `wait_for_pid`/маркер-файлы из SKILL.md; poll по *содержимому*, не по
  существованию);
- **лидер реапнут** (после `tick_until_reaped` / обработки `Exited`) → потоки
  присоединены bounded-join'ом (§6.3), файл полон, ротация применена —
  **синхронный ассерт без поллинга**. Это и есть тестируемое следствие
  решения «join при реапинге».

Вывод стабов — только `echo`/`printf` шелла: каждая команда — отдельный
`write(2)` без буферизации stdio (пишущий в pipe C-процесс буферизует stdout
блоками — вывод «зависал» бы в его буфере, флейк).

### 8.1 Юниты `src/config.rs` (mod tests)

- `parses_log_section_full` — оба пути, `max-size-bytes`, `keep`.
- `parses_log_with_defaults` — только `stdout-path`: `max_size_bytes` =
  10485760, `keep` = 5, `stderr_path` = None.
- `parses_log_with_only_stderr`.
- `rejects_log_without_any_path` — через `load()`: `ConfigError::Invalid`,
  Display содержит `process "web"` и `stdout-path`/`stderr-path`.
- `rejects_log_with_identical_paths` — stdout-path == stderr-path → Invalid.
- `rejects_duplicate_log_paths_across_processes` — два процесса, один путь →
  Invalid с этим путём.
- `rejects_zero_log_max_size_and_zero_keep` — оба нуля → ошибка парсинга
  (`NonZero*`), по образцу `rejects_zero_port_interval_timeout_and_threshold`.
- `process_without_log_parses` — поле `None`.
- Дополнение `process_config_equality_notices_every_field` — вариант с
  log-секцией (None→Some и Some→изменённый `max-size-bytes`).

### 8.2 Юниты `src/logs.rs` (mod tests) — `RotatingFile` без процессов и потоков

На tempdir, крошечные лимиты (десятки байт):

- `writes_below_limit_do_not_rotate` — контент на месте, `.1` нет.
- `reaching_limit_rotates_current_into_dot1` — содержимое до лимита уехало в
  `.1`, текущий после следующей записи начинается заново.
- `rotation_chain_shifts_and_drops_oldest` — keep = 2, три ротации: `.1`
  новее `.2`, самый старый контент исчез, файлов ротированных ровно 2.
- `keep_one_replaces_single_rotated_file`.
- `existing_file_is_appended_and_counted` — файл с содержимым близко к
  лимиту существует до `new()` → первая запись дописывает и ротирует
  (размер взят из metadata — контракт респавна).
- `oversized_chunk_is_written_whole_then_rotated` — чанк больше лимита:
  записан целиком, ротация сразу после.
- `write_error_recovers_on_next_write` — путь в несуществующей директории →
  `Err` (чанк потерян); создать директорию → следующий `write` проходит
  (ленивое переоткрытие — drain-first политика).
- `concatenation_preserves_all_bytes` — серия записей через несколько
  ротаций: `.N…​.1 + текущий` = ровно поданные байты (ничего не потеряно и не
  задвоено).

### 8.3 Юниты `src/process.rs` (mod tests)

- `spawn_without_log_inherits_stdio` — у `Spawned` оба `*_capture` — `None`
  (opt-in контракт).
- `spawn_with_log_reaches_eof_after_child_exit` — **тест фд-гигиены**: конфиг
  c `stdout-path` (сам файл не используется — поток здесь не запускается),
  стаб `sh -c 'echo hi'`; читать из `stdout_capture` до `Ok(0)`, собрать
  байты → `"hi\n"`, затем `child.wait()`. EOF обязан прийти без каких-либо
  действий теста с write-концом — если `spawn()` утёк родительскую копию,
  `read` блокируется вечно и тест громко виснет (прокомментировать в тесте:
  зависание = регрессия фд-гигиены, см. §4).
- `spawn_with_only_stderr_captures_only_stderr` — `stdout_capture` None,
  `stderr_capture` Some; стаб пишет в stderr (`echo err 1>&2`), прочитано из
  pipe.

### 8.4 In-process тесты — новый файл `tests/logs.rs`

`FakeClock`, без сокетов и хендлеров; `tick()` руками. Хелперы
`cfg`/`sh`/`wait_for_pid`/`is_alive`/`get_pid`/`tick_until_reaped`/
`tick_until_new_pid` скопировать из `tests/health.rs` (осознанное дублирование
крейтов — пометить в преамбуле; там же — двухрежимный паттерн наблюдения из
§8). Новые хелперы: `with_log(cfg, stdout, stderr, max_size, keep)` (секция
через `toml::from_str`, как `with_exec_health`) и
`wait_for_log(path, pred, timeout)` — поллинг содержимого с реальным
дедлайном. Стабам маленькие лимиты (сотни байт) — ротация укладывается в
секунды.

- `stdout_is_captured_to_file` — стаб в цикле `echo`-ит маркер;
  `wait_for_log` до появления маркера (процесс жив → только поллинг); stderr
  файла не появилось.
- `stdout_and_stderr_go_to_separate_files` — стаб пишет разные маркеры в оба
  потока; каждый файл получил свой и не получил чужой.
- `rotation_happens_while_process_is_alive` — **ключевой тест этапа**:
  max-size ~256, keep 2, болтливый стаб; `wait_for_log`-поллингом дождаться
  существования `.1`, затем `.2`; ассерт: ротированных не больше keep;
  текущий файл существует. (Число `.N`-файлов считать по факту в директории.)
- `output_is_complete_after_reap` — синхронная гарантия join: стаб печатает
  маркер и выходит (`restart = "never"`); `tick_until_reaped` → **синхронный**
  ассерт (без поллинга): файл содержит маркер целиком.
- `grandchild_output_is_captured` — стаб-родитель форкает внука
  (`( echo from-grandchild ) &` — fork наследует write-конец), сам живёт;
  `wait_for_log` до строки внука. Доказывает «pipe — на всё дерево».
- `restarted_instance_appends_to_same_file` — маркер инстанса различим
  (например `echo start-$$`); `handle_command(Restart)` →
  `tick_until_reaped`/`tick_until_new_pid` → `wait_for_log`: в файле маркеры
  обоих инстансов (append, не truncate; новый читатель продолжил счёт
  размера).
- `stopped_process_file_is_complete_and_quiet` — оператор `stop`, реапинг →
  синхронный ассерт содержимого; далее файл не растёт (два чтения с зазором
  реального времени).
- `changed_log_config_forces_restart` — §6.4: `apply_config` с изменённым
  только `max-size-bytes` → процесс в снапшоте `stopping`, после
  `tick_until_new_pid` — `restart_count == 1` (адресация по имени, правило
  Этапа 8); вывод нового инстанса появляется в файле.
- `deaf_tree_is_flushed_after_sigkill_escalation` — DEAF-стаб
  (`trap '' TERM`, маркеры в stdout), grace 2: `stop` → advance(3) → тики до
  реапинга (SIGKILL-путь) → синхронный ассерт: всё, что стаб успел вывести,
  в файле. Доказывает сходимость EOF через killpg-sweep, а не через
  «вежливый» выход.

### 8.5 e2e на реальном бинарнике — новый файл `tests/logs_e2e.rs`

Хелперы `write_config`/`start_supervisor`/`wait_for_state`/
`wait_with_timeout`/`wait_until_gone` скопировать из `tests/health_e2e.rs`
(осознанное дублирование); изоляция `--state-file` и `--control-socket`
обязательна; лог-пути — в том же tempdir.

- `captured_output_rotates_end_to_end` — **критерий приёмки целиком**: конфиг
  с `[process.log]` (max-size 256, keep 2), болтливый стаб;
  `wait_for_state` до `running`; поллингом с дедлайном дождаться `.1` и `.2`;
  ассерт «ротированных ≤ keep»; SIGTERM → exit 0; лог-файлы **остались** на
  диске (решение §2.1 — не удаляются при выходе, в отличие от state-файла и
  сокета), конкатенация непуста; state-файл и сокет — удалены (существующий
  контракт не сломан).
- `process_without_log_section_behaves_as_before` — конфиг из двух процессов,
  секция только у одного: второй `running`, никаких лог-файлов для него не
  появилось; shutdown штатный. (Дёшево; ловит случайный захват всех подряд.)
- `invalid_log_section_fails_run_with_config_error` — секция без обоих путей:
  exit 1, лог содержит `stdout-path` (или текст ошибки), state-файл не
  появился — по образцу `invalid_health_check_fails_run_with_config_error`.

### 8.6 Существующие тесты

Все 216 обязаны остаться зелёными. Допустимые правки — только механические:
`log: None` в 39 литералах `ProcessConfig` (§3), `.child` у вызовов
`process::spawn` в `tests/spawn.rs` и юнит-тестах `src/process.rs` /
`src/supervise.rs` (§4), плюс аддитивные варианты в
`process_config_equality_notices_every_field` и
`plan_reload_notices_each_field` (§3). Поведенческих правок и правок
ожиданий — ноль; `tests/signals.rs`, `tests/tree.rs`, `tests/cli.rs`,
`tests/control.rs`, `tests/status.rs`, `tests/health_e2e.rs`,
`tests/reload_e2e.rs` не меняются вовсе. Проверить прогоном.

## 9. Порядок работ (шаг = один связный коммит)

1. **Конфиг** (`src/config.rs` + `log: None` во всех литералах по всем
   файлам): `LogConfig`, дефолты, `validate()`, проверки в `load()`
   (обязательность пути, различие путей, глобальная уникальность); юниты
   §8.1. Полный зелёный прогон — правка литералов затрагивает все крейты.
2. **Ядро ротации** (`src/logs.rs`, `lib.rs`): `RotatingFile` +
   `spawn_reader`; юниты §8.2.
3. **Захват в spawn** (`src/process.rs` + механические `.child` у вызовов):
   `Spawned`, `CapturePipe`, pipe-подключение, комментарий фд-гигиены; юниты
   §8.3. На этом шаге supervise ещё игнорирует capture-fd (дропает их из
   `Spawned` — безопасно, потому что log-секции никто из существующих тестов
   не задаёт); полный зелёный прогон.
4. **Интеграция** (`src/supervise.rs`): `LogState`, `start_instance`,
   `join_log_readers` + константы, вызовы в `tick()`/`handle_poll_error`;
   in-process тесты §8.4 (`tests/logs.rs`).
5. **e2e** (`tests/logs_e2e.rs`, §8.5). Прогнать 10 раз подряд
   (`for i in $(seq 10); do cargo test --test logs --test logs_e2e || break; done`).
6. **Доки** (§10).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 10. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`: новый раздел «Этап 9 — Ротация логов» по
  фактической реализации: решения пользователя из §2 (явно, как принятые);
  почему pipe + поток-читатель, а не rename-под-дескриптором (пишет в старый
  inode) и не copy-truncate (sparse-дыра от несброшенной позиции записи);
  **первый поток ОС в проекте** — где граница («расписание и супервизия —
  однопоточный цикл на `Clock`; захват вывода — по потоку на pipe, без часов
  вообще») и почему это не размывает `FakeClock`-детерминизм (ротация —
  функция размера, не времени); фд-гигиена write-конца; сходимость EOF через
  killpg-sweep Этапа 4; bounded join при реапинге (и почему не голый join);
  drain-first политика ошибок; схема state-файла не изменена (version 1);
  известные ограничения (список из §11). «Модульная структура» — добавить
  `logs.rs`.
- `docs/POST_MVP_PLAN.md`: раздел «Ротация логов» пометить реализованным в
  Этапе 9 по образцу пунктов про Этапы 6–8. **Обязательно снять фразу
  «Возможна интеграция с внешним `logrotate` через SIGHUP»** с явной
  причиной отклонения: SIGHUP занят перезагрузкой конфига с Этапа 8
  (`RELOAD_PENDING`), ротация — полностью внутренняя и бессигнальная.
  Оговорки-кандидаты, остающиеся здесь же: сжатие ротированных; ротация по
  времени (v1 — только по размеру); line-aware границы ротации; видимость
  лог-путей в `status`.
- `docs/PLAN.md`: добавить Этап 9 в список этапов; финальный абзац
  («Этап 6 закрыл…, Этап 7 —…, Этап 8 —…») дополнить Этапом 9.
- `README.md`: строка статуса этапов; абзац про захват логов с примером
  секции `[process.log]` и семантикой `.1`…`.keep`.
- `examples/supervisor.toml`: закомментированный пример секции с пояснением
  дефолтов (прецедент health-check).
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`, в «грабли» (по
  фактическим находкам; ожидаемые кандидаты): родитель обязан закрыть свою
  копию write-конца pipe сразу после `spawn()` — иначе читатель не увидит EOF
  никогда (у нас — владением: `Stdio::from(OwnedFd)` внутрь локального
  `Command`); EOF наступает лишь когда write-конец закрыли **все** держатели,
  включая внуков — killpg-sweep Этапа 4 и есть протокол завершения читателя;
  читатель дренирует pipe до EOF при любых ошибках файла (остановка чтения —
  блокировка ребёнка на полном буфере, дроп read-конца — SIGPIPE-смерть
  ребёнка); join потока при реапинге — только bounded через `is_finished()`
  (сбежавший из группы орфан держит pipe вечно); содержимое лог-файла живого
  процесса — только поллинг с дедлайном, после реапинга — синхронный ассерт;
  стабы для pipe-тестов пишут через `echo`/`printf` шелла (stdio-буферизация
  C-программ задерживает вывод в pipe).
- `grep -rn "TODO(Этап 9)" src/` пуст; финальную редакцию формулировок делает
  основная сессия — здесь достаточно фактической точности.

## 11. Границы — что НЕ трогать, и известные ограничения

Не трогать:

- **Зависимости: ноль правок `Cargo.toml`** — `nix::unistd::pipe()` (фича
  `process` уже включена), `std::thread`/`std::fs`/`std::io`. В частности,
  не добавлять фичу `fs` ради `pipe2(O_CLOEXEC)` (см. ограничение 5) и не
  поднимать `rust-version` ради `std::io::pipe` (1.87). Docker Compose не
  добавлять.
- Механика process groups / teardown Этапа 4: `poll_child` (порядок
  peek → sweep → reap), `begin_shutdown`, `escalate_to_kill`, backoff,
  `STABLE_RESET`, бюджет poll-ошибок — семантика без изменений; в
  `handle_poll_error` — единственная вставка `join_log_readers` после
  `running = None`.
- `tick()`: ровно две разрешённые правки — вызов `join_log_readers(proc)` в
  ветке `PollOutcome::Exited` (§6.3) и замена `process::spawn` →
  `start_instance` в респавн-ветке (§6.2). Ни одной новой ветки обхода.
- `src/signal.rs` — вообще без правок: ротация бессигнальна по решению
  пользователя, SIGHUP принадлежит reload'у Этапа 8.
- `handle_command` / `src/control.rs` — ноль правок: ротация — не команда
  сокета. `src/cli.rs`, `main.rs` — без правок (флагов нет).
- Схема state-файла (`version = 1`) — без новых полей; пути/состояние
  ротации в снапшот не попадают (прецедент health Этапа 7).
- API `Clock`/`FakeClock` не расширяется: у ротации нет времени вообще
  (размер — не время), у join — реальные ретраи класса `REAP_RETRIES`.
- Reload Этапа 8 (`plan_reload`/`apply_config`/`prune_removed`) — ноль
  правок: изменение log-секции проходит существующей веткой `changed` через
  derived `PartialEq` (§6.4).
- Известные ограничения Этапов 4–8 — принятое поведение, не «чинить».

Известные ограничения Этапа 9 (записать в TECHNICAL_PLAN как осознанные):

1. Ротация только по размеру; по времени — не в v1 (решение §2.1), кандидат
   POST_MVP.
2. Без сжатия ротированных файлов (решение пользователя), кандидат POST_MVP.
3. Границы ротации — границы чанков `read()` (≤ 8 KiB), не строк: строка
   может разъехаться между `<path>.1` и текущим файлом; текущий файл может
   превысить `max-size-bytes` на величину до одного чанка.
4. Потомок, сбежавший из process-группы (собственный `setsid`) и держащий
   унаследованный stdout, оттягивает EOF; bounded join при реапинге тогда
   детачит читателя (warn-лог), и до его естественного завершения ротация
   старого и нового читателей одного пути не синхронизирована — тот же класс,
   что «сбежавший орфан переживает teardown» из ограничений Этапа 4.
5. `pipe()` без `O_CLOEXEC`: долгоживущие read-концы наследуются позже
   спавнящимися детьми и exec-пробами как лишние fd. На EOF и корректность не
   влияет (однопоточный последовательный spawn, write-концы закрываются до
   следующего форка), `pipe2(O_CLOEXEC)` потребовал бы фичи `fs` у `nix` при
   замороженном `Cargo.toml` — косметика, принятая осознанно.
6. Ошибки записи лог-файла (полный диск, удалённая директория) деградируют в
   потерю вывода с warn-логом и ленивым переоткрытием; чтение pipe не
   останавливается никогда (drain-first, §5).
7. Захваченный вывод буферизуется самим приложением: не-tty stdout у
   большинства stdio-рантаймов буферизуется блоками, вывод появляется в
   файле с задержкой до сброса буфера — забота приложения
   (`stdbuf`/line-buffering), не супервизора.
8. Пути логов не видны в `status`/state-файле; изменение `[process.log]`
   через reload — это рестарт процесса (как любое изменение конфига Этапа 8):
   сменить путь лога «на лету» без рестарта нельзя.
9. Логи самого демона (`tracing`) не захватываются и не ротируются — этап
   касается только stdout/stderr супервизируемых процессов.
10. Блокировка цикла на bounded join — до ~1 с в патологическом случае
    (детач-путь); нормальный случай — миллисекунды. Четвёртое
    санкционированное исключение из «цикл не блокирует» (после
    `handle_poll_error`, таймаутов сокета и пробы health).

## 12. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` —
   зелёные; `cargo test --test logs --test logs_e2e` — зелёные 10 прогонов
   подряд.
2. Все 216 существующих тестов проходят; их единственные правки —
   механические из §8.6 (`log: None`, `.child`, аддитивные варианты двух
   тестов полноты полей).
3. `tests/logs.rs::rotation_happens_while_process_is_alive` и
   `tests/logs_e2e.rs::captured_output_rotates_end_to_end` доказывают
   критерий приёмки (живая ротация, ретеншн ≤ keep, файлы переживают
   shutdown); `output_is_complete_after_reap` и
   `deaf_tree_is_flushed_after_sigkill_escalation` — синхронную гарантию
   join-при-реапинге, включая SIGKILL-путь.
4. Фд-гигиена закрыта тестом
   `spawn_with_log_reaches_eof_after_child_exit` (§8.3); захват дерева —
   `grandchild_output_is_captured`; drain-first —
   `write_error_recovers_on_next_write`.
5. Целостность данных через ротации закрыта
   `concatenation_preserves_all_bytes` (юнит) и продолжение файла респавном —
   `restarted_instance_appends_to_same_file` (append + счёт размера из
   metadata).
6. Взаимодействие с reload закрыто `changed_log_config_forces_restart` —
   без единой правки reload-кода.
7. `Cargo.toml`, `src/signal.rs`, `src/cli.rs`, `src/control.rs`,
   `src/health.rs`, `src/state.rs`, `src/clock.rs`, `src/main.rs` не
   изменены; схема state-файла не изменена (проверить `git diff --stat`).
8. Доки из §10 актуализированы, включая снятие упоминания «logrotate через
   SIGHUP» в POST_MVP_PLAN.md с причиной отклонения и решения пользователя в
   TECHNICAL_PLAN.md.
