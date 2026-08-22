# План Этапа 8 — Перезагрузка конфига без рестарта демона (SIGHUP → diff → применить)

Ветка: `этап-8/config-reload` (уже создана). Исполнителю: никаких git-коммитов —
commit/push/PR делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/commands.rs` / `tests/health.rs`.

## 1. Цель и критерий приёмки

Сейчас конфиг читается ровно один раз, при `run <config>`; любое изменение
набора процессов требует рестарта демона (и, значит, teardown всех деревьев).
Этап 8 добавляет перезагрузку: по SIGHUP демон перечитывает **тот же файл**,
что был передан в `run`, сравнивает новый список `[[process]]` со старым **по
имени процесса** и применяет только разницу:

- имя исчезло из конфига → процесс останавливается тем же путём, что
  операторский `stop` (TERM → grace → SIGKILL), и после полного реапинга его
  запись **удаляется из списка супервизии** (и из `status`);
- имя осталось, но изменилось хоть одно поле (`command` / `env` / `workdir` /
  `restart` / `stop-grace-secs` / `health-check`) → немедленный принудительный
  рестарт тем же путём, что операторский `restart` и health-рестарт Этапа 7;
  новый инстанс стартует уже с новым конфигом;
- имя без изменений → процесс не трогается вообще: ни рестарта, ни сброса
  расписания health-проб, ни лишних info-логов;
- новое имя → спавнится сразу, как при старте демона;
- битый новый конфиг (ошибка парсинга TOML или валидации) → reload полностью
  отменяется: `error!`-лог с причиной, старый конфиг и все процессы нетронуты,
  демон живёт. Всё или ничего — частичного применения не бывает.

Критерий приёмки: на живом демоне правится файл конфига (один процесс
неизменён, у одного изменена команда, один убран, один добавлен) и посылается
`kill -HUP <pid>`. После этого в `status`: неизменённый — тот же PID и
`restart-count`; изменённый — новый PID, выросший `restart-count`, работает по
новой команде; убранный — исчез из таблицы (а его дерево мертво); добавленный —
`running`. `kill -HUP` с испорченным файлом ничего не меняет и не роняет демона
(в логе — `error` с причиной). SIGHUP во время shutdown игнорируется; SIGTERM
после reload по-прежнему корректно гасит всё и даёт exit 0.

## 2. Архитектурные решения — приняты пользователем, НЕ пересматривать

1. **Убранный процесс** останавливается как операторский `stop <name>`
   (Этап 6): `UserIntent::Stopped` + `signal_terminate` (TERM → grace →
   SIGKILL). После полного реапинга запись **убирается из `procs` целиком** —
   в отличие от операторского stop, ей нечего делать в списке: процесса больше
   нет в конфиге.
2. **Изменённый процесс** (не равен по `PartialEq` хоть в одном поле) →
   немедленный принудительный рестарт через **переиспользование
   `UserIntent::RestartPending` + `signal_terminate`** — тот же путь, что
   операторский `restart` и health-рестарт. Третий вариант интента не вводить
   (прецедент Этапа 7: `UnhealthyRestart` был отвергнут по той же логике).
3. **Неизменённый процесс не трогается вообще**: ни рестарт, ни сброс
   расписания/счётчиков health-проб, ни info-логов на процесс.
4. **Новое имя** — заспавнить сразу, как в `new()`.
5. **Битый новый конфиг** (ошибка парсинга TOML или `ConfigError::Invalid`) →
   reload отменяется целиком, `error!`-лог, демон не падает и не паникует.
   Только «всё или ничего».
6. **SIGHUP во время `shutting_down` — игнорировать** (как пробы уже подавлены
   в `run_due_health_check`).
7. **Scope: только SIGHUP.** Команды `reload` через control-socket нет и не
   добавляется.

Плюс три технических решения, зафиксированных вместе с постановкой:

- **`Supervised.config` переводится с заимствования `&'a ProcessConfig` на
  владеющий `Arc<ProcessConfig>`** (§5): новый `Vec<ProcessConfig>`, прочитанный
  по SIGHUP, живёт меньше исходного среза `configs`, так что reload с
  заимствованием невозможен в принципе. Лайфтайм-параметр `'a` уходит из
  `SupervisorLoop` и `Supervised` целиком; внешняя сигнатура `new()` не
  меняется.
- **Для SIGHUP — отдельный атомик** (`RELOAD_PENDING: AtomicBool`) рядом с
  существующим `PENDING`, а не тот же `AtomicI32` (§3): `PENDING` хранит только
  последний сигнал, и SIGHUP, попавший туда, мог бы затереть одновременный
  SIGTERM/SIGINT — потеря сигнала остановки недопустима. Комментарий в
  `src/signal.rs:90-94` это прямо предвидел.
- **Путь к конфигу — opt-in поле по прецеденту `state_writer` /
  `control_server`**: builder-метод `with_config_reload(path)` (§6). Тесты, не
  вызывающие его, на SIGHUP не реагируют, как `maybe_write_state` ничего не
  пишет без `state_writer`. Сам diff+apply — отдельная `pub`-функция, чтобы
  in-process тесты гоняли его на `FakeClock` без файлов и сигналов.

### 2.1 Решения плана (приняты здесь; утверждает основная сессия при ревью)

- **Дубликаты имён процессов в конфиге становятся ошибкой загрузки**
  (`ConfigError::Invalid`, §4). Diff «по имени» с неуникальными именами
  неопределён; control-socket и так адресует только первый носитель имени.
  Это делает невалидными конфиги, которые раньше «работали» (оба одноимённых
  процесса супервизировались) — осознанная цена за корректный ключ.
- **`done`-процесс reload не оживляет никогда** — ни изменение конфига, ни
  что-либо ещё: инвариант «DONE не оживляется» Этапа 6 сохраняется. Изменённый
  `done` получает новый `Arc` конфига (косметика), но остаётся `done`; убранный
  `done` удаляется из списка немедленно (реапить нечего).
- **Оператор побеждает автоматику** (прецедент Этапа 7): изменение конфига
  user-stopped процесса НЕ запускает его — только подменяет конфиг, чтобы
  будущий `start <name>` использовал новый. Исключение — remove: убранный
  процесс убирается из списка даже если был остановлен оператором (конфиг —
  более сильный факт, чем интент: имени больше не существует).
- **Удаление из `procs` — двухфазное**: пометка `pending_removal: bool` при
  apply, физический `retain` — только когда `running == None` (реапить больше
  нечего). Точный момент и взаимодействие с индексами — §6.4.
- **Новый `stop-grace-secs` применяется уже к текущему стопу** изменённого
  процесса: `Arc` подменяется до `signal_terminate`, и дедлайн эскалации
  считается от нового конфига. Одна согласованная точка истины вместо «старый
  grace для старого инстанса».

## 3. Сигнал: `src/signal.rs` — второй атомик, аддитивно

Существующая семантика `PENDING` / `take_pending()` / SIGTERM / SIGINT не
меняется ни на строку поведения; существующие тесты не трогаются.

```rust
/// Set when SIGHUP arrives; drained by `take_reload_pending`. A separate flag,
/// not a third value in `PENDING`: `PENDING` keeps only the *last* signal, so a
/// SIGHUP landing there could overwrite a concurrent SIGTERM/SIGINT and lose
/// the shutdown request. A reload must never mask a stop.
static RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

/// Takes the pending reload request, clearing the flag.
pub fn take_reload_pending() -> bool {
    RELOAD_PENDING.swap(false, Ordering::Relaxed)
}
```

- `handle_signal` ветвится по уже полученному параметру перед единственной
  атомарной записью (сравнение `signo == libc::SIGHUP` — async-signal-safe,
  бюджет обработчика не растёт):

```rust
extern "C" fn handle_signal(signo: libc::c_int) {
    if signo == libc::SIGHUP {
        RELOAD_PENDING.store(true, Ordering::Relaxed);
    } else {
        PENDING.store(signo, Ordering::Relaxed);
    }
}
```

- `install_handlers`: SIGHUP добавляется и в маску (все **три** сигнала
  маскируют друг друга, пока обработчик выполняется — хендлеры не вкладываются),
  и третьим `sigaction(Signal::SIGHUP, &act)`. Побочное следствие, осознанное:
  до Этапа 8 SIGHUP убивал демона диспозицией по умолчанию, теперь —
  перезагружает конфиг.
- Комментарий у `signal_from_raw` (строки про «a future signal (SIGHUP…)»)
  актуализировать: предсказание сбылось, SIGHUP живёт в отдельном атомике и в
  `PENDING` не попадает по построению.
- Обновить doc-комментарии модуля и `install_handlers` (теперь
  SIGTERM/SIGINT/SIGHUP).

Юнит-тест: `reload_pending_round_trips_and_clears` — прямой вызов
`handle_signal(libc::SIGHUP)` (это обычная `extern "C"` функция, вызываемая из
теста), затем `take_reload_pending()` → `true`, повторно → `false`. **Не
трогать `PENDING` в этом тесте** (включая ассерты «SIGHUP не попал в
`PENDING`»): флаг process-global, им уже владеет единственный тест
`pending_round_trips_and_clears`, второй читатель — гонка в многопоточном
раннере. Невозможность попадания SIGHUP в `PENDING` гарантируется ветвлением по
построению, комментария достаточно.

## 4. Конфиг: `src/config.rs` — derive'ы и уникальность имён

- `ProcessConfig` и `HealthCheckConfig` получают `#[derive(..., Clone,
  PartialEq)]` (сейчас нет ни там, ни там). Все вложенные поля уже
  `Clone`/`PartialEq`-совместимы: `String`, `Vec<String>`, `Option<PathBuf>`,
  `Option<BTreeMap<String, String>>`, `RestartPolicy`, `u64`, `ProbeKind`,
  `Option<IpAddr>`, `NonZeroU16/U32/U64`, `Option<String>`. `Eq` не требуется
  (и не нужен: сравнение только на равенство). `Clone` нужен Arc-миграции
  (§5), `PartialEq` — diff'у «изменилось / не изменилось».
- `load()` после существующей валидации health-check дополнительно проверяет
  **уникальность имён** (решение §2.1): повтор имени →
  `ConfigError::Invalid { message: r#"duplicate process name "web""# }`.
  Существующий вариант `Invalid` переиспользуется, его doc-комментарий («A
  syntactically valid config whose health-check section fails…») обобщить:
  cross-field/cross-process валидация. Новых вариантов ошибок и exit-кодов нет.
- Сравнение сознательно — по значениям (`PartialEq` структуры), а не по тексту
  файла: перестановка `[[process]]`-секций, комментарии и форматирование TOML
  изменением не считаются; `env` — `BTreeMap`, порядок ключей не важен.

Юнит-тесты (mod tests):

- `rejects_duplicate_process_names` — через `load()` на временном файле:
  `ConfigError::Invalid`, Display содержит имя-дубликат.
- `process_config_equality_notices_every_field` — базовый конфиг равен своей
  копии (`clone()`); затем по одному мутируется каждое поле (`command`, `env`,
  `workdir`, `restart`, `stop_grace_secs`, `health_check`) и каждый вариант
  неравен базовому. Страховка от будущей ручной реализации `PartialEq`,
  «забывшей» поле — ровно тот класс тихой поломки, который diff превратит в
  «изменение не замечено, процесс не рестартует».

## 5. Владение конфигом: `Arc<ProcessConfig>` вместо `&'a` (`src/supervise.rs`)

Механическая, но сквозная правка; поведенчески — ноль изменений, отдельный
коммит с полным зелёным прогоном до начала reload-логики.

- `Supervised.config: Arc<ProcessConfig>`; лайфтайм-параметр убирается:
  `struct Supervised`, `pub struct SupervisorLoop<C: Clock>`,
  `impl<C: Clock> SupervisorLoop<C>`; `signal_terminate(proc: &mut Supervised,
  …)`, `handle_poll_error(proc: &mut Supervised, …)`, `uptime_of(&self, proc:
  &Supervised)`.
- Внешняя сигнатура **не меняется**: `pub fn new(configs: &[ProcessConfig],
  clock: C) -> Self` — внутри каждый конфиг оборачивается
  `Arc::new(config.clone())`. Все 54 вызова `SupervisorLoop::new(` в
  `src/main.rs` и 5 тестовых файлах остаются как есть.
- Обращения `proc.config.name`, `proc.config.stop_grace()`,
  `proc.config.health_check` работают через deref без правок; вызовы
  `process::spawn(proc.config)` становятся `process::spawn(&proc.config)`
  (deref-coercion `&Arc<T> → &T`).
- Обоснование `Arc`, зафиксировать комментарием у поля: конфиг раздаётся в
  несколько мест (respawn в `tick()`, health-состояние, снапшот) и при reload
  подменяется целиком; `Arc::clone` при этом дешёв и не копирует `Vec<String>`
  команды, а сам `ProcessConfig` иммутабелен после загрузки — классический
  случай shared ownership без внутренней мутабельности.

**Механическая правка тестов** (единственная в существующих файлах, аналог
`health_check: None` Этапа 7) — аннотации `SupervisorLoop<'_, FakeClock>` →
`SupervisorLoop<FakeClock>` в хелперах, ровно 16 мест в 5 файлах:

- `tests/commands.rs` — 5 (`get_pid`, `tick_until_reaped`,
  `tick_until_new_pid`, `tick_until_restart_scheduled`, `tick_until_done`);
- `tests/health.rs` — 5 (`get_pid`, `state_of`, `tick_until_reaped`,
  `tick_until_new_pid`, `ensure_running`);
- `tests/restart.rs` — 2 (`wait_for_restart_scheduled`, `advance_and_respawn`);
- `tests/shutdown.rs` — 2 (`tick_until_done`, `tick_until_restart_scheduled`);
- `tests/state.rs` — 2 (`tick_until_done`, `tick_until_restart_scheduled`).

Плюс в `src/supervise.rs` юнит-тест `poll_error_gives_up_and_kills_group_after_max`
конструирует `Supervised` литералом: `config: &config` → `config:
Arc::new(config)` (конфиг после `spawn` больше не нужен по ссылке) и новое поле
`pending_removal: false` (§6). Больше ни один существующий тест не меняется —
проверить прогоном, а не декларировать.

## 6. Reload: diff и применение (`src/supervise.rs`)

Новый модуль не заводится: diff оперирует `Supervised`/`ProcessConfig` и
процентов на девяносто состоит из переходов состояния супервизии — это ткань
`supervise.rs`, как `handle_command`. Чистая часть (сравнение списков) выделена
в отдельную функцию для юнит-тестов без процессов.

### 6.1 Чистый diff

```rust
/// What a reload decided for each process name, as indices — pure data so the
/// diff is unit-tested without spawning anything. Names are unique on both
/// sides: `load()` rejects duplicates (and `new()`'s procs inherit uniqueness
/// from the initial load).
#[derive(Debug, Default, PartialEq, Eq)]
struct ReloadPlan {
    /// (procs index, new-config index): name present on both sides, configs
    /// equal. Untouched by apply — except the pending-removal resurrection
    /// (§6.3, "повторное добавление во время удаления").
    unchanged: Vec<(usize, usize)>,
    /// (procs index, new-config index): name present on both sides, configs
    /// differ — forced restart.
    changed: Vec<(usize, usize)>,
    /// procs indices whose name is gone from the new config.
    removed: Vec<usize>,
    /// new-config indices whose name is not currently supervised.
    added: Vec<usize>,
}

/// Pure by-name diff of the running config against the newly loaded one.
fn plan_reload(old: &[&ProcessConfig], new: &[ProcessConfig]) -> ReloadPlan
```

Порядок внутри векторов — порядок обхода (`removed`/`unchanged`/`changed` — по
`old`, `added` — по `new`), детерминированный для тестов.

### 6.2 Хелперы, извлечённые из `new()`

Рефакторинг без изменения поведения, чтобы add-путь и rebuild health не
дублировали код:

```rust
/// Builds the health state from an (already validated) config; a bad section
/// is logged and dropped, never a panic — extracted verbatim from `new()`.
fn build_health(config: &ProcessConfig) -> Option<HealthState>

/// Spawns one process and wraps it into a fresh Supervised entry — the shared
/// construction of `new()` and the reload "added" path. The caller logs the
/// error and decides what it means (had_start_errors at startup; skip on
/// reload).
fn start_supervised(config: Arc<ProcessConfig>, now: Instant)
    -> Result<Supervised, process::SpawnError>
```

`new()` переписывается через них; `had_start_errors` остаётся как был.

### 6.3 `apply_config` — применение diff'а

```rust
/// Applies a newly loaded process list to the running supervisor: by-name diff,
/// then stop-and-prune the removed, force-restart the changed, spawn the added,
/// leave the unchanged strictly alone. Pure of file IO — `reload_config` reads
/// the file; in-process tests call this directly. Precondition: names in `new`
/// are unique (guaranteed by `load()`).
pub fn apply_config(&mut self, new: Vec<ProcessConfig>)
```

Шаги (в этом порядке):

1. `plan_reload` по `procs` × `new` (старая сторона — `&*p.config` каждой
   записи, включая done/user-stopped/pending-removal).
2. **`unchanged`**: ничего — с одним исключением. Если у записи
   `pending_removal == true` (имя убрали прошлым reload'ом, TERM в полёте, а
   этот reload вернул имя): `pending_removal = false`, подменить
   `proc.config = Arc::new(new_cfg)`, пересобрать `health` (см. п. 3), интент →
   `UserIntent::RestartPending` — после реапинга уже уходящего инстанса процесс
   вернётся, а не останется трупом с интентом `Stopped`. То же самое для такой
   записи в `changed`. (Запись с `pending_removal` всегда имеет
   `running == Some` — иначе она была бы уже вырезана, §6.4.)
3. **`changed`** — для каждой пары `(i, j)`:
   - `proc.config = Arc::new(new[j].clone())` — подмена **до** сигнала: respawn
     в `tick()` возьмёт новую команду/env/workdir, а `signal_terminate` — новый
     `stop-grace-secs` (решение §2.1);
   - **пересобрать `proc.health = build_health(&proc.config)`** — та самая
     грабля: перевзвод по generation в `run_due_health_check` сбрасывает только
     `HealthSchedule`, но `HealthState.probe` строится один раз в `new()` и
     иначе не обновился бы никогда — изменённая секция `[process.health-check]`
     продолжила бы проверяться старой пробой вечно. Пересборка в момент apply
     покрывает и добавление секции (None → Some), и удаление (Some → None);
     `schedule: None` перевзведётся штатно;
   - дальше по классу состояния (условия — те же выражения, что в
     `handle_command`; сам `handle_command` не трогается, §11):
     - `done` → больше ничего (не оживляется, §2.1);
     - `running.is_some() && stop == StopPhase::Idle` (RUNNING) →
       `intent = RestartPending; signal_terminate(proc, now)` — ровно
       операторский `restart`;
     - `running.is_some() && stop != Idle` (STOPPING) → ничего сверх подмены
       конфига: `RestartPending` в полёте сам респавнит уже с новым конфигом, а
       `Stopped` в полёте — оператор побеждает, процесс останется стоять;
     - `running == None && intent == Stopped` (USER_STOPPED) → ничего: новый
       конфиг возьмёт будущий `start <name>`;
     - иначе (BACKOFF / транзиентный зазор) → `next_restart_at = Some(now)` —
       срезать ожидание, как `handle_command` на BACKOFF × restart; ступень
       backoff не сбрасывается (прецедент Этапа 6: команда ускоряет один
       рестарт, не объявляет процесс здоровым).
4. **`removed`** — для каждой записи: `pending_removal = true;
   intent = UserIntent::Stopped; next_restart_at = None` (инвариант
   `Stopped ⟹ next_restart_at == None`); если `running.is_some() && stop ==
   Idle` — `signal_terminate(proc, now)`; если stop уже в полёте — не слать TERM
   второй раз (та же логика, что строка STOPPING × stop). Запись с
   `running == None` (backoff, user-stopped, done) будет вырезана прюнингом в
   конце apply — немедленно, реапить нечего.
5. **`added`** — для каждого `j`: `start_supervised(Arc::new(new[j].clone()),
   now)`; `Ok` → push в конец `procs` (существующие записи не сдвигаются),
   `info!`-лог со spawn-pid; `Err` → `error!`-лог и пропуск — best-effort, как
   в `new()`: неудавшийся процесс не трекается и не ретраится, а
   `had_start_errors` (контракт «ошибки старта демона» и его exit-код) reload
   не трогает. Записать в известные ограничения.
6. `self.prune_removed()` (§6.4).
7. Итоговый `info!` одной строкой: `changed = N, removed = N, added = N,
   unchanged = N, "config reload applied"`. При полном отсутствии разницы —
   `info!("config reload: no changes")`. Per-process info-логи — только у
   changed/removed/added (точка атрибуции причины, как warn «forcing restart» у
   health); unchanged не логируются (решение 3).

### 6.4 Удаление из `procs`: `pending_removal` и индексы

Новое поле `Supervised.pending_removal: bool` (в `new()` и `start_supervised` —
`false`). Физическое удаление:

```rust
/// Drops every entry marked for removal whose leader is fully reaped. Split
/// out because it runs from two places: the end of `apply_config` (entries
/// with no live child are removable immediately) and the end of `tick()` (a
/// live child goes through TERM → grace → KILL first, and only the reap makes
/// its entry removable).
fn prune_removed(&mut self) {
    self.procs.retain(|p| !(p.pending_removal && p.running.is_none()));
}
```

- Вызов в конце `tick()` — вторая из двух разрешённых правок `tick()` (§11).
  `retain` не блокирует и не является side-каналом — это переход состояния
  супервизии, ему место в `tick()`, а не в `run()`: тест, гоняющий `tick()`
  руками, не должен уметь «забыть» прюнинг и наблюдать процесс-призрак.
- **Точный момент исчезновения записи**: живой процесс — пометка при apply →
  TERM → (grace → SIGKILL при глухоте) → реапинг в `poll_child` → ветка
  `UserIntent::Stopped` в `tick()` (лог) → `retain` в конце **того же
  `tick()`-вызова**. Неживой (`running == None`) — сразу в `prune_removed()` в
  конце `apply_config`, без единого тика.
- **Индексы.** До Этапа 8 из `procs` не удалялось ничего и никогда; тестовые
  аксессоры по индексу (`restart_count(index)`, `is_done(index)`,
  `next_restart_delay(index)`, `snapshot().process[index]`) полагаются на
  стабильность. Она сохраняется для всех, кто не вызывает reload: существующие
  тесты Этапов 1–7 не ставят `pending_removal`, их `retain` не вырезает ничего
  — индексы не переживают только сам reload, которого в этих тестах нет
  (проверено: ни один существующий тест не вызывает
  `apply_config`/`reload_config`). Новые reload-тесты после apply адресуются
  по имени через `snapshot()` (там есть `name`), а не по донастроечным
  индексам. Зафиксировать это правило в преамбуле `tests/reload.rs`.
- Побочное следствие, осознанное: `any_active()` после прюнинга пуст, если
  reload убрал все процессы, — демон штатно завершает `run()` (exit 0, файлы
  прибраны), как если бы все процессы стали `done`. Записать в известные
  ограничения.

### 6.5 `reload_config`, builder и проводка в `run()` / `main.rs`

```rust
/// `run <config>`'s path, kept so SIGHUP can re-read it. Opt-in like
/// `state_writer`/`control_server`: without it SIGHUP is drained and ignored.
reload_path: Option<PathBuf>,   // поле SupervisorLoop

pub fn with_config_reload(mut self, path: PathBuf) -> Self

/// Handles one SIGHUP: re-reads the config file and applies the diff. The
/// whole reload is all-or-nothing — any load error (IO, parse, validation)
/// keeps the old config and every process untouched (user decision). `pub` so
/// e2e-shaped in-process tests can drive the file-reading path; the pure
/// diff+apply is `apply_config`.
pub fn reload_config(&mut self) {
    if self.shutting_down { /* debug!-лог, return — решение 6 */ }
    let Some(path) = self.reload_path.clone() else { return };
    tracing::info!(path = %path.display(), "SIGHUP: reloading config");
    match crate::config::load(&path) {
        Ok(config) => self.apply_config(config.process),
        Err(err) => tracing::error!(
            error = %err,
            "config reload failed; keeping the current config and processes"
        ),
    }
}
```

`run()` — одна вставка сразу после блока `take_pending()`, **до** проверки
`any_active()`:

```rust
if crate::signal::take_reload_pending() {
    self.reload_config();
}
```

Обоснование места: (а) SIGTERM обрабатывается раньше SIGHUP, и одновременная
доставка обоих даёт shutdown + проигнорированный reload (решение 6 — проверка
`shutting_down` внутри `reload_config`); (б) до `any_active()` — чтобы reload,
добавивший процессы демону, у которого все прежние уже `done`, успел записать
их прежде, чем цикл решит завершиться. Флаг дренируется и без `reload_path` —
повторный SIGHUP не «залипает».

`main.rs`: к цепочке builder'ов добавляется
`.with_config_reload(config_path.to_path_buf())`; заодно упомянуть SIGHUP в
doc-комментарии шапки. Больше `main.rs` не меняется (CLI-флагов нет — §11).

## 7. Наблюдаемость и state-файл — решения

- **Схема state-файла НЕ меняется, `version` остаётся 1.** Убранный процесс
  исчезает из снапшота как естественное следствие удаления из `procs` — это не
  новое поле схемы. До реапинга он виден как `stopping`; после — его нет в
  таблице `status` (интервал публикации 1 с — та же задокументированная
  staleness, что раньше).
- Изменённый процесс проходит в `status` обычный путь `stopping → restarting →
  running` с ростом `restart-count` — от операторского `restart` неотличим (тот
  же класс ограничения, что у health-рестарта Этапа 7).
- **Логи — единственный прямой канал наблюдения reload'а**: `info!` на приём
  SIGHUP, `error!` на отклонённый конфиг, per-process `info!` на
  removed/changed/added, итоговая строка с счётчиками (§6.3, п. 7).
- Правка текста одного существующего лога в `tick()`: в ветке
  `UserIntent::Stopped` сообщение `"stopped by operator"` становится неверной
  атрибуцией (интент теперь взводят и оператор, и reload-удаление). Заменить на
  нейтральное `"stopped; no respawn scheduled"` — прецедент Этапа 7 с
  `"forced restart"`. Обновить doc-комментарии `UserIntent::Stopped` (два
  взводящих) и `UserIntent::RestartPending` (три взводящих: operator restart,
  health-порог, reload-изменение), `ProcClass::Stopping` — аналогично.

## 8. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность по содержимому файла, поллинг с дедлайном вместо sleep, каждый
e2e-запуск изолирует `--state-file` и `--control-socket` в своём tempdir,
имена сокетов короткие. Новые тесты на процессы/сигналы прогнать 10 раз подряд.

### 8.1 Юниты `src/signal.rs`

- `reload_pending_round_trips_and_clears` — §3; `PENDING` не трогать.

### 8.2 Юниты `src/config.rs`

- `rejects_duplicate_process_names`, `process_config_equality_notices_every_field`
  — §4.

### 8.3 Юниты `src/supervise.rs` (mod tests) — чистый diff, без процессов

На литералах `ProcessConfig` (без spawn):

- `plan_reload_classifies_all_four_kinds` — old {a, b, c}, new {a без
  изменений, b с другой командой, d новый} → unchanged = [(0,0)],
  changed = [(1,1)], removed = [2], added = [2]; порядок детерминирован.
- `plan_reload_notices_each_field` — для каждого поля из решения 2 (command /
  env / workdir / restart / stop-grace-secs / health-check, включая None→Some и
  Some→изменённый health-check) пара конфигов с единственным отличием
  попадает в `changed`.
- `plan_reload_rename_is_remove_plus_add` — то же тело команды под другим
  именем → removed + added, не changed.
- `plan_reload_reorder_is_unchanged` — те же процессы в другом порядке секций →
  все unchanged (diff по имени, не по позиции).
- `plan_reload_empty_new_removes_everything` / `plan_reload_identical_is_all_unchanged`.

### 8.4 In-process тесты — новый файл `tests/reload.rs`

`FakeClock`, без сокетов, файлов конфига (кроме двух тестов на
`reload_config`) и хендлеров: `apply_config()` + `tick()` руками. Хелперы
`cfg`/`sh`/`DEAF_SCRIPT`/`deaf`/`wait_for_pid`/`is_alive`/`get_pid`/
`tick_until_reaped`/`tick_until_new_pid`/`ensure_running` и `with_exec_health`
скопировать из `tests/health.rs` (осознанное дублирование крейтов — пометить в
преамбуле; там же — правило «после reload адресоваться по имени из
`snapshot()`, индексы не переживают прюнинг»). Наблюдаемость новой команды —
маркер-файлы: стаб пишет свой argv-маркер в tempdir, у нового конфига маркер
другой.

- `unchanged_process_is_untouched` — apply того же списка (`clone()`): pid тот
  же, `restart_count == 0`, снапшот `running`; и — решение 3 дословно — exec
  health-проба с файлом-счётчиком: reload посреди интервала не сдвигает
  расписание (advance до исходного `next_check_at` → проба исполнилась ровно
  по старому графику, счётчик неуспехов не сброшен «в ноль заново» — две
  красных до reload + одна после при threshold 3 дают рестарт).
- `changed_command_forces_restart_with_new_config` — **ключевой тест этапа**:
  изменена команда → снапшот `stopping` сразу после apply; `tick_until_reaped`
  → `tick_until_new_pid` → новый pid, `restart_count == 1`, новый инстанс
  пишет маркер **новой** команды.
- `changed_stop_grace_applies_to_current_stop` — DEAF-стаб, старый grace 600,
  новый 2: apply → advance(3) → tick → старый pid мёртв (SIGKILL по **новому**
  grace), затем респавн. Доказывает и «подмена Arc до signal_terminate», и
  переиспользование эскалации целиком.
- `changed_health_check_probes_with_new_probe` — грабля §6.3 п. 3: проба
  пишет в файл A; reload меняет только `[process.health-check]` (файл B) →
  процесс рестартует (health-check — тоже поле конфига, решение 2), после
  рестарта advance по расписанию → растёт файл B, файл A больше не растёт.
- `removed_process_is_stopped_and_pruned` — два процесса, новый конфиг без
  второго: после apply второй — `stopping`; `tick` до исчезновения имени из
  снапшота → его pid мёртв (`is_alive == false`), в снапшоте один процесс,
  первый не тронут (pid тот же). Адресация — по имени.
- `removed_deaf_process_is_killed_and_pruned` — DEAF-стаб, grace 2: убран из
  конфига → advance(3) → tick → SIGKILL-путь → запись исчезла. TERM → grace →
  KILL на remove-пути.
- `removed_stopped_process_is_pruned_immediately` — операторский `stop`,
  `tick_until_reaped`, затем apply без него → записи нет сразу после apply, без
  единого tick.
- `added_process_is_spawned` — новое имя → в снапшоте `running`, в конце
  списка; его маркер-файл появился; существующие процессы не тронуты.
- `readded_during_removal_comes_back` — §6.3 п. 2: DEAF-стаб (реапинг не
  успевает), apply без него (TERM в полёте), затем apply снова с ним (та же
  или изменённая команда) → после реапинга старого инстанса процесс
  респавнится, из снапшота не исчезает навсегда.
- `changed_user_stopped_process_stays_stopped` — `stop` через
  `handle_command`, реапинг; apply с изменённой командой → снапшот `stopped`,
  респавна нет; затем `handle_command(Start)` → новый инстанс пишет маркер
  **новой** команды (конфиг подменён, оператор побеждал лишь запуск).
- `reload_error_keeps_everything` — через `with_config_reload` + реальный
  temp-файл: демоны-процессы живые, в файл пишется битый TOML →
  `reload_config()` → pid'ы и снапшот не изменились; затем в файл пишется
  валидный конфиг с изменением → `reload_config()` применяет. Покрывает и
  `ConfigError::Parse`, и (вторым файлом с `tcp` без `port`)
  `ConfigError::Invalid` — оба пути «всё или ничего».
- `reload_ignored_during_shutdown` — `begin_shutdown(SIGTERM)`, затем
  `apply_config`-через-`reload_config` с добавленным процессом → ничего не
  заспавнено, снапшот не изменился, shutdown доводится тиками до `is_done`
  штатно.
- `removing_every_process_ends_the_run` — apply с пустым списком (`Vec::new()`)
  → после реапинга `procs` пуст; ассерт через снапшот (пустая таблица) и
  отсутствие живых pid. (Поведение `run()` «выйти при пустом наборе» уже
  покрыто `any_active`-семантикой; здесь фиксируется сама зачистка.)

### 8.5 e2e на реальном бинарнике — новый файл `tests/reload_e2e.rs`

Хелперы `write_config`/`start_supervisor`/`wait_for_state`/`wait_with_timeout`
скопировать из `tests/health_e2e.rs`/`tests/control.rs` (осознанное
дублирование); изоляция `--state-file` и `--control-socket` обязательна; путь
конфига — в том же tempdir, перезаписывается через `std::fs::write` перед
SIGHUP.

**Handshake перед `kill -HUP` — только по state-файлу.** `install_handlers`
ставится в `main.rs` до первого spawn, а снапшот публикуется позже, из `run()`
— поэтому «`wait_for_state` увидел процессы» ⟹ «SIGHUP-хендлер уже стоит».
HUP, посланный до установки хендлера, убил бы демона диспозицией по умолчанию —
флейк, не отличимый от бага. Зафиксировать комментарием в хелпере.

- `sighup_applies_diff` — **критерий приёмки целиком**, все четыре судьбы в
  одном reload: конфиг A = {keep, change, gone}; дождаться `running` всех трёх
  и записать pid'ы; переписать файл на B = {keep без изменений, change с новой
  командой, new добавлен}; `kill -HUP`; `wait_for_state` до: keep — тот же pid
  и `restart-count == 0`; change — новый pid, `restart-count >= 1`; gone —
  отсутствует в снапшоте, его pid мёртв (`wait_until_gone`-поллинг); new —
  `running`. Затем SIGTERM → exit 0, все стабы мертвы, state-файл и сокет
  удалены.
- `sighup_with_broken_config_changes_nothing` — живой демон; файл
  перезаписывается мусором (`[[process\n`); `kill -HUP`; поллингом лога
  дождаться строки `config reload failed` (stdout — `tracing` пишет туда,
  грабля SKILL.md), затем убедиться: демон жив, снапшот с теми же pid.
  Восстановить валидный файл с изменением → HUP → изменение применилось
  (доказывает, что демон не залип после отказа). SIGTERM → exit 0.
- `sighup_before_any_change_is_a_no_op` — HUP без правки файла: pid'ы и
  `restart-count` стабильны на два чтения снапшота с зазором; в логе
  `no changes`. Дёшево и ловит случайный «рестарт всех по каждому HUP».

Ожидание «второй сигнал после первого» строить по логу/снапшоту, не по
`sleep` (грабля коалесцирования из SKILL.md — здесь сигналы в разных атомиках,
но порядок доставки всё равно наблюдается только по эффекту).

### 8.6 Существующие тесты

Все 191 обязаны остаться зелёными. Допустимые правки — только механические из
§5: 16 аннотаций типов в 5 файлах и литерал `Supervised` в юнит-тесте
`supervise.rs` (`Arc::new` + `pending_removal: false`). Поведенческих правок и
правок ожиданий — ноль; `tests/signals.rs`, `tests/tree.rs`, `tests/cli.rs`,
`tests/control.rs`, `tests/status.rs`, `tests/spawn.rs`, `tests/health_e2e.rs`
не меняются вовсе. Проверить прогоном.

## 9. Порядок работ (шаг = один связный коммит)

1. **Сигнал** (`src/signal.rs`): `RELOAD_PENDING`, `take_reload_pending`,
   SIGHUP в маске и `sigaction`, актуализация комментариев; юнит §8.1.
2. **Конфиг** (`src/config.rs`): `Clone`/`PartialEq` на
   `ProcessConfig`/`HealthCheckConfig`, уникальность имён в `load()`; юниты
   §8.2.
3. **Arc-миграция** (`src/supervise.rs` + 5 тестовых файлов): §5 целиком, ноль
   поведенческих изменений; полный зелёный прогон до перехода к шагу 4.
4. **Reload-ядро** (`src/supervise.rs`, `src/main.rs`): `ReloadPlan` /
   `plan_reload` / `build_health` / `start_supervised` / `pending_removal` /
   `apply_config` / `prune_removed` / `reload_config` / `with_config_reload`;
   вставка в `run()`; правка лога и doc-комментариев §7; юниты §8.3 +
   in-process §8.4 (`tests/reload.rs`).
5. **e2e** (`tests/reload_e2e.rs`, §8.5). Прогнать 10 раз подряд
   (`for i in $(seq 10); do cargo test --test reload --test reload_e2e || break; done`).
6. **Доки** (§10).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 10. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`: новый раздел «Этап 8 — Перезагрузка конфига
  (SIGHUP)» по фактической реализации: решения пользователя из §2 (явно, как
  принятые); отдельный атомик для SIGHUP и почему не `PENDING` (коалесцирование
  с сигналом остановки); `Arc<ProcessConfig>` и снятие лайфтайма; diff по имени
  + `PartialEq`, уникальность имён как ошибка загрузки; таблица судеб
  (unchanged/changed/removed/added × классы состояния, включая done и
  user-stopped); двухфазное удаление и момент прюнинга; «всё или ничего» при
  битом конфиге; схема state-файла не изменена (version 1); известные
  ограничения (список из §11). В раздел Этапа 6 — строку «Не в скоупе v1 …
  перезагрузка конфига без рестарта демона» дополнить ссылкой «реализовано в
  Этапе 8».
- `docs/POST_MVP_PLAN.md`: пункт «Перезагрузка конфига без рестарта демона
  (SIGHUP → diff → применить)» в «Прочих идеях» пометить реализованным в
  Этапе 8 по образцу пункта про control-socket, с оговорками: только SIGHUP
  (команды `reload` в сокете нет — кандидат сюда же), `done` не оживляется,
  изменение user-stopped не запускает, неудавшийся spawn добавленного не
  ретраится.
- `docs/PLAN.md`: добавить Этап 8 в список этапов; абзац «Этап 6 закрыл … ,
  Этап 7 — …» в конце дополнить Этапом 8.
- `README.md`: строка статуса этапов; краткий абзац «перезагрузка конфига:
  правите файл → `kill -HUP $(pid демона)`» с оговоркой «всё или ничего».
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`, в «грабли» (по
  фактическим находкам; ожидаемые кандидаты): второй сигнал — второй атомик
  (`PENDING` хранит только последний, SIGHUP в нём терял бы SIGTERM);
  e2e-handshake перед `kill -HUP` — по state-файлу, иначе HUP до установки
  хендлера убивает демона диспозицией по умолчанию; `Arc<ProcessConfig>` как
  цена владения при reload (заимствование из среза аргументов не переживает
  подмену конфига); индексные аксессоры не переживают удаление из `procs` —
  после reload адресоваться по имени.
- `examples/supervisor.toml`: комментарий-подсказку про `kill -HUP` рядом с
  шапкой (не новые поля — конфиг-схема этапом не меняется).
- `grep -rn "TODO(Этап 8)" src/` пуст; финальную редакцию формулировок делает
  основная сессия — здесь достаточно фактической точности.

## 11. Границы — что НЕ трогать, и известные ограничения

Не трогать:

- **`handle_command` и его таблица переходов — ноль правок**; протокол
  control-socket (`src/control.rs`) — вообще без изменений: reload — не команда
  сокета (решение 7). Условия классов состояния в `apply_config` — свои
  выражения, не рефакторинг `handle_command`.
- **CLI (`src/cli.rs`) — без правок**: SIGHUP не проходит через argv, флагов
  нет, таблица exit-кодов не меняется.
- `tick()`: ровно две разрешённые правки — текст лога + doc-комментарии в
  ветке `UserIntent::Stopped` (§7) и вызов `prune_removed()` в самом конце
  (§6.4). Ни одной новой ветки в теле обхода процессов.
- Существующая семантика `PENDING`/`take_pending()`/SIGTERM/SIGINT — только
  аддитивные правки рядом (второй атомик); ни одна старая ветка и ни один
  старый тест `signal.rs` не меняются поведенчески.
- Порядок peek → killpg-sweep → reap в `poll_child`; `begin_shutdown`,
  `escalate_to_kill`, backoff, `STABLE_RESET`, бюджет poll-ошибок — семантика
  без изменений.
- Схема state-файла (`version = 1`) — без новых полей; исчезновение убранного
  процесса из снапшота — следствие удаления из `procs`, не расширение схемы.
- API `Clock`/`FakeClock` не расширяется (reload не вводит своего времени:
  дедлайны стопов — существующий `signal_terminate` на инъектируемых часах).
- Зависимости: **ноль правок `Cargo.toml`** — `serde`/`toml`/`nix`/`std`
  покрывают всё. Docker Compose не добавлять.
- Известные ограничения Этапов 4–7 — принятое поведение, не «чинить».

Известные ограничения Этапа 8 (записать в TECHNICAL_PLAN как осознанные):

1. Только SIGHUP; команды `reload` через control-socket нет (решение 7) —
   кандидат в POST_MVP.
2. Reload применяется не мгновенно: изменённые/убранные живые процессы идут
   через TERM → grace → SIGKILL, полное применение занимает до их
   `stop-grace-secs`; «конфиг применён» наблюдается через `status`/логи, а не
   через подтверждение сигнала (у SIGHUP нет ответа по построению).
3. `done`-процесс не оживляется reload'ом ни при каком изменении конфига —
   инвариант «DONE не оживляется» Этапа 6; лечится только рестартом демона.
4. Изменение конфига user-stopped процесса не запускает его — оператор
   побеждает; новый конфиг подхватит будущий `start <name>`.
5. Неудавшийся spawn добавленного процесса — best-effort: `error!`-лог, без
   трекинга и ретраев (как в `new()`); `had_start_errors` и exit-код демона
   отражают только ошибки старта, не reload'а.
6. Рестарт изменённого процесса инкрементирует общий `restart_count` и, как
   операторский `restart`, не применяет и не сбрасывает backoff; в `status`
   он неотличим от операторского (тот же класс ограничения, что у
   health-рестарта Этапа 7).
7. Переименование процесса неотличимо от «убрать + добавить»: старое дерево
   гасится, новое стартует с нуля (никакого переноса состояния по «сходству»
   команд).
8. Reload, убравший все процессы, штатно завершает демона после teardown
   (пустой `procs` ⟹ `any_active() == false`) — симметрично «все стали
   `done`».
9. Конфиги с дублирующимися именами процессов теперь отклоняются на загрузке
   (в т.ч. при первом старте) — ужесточение относительно Этапов 1–7, цена
   корректного diff-ключа.
10. Латентность реакции на SIGHUP — до одного тика (50 мс) плюс, в худшем
    случае, длительность блокирующих операций текущей итерации (проба
    health-check и т.п.) — тот же класс, что у SIGTERM.

## 12. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` — зелёные;
   `cargo test --test reload --test reload_e2e` — зелёные 10 прогонов подряд.
2. Все 191 существующих теста проходят; их единственные правки — механические
   из §5 (16 аннотаций типов + литерал `Supervised` в юнит-тесте
   `supervise.rs`).
3. `tests/reload_e2e.rs::sighup_applies_diff` доказывает критерий приёмки
   целиком (четыре судьбы за один HUP на реальном бинарнике);
   `tests/reload.rs::changed_command_forces_restart_with_new_config` и
   `removed_process_is_stopped_and_pruned` — те же переходы in-process на
   `FakeClock`.
4. «Всё или ничего» покрыто с обеих сторон: `reload_error_keeps_everything`
   (in-process, Parse и Invalid) и `sighup_with_broken_config_changes_nothing`
   (e2e, включая последующий успешный reload).
5. Грабля health-пробы закрыта тестом
   `changed_health_check_probes_with_new_probe`: после reload с изменённой
   секцией `[process.health-check]` процесс проверяется новой пробой, старая
   больше не исполняется.
6. Взаимодействия зафиксированы тестами: SIGHUP при shutdown игнорируется
   (`reload_ignored_during_shutdown`); оператор побеждает
   (`changed_user_stopped_process_stays_stopped`); глухой к TERM процесс
   добивается по новому grace (`changed_stop_grace_applies_to_current_stop`) и
   на remove-пути (`removed_deaf_process_is_killed_and_pruned`); повторное
   добавление во время удаления не теряет процесс
   (`readded_during_removal_comes_back`).
7. `Cargo.toml`, `src/cli.rs`, `src/control.rs`, `src/health.rs`,
   `src/process.rs`, `src/state.rs` не изменены; схема state-файла не изменена
   (проверить `git diff --stat`).
8. Доки из §10 актуализированы, включая пометку о реализации в
   POST_MVP_PLAN.md и решения пользователя в TECHNICAL_PLAN.md.
