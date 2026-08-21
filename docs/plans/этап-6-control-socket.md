# План Этапа 6 — Control-socket (`start` / `stop` / `restart <name>`)

Ветка: `этап-6/control-socket`. Исполнителю: никаких git-коммитов — commit/push/PR
делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/shutdown.rs` / `tests/status.rs`.

## 1. Цель и критерий приёмки

Демон получает управляющий unix-socket, а CLI — три новые подкоманды:
`supervisor-rs stop <name>` останавливает супервизируемый процесс и удерживает
его остановленным (restart policy для него подавляется), `start <name>` снова
включает остановленный, `restart <name>` принудительно перезапускает — всё на
работающем демоне, без его рестарта. Обмен — текстовый протокол поверх
`SOCK_STREAM` unix-domain-socket, одна команда на соединение.

Критерий приёмки: на живом демоне с процессом policy `always`
`supervisor-rs stop web` приводит к тому, что `status` показывает `web` как
`stopped` и процесс не воскресает; `start web` возвращает его в `running` с
новым PID; `restart web` меняет PID работающего процесса. Команда на
несуществующее имя, на остановленный демон и во время shutdown даёт внятную
ошибку и exit 1 у клиента, не панику. Штатное завершение демона удаляет файл
сокета; осиротевший файл сокета от аварийно умершего демона не мешает
следующему запуску.

## 2. Архитектурное решение — принято пользователем, НЕ пересматривать

**Control-socket встраивается в существующий poll-loop; перевода цикла на
событийную модель (self-pipe / `signalfd` / `poll(2)` по дескрипторам) НЕ
будет.** Это осознанное расхождение с изначальным предположением
`docs/POST_MVP_PLAN.md` и с условием пересмотра сигнального механизма из
Этапа 3 («если цикл станет событийным — вернуться к self-pipe/signalfd»):
условие объявляется несработавшим и в этом этапе, решением пользователя.

Механика: `UnixListener` в неблокирующем режиме (`set_nonblocking(true)`),
`accept()` опрашивается раз в тик из `run()` — **тот же паттерн, что
`maybe_write_state()` в Этапе 5**: неблокирующая операция, вызываемая из
`run()` после `tick()`, не из `tick()`. Цена — задержка ответа на команду до
одного тика (50 мс); для админ-CLI это осознанно приемлемо, ровно как 50-мс
латентность сигналов в Этапе 3.

Следствия:

- `src/signal.rs` (глобальный `AtomicI32`, `take_pending()`) не трогается
  вообще.
- `tick()` остаётся свободным от сайд-каналов; вся работа с сокетом живёт в
  новом приватном `poll_control()`, вызываемом из `run()`.
- По итогам этапа править доки: `POST_MVP_PLAN.md` (пункт про control-socket)
  и `TECHNICAL_PLAN.md` (формулировки Этапов 3 и 5, ожидавшие «событийный цикл
  вместе с control-socket») — см. §9.

## 3. Протокол и путь сокета — новый модуль `src/control.rs`

### Протокол — однострочный текстовый

**Решение: простой line-протокол, не JSON и не бинарный.** Обоснование —
консистентно с Этапом 5: `serde_json` был бы новым крейтом ради ничего, а весь
словарь протокола — три глагола и имя; TOML поверх сокета — оверкилл для
одной строки. Текстовый протокол вдобавок отлаживается руками
(`nc -U <sock>`). Отвергнуто и версионирование протокола: клиент и сервер —
один бинарник, схема несовпадения версий здесь не живёт.

Формат (LF-терминированные строки, UTF-8):

- Запрос: `<verb> <name>\n`, где `verb` ∈ `start` | `stop` | `restart`, а
  `name` — всё после первого пробела до `\n` (имя из конфига может содержать
  пробелы; `split_once(' ')`, не `split_whitespace`). Имя не пустое.
- Ответ: ровно одна строка — `ok\n`, `ok: <text>\n` или `error: <text>\n` —
  после чего сервер закрывает соединение.
- Одна команда — одно соединение. Никаких сессий, пайплайнинга, keep-alive.

```rust
// src/control.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Start(String),
    Stop(String),
    Restart(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(Option<String>),   // renders as "ok" / "ok: <text>"
    Error(String),        // renders as "error: <text>"
}

/// Parses one request line (trailing '\n'/'\r' stripped by the caller or here).
pub fn parse_request(line: &str) -> Result<Request, String>;
/// Renders a response line WITHOUT the trailing newline.
pub fn render_response(resp: &Response) -> String;
/// Parses a response line back (used by the CLI client and roundtrip tests).
pub fn parse_response(line: &str) -> Response;
```

Лимиты: `MAX_REQUEST_BYTES = 4096` — запрос длиннее без `\n` отбрасывается с
ответом `error: malformed request`. Имя с `\n`/`\r` в протоколе невыразимо —
клиентская сторона отвергает такие имена как usage-ошибку (§6), серверная
просто никогда их не увидит.

### Путь сокета

Тот же каталог, что у файла состояния Этапа 5 — он уже создаётся с mode
`0700`, что и является всей моделью доступа (сокет доступен только своему
uid): `$XDG_RUNTIME_DIR/supervisor-rs/control.sock`, фолбэк
`/tmp/supervisor-rs-<uid>/control.sock`. Переопределение — флаг
`--control-socket <path>` (у `run` и у трёх клиентских подкоманд);
env-переменную не вводить, по прецеденту `--state-file`.

Чтобы не дублировать XDG-логику, в `src/state.rs` выделить чистую
`pub fn runtime_dir_from(xdg_runtime_dir: Option<&str>) -> PathBuf`
(возвращает `$XDG_RUNTIME_DIR/supervisor-rs` либо `/tmp/supervisor-rs-<uid>`);
`state::default_path_from` становится `runtime_dir_from(x).join("state.toml")`
— чистый рефактор, существующие юниты `default_path_*` не меняются. В
`control.rs`:

```rust
pub fn default_socket_path() -> PathBuf;                            // env-обёртка
pub fn default_socket_path_from(xdg: Option<&str>) -> PathBuf;      // чистая
```

**Грабля `sun_path`:** путь unix-сокета ограничен ~108 байтами; переполнение —
громкая ошибка `bind`, не тихая порча. В тестах держать имена файлов сокетов
короткими (`c.sock` в tempdir). Записать в SKILL.md (§9).

## 4. `ControlServer`: bind, осиротевший сокет, accept, таймауты

```rust
// src/control.rs
pub struct ControlServer {
    listener: UnixListener,   // std::os::unix::net — новых крейтов нет
    path: PathBuf,
}

#[derive(Debug)]
pub enum BindError {
    /// A live daemon already answers on this socket.
    AlreadyRunning(PathBuf),
    Io(std::io::Error),
}
// + Display/Error в стиле ConfigError

impl ControlServer {
    /// Probe-then-bind, см. ниже. Creates the parent dir (mode 0700) the same
    /// way state::write_atomic does.
    pub fn bind(path: &Path) -> Result<Self, BindError>;
    pub fn path(&self) -> &Path;
    /// Non-blocking accept: None on WouldBlock; unexpected errors are logged
    /// at warn and also answered with None (supervision must not care).
    pub fn try_accept(&self) -> Option<UnixStream>;
    /// Best-effort removal of the socket file on clean exit.
    pub fn cleanup(&self);
}

/// Reads one request line from an accepted connection, bounded by
/// REQUEST_IO_TIMEOUT and MAX_REQUEST_BYTES.
pub fn read_request(stream: &mut UnixStream) -> Result<Request, String>;
/// Writes the response line; EPIPE and friends are logged at debug and
/// swallowed — the client is gone, nobody is owed anything.
pub fn respond(stream: &mut UnixStream, resp: &Response);
```

### Осиротевший файл сокета (аналог «файл состояния при аварийном завершении»)

`bind` на существующий путь падает с `EADDRINUSE` независимо от того, жив ли
владелец. **Решение — probe-then-bind:** если файл по пути существует, сделать
`UnixStream::connect(path)`:

- соединение удалось → на сокете живой демон → `BindError::AlreadyRunning`
  (пробное соединение просто закрыть, ничего не писать);
- соединение не удалось (`ECONNREFUSED` для мёртвого сокета, любая другая
  ошибка) → файл осиротел: `fs::remove_file(path)` и обычный `bind`.

Это прямее, чем сверка с `daemon-pid` из state-файла (state-файл может
отсутствовать, быть от другого `--state-file` или сам быть устаревшим), и не
делит с `daemon_alive` уязвимость к переиспользованию pid: живость
доказывается самим сокетом. Гонка «два демона стартуют одновременно и оба
видят осиротевший файл» осознанно не закрывается — тот же класс ограничения,
что «один супервизор на uid» из Этапа 5 (ограничение 2); записать в известные
ограничения этапа.

Ошибка `bind` при старте демона — **exit 1** (класс ошибок старта, как ошибка
конфига). Важен порядок в `main.rs`: bind делается **до** `SupervisorLoop::new`
(то есть до первого spawn) — отказ «уже запущен» не должен успеть наплодить
детей, которых пришлось бы гасить.

### Таймауты чтения/записи — ограниченно-блокирующее исключение

`try_accept` неблокирующий, но принятая коннекция в момент `accept` может ещё
не содержать данных (клиент подключился, но `write` не доехал). Полноценный
неблокирующий разбор потребовал бы per-connection буферов, живущих между
тиками — машинку состояний ради админ-CLI. **Решение v1:** на принятом стриме
выставить `set_read_timeout` / `set_write_timeout` =
`REQUEST_IO_TIMEOUT = 250 ms` и читать блокирующе; таймаут = плохой клиент →
`error: malformed request` (best-effort) и дроп соединения.

Обоснование допустимости: воспитанный клиент (наш же бинарник) пишет команду
одним `write` сразу после `connect` — окно «принят, но данных нет»
микросекундное, таймаут в штатной жизни не срабатывает никогда. Задержать цикл
на 2×250 мс может только злонамеренный клиент с тем же uid (каталог 0700), а
такой uid может послать демону и SIGKILL — угрозы нет. Прецедент
ограниченно-блокирующей операции в цикле уже есть:
`handle_poll_error` с его `REAP_RETRIES × REAP_RETRY_DELAY` реального sleep.
Таймауты — это `SO_RCVTIMEO`/`SO_SNDTIMEO` на сокете, **API `Clock` не
расширяется** (это ядро ОС, а не логическое время; `FakeClock` тут нечего
подменять). Обработка — не более **одного** соединения за тик: очередь ждёт в
listen-backlog ядра, второй клиент получает ответ тиком позже; для
последовательных вызовов CLI это незаметно.

## 5. Семантика команд и интеграция в `SupervisorLoop`

### Новое поле `Supervised.intent` — отдельно от `StopPhase`

`StopPhase` **переиспользуется** для гашения по команде: это per-process
машина эскалации «TERM → дедлайн → KILL», продвигаемая обычным `tick()`, и её
ветка эскалации в `tick()` уже не зависит от `shutting_down` — второй такой же
машины не будет. А вот **решение «что делать после реапинга»** (перезапускать
по policy / не перезапускать / перезапустить принудительно) — новое измерение,
и его нельзя вешать ни на `StopPhase` (он обнуляется при реапинге), ни на
`shutting_down` (это режим всего супервизора, команда же адресна). Отсюда:

```rust
/// What the operator asked for over the control socket. Orthogonal to both
/// StopPhase (per-process TERM→KILL escalation, reused as-is) and
/// `shutting_down` (whole-supervisor shutdown): intent decides what happens
/// *after* the leader is reaped, which neither of those tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserIntent {
    /// Normal supervision; the restart policy applies.
    None,
    /// `stop <name>`: no respawn after exit, `done` stays false so the daemon
    /// keeps running and `start <name>` can revive it. Cleared only by `start`.
    Stopped,
    /// `restart <name>`: schedule an immediate respawn once the exit is
    /// reaped, then revert to None. Transient, unlike Stopped.
    RestartPending,
}
```

Поле: `intent: UserIntent` в `Supervised`, инициализируется `None` в `new()` и
в ветке респавна не трогается (сбрасывается в `None` только в ветке реапинга,
см. ниже). Инвариант: `intent == Stopped ⟹ next_restart_at == None`
(поддерживается двумя точками, где `Stopped` выставляется).

Ключевое отличие от `done`: `done` — терминальное «супервизору здесь больше
нечего делать», по нему `run()` выходит. Пользовательский `stop` НЕ делает
процесс `done` — иначе `stop` единственного процесса завершил бы весь демон и
`start` стало бы некому принимать. Остановленный процесс: `running == None`,
`done == false`, `next_restart_at == None`, `intent == Stopped` — `tick()` для
него no-op, демон живёт. В снапшоте Этапа 5 такой процесс попадает в ветку
`_ => Stopped` — **схема state-файла не меняется вовсе** (`version` остаётся
1); слово `stopped` покрывает и «остановлен оператором», и «завершился
терминально» — осознанно, различение отложено (записать в ограничения).

### `handle_command` — таблица переходов (предписывающе)

```rust
impl<'a, C: Clock> SupervisorLoop<'a, C> {
    /// Applies one control request. Pure with respect to sockets — pub so
    /// in-process tests can drive commands without any IO.
    pub fn handle_command(&mut self, req: &Request) -> Response;
}
```

Первые две проверки, до всего остального:

1. `self.shutting_down` → `Error("supervisor is shutting down")` — для всех
   трёх глаголов. Shutdown уже владеет и рестартами, и `StopPhase` всех
   процессов; команда, вклинившаяся в него, дала бы гонку двух хозяев.
2. Имя не найдено в `procs` → `Error("no such process \"<name>\"")`.
   Ограничение (наследие best-effort старта Этапа 2): процесс, чей первый
   spawn провалился, в `procs` отсутствует и отвечает «no such process» — как
   и в `status` Этапа 5; записать в ограничения этапа.

Классификация состояния процесса (локальные термины для таблицы):
RUNNING = `running.is_some() && stop == Idle`;
STOPPING = `running.is_some() && stop != Idle` (вне shutdown это возможно
только после команды `stop`/`restart`, поэтому здесь `intent ∈ {Stopped,
RestartPending}`);
BACKOFF = `running == None && next_restart_at.is_some()`;
USER_STOPPED = `running == None && intent == Stopped`;
DONE = `done == true`.

| Состояние | `stop` | `start` | `restart` |
|---|---|---|---|
| RUNNING | `intent = Stopped`; `signal_group(TERM)`; `stop = Terminating{now + stop_grace}` → `Ok("stopping \"<name>\"")` | `Ok("\"<name>\" is already running")` | `intent = RestartPending`; `signal_group(TERM)`; `stop = Terminating{...}` → `Ok("restarting \"<name>\"")` |
| STOPPING | `intent = Stopped` (перекрывает restart-в-полёте) → `Ok("stopping \"<name>\"")` | `Error("\"<name>\" is stopping; retry once it has stopped")` | если `intent == RestartPending` → `Ok("\"<name>\" restart already in progress")`; иначе → `Error("\"<name>\" is stopping")` |
| BACKOFF | `intent = Stopped`; `next_restart_at = None` → `Ok("\"<name>\" stopped")` | `next_restart_at = Some(now)` → `Ok("starting \"<name>\"")` | `next_restart_at = Some(now)` → `Ok("restarting \"<name>\"")` |
| USER_STOPPED | `Ok("\"<name>\" is already stopped")` | `intent = None`; `next_restart_at = Some(now)` → `Ok("starting \"<name>\"")` | `Error("\"<name>\" is stopped; use 'start <name>'")` |
| DONE | `Ok("\"<name>\" is already stopped")` (идемпотентно, intent не трогать) | `Error("\"<name>\" has finished and cannot be started")` | `Error("\"<name>\" has finished and cannot be restarted")` |

Решения, зафиксированные таблицей (в код — комментариями):

- **Ответ — асинхронный ack, не подтверждение смерти.** `ok: stopping` значит
  «сигнал послан, машина `StopPhase` взведена», а не «процесс мёртв»:
  синхронное ожидание блокировало бы цикл на весь grace. Наблюдать прогресс —
  через `status` (`stopping` → `stopped`). Семантика systemctl `--no-block`.
- **`stop`/`start` идемпотентны** (повтор — `ok`, как у systemctl), `restart`
  по неработающему — ошибка: «перезапустить остановленное» — противоречие,
  пользователю нужен `start`.
- **`start`/`restart` из BACKOFF просто делает рестарт немедленным** —
  `next_restart_at = now`, ожидание backoff срезается. Состояние `Backoff`
  (текущая ступень) при этом не сбрасывается: команда ускоряет один рестарт,
  а не объявляет процесс здоровым.
- **`start`/`restart` не спавнят синхронно** — только выставляют
  `next_restart_at = Some(now)`; сам spawn делает существующая ветка респавна
  в `tick()` на следующем тике. Это бесплатно переиспользует её обработку
  ошибок spawn (ретрай с backoff), инкремент `restart_count` и установку
  `started_at`/`pgid`. Следствие: `restart_count` растёт и от операторских
  start/restart — счётчик означает «сколько раз респавнился», записать в док
  поля.
- **`DONE` не оживляется.** `done` — терминальный контракт `run()`; ревив
  сделал бы «все done → выход» ложным задним числом. Кому нужен процесс после
  policy `never` — тому `restart = always` или рестарт демона. Записать в
  ограничения.

### Правки `tick()` — ровно одна ветка

В обработке `PollOutcome::Exited`, после `proc.running = None; proc.stop =
Idle;`, текущее `if self.shutting_down {...} else if policy {...} else {done}`
расширяется до:

```rust
if self.shutting_down {
    /* как сейчас: done = true */
} else {
    match proc.intent {
        UserIntent::RestartPending => {
            proc.intent = UserIntent::None;
            proc.next_restart_at = Some(self.clock.now());
            tracing::info!(name = %proc.config.name, "operator restart: respawn scheduled");
        }
        UserIntent::Stopped => {
            // Not `done`: the daemon stays up so `start <name>` can revive it.
            tracing::info!(name = %proc.config.name, "stopped by operator");
        }
        UserIntent::None => { /* существующая policy-ветка дословно */ }
    }
}
```

Ветка респавна, guard `!done && !shutting_down && next_restart_at due`, не
меняется: у `Stopped` нет `next_restart_at` по инварианту. Порядок
peek → killpg-sweep → reap в `poll_child` не трогается вообще.

### Правки `run()` и `main.rs`

```rust
// SupervisorLoop
pub fn with_control_server(mut self, server: ControlServer) -> Self;  // opt-in, как with_state_file

// run(), внутри цикла — после maybe_write_state(), до sleep:
self.poll_control();
// после цикла, рядом с state::remove:
if let Some(server) = &self.control_server { server.cleanup(); }
```

`poll_control` (приватный): `try_accept()` → нет клиента — return; есть —
`read_request` → `Ok(req)` → `let resp = self.handle_command(&req)` (займ
сервера к этому моменту уже отпущен: `try_accept` возвращает владеемый
`UnixStream`) → `respond`; `Err(msg)` → `respond(Error(msg))`. Один клиент за
тик (§4).

`main.rs`, ветка `run`: порядок — load config → `install_handlers` →
`ControlServer::bind(control_socket.unwrap_or_else(default_socket_path))`
(ошибка → `tracing::error!` + exit 1; **до первого spawn**, см. §4) →
`SupervisorLoop::new(...).with_state_file(...).with_control_server(server)`.

`begin_shutdown` менять не нужно: для процесса `Stopped` (`running == None`)
он уже выставляет `done = true`, и демон штатно выходит — SIGTERM супервизору
с остановленными оператором процессами работает без единой правки.

## 6. CLI: грамматика, клиент, exit-коды

### `src/cli.rs`

```rust
pub enum Command {
    Run {
        config: PathBuf,
        state_file: Option<PathBuf>,
        control_socket: Option<PathBuf>,   // новое
    },
    Status { state_file: Option<PathBuf> },
    /// start/stop/restart <name>: thin client of the control socket.
    Control {
        request: crate::control::Request,
        control_socket: Option<PathBuf>,
    },
    Help,
}
```

Один вариант `Control` вместо трёх зеркальных: `main.rs` обрабатывает их
одинаково, а глагол уже выражен типом `Request`.

Правила `parse` (дополнение к существующим):

- Подкоманды `start` / `stop` / `restart`: ровно один позиционный `<name>`;
  ноль или два → `UsageError`.
- Имя с `\n`, `\r` или пустое → `UsageError` («process name must be a single
  line») — защита от инъекции в line-протокол; обычные пробелы в имени
  допустимы (протокол их переносит, §3), но такой аргумент пользователь и так
  заквотит сам.
- `--control-socket <path>` — у `run`, `start`, `stop`, `restart`; повторный —
  последний побеждает (по прецеденту `--state-file`).
- **Флаги теперь проверяются на применимость к подкоманде**: `--state-file` у
  `start`/`stop`/`restart` → `UsageError`, `--control-socket` у `status` →
  `UsageError`. Сейчас парсер собирает флаги до диспетчеризации — добавить
  после `match` подкоманды проверку «флаг задан, но подкоманде не положен».
  Это ужесточение и для существующей пары (`--control-socket` у `status`), но
  не ломает ни один работающий вызов.

`usage()` — новый текст (дословно):

```
supervisor-rs — a minimal process supervisor (mini-systemd) for Unix

Usage:
  supervisor-rs run <config-path> [--state-file <path>] [--control-socket <path>]
  supervisor-rs status [--state-file <path>]
  supervisor-rs start <name> [--control-socket <path>]
  supervisor-rs stop <name> [--control-socket <path>]
  supervisor-rs restart <name> [--control-socket <path>]

Subcommands:
  run      Start the supervisor daemon with the given TOML config.
  status   Show the state of the supervised processes of a running daemon.
  start    Start a process previously stopped with 'stop'.
  stop     Stop a process and keep it stopped (its restart policy is suspended).
  restart  Stop a running process and start it again.

Options:
  --state-file <path>      Path of the state snapshot the daemon writes and
                           status reads. Default: $XDG_RUNTIME_DIR/supervisor-rs/
                           state.toml, or /tmp/supervisor-rs-<uid>/state.toml
                           when XDG_RUNTIME_DIR is not set.
  --control-socket <path>  Path of the control socket the daemon listens on and
                           start/stop/restart connect to. Default:
                           $XDG_RUNTIME_DIR/supervisor-rs/control.sock, or
                           /tmp/supervisor-rs-<uid>/control.sock when
                           XDG_RUNTIME_DIR is not set.
  -h, --help               Print this help.
```

### Клиент — `control::send_command` + обвязка в `main.rs`

Не по образцу `status` (тот читает файл) — клиент открывает сокет:

```rust
// src/control.rs
/// Connect → write one request line → read one response line. Both directions
/// bounded by CLIENT_IO_TIMEOUT (5 s): a wedged daemon must not hang the CLI.
/// The one-tick (50 ms) answer latency of the poll loop fits with a huge margin.
pub fn send_command(socket: &Path, req: &Request) -> Result<Response, ClientError>;

#[derive(Debug)]
pub enum ClientError {
    /// Connect failed — the usual "daemon is not running" case.
    Connect { path: PathBuf, source: std::io::Error },
    Io(std::io::Error),
    /// The daemon answered something that is not a response line.
    Malformed(String),
}
// + Display/Error; Connect renders as
// "cannot connect to supervisor at <path>: <err> (is the daemon running?)"
```

`main.rs`, ветка `Control { request, control_socket }`:
путь = `control_socket.unwrap_or_else(control::default_socket_path)`;
`send_command`; `Ok(Response::Ok(msg))` → `println!` (`ok` либо `ok: <msg>`),
exit 0; `Ok(Response::Error(msg))` → `eprintln!("error: {msg}")`, exit 1;
`Err(client_err)` → `eprintln!("error: {client_err}")`, exit 1. Результат
команды — через `println!`/`eprintln!`, не `tracing` (конвенция SKILL.md).

### Exit-коды — существующая таблица, без новой нумерации

Usage → 2 (включая все новые грамматические ошибки); `run`: ошибка конфига → 1,
**ошибка bind control-socket → 1** (новый член класса «ошибка старта»),
`had_start_errors` → 1, иначе 0; `status` без демона → 1;
**новое:** `start`/`stop`/`restart` — 0 при `ok`-ответе, 1 при `error:`-ответе
либо ошибке подключения. Ничего из существующего не меняется.

## 7. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность по содержимому файла/снапшота, поллинг с дедлайном вместо
одиночных assert и sleep, `wait_with_timeout` для бинарника, новые тесты на
сигналы/процессы — 10 прогонов подряд. Каждый e2e-запуск бинарника изолирует
**и** `--state-file`, **и** `--control-socket` в своём tempdir (дефолтный путь
сокета один на uid — та же грабля, что у state-файла). Handshake — по файлу
состояния (`wait_for_state`), не по pid-файлам.

### 7.1 Юниты `src/control.rs` (mod tests)

Протокол (чистые функции):

- `parse_request_accepts_three_verbs` — `"stop web"`, `"start web"`,
  `"restart web"` (+ вариант с хвостовым `\n`).
- `parse_request_keeps_spaces_in_name` — `"stop my app"` → `Stop("my app")`.
- `parse_request_rejects_unknown_verb`
- `parse_request_rejects_missing_name` — `"stop"` и `"stop "`.
- `parse_request_rejects_empty_line`
- `response_roundtrips_through_render_and_parse` — все три формы (`ok`,
  `ok: msg`, `error: msg`).
- `default_socket_path_prefers_xdg` / `default_socket_path_falls_back_to_tmp`
  — через чистую `default_socket_path_from`, без мутации env (и сверка, что
  каталог совпадает с каталогом `state::default_path_from`).

Сокет (реальные `UnixListener`/`UnixStream` в tempdir, короткие имена файлов —
грабля `sun_path`):

- `bind_creates_socket_file` — после `bind` файл существует; `cleanup()`
  удаляет.
- `bind_removes_orphaned_socket_file` — забиндить и дропнуть listener (файл
  остаётся — воспроизведение аварийной смерти демона), второй `bind` по тому
  же пути успешен.
- `bind_refuses_live_socket` — держать первый `ControlServer` живым, второй
  `bind` → `BindError::AlreadyRunning`.
- `try_accept_returns_none_without_client` — неблокируемость: вызов
  возвращается сразу с `None`.
- `request_response_roundtrip_over_socket` — из потока-клиента
  `send_command(path, Stop("web"))`; сервер `try_accept` (поллинг с
  дедлайном) → `read_request` → `respond(Ok(...))`; клиент получил `Ok`.
- `read_request_times_out_on_silent_client` — клиент коннектится и молчит;
  `read_request` возвращает `Err` не позже ~`REQUEST_IO_TIMEOUT` с запасом
  (assert по дедлайну ≤ 2 с, не по точному времени).

### 7.2 Юниты `src/cli.rs` (mod tests, дополнение к 14 существующим)

- `parses_stop_with_name` / `parses_start_with_name` /
  `parses_restart_with_name`
- `parses_stop_with_control_socket`
- `parses_run_with_control_socket`
- `rejects_stop_without_name`
- `rejects_stop_with_two_names`
- `rejects_control_socket_without_value`
- `rejects_multiline_process_name` — `stop "a\nb"` → usage (инъекция в
  line-протокол зафиксирована контрактом).
- `rejects_state_file_on_stop` / `rejects_control_socket_on_status` — проверка
  применимости флагов.

### 7.3 In-process тесты команд — новый файл `tests/commands.rs`

`FakeClock`, без сокетов и без реальных хендлеров: команды подаются напрямую в
`handle_command`, продвижение — `tick()`. Хелперы `cfg()`/`sh()`/DEAF-стаб
скопировать из `tests/shutdown.rs` (осознанное дублирование, пометить
комментарием — см. преамбулы существующих файлов).

- `stop_kills_process_and_policy_always_does_not_resurrect_it` — живой стаб,
  `restart = "always"`; `handle_command(Stop)` → `Ok`, содержит `stopping`;
  tick до реапинга (поллинг с дедлайном по `is_alive`-хелперу); затем
  `advance` на десятки секунд + tick'и → `is_done(0) == false`, снапшот
  показывает `stopped`, `pid == None`, новый инстанс не появился. Ключевой
  тест этапа.
- `stop_escalates_to_sigkill_after_grace` — глухой к TERM стаб,
  `stop-grace-secs = 2`; `Stop`; advance(3 c) + tick → процесс мёртв.
  Доказывает, что машина `StopPhase` реально работает и вне shutdown.
- `start_revives_user_stopped_process` — после сценария `stop` подать
  `Start` → `Ok: starting`; tick → процесс снова жив, pid новый,
  `restart_count == 1`, снапшот `running`.
- `restart_respawns_regardless_of_policy_never` — живой стаб,
  `restart = "never"`; `Restart` → tick до реапинга → следующий tick спавнит
  новый инстанс (pid отличается), `is_done(0) == false`.
- `restart_during_backoff_is_immediate` — падающий стаб, `on-failure`;
  дождаться `next_restart_delay(0).is_some()`; `Restart` → `Ok`;
  `next_restart_delay(0) == Some(0)`; tick → новый инстанс без advance.
- `stop_cancels_scheduled_restart` — падающий стаб; в BACKOFF подать `Stop` →
  `Ok`, `next_restart_delay(0) == None`; advance(60 c) + tick'и → респавна
  нет, `is_done(0) == false`.
- `stop_is_idempotent` — второй `Stop` по уже остановленному →
  `Ok: ... already stopped`.
- `start_on_running_is_idempotent_ok`
- `restart_on_user_stopped_is_an_error` — ответ содержит `use 'start`.
- `commands_name_not_found` — `Stop("ghost")` → `Error`, содержит
  `no such process`.
- `commands_rejected_during_shutdown` — `begin_shutdown(SIGTERM)`; все три
  команды → `Error`, содержит `shutting down`.
- `daemon_survives_stop_of_its_only_process` — конфиг из одного процесса;
  после полного цикла `stop` → `is_done(0) == false` (демон не вышел бы:
  `done` не выставлен).
- `shutdown_finishes_cleanly_with_user_stopped_process` — процесс остановлен
  оператором, затем `begin_shutdown` → `is_done(0) == true` (демон способен
  штатно выйти).
- `stop_then_restart_override` — `Restart` по живому (STOPPING в полёте),
  затем `Stop` → `Ok`; после реапинга респавна нет (интент перекрыт).

### 7.4 e2e на реальном бинарнике — новый файл `tests/control.rs`

Хелперы `write_config` / `start_supervisor` / `wait_for_state` /
`wait_with_timeout` скопировать из `tests/status.rs` (осознанное
дублирование); `start_supervisor` расширить флагом `--control-socket`.
Клиентские вызовы — `Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))` с
`--control-socket` в tempdir теста.

- `stop_start_restart_roundtrip` — демон, стаб `sleep`-цикл,
  `restart = "always"`. Дождаться `running` (+ pid1). `stop web` → exit 0,
  stdout начинается с `ok`; `wait_for_state` до `stopped` с `pid == None`
  (policy `always` не воскресил — это и проверяется самим достижением
  `stopped`). `start web` → `wait_for_state` до `running` с pid2 ≠ pid1 и
  `restart-count == 1`. `restart web` → `wait_for_state` до `running` с
  pid3 ≠ pid2, `restart-count == 2`. SIGTERM демону, exit 0.
- `stop_of_unknown_name_reports_error` — живой демон; `stop ghost` → exit 1,
  stderr содержит `no such process`.
- `client_reports_connect_error_without_daemon` — демона нет; `stop web
  --control-socket <tempdir>/absent.sock` → exit 1, stderr содержит `cannot
  connect`, без паники (в т.ч. это фиксирует поведение «сокета нет вовсе»).
- `restart_is_rejected_during_daemon_shutdown` — глухой к TERM стаб,
  `stop-grace-secs = 30`; SIGTERM демону; `wait_for_state` до `stopping`
  (handshake «shutdown начался»); `restart web` → exit 1, вывод содержит
  `shutting down`. Затем второй SIGTERM (эскалация) + `wait_with_timeout`.
- `daemon_removes_socket_on_clean_exit` — после SIGTERM и `wait_with_timeout`
  файл сокета отсутствует (удаление происходит в `run()` до выхода процесса —
  детерминированный одиночный assert, как для state-файла).
- `daemon_starts_over_orphaned_socket` — в tempdir забиндить `UnixListener` и
  дропнуть его (осиротевший файл); стартовать демона с этим
  `--control-socket`; дождаться `running`; `stop web` работает → сокет был
  пересоздан.
- `second_daemon_refuses_busy_socket` — демон 1 жив; демон 2 с тем же
  `--control-socket`, но другим `--state-file` и своим конфигом →
  `wait_with_timeout` демона 2, exit 1 (детей не оставил: его стаб пишет
  pid-файл? — не усложнять: конфиг демона 2 из одного `sleep`-стаба, после
  exit 1 проверить, что state-файл демона 2 не появился — bind падает до
  spawn).

### 7.5 Дополнения `tests/cli.rs`

- `stop_without_name_is_a_usage_error` (код 2, stderr содержит `Usage:`)
- `unknown_flag_on_stop_is_a_usage_error` (код 2)
- `help_mentions_control_subcommands` — stdout `--help` содержит `stop` и
  `--control-socket`.

### 7.6 Существующие тесты

Не меняются вовсе: `with_control_server` — opt-in, `poll_control` без сервера
— no-op, конструкторы и грамматика `run`/`status` не тронуты. Все 96 текущих
тестов обязаны остаться зелёными без единой правки (в отличие от Этапа 5 —
это надо проверить прогоном, а не декларировать).

## 8. Порядок работ (шаг = один связный коммит)

1. **Протокол и пути** (`src/control.rs`, часть 1): `Request` / `Response` /
   `parse_request` / `render_response` / `parse_response`,
   `default_socket_path(_from)`; рефактор `state::runtime_dir_from`;
   `pub mod control;` в `lib.rs`; юниты протокола и путей из §7.1.
2. **`ControlServer` и клиент** (`src/control.rs`, часть 2): `bind` с
   probe-then-bind, `try_accept`, `read_request`/`respond` с таймаутами,
   `cleanup`, `send_command`/`ClientError`; сокетные юниты из §7.1.
3. **`UserIntent` + `handle_command` + ветка в `tick()`**
   (`src/supervise.rs`): поле `intent`, таблица переходов §5, правка ветки
   `Exited`; новый `tests/commands.rs` (§7.3). Сокеты в этом шаге не
   участвуют.
4. **Интеграция в `run()` и `main.rs`**: `with_control_server`,
   `poll_control`, cleanup на выходе; в `main.rs` — bind до spawn, exit 1 при
   `BindError`.
5. **CLI**: `Command::Control`, `--control-socket`, проверка применимости
   флагов, новый `usage()`; обвязка клиента в `main.rs`; юниты §7.2 и
   дополнения `tests/cli.rs` (§7.5).
6. **e2e `tests/control.rs`** (§7.4). Прогнать 10 раз подряд
   (`for i in $(seq 10); do cargo test --test control || break; done`); также
   10× `--test commands`.
7. **Доки** (§9).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 9. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`:
  - новый раздел «Этап 6 — Control-socket» по фактической реализации: решение
    пользователя «poll-loop, не событийная модель» (явно, как принятое, с
    формулировкой расхождения с прежним ожиданием), протокол, путь сокета,
    probe-then-bind, `UserIntent` и его отношение к `StopPhase`/`done`,
    асинхронный ack, ограниченно-блокирующие таймауты чтения как второе (после
    `handle_poll_error`) санкционированное исключение, exit-коды клиента;
  - известные ограничения Этапа 6: гонка одновременного двойного старта на
    осиротевшем сокете; `stopped` в `status` не различает «оператором» и
    «терминально»; DONE не оживляется; процессы с провалившимся первым spawn
    отвечают «no such process»; `restart_count` растёт от операторских команд;
    один клиент за тик; имя с `\n` неадресуемо (отвергается клиентом);
  - **раздел Этапа 3**: в «Условие пересмотра» дописать, что в Этапе 6 условие
    снова не сработало — control-socket по решению пользователя встроен в
    poll-loop (неблокирующий accept раз в тик), событийный цикл не введён;
    фразу «Событийный цикл теперь ожидается только вместе с control-socket из
    POST-MVP» скорректировать — ожидание не подтвердилось;
  - **раздел Этапа 5**, блок «Что осталось за рамками»: убрать «а с ними и
    переход цикла на событийный, то есть self-pipe/signalfd» — реализовано
    иначе, сослаться на Этап 6;
  - «Модульная структура»: добавить `control.rs`.
- `docs/POST_MVP_PLAN.md`: пункт «Полноценный control-socket…» пометить
  реализованным в Этапе 6 (с оговоркой: без событийного цикла — предположение
  не подтвердилось) либо перенести формулировку в TECHNICAL_PLAN и оставить
  здесь ссылку.
- `docs/PLAN.md`: добавить Этап 6 в список этапов (MVP = Этапы 1–5,
  Этап 6 — первый пост-MVP этап); строку про «Управляющие подкоманды…
  остались за рамками MVP» в Этапе 5 дополнить ссылкой на Этап 6.
- `README.md`: примеры `stop`/`start`/`restart`, строка статуса этапов.
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`, в «грабли»:
  `sun_path` ≈ 108 байт — короткие имена сокетов в tempdir; probe-then-bind
  для осиротевшего unix-сокета (`EADDRINUSE` не различает живого и мёртвого
  владельца); неблокирующий `accept` + bounded `set_read_timeout` на принятом
  стриме как паттерн «сокет в poll-loop»; каждый e2e-запуск изолирует
  `--control-socket` в tempdir (как `--state-file`); Rust глушит SIGPIPE до
  `main` — запись в закрытый клиентом сокет даёт `EPIPE`-ошибку, а не смерть
  процесса, ловить и логировать.
- `src/main.rs` / `src/cli.rs` / `src/supervise.rs`: doc-заголовки — упомянуть
  Этап 6; `grep -rn "TODO(Этап 6)" src/` должен быть пуст (если TODO
  появлялись).
- Финальную редакцию формулировок делает основная сессия — здесь достаточно
  фактической точности.

## 10. Границы — что НЕ трогать

- `src/signal.rs` целиком: `AtomicI32` / `take_pending` / `install_handlers`.
  Self-pipe / `signalfd` / `poll(2)`-цикл не вводить — решение пользователя
  (§2).
- Порядок peek → killpg-sweep → reap в `poll_child` и комментарии-обоснования;
  инвариант «`Running` = killpg безопасен, обнуляется после реапинга».
- `tick()`: никакой работы с сокетом внутри; команды применяются через
  `handle_command`, вызываемый из `poll_control()` в `run()`. Единственная
  правка `tick()` — ветка `intent` в обработке `Exited` (§5).
- API `Clock`/`FakeClock` не расширять: таймауты сокета — `SO_RCVTIMEO` на
  уровне ОС, логического времени не касаются.
- Машина `StopPhase`, `begin_shutdown`, `escalate_to_kill`, бюджет
  poll-ошибок, backoff, `STABLE_RESET` — семантика не меняется;
  `begin_shutdown` не правится вовсе.
- Схема state-файла: `version` остаётся 1, ни полей, ни значений `state` не
  добавлять; `status` не меняется и `--control-socket` не получает.
- Зависимости: **ноль правок `Cargo.toml`** — `std::os::unix::net` покрывает
  всё, новых фич `nix` не требуется. Docker Compose не добавлять.
- Таблица exit-кодов: usage → 2; config / bind сокета / `had_start_errors` →
  1; иначе 0; `status` без демона → 1; клиентские команды → 0/1 по ответу.
  Новую нумерацию не изобретать.
- Известные ограничения Этапов 4 (5 пунктов) и 5 (4 пункта) — принятое
  поведение, не «чинить».
- Не в скоупе v1 (записать, не реализовывать): параллельные клиенты в один тик
  (очередь в backlog), `status`/список процессов через сокет, оживление DONE,
  различение `stopped`-оператором в `status`, аутентификация сверх 0700-каталога,
  `--instance`, перезагрузка конфига.

## 11. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` — зелёные;
   `cargo test --test control` и `--test commands` — зелёные 10 прогонов
   подряд.
2. Все 96 тестов Этапов 1–5 проходят **без правок** (§7.6).
3. `tests/control.rs::stop_start_restart_roundtrip` доказывает критерий
   приёмки: stop останавливает вопреки policy `always`, start возвращает с
   новым pid, restart меняет pid.
4. Ошибочные пути зафиксированы тестами: несуществующее имя, повторный `stop`,
   `restart` остановленного, команды во время shutdown, клиент без демона,
   занятый и осиротевший сокет.
5. Сокет удаляется при штатном выходе
   (`daemon_removes_socket_on_clean_exit`); демон стартует поверх
   осиротевшего файла (`daemon_starts_over_orphaned_socket`).
6. `Cargo.toml` не изменён; `src/signal.rs` не изменён (проверить
   `git diff --stat` по этим файлам).
7. Доки из §9 актуализированы, включая правки условия пересмотра Этапа 3 и
   формулировки POST_MVP про событийный цикл.
