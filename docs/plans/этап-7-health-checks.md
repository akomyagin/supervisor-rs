# План Этапа 7 — Health-checks (exec / tcp / http пробы, рестарт «залипшего» процесса)

Ветка: `этап-7/health-checks`. Исполнителю: никаких git-коммитов — commit/push/PR
делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/commands.rs` / `tests/shutdown.rs` /
`tests/status.rs`.

## 1. Цель и критерий приёмки

В MVP «жив» = «процесс существует» (peek по pid). Этап 7 добавляет активные
проверки здоровья: для процесса можно сконфигурировать одну пробу —
**exec** (запустить команду, успех по коду выхода 0), **tcp** (порт принимает
соединение) или **http** (эндпоинт отвечает 2xx). Проба выполняется раз в
`interval-secs`; `failure-threshold` неуспехов **подряд** приводят к
принудительному рестарту процесса, даже если он жив, но «залип» — через ту же
машину TERM → grace → SIGKILL, что и операторский `restart` Этапа 6, и с тем
же учётом `restart_count`.

Критерий приёмки: на живом демоне процесс с пробой и зелёным здоровьем
работает нетронутым (`restart-count` не растёт); когда проба начинает
проваливаться, ровно после `failure-threshold` неуспехов подряд — не раньше —
процесс перезапускается (в `status` виден новый PID и выросший
`restart-count`), причём процесс, игнорирующий SIGTERM, добивается SIGKILL по
`stop-grace-secs`. Успешная проба между неуспехами сбрасывает счётчик. После
рестарта расписание проб начинается заново, с уважением `start-period-secs`.
Операторский `stop` останавливает и пробы; shutdown демона пробами не
задерживается и не нарушается. Некорректная секция `health-check` в конфиге —
ошибка загрузки конфига (exit 1), не паника в рантайме.

## 2. Архитектурные решения — приняты пользователем, НЕ пересматривать

1. **Пробы выполняются ограниченно-блокирующе, ровно одна проба за тик.**
   Тот же паттерн, что control-socket Этапа 6 (`try_accept` + bounded
   `set_read_timeout` на принятом стриме) и `handle_poll_error` Этапа 4
   (реальный retry-sleep в цикле): НЕ полностью неблокирующая машина состояний,
   растянутая на несколько тиков. Один тик может блокироваться на время
   таймаута одной пробы (`timeout-secs`, секунды) — осознанная цена, третье
   санкционированное исключение из «цикл не блокирует». Если срок настал сразу
   у нескольких проб, за тик выполняется **одна** (первая по порядку
   `procs`), остальные — по одной на следующих тиках: цикл никогда не
   блокируется на сумму нескольких таймаутов. Таймаут пробы — реальное время
   ОС (`SO_RCVTIMEO` / `connect_timeout` / дедлайн по `std::time::Instant`),
   **API `Clock`/`FakeClock` не расширяется**; по инъектируемым часам идёт
   только *расписание* (когда проба должна случиться) — как
   `maybe_write_state`.
2. **HTTP-проба — самописный минимальный HTTP/1.1 GET поверх TCP, без
   внешнего HTTP-крейта.** Запрос собирается вручную
   (`GET <path> HTTP/1.1\r\nHost: <host>:<port>\r\nConnection: close\r\n\r\n`),
   читается **только статус-строка**, успех — код 2xx. Без TLS, редиректов,
   chunked, keep-alive. **Новых зависимостей `Cargo.toml` этап не добавляет
   вовсе**: TCP — `std::net::TcpStream`, exec — уже используемый
   `std::process::Command`. Обоснование консистентно с Этапами 5–6 (TOML вместо
   `serde_json`, line-протокол вместо фреймворка): проект — практика системного
   программирования, а не web-разработка; словарь HTTP-пробы — одна строка
   запроса и три цифры кода ответа, крейт ради этого не окупается.

Оба решения зафиксировать в коде комментариями и в `TECHNICAL_PLAN.md` как
принятые пользователем.

### Решение по скоупу: одна liveness-проба, без триады startup/readiness/liveness

`POST_MVP_PLAN.md` упоминал «startup/liveness/readiness различия по мотивам
systemd/k8s». В v1 — **только один вид проверки на процесс**, семантика «жив
ли» (liveness): порог неуспехов → рестарт. Обоснование:

- **readiness** в k8s управляет маршрутизацией трафика — у supervisor-rs нет
  ни балансировщика, ни зависимостей между процессами (те — отдельный пункт
  POST_MVP), состоянию «готов, но не жив» некуда деться;
- **startup-проба** существует, чтобы не убить медленно стартующее приложение
  liveness-пробой; её работу здесь выполняет `start-period-secs` (§3) — одно
  поле вместо второй пробы со своим расписанием;
- **liveness** — единственный вид, чьё действие (рестарт) вообще есть в
  арсенале супервизора, и ровно он назван в формулировке задачи («рестарт
  живого, но залипшего»).

Записать в «Не в скоупе v1» и отразить в POST_MVP_PLAN при актуализации доков.

## 3. Конфиг: секция `[process.health-check]` (`src/config.rs`)

### Форма TOML

Одна опциональная таблица на процесс — `[process.health-check]`, не массив
`[[...]]`: одна проба на процесс в v1 (несколько проб — не в скоупе). В TOML
подтаблица привязывается к последнему элементу `[[process]]`:

```toml
[[process]]
name = "web"
command = ["/usr/bin/myserver", "--port", "8080"]
restart = "always"

[process.health-check]
type = "http"                # exec | tcp | http — обязательное
port = 8080                  # tcp/http: обязательное
path = "/health"             # http: default "/"
# host = "127.0.0.1"         # tcp/http: default 127.0.0.1, только IP-адрес
interval-secs = 10           # default 10
timeout-secs = 5             # default 5
failure-threshold = 3        # default 3
start-period-secs = 15       # default 0
```

Для exec-пробы: `type = "exec"`, `command = ["/usr/bin/curl", ...]` (тот же
формат argv-массива, что у команды процесса).

### Структуры (предписывающе)

Разбор — «сырая» структура с serde, затем **ручная валидация** в типизированную
пробу. Не `#[serde(flatten)]` + internally-tagged enum: связка flatten+tag через
буферизацию `Content` — известная шершавая кромка serde/toml, а ручная
валидация даёт точные сообщения об ошибках и строгость к неприменимым полям.
Это консистентно со стилем проекта (ручной разбор argv вместо `clap`); при этом
бесплатную валидацию **типами** берём по прецеденту `u64` у
`stop-grace-secs`: `NonZeroU*` отсекает нули, `IpAddr` отсекает hostname.

```rust
// src/config.rs
use std::net::IpAddr;
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};

pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 10;
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_HEALTH_FAILURE_THRESHOLD: u32 = 3;
// start-period default 0 — отдельная константа не нужна, #[serde(default)].

/// Raw, as-parsed health check section. Cross-field validation (which fields
/// the chosen `type` requires and which it must not carry) happens in
/// [`HealthCheckConfig::probe`], called by `load()` — a bad section is a config
/// error at load time, never a runtime panic.
#[derive(Debug, Deserialize)]
pub struct HealthCheckConfig {
    #[serde(rename = "type")]
    pub kind: ProbeKind,
    /// exec only.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// tcp/http; an IP address by design — see "no DNS" note below.
    #[serde(default)]
    pub host: Option<IpAddr>,
    /// tcp/http, required there.
    #[serde(default)]
    pub port: Option<NonZeroU16>,
    /// http only; default "/".
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "interval-secs", default = "default_health_interval")]
    pub interval_secs: NonZeroU64,
    #[serde(rename = "timeout-secs", default = "default_health_timeout")]
    pub timeout_secs: NonZeroU64,
    #[serde(rename = "failure-threshold", default = "default_health_threshold")]
    pub failure_threshold: NonZeroU32,
    /// Delay before the probe schedule starts counting, on top of one interval
    /// (see §5 for the exact first-probe formula). Zero is meaningful.
    #[serde(rename = "start-period-secs", default)]
    pub start_period_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeKind {
    Exec,
    Tcp,
    Http,
}

/// A validated, ready-to-run probe. Built from the raw section exactly once
/// per construction site; carrying `SocketAddr` (not host+port) means the
/// runner never re-parses anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthProbe {
    Exec { command: Vec<String> },
    Tcp { addr: std::net::SocketAddr },
    Http { addr: std::net::SocketAddr, path: String },
}

impl HealthCheckConfig {
    /// Validates the section into a typed probe. Pure; unit-tested directly.
    pub fn probe(&self) -> Result<HealthProbe, String>;
    pub fn interval(&self) -> Duration;      // from interval_secs
    pub fn timeout(&self) -> Duration;       // from timeout_secs
    pub fn start_period(&self) -> Duration;  // from start_period_secs
}
```

Правила `probe()` (все нарушения — `Err(String)` с именем поля):

- `exec`: требует непустой `command`; `host`/`port`/`path` **запрещены**
  (`health-check type "exec" does not take "port"` — строгость вместо тихого
  игнора: та же логика, что тест `rejects_snake_case_stop_grace`, где молча
  проигнорированное поле — footgun).
- `tcp`: требует `port`; `host` опционален (default `127.0.0.1`);
  `command`/`path` запрещены.
- `http`: требует `port`; `host` опционален (default `127.0.0.1`); `path`
  опционален (default `"/"`), обязан начинаться с `/`; `command` запрещён.

**Решение «DNS в v1 нет»: `host` — только IP-адрес, тип `IpAddr».** У `std`
нет DNS-резолвинга с таймаутом: `ToSocketAddrs` может заблокировать поток на
время, не ограниченное `timeout-secs`, и пробила бы принятый бюджет «один тик
блокируется не дольше таймаута одной пробы». Тип поля `IpAddr` делает
валидацию бесплатной на этапе парсинга (строка `"localhost"` — ошибка парсинга
конфига с внятным serde-сообщением). Супервизируемый процесс всегда локален,
так что `127.0.0.1` покрывает основной случай; hostname — кандидат в POST_MVP
(потребует ручного бюджетирования резолвинга). Записать в известные
ограничения.

### `ProcessConfig` и `ConfigError`

- Новое поле, последним в структуре:
  `#[serde(rename = "health-check", default)] pub health_check:
  Option<HealthCheckConfig>`. `ProcessConfig` — только `Deserialize` (не
  `Serialize`), так что грабля «скаляры до подтаблиц» на порядок полей не
  влияет, но подтаблицу всё равно держим последней — единый стиль со
  `StateSnapshot`.
- **Все места, где `ProcessConfig` конструируется литералом, получают
  `health_check: None`.** Найти все: `grep -rn "ProcessConfig {" src tests`.
  Ожидаемо: `src/process.rs` (хелпер `cfg` в тестах), `src/supervise.rs`
  (тест give-up ветки), `tests/commands.rs`, `tests/shutdown.rs`, возможно
  `tests/state.rs` — у каждого интеграционного крейта свой хелпер `cfg()`
  (осознанное дублирование, см. их преамбулы). Это единственная правка
  существующих тестов, чисто механическая.
- `ConfigError` получает третий вариант:

```rust
Invalid {
    path: PathBuf,
    /// Which process and what is wrong, e.g.
    /// `process "web": health-check type "tcp" requires "port"`.
    message: String,
}
```

  `load()` после `toml::from_str` проходит по процессам и для каждого
  `health_check` зовёт `probe()`; `Err(msg)` → `ConfigError::Invalid` с
  префиксом `process "<name>": `. Display/Error — в стиле существующих
  вариантов. Ошибка конфига у `run` уже даёт exit 1 — новых exit-кодов нет.

## 4. Исполнение пробы — новый модуль `src/health.rs`

Раннер отделён от расписания: `health.rs` ничего не знает ни о
`SupervisorLoop`, ни о `Clock` — чистая функция «выполни одну пробу за
ограниченное реальное время», юнит-тестируемая на реальных сокетах и командах
(по прецеденту `control.rs`). `pub mod health;` в `lib.rs`.

```rust
// src/health.rs
use crate::config::HealthProbe;
use std::time::Duration;

/// Longest HTTP status line the probe will read before giving up. A server
/// that produces no '\n' within this many bytes is not speaking HTTP/1.x.
const MAX_STATUS_LINE_BYTES: usize = 256;
/// How often the exec probe re-polls its child while waiting for the exit.
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Debug)]
pub enum ProbeError {
    /// exec: the command could not be spawned at all.
    Spawn(std::io::Error),
    /// exec: non-zero exit (or killed by a signal); carries the ExitStatus text.
    Failed(String),
    /// The probe did not finish within `timeout-secs`.
    Timeout { after: Duration },
    /// tcp/http: connect refused / failed.
    Connect(std::io::Error),
    /// http: IO after connect (reset, EOF before status line, read timeout).
    Io(std::io::Error),
    /// http: well-formed status line with a non-2xx code.
    HttpStatus(u16),
    /// http: the answer does not parse as an HTTP/1.x status line.
    Malformed(String),
}
// + Display (человекочитаемо, попадает в warn-лог) + std::error::Error.

/// Runs one probe to completion, bounded by `timeout` of *real* time (an OS
/// deadline, deliberately not routed through Clock — see the Этап 7 plan §2).
pub fn run_probe(probe: &HealthProbe, timeout: Duration) -> Result<(), ProbeError>;
```

Внутри — по одной приватной функции на вид пробы. Общий приём: в начале
`let deadline = std::time::Instant::now() + timeout;`, каждый следующий
блокирующий шаг получает `deadline - now` (остаток бюджета); остаток исчерпан →
`ProbeError::Timeout`. Бюджет один на всю пробу, а не на каждый шаг.

### exec

`std::process::Command::new(&command[0]).args(&command[1..])`, все три
stdio — `Stdio::null()` (наследование замусорило бы лог демона; вывод пробы не
интерпретируется, только код выхода). `spawn()` → цикл: `try_wait()` каждые
`EXEC_POLL_INTERVAL` реального `std::thread::sleep` до `deadline`. Статус
success → `Ok(())`; иначе `Failed`. Дедлайн истёк → `child.kill()` (SIGKILL) +
`child.wait()` (обязательный реап — иначе зомби на всё время жизни демона) →
`Timeout`. Пробе **не** ставится своя process-группа (`setsid`): проба — это
короткая проверка, а не супервизируемое дерево; потомки зависшей и убитой
пробы осиротеют на init — записать в известные ограничения («команда пробы
должна быть простой и не форкать»).

### tcp

`TcpStream::connect_timeout(&addr, timeout)`: `Ok` → соединение сразу
закрывается (drop) → `Ok(())`; `Err` → `Connect` (истёкший таймаут ОС отдаёт
как `TimedOut`-ошибку — замапить в `Timeout` для честного лога).

### http

1. `TcpStream::connect_timeout(&addr, остаток)` → `Connect`/`Timeout`.
2. `set_write_timeout(остаток)`, `set_read_timeout(остаток)` (минимум 1 мс,
   чтобы `Some(0)` не превратился в паникующий/бесконечный ноль — у `std`
   `set_read_timeout(Some(ZERO))` — ошибка).
3. Один `write_all`:
   `GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\n\r\n`.
4. Чтение статус-строки — побайтово до `\n`, не более
   `MAX_STATUS_LINE_BYTES` (тот же bounded-паттерн, что `read_request` в
   `control.rs`); EOF до перевода строки → `Io`; превышение лимита →
   `Malformed`.
5. Разбор: `"HTTP/1."`-префикс первого токена, второй токен — `u16` (это
   выдерживает и `HTTP/1.0`, и отсутствие reason-phrase); не парсится →
   `Malformed(строка)`. Код `200..=299` → `Ok(())`, иначе `HttpStatus(code)`.
   3xx — неуспех (редиректов нет by design). Остаток тела не читается:
   `Connection: close` попросили, но ждать закрытия незачем — сокет просто
   дропается.

## 5. Интеграция в `SupervisorLoop` (`src/supervise.rs`)

### Состояние на процессе

```rust
/// Probe schedule for the *current* instance of the process. One struct, not
/// two Options obliged to agree — the same lesson as merging child+pgid into
/// `Running` in Этап 5.
struct HealthSchedule {
    /// `restart_count` value this schedule was armed for. A mismatch means a
    /// new instance is up: re-arm afresh (fresh start-period, zeroed failures).
    /// The counter is the generation marker on purpose: it increments on every
    /// respawn — policy, operator or health-triggered alike.
    armed_for: u32,
    /// Next probe is due at this instant, on the injected clock.
    next_check_at: Instant,
}

struct HealthState {
    /// Validated once at construction; the runner never parses config.
    probe: HealthProbe,
    /// `None` until first armed for an instance.
    schedule: Option<HealthSchedule>,
    consecutive_failures: u32,
}
```

Поле `health: Option<HealthState>` на `Supervised` (`Some` ⇔ в конфиге есть
`health-check`). Заполняется в `SupervisorLoop::new()`:
`config.health_check.as_ref().map(...)` c `probe()`; `Err` здесь недостижим
после валидации в `load()`, но in-process тесты конструируют `ProcessConfig`
руками — поэтому `Err` не паникует, а логируется `error!` и даёт
`health: None` (defensive, в духе best-effort старта Этапа 2). Интервалы и
порог **не** копируются в `HealthState` — читаются из `proc.config` (единственный
источник истины).

### Расписание: семантика таймера

- **Первая проба текущего инстанса** назначается на
  `started_at + start_period + interval` (всё — инъектируемые часы;
  `started_at` уже ведётся по `clock.now()`). При `start-period-secs = 0`
  (дефолт) первая проба — через один интервал после старта: пробы в момент
  `t=0`, когда процесс ещё не открыл порт, не бывает никогда, а «не считать
  нормальный старт неуспехом» при медленном старте решается явным
  `start-period-secs`. Отвергнутая альтернатива — «пробы идут сразу, но
  неуспехи внутри start-period не считаются»: это второй режим счётчика и
  ненаблюдаемое «нездоров, но прощён», одна формула проще и тестируемее.
- **Последующие пробы**: `next_check_at = clock.now() + interval`,
  назначается **после завершения** пробы (интервал — между концом одной и
  началом следующей; на `SystemClock` длительность пробы естественно
  раздвигает расписание, на `FakeClock` временем управляет тест).
- **Перевзвод (re-arm)**: при каждом вызове, если
  `schedule.armed_for != proc.restart_count`, расписание строится заново от
  текущего `started_at` и `consecutive_failures = 0`. Так рестарт любого
  происхождения (policy, оператор, health) автоматически даёт новому инстансу
  чистый счётчик и полный start-period — **без единой правки веток `tick()`
  или `handle_command`**: генерация читается, а не проталкивается.
- **Пауза у остановленного**: `running == None` (операторский stop, backoff,
  done) или `stop != StopPhase::Idle` (гашение в полёте) → процесс
  пропускается, проба не выполняется, расписание не двигается. После
  `start <name>` респавн инкрементирует `restart_count` → перевзвод. Это и
  есть ответ «таймер паузится у остановленного оператором».

### `run_due_health_check()` — новая функция, вызывается из `run()`

```rust
/// Runs at most ONE due health probe per call (the user's decision: a tick
/// may block for up to one probe timeout, never for a sum of them). Called
/// from `run()` after `poll_control()`, never from `tick()` — the same
/// discipline as `maybe_write_state`. `pub` so in-process tests drive it
/// directly with a FakeClock.
pub fn run_due_health_check(&mut self) {
    if self.shutting_down {
        return; // shutdown owns every StopPhase; probes must not interfere
    }
    for proc in &mut self.procs { ... }
}
```

Логика по каждому `proc` (первый подошедший — исполняется, затем `return`):

1. `let Some(health) = proc.health.as_mut() else { continue };`
2. `proc.running.is_none() || proc.stop != StopPhase::Idle` → `continue`
   (пауза, см. выше). Проверка `intent` не нужна: `Stopped`/`RestartPending`
   при живом процессе всегда сопровождаются `stop != Idle`, а после реапинга —
   `running == None`.
3. Перевзвод при несовпадении генерации (см. выше) — и **`continue`
   не делать**: свежевзведённое расписание сразу проверяется на «настало ли»
   (при нулевых start-period+interval оно настать не может — `NonZeroU64`).
4. `clock.now() < next_check_at` → `continue`.
5. Настало: `let result = health::run_probe(&health.probe,
   proc.config.health_check…timeout());` — единственное блокирующее место,
   ограничено `timeout-secs`.
6. `next_check_at = self.clock.now() + interval` (часы читаются **после**
   пробы — на `SystemClock` проба заняла реальное время).
7. `Ok`: если `consecutive_failures > 0` — `info!` «healthy again after N
   failures», сброс в 0; иначе `debug!` «probe ok» (не `info!`: строка раз в
   интервал на процесс замусорила бы лог).
8. `Err(e)`: `consecutive_failures += 1`; `warn!` с `name`, ошибкой (Display),
   `failures = X`, `threshold = Y` — по прецеденту логов `handle_poll_error`.
   Если `consecutive_failures >= failure_threshold`:
   `warn!(... "unhealthy after N consecutive probe failures; forcing restart")`,
   затем — ровно то, что делает `handle_command` на RUNNING × restart:
   `proc.intent = UserIntent::RestartPending;
   signal_terminate(proc, self.clock.now());` (часы — свежие, не значение до
   пробы: на `SystemClock` устаревший `now` срезал бы grace на длительность
   пробы). Счётчик не трогать — перевзвод по новой генерации обнулит его сам.
9. `return;` — одна проба за вызов. Голодания нет: исполненная проба
   отодвигает свой `next_check_at` на интервал, следующий тик достаётся
   следующей подошедшей.

### Переход «залип → рестарт»: переиспользование, а не параллельный путь

После пункта 8 работает **исключительно существующая машина**, без единой
новой ветки:

- `StopPhase::Terminating{deadline}` взведён `signal_terminate` (та же
  функция, что у операторских `stop`/`restart`); TERM ушёл группе;
- залипший процесс, игнорирующий TERM, добивается SIGKILL существующей веткой
  эскалации в `tick()` (grace истёк → `killpg(SIGKILL)` → `Killing`);
- выход лидера → существующий порядок peek → killpg-sweep → reap в
  `poll_child` (не трогается вовсе);
- ветка `PollOutcome::Exited` видит `intent == RestartPending` → снимает
  интент, ставит `next_restart_at = now` → существующая ветка респавна
  спавнит новый инстанс, инкрементирует `restart_count`, ставит
  `started_at`/`pgid` и наследует обработку ошибок spawn с backoff.

**Решение по интенту: переиспользовать `UserIntent::RestartPending`, третий
вариант (`UnhealthyRestart`) не вводить.** Обоснование: новый вариант
продублировал бы арм `RestartPending` в `tick()` и потребовал бы решений по
всей строке STOPPING таблицы `handle_command` (что отвечает оператору, чей
`restart` совпал с health-рестартом, и т.п.) — ради единственного
наблюдаемого отличия: подписи одной строчки лога. Причина рестарта и так
попадает в лог в точке триггера (warn «unhealthy … forcing restart» — п. 8).
Следствия, осознанные:

- операторский `restart` во время health-рестарта в полёте получает
  `ok: restart already in progress` — корректно по сути;
- операторский `stop` во время health-рестарта перекрывает интент на
  `Stopped` (существующая строка STOPPING × stop) — оператор побеждает
  автоматику, процесс остаётся остановленным и не пробится (пауза, §5);
- в `status` health-рестарт неотличим от операторского (`stopping` →
  `restarting` → `running`, `restart-count` растёт) — тот же класс
  ограничения, что «`stopped` не различает оператора и терминальный выход»
  из Этапа 6.

**Единственная правка существующего кода `tick()` — текст одного лога.** В
арме `UserIntent::RestartPending` сообщение
`"operator restart: respawn scheduled"` станет неверной атрибуцией (интент
теперь ставит и health-механизм). Заменить на нейтральное
`"forced restart: respawn scheduled"` — правка строки лога, ни одного
поведенческого изменения; заодно обновить doc-комментарии `UserIntent`
(у `RestartPending` теперь два взводящих: `restart <name>` и health-порог) и
комментарий у `ProcClass::Stopping`. Больше `tick()` не трогается.

Backoff при health-рестарте: как у операторского `restart` (прецедент
Этапа 6) — респавн немедленный, ступень backoff не сбрасывается и не
применяется. Анти-молотилки это не ломает: перманентно нездоровый процесс
рестартится не чаще, чем раз в `start_period + threshold × interval`
(минимум ~30 с на дефолтах) — каденция проб и есть троттлинг. Записать в
известные ограничения.

### Правка `run()` — одна строка

```rust
self.tick();
self.maybe_write_state();
self.poll_control();
self.run_due_health_check();   // новое; после poll_control — команда,
                               // принятая в этом тике (stop), уже взвела
                               // StopPhase и корректно подавила пробу
self.clock.sleep(TICK);
```

`main.rs` не меняется вовсе: у health-checks нет ни флагов, ни подкоманд, ни
влияния на exit-коды.

## 6. Файл состояния, CLI, наблюдаемость — решения

- **Схема state-файла НЕ меняется, `version` остаётся 1.** Ни
  `consecutive_failures`, ни «последний результат пробы» в снапшот не
  попадают. Обоснование: health-рестарт полностью наблюдаем существующими
  полями (`state` проходит `stopping → restarting → running`, `restart-count`
  растёт, `pid` меняется, `uptime` обнуляется), а поле «здоровье» осмысленно
  только вместе с различением причин рестарта — которое Этап 6 уже осознанно
  отложил (его ограничение 2). Расширять схему ради поля, которое `status`
  печатал бы без контекста, рано; «health в status» — кандидат POST_MVP,
  записать в известные ограничения этапа.
- **CLI не расширяется**: ни новой подкоманды, ни флагов; `cli.rs`, `main.rs`,
  таблица exit-кодов — ноль правок (единственный новый путь к exit 1 —
  `ConfigError::Invalid`, но это существующий класс «ошибка конфига»).
  Прецедент минимализма Этапов 5–6.
- **Логи — единственный прямой канал наблюдения проб**: `warn!` на каждый
  неуспех (с счётчиком и порогом), `warn!` на триггер рестарта, `info!` на
  выздоровление, `debug!` на рутинный успех.

## 7. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность по содержимому файла, поллинг с дедлайном вместо sleep,
`wait_with_timeout` для бинарника, новые тесты на процессы/сигналы — 10
прогонов подряд. Каждый e2e-запуск изолирует `--state-file` **и**
`--control-socket` в своём tempdir. TCP-заглушки в тестах — реальный
`std::net::TcpListener`, привязанный к порту 0 (ОС выдаёт свободный порт,
`listener.local_addr()` сообщает его) — не хардкодить порты.

### 7.1 Юниты `src/config.rs` (mod tests)

- `parses_exec_health_check` — `type = "exec"`, `command = [...]`;
  `probe()` → `HealthProbe::Exec`.
- `parses_tcp_health_check_with_defaults` — только `type`+`port`; host
  `127.0.0.1`, interval 10, timeout 5, threshold 3, start-period 0;
  `probe()` → `Tcp { addr: 127.0.0.1:<port> }`.
- `parses_http_health_check_full` — все поля явно, host `"::1"` (IPv6 тоже
  `IpAddr`), `path = "/health"`.
- `http_path_defaults_to_slash`.
- `rejects_tcp_health_check_without_port` — `probe()` → `Err`, содержит
  `"port"`.
- `rejects_exec_health_check_with_empty_command` — и отсутствующий
  `command`, и `command = []`.
- `rejects_inapplicable_probe_field` — `port` у exec и `command` у tcp →
  `Err` с именем поля (строгость — контракт).
- `rejects_http_path_without_leading_slash`.
- `rejects_hostname_in_health_check_host` — `host = "localhost"` →
  ошибка **парсинга** (тип `IpAddr`), `toml::from_str::<Config>` err.
- `rejects_zero_port_interval_timeout_and_threshold` — четыре нуля, каждый —
  ошибка парсинга (`NonZero*`).
- `rejects_unknown_health_check_type` — `type = "grpc"`.
- `process_without_health_check_parses` — поле `None`, существующие конфиги
  не затронуты.
- `load_reports_invalid_health_check_with_process_name` — через `load()` на
  временном файле: `ConfigError::Invalid`, Display содержит `process "web"`
  и имя поля.

### 7.2 Юниты `src/health.rs` (mod tests)

exec (реальные команды):

- `exec_probe_succeeds_on_zero_exit` — `/usr/bin/env true`.
- `exec_probe_fails_on_nonzero_exit` — `/usr/bin/env false` →
  `ProbeError::Failed`.
- `exec_probe_reports_unspawnable_command` — несуществующий путь → `Spawn`.
- `exec_probe_times_out_and_kills_the_command` — `sh -c 'sleep 30'` с
  таймаутом 1 с: `Timeout`, elapsed < 3 с (assert по дедлайну, не по точному
  времени), и pid команды мёртв/зомби не позднее дедлайна поллинга
  (`kill(pid,0)`-хелпер; проба — прямой ребёнок и реапится `wait()`-ом, так
  что `ESRCH` детерминирован).

tcp (реальный `TcpListener` на порту 0):

- `tcp_probe_succeeds_on_listening_port`.
- `tcp_probe_fails_on_closed_port` — забиндить, узнать порт, дропнуть
  listener, пробить → `Connect` (окно переиспользования порта ничтожно и
  даёт красное, не зелёное).

http (поток-заглушка: `TcpListener` + `accept` + рукописный ответ):

- `http_probe_accepts_2xx` — сервер отвечает
  `HTTP/1.1 200 OK\r\n\r\n`; заодно assert на принятый запрос: первая строка
  `GET /health HTTP/1.1`, есть `Host:` и `Connection: close`.
- `http_probe_accepts_204` — граница класса 2xx (и `HTTP/1.0` в ответе —
  парсер терпит минорную версию).
- `http_probe_rejects_500` — → `HttpStatus(500)`.
- `http_probe_rejects_garbage_status_line` — `not http at all\r\n` →
  `Malformed`.
- `http_probe_fails_on_immediate_disconnect` — accept и сразу закрыть → `Io`.
- `http_probe_times_out_on_silent_server` — accept, ничего не писать;
  `Timeout`/`Io` не позже ~таймаута (assert по дедлайну ≤ 3 с).

### 7.3 In-process тесты — новый файл `tests/health.rs`

`FakeClock`, без сокетов и хендлеров: `run_due_health_check()` +
`tick()` руками. Хелперы `cfg()`/`sh()`/`DEAF_SCRIPT`/`wait_for_pid`/
`tick_until_reaped`/`tick_until_new_pid` скопировать из `tests/commands.rs`
(осознанное дублирование крейтов, пометить в преамбуле), `cfg()` дополнить
параметром/сеттером health-check. Исполнение проб — реальное (быстрые exec-пробы
на `sh -c`), расписание — логическое. Счётчик исполнений пробы — exec-проба
`sh -c 'echo x >> <tempdir>/probes; exit <code>'`: число строк файла = число
проб; управляемый успех — `sh -c 'test -e <tempdir>/healthy'` (создание/удаление
флага переключает исход).

- `no_probe_before_first_deadline` — start-period 60, interval 10: advance
  на 69 c + вызовы → файл проб пуст; advance до 71 — ровно одна строка.
  Обе стороны дедлайна, по прецеденту тестов `STATE_WRITE_INTERVAL`.
- `healthy_process_is_left_alone` — зелёная проба, десяток интервалов:
  pid стабилен, `restart_count == 0`, снапшот `running`.
- `probe_respects_interval_between_runs` — после первой пробы advance на
  interval−1 → вторая не исполняется; +2 c — исполняется.
- `failures_below_threshold_do_not_restart` — threshold 3, две красных
  пробы: pid прежний, снапшот `running` (не `stopping`).
- `reaching_threshold_forces_restart` — **ключевой тест этапа**: третья
  красная проба → снапшот `stopping` (StopPhase взведён); `tick_until_reaped`
  → `tick_until_new_pid` → новый pid, `restart_count == 1`,
  `is_done == false`.
- `success_resets_consecutive_failures` — 2 красных → флаг создать → зелёная
  → флаг удалить → ещё 2 красных: рестарта нет; третья красная подряд —
  рестарт. Доказывает «подряд», а не «всего».
- `restarted_instance_gets_fresh_schedule` — после health-рестарта advance
  меньше `start_period + interval` нового инстанса → проб нет; после — идут,
  счётчик неуспехов начинается с нуля (генерация по `restart_count`).
- `stuck_process_is_killed_via_grace_escalation` — DEAF-стаб
  (`trap '' TERM`), `stop-grace-secs = 2`, красная проба до порога: после
  триггера advance(3 c) + tick → процесс мёртв (SIGKILL-путь), затем
  респавн. Доказывает переиспользование машины `StopPhase` целиком.
- `at_most_one_probe_per_call` — два процесса, обе пробы просрочены: один
  вызов `run_due_health_check` → суммарно ровно одна новая строка в
  файлах-счётчиках; следующий вызов — вторая.
- `user_stopped_process_is_not_probed` — `handle_command(Stop)` →
  `tick_until_reaped` → advance на много интервалов + вызовы → счётчик проб
  не растёт; `handle_command(Start)` → tick до респавна → пробы
  возобновляются по свежему расписанию.
- `no_probes_while_stop_is_in_flight` — DEAF-стаб, `Stop` (процесс ещё жив,
  `stop != Idle`), проба просрочена → не исполняется.
- `no_probes_during_shutdown` — `begin_shutdown(SIGTERM)`, проба просрочена
  → не исполняется; `is_done` доводится тиками штатно (пробы не мешают
  выходу).
- `operator_stop_overrides_health_restart_in_flight` — health-порог
  сработал (STOPPING, intent RestartPending), затем `handle_command(Stop)` →
  после реапинга респавна нет, снапшот `stopped`.
- `process_without_health_check_is_never_probed` — соседний процесс без
  секции: файл-счётчик отсутствует после многих интервалов (дёшево, ловит
  перепутанные индексы).

### 7.4 e2e на реальном бинарнике — новый файл `tests/health_e2e.rs`

Хелперы `write_config`/`start_supervisor`/`wait_for_state`/`wait_with_timeout`
скопировать из `tests/control.rs`/`tests/status.rs` (осознанное дублирование);
изоляция `--state-file` и `--control-socket` в tempdir обязательна. Интервалы
здесь реальные: `interval-secs = 1`, `failure-threshold = 2`,
`start-period-secs = 0` — сценарий укладывается в ~5 с; дедлайны ожиданий
держать 15–20 с (загруженный раннер).

- `unhealthy_process_is_restarted_after_threshold` — **критерий приёмки
  целиком**: стаб `sleep`-цикл, exec-проба `test -e <flag>`; флаг создан →
  дождаться `running` + pid1, выждать ≥ 3 интервалов поллингом снапшота и
  убедиться `restart-count == 0` (здорового не трогают); удалить флаг →
  `wait_for_state` до `running` с `restart-count >= 1` и pid2 ≠ pid1
  (рестарт случился); вернуть флаг → рестарты прекращаются (счётчик
  стабилизируется на два последовательных чтения с зазором ≥ 2 интервалов).
  SIGTERM демону → exit 0, стабы мертвы (`wait_until_gone`-поллинг).
- `daemon_shuts_down_cleanly_while_probing` — процесс с частой пробой
  (interval 1 c); SIGTERM в разгар проб → `wait_with_timeout` → exit 0,
  state-файл и сокет удалены. Пробы не подвешивают shutdown.
- `invalid_health_check_fails_run_with_config_error` — конфиг с
  `type = "tcp"` без `port`: exit 1 (класс «ошибка конфига»), лог содержит
  `health-check`; state-файл не появился (упали до spawn).

Транспортных e2e (tcp/http-пробы на реальном сервере) не делается осознанно:
транспорт исчерпывающе покрыт юнитами §7.2 на реальных сокетах, e2e
доказывает **проводку** цикла (конфиг → расписание → рестарт → state-файл), и
для этого достаточно exec-пробы; поднимать TCP-сервер из sh-стаба —
непереносимо (`nc` не везде одинаков).

### 7.5 Существующие тесты

Все 147 обязаны остаться зелёными. Единственная допустимая правка —
механическое `health_check: None` в литералах `ProcessConfig` (§3);
поведенческих правок и правок ожиданий — ноль. Проверить прогоном, а не
декларировать. `tests/commands.rs`, `tests/control.rs`, `tests/signals.rs`,
`tests/tree.rs`, `tests/shutdown.rs`, `tests/status.rs`, `tests/cli.rs`,
`tests/state.rs` семантически не меняются.

## 8. Порядок работ (шаг = один связный коммит)

1. **Конфиг** (`src/config.rs`): `HealthCheckConfig` / `ProbeKind` /
   `HealthProbe` / `probe()` / константы дефолтов; `ConfigError::Invalid`;
   валидация в `load()`; поле `health_check` в `ProcessConfig` + механические
   `health_check: None` по всем литералам (grep!); юниты §7.1.
2. **Раннер** (`src/health.rs`): `run_probe` + `ProbeError`, три вида проб;
   `pub mod health;` в `lib.rs`; юниты §7.2.
3. **Расписание и триггер** (`src/supervise.rs`): `HealthState`/
   `HealthSchedule`, поле `health` на `Supervised`, `run_due_health_check()`,
   вызов в `run()`, правка текста лога в арме `RestartPending` + doc-комменты
   `UserIntent`/`ProcClass`; in-process тесты §7.3 (`tests/health.rs`).
4. **e2e** (`tests/health_e2e.rs`, §7.4). Прогнать 10 раз подряд
   (`for i in $(seq 10); do cargo test --test health --test health_e2e || break; done`).
5. **Доки** (§9).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 9. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`: новый раздел «Этап 7 — Health-checks» по
  фактической реализации: оба решения пользователя из §2 (явно, как
  принятые); схема конфига и дефолты; формула первой пробы и семантика
  start-period; «одна liveness-проба, триада не вводится» с обоснованием;
  переиспользование `RestartPending`/`StopPhase` (без третьего интента — с
  обоснованием); «одна проба за тик» как третье санкционированное исключение
  из «цикл не блокирует» (после `handle_poll_error` и таймаутов
  control-socket); `IpAddr`-only и отказ от DNS; схема state-файла не
  изменена (version 1); известные ограничения этапа (список из §10);
  «Модульная структура» — добавить `health.rs`; строку «Экспорт метрик и
  health-checks остаются в POST_MVP_PLAN.md» в конце раздела Этапа 5 и в «Что
  вырезано из MVP» — скорректировать ссылкой на Этап 7.
- `docs/POST_MVP_PLAN.md`: раздел «Health-checks сверх „процесс жив“»
  пометить реализованным в Этапе 7 по образцу пункта про control-socket, с
  оговорками: startup/liveness/readiness-триада **не** подтвердилась — одна
  liveness-проба + `start-period-secs`; hostname/DNS, TLS, видимость health в
  `status`, несколько проб на процесс — остаются кандидатами здесь же.
- `docs/PLAN.md`: добавить Этап 7 в список этапов; упоминание health-checks
  в «После MVP» скорректировать.
- `README.md`: пример секции `[process.health-check]`, строка статуса этапов.
- `examples/supervisor.toml`: закомментированный или рабочий пример пробы с
  пояснением дефолтов (по прецеденту `stop-grace-secs`).
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`, в «грабли» (по
  фактическим находкам, ожидаемые кандидаты): таймаут пробы — реальное время,
  `Clock` не участвует (расписание — логическое, исполнение — ОС); тип поля
  `IpAddr` как бесплатная валидация «только IP»; `TcpListener` на порту 0 в
  тестах вместо хардкода портов; `set_read_timeout(Some(ZERO))` — ошибка в
  `std`, минимальный ненулевой остаток бюджета; exec-проба обязана реапнуть
  убитого по таймауту ребёнка (`kill()` + `wait()`), иначе зомби.
- `grep -rn "TODO(Этап 7)" src/` пуст; финальную редакцию формулировок делает
  основная сессия — здесь достаточно фактической точности.

## 10. Границы — что НЕ трогать, и известные ограничения

Не трогать:

- Порядок peek → killpg-sweep → reap в `poll_child` и его комментарии;
  инвариант `Running`-структуры — вообще без правок.
- `tick()`: единственная разрешённая правка — текст лога и doc-комментарии в
  арме `RestartPending` (§5, обоснование атрибуции); ни одной новой ветки, ни
  одного нового вызова из `tick()`. Вся новая логика — в
  `run_due_health_check()`, вызываемом из `run()`.
- `handle_command` и его таблица переходов — ноль правок (интент
  переиспользуется записью поля, не новой строкой таблицы).
- `begin_shutdown`, `escalate_to_kill`, бюджет poll-ошибок, backoff,
  `STABLE_RESET` — семантика не меняется.
- API `Clock`/`FakeClock` не расширять (исполнение пробы — реальное время ОС).
- `src/signal.rs`, `src/cli.rs`, `src/control.rs`, `src/main.rs`, схема
  state-файла (`version = 1`), таблица exit-кодов — без правок
  (проверить `git diff --stat`).
- Зависимости: **ноль правок `Cargo.toml`** — `std::net`, `std::process`,
  существующие фичи `nix`. Docker Compose не добавлять.
- Известные ограничения Этапов 4 (5 пунктов), 5 (4 пункта), 6 (7 пунктов) —
  принятое поведение, не «чинить».

Известные ограничения Этапа 7 (записать в TECHNICAL_PLAN как осознанные):

1. Тик может блокироваться до `timeout-secs` одной пробы (решение
   пользователя, §2): на это время растёт латентность реакции на сигналы,
   команды сокета и другие пробы. Ограничено одной пробой за тик.
2. Health не отражён в state-файле/`status` отдельным полем — только логи и
   косвенно (`stopping`/`restarting`, рост `restart-count`); причина рестарта
   (оператор vs health) в `status` неразличима — тот же класс, что
   ограничение 2 Этапа 6.
3. `host` — только IP-адрес; hostname/DNS не поддержан (в `std` нет
   резолвинга с таймаутом — он пробил бы бюджет пробы).
4. exec-проба не получает своей process-группы: потомки зависшей и убитой по
   таймауту команды-пробы осиротеют. Команда пробы должна быть простой
   проверкой, не форкать демонов.
5. HTTP: без TLS, редиректов (3xx = неуспех), chunked, keep-alive; читается
   только статус-строка; stdout/stderr exec-пробы отбрасываются.
6. Health-рестарт, как и операторский, не применяет backoff (респавн
   немедленный) и инкрементирует общий `restart_count`; троттлинг —
   каденция самих проб (минимум `threshold × interval`).
7. Одна проба на процесс; несколько проб и readiness/liveness-разделение —
   POST_MVP.

## 11. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` —
   зелёные; `cargo test --test health` и `--test health_e2e` — зелёные 10
   прогонов подряд.
2. Все 147 тестов Этапов 1–6 проходят; их единственная правка — механическое
   `health_check: None` в литералах `ProcessConfig`.
3. `tests/health.rs::reaching_threshold_forces_restart` и
   `tests/health_e2e.rs::unhealthy_process_is_restarted_after_threshold`
   доказывают критерий приёмки: рестарт ровно после N подряд, не раньше;
   здорового не трогают; сброс счётчика успехом покрыт
   (`success_resets_consecutive_failures`).
4. Залипший насмерть процесс покрыт
   (`stuck_process_is_killed_via_grace_escalation` — SIGKILL по grace).
5. Взаимодействия зафиксированы тестами: операторский stop паузит пробы и
   перекрывает health-рестарт; shutdown исключает пробы; рестарт даёт свежий
   start-period и нулевой счётчик.
6. `Cargo.toml`, `src/signal.rs`, `src/cli.rs`, `src/control.rs`,
   `src/main.rs` не изменены; схема state-файла не изменена (проверить
   `git diff --stat`).
7. Доки из §9 актуализированы, включая пометку о реализации в
   POST_MVP_PLAN.md и оба решения пользователя в TECHNICAL_PLAN.md.
