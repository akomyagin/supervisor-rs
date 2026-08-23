# План Этапа 10 — Resource limits (setrlimit + cgroup v2 для CPU/памяти)

Ветка: `этап-10/resource-limits` (уже создана). Исполнителю: никаких
git-коммитов — commit/push/PR делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и грабли
тестирования) и преамбулы `tests/logs.rs` / `tests/health.rs` /
`tests/health_e2e.rs`.

## 1. Цель и критерий приёмки

Сейчас супервизируемые процессы наследуют ресурсные лимиты супервизора и не
изолированы по CPU/памяти. Этап 10 добавляет два опциональных механизма
лимитов на процесс:

```toml
[[process]]
name = "web"
command = ["/usr/bin/myserver", "--port", "8080"]

[process.rlimit]                # per-process, setrlimit(2), soft = hard
nofile = 1024                   # RLIMIT_NOFILE: макс. открытых fd
as-bytes = 536870912            # RLIMIT_AS: адресное пространство, байты
cpu-secs = 300                  # RLIMIT_CPU: CPU-секунды (SIGXCPU)

[process.cgroup]                # per-tree, cgroup v2: лимит на всё дерево
cpu-max-percent = 50            # cpu.max: 50 = половина одного CPU, 200 = два
memory-max-bytes = 268435456    # memory.max, байты
```

- Без секций поведение прежнее до байта (opt-in, прецедент
  `[process.health-check]` / `[process.log]`).
- `[process.rlimit]`: лимиты ставятся в `pre_exec`-хуке (между fork и exec),
  рядом с существующим `setsid()`; наследуются всеми потомками. Хотя бы один
  из трёх ключей обязателен.
- `[process.cgroup]`: до спавна создаётся cgroup-поддерево
  `<cgroup-root>/<name>` (default root `/sys/fs/cgroup/supervisor-rs`,
  переопределяется флагом `--cgroup-root` — прецедент `--state-file`), в него
  пишутся `cpu.max`/`memory.max`, а сразу после `spawn()` pid ребёнка
  записывается в `cgroup.procs` **из родителя**. Лимит действует на всё
  дерево (наследуется при fork после attach). Хотя бы один из двух ключей
  обязателен.
- **Неудача настройки cgroup (нет прав, нет делегирования, нет cgroupfs, нет
  Linux) — ошибка спавна этого процесса**: `error!`-лог, процесс не
  трекается, соседи не страдают, `had_start_errors` → exit 1 в конце (тот же
  класс, что best-effort старт Этапов 1–2). Не деградировать молча в «без
  лимитов»: оператор попросил изоляцию — соврать «работает» хуже, чем не
  стартовать (обоснование §2).
- Teardown Этапа 4 (peek → sweep → reap, killpg) **не меняется ни на строку**
  — cgroup не участвует в остановке дерева (решение пользователя, §2).

Критерий приёмки: на живом демоне процесс с `[process.rlimit]` реально несёт
лимиты (наблюдаемо через `ulimit`-builtin в стабе); процесс с
`[process.cgroup]` на системе с делегированным cgroup v2 оказывается в
`<root>/<name>/cgroup.procs`, а `cpu.max`/`memory.max` содержат
сконфигурированные значения; рестарт (policy/оператор/health/reload)
сохраняет лимиты; SIGTERM демону — exit 0, cgroup-директории убраны (это
служебный артефакт рантайма, как state-файл и сокет, — не продукт, как
лог-файлы). Пустая секция (`[process.rlimit]`/`[process.cgroup]` без единого
ключа) и нулевые значения — ошибка загрузки конфига (exit 1). **В
непривилегированной песочнице `cargo test` остаётся зелёным**: cgroup-тесты
гейтятся на реальную возможность писать в cgroupfs и громко пропускаются
(§9.1), а вся rlimit-часть и негативные cgroup-тесты привилегий не требуют.

## 2. Архитектурные решения — приняты пользователем, НЕ пересматривать

1. **Оба механизма сразу**: и `setrlimit` (per-process), и cgroup v2 (реальные
   лимиты CPU/памяти на дерево). cgroup-тесты гейтятся на привилегии и
   **чисто, но громко** пропускаются там, где прав нет; обычный `cargo test`
   обязан оставаться зелёным в непривилегированной среде.
2. **cgroup — ТОЛЬКО для лимитов CPU/памяти, не для teardown.** Существующий
   `killpg`-путь Этапа 4 (peek → sweep → reap) не трогается вообще:
   `cgroup.kill` не вводится, не заменяет и не дополняет его. Известное
   ограничение «сбежавший из process-группы (свой `setsid`) орфан недостижим
   для `killpg`» остаётся как есть — кандидат в POST_MVP на будущее (и там
   остаётся пункт «`cgroup.kill` как teardown строже `killpg`»). Следствие:
   самый зрелый код проекта (teardown) в этом этапе не меняется вовсе.
3. **`setrlimit` — в существующем `pre_exec`-хуке**, рядом с `setsid()`:
   тот же async-signal-safe контекст, `setrlimit` не аллоцирует и не логирует
   (простой syscall), fork-safety хука при живых потоках-читателях Этапа 9 не
   меняется. Отдельный путь не заводится.
4. **Запись pid в `cgroup.procs` — в родителе, сразу после `cmd.spawn()`**,
   НЕ в `pre_exec`: форматирование pid — аллокация, запрещённая в форкнутом
   ребёнке при живых потоках родителя (тот же класс риска, что в плане
   Этапа 9). Цена — короткое окно между `exec()` и attach, где процесс ещё
   вне cgroup; осознанный компромисс. Атомарная альтернатива
   `clone3(CLONE_INTO_CGROUP)` (kernel 5.7+, сырой syscall) отклонена как
   непропорциональная сложность для учебного проекта — фиксируется как
   отклонённая альтернатива и известное ограничение (§12).
5. **`--cgroup-root <path>`** — переопределяемый флаг корня cgroup (default
   `/sys/fs/cgroup/supervisor-rs`) по прецеденту `--state-file` /
   `--control-socket`; единственный этап, где легитимно трогать
   `src/cli.rs` / `src/main.rs`.
6. **`Cargo.toml` — ровно одна правка**: `"resource"` в списке фич уже
   подключённого `nix` (нужна для `nix::sys::resource::setrlimit`). Никаких
   новых крейтов.

### 2.1 Решения плана (приняты здесь; утверждает основная сессия при ревью)

- **Две раздельные секции `[process.rlimit]` и `[process.cgroup]`**, не одна
  `[process.limits]`. Обоснование: механизмы различаются по всем осям — охват
  (процесс+наследники vs всё дерево через ядро), место применения (`pre_exec`
  vs файлы cgroupfs из родителя), требования к среде (rlimit работает везде и
  без привилегий — только вниз; cgroup требует делегированного v2) и режим
  отказа (rlimit-ошибка — EPERM на spawn; cgroup-ошибка — вся настройка).
  Слитная секция размыла бы границу «какой ключ куда» и сделала бы
  валидацию «хотя бы один ключ» двусмысленной (нужен cgroup-каталог или
  нет — стало бы неявным). Прецедент: health и log — тоже отдельные секции
  по механизмам. Обе новые секции — `Option`, поля в `ProcessConfig`
  последними после `log` (подтаблицы в порядке этапов, «скаляры до
  подтаблиц» соблюдено): сначала `rlimit`, затем `cgroup`.
- **Политика «cgroup setup failed → процесс не стартует»** (рекомендация
  пользователя — принята): любая ошибка цепочки mkdir root → включение
  контроллеров → mkdir `<name>` → запись лимитов → attach pid — это
  `SpawnError` этого процесса. Прецеденты: Этап 9 проваливает инстанс
  целиком, если не создался поток-читатель; «наполовину настроенный»
  инстанс — ложь оператору. На attach-ошибке уже спавнутый ребёнок
  SIGKILL-ится и реапится (как exec-проба Этапа 7 реапит убитого по
  таймауту) — зомби не утекает.
- **soft = hard = значение** для всех rlimit-полей. Раздельные soft/hard —
  два ключа на лимит ради сценария «процесс сам поднимает себе soft»,
  которого у супервизируемых демонов нет; v1 не усложняем (кандидат
  POST_MVP). Следствие: без привилегий лимит можно только опускать —
  значение выше hard-лимита самого супервизора даёт EPERM в `pre_exec` →
  штатный `SpawnError::Spawn` (лимит-выше-своего — ошибка конфигурации
  среды, видимая в логе).
- **`cpu-max-percent` — одно число, а не сырые quota/period.** cgroup
  `cpu.max` принимает `"<quota_us> <period_us>"`; экспортировать оба —
  протечь абстракцией ядра в TOML. Процент одного CPU (50 = половина, 200 =
  два ядра; period фиксирован 100 ms → quota = percent × 1000 µs) — один
  понятный knob. Верхней границы нет (ядро принимает любую quota).
- **Контроллеры включаются лениво и точечно**: при настройке процесса в
  `<root>/cgroup.subtree_control` дописывается только нужное (`+cpu` при
  `cpu-max-percent`, `+memory` при `memory-max-bytes`); записи инкрементальны
  и идемпотентны. Требование «предок root'а уже делегировал контроллеры»
  (его `cgroup.subtree_control`) — вне контроля супервизора: его нарушение
  проявляется как ошибка записи → политика выше. Зафиксировать
  doc-комментарием контракт делегирования.
- **cgroup-директории — служебный артефакт рантайма** (класс state-файла и
  сокета, не лог-файлов): убираются при штатном выходе демона. Механика —
  **`Drop` у `ProcessCgroup`** (best-effort `rmdir`, `debug!` на неудачу):
  один механизм структурно покрывает все пути — эпилог `run()` (явный
  `take()` перед rmdir root), prune удалённого reload'ом процесса (запись
  дропается → dir убирается) и замену хендла при респавне (новый инстанс уже
  attach-нут → `rmdir` старого хендла даёт EBUSY → no-op). Явной правки
  `prune_removed`/`apply_config`-логики не требуется. Root-директория
  убирается в эпилоге `run()` тем же best-effort `rmdir` (не пуста — чужой
  демон или орфан — debug-лог, оставить). Демон, убитый SIGKILL, оставляет
  директории — тот же класс, что осиротевший state-файл; следующий старт
  переиспользует их (mkdir EEXIST — ок, лимиты перезаписываются).
- **Респавн переиспользует ту же cgroup-директорию** (mkdir EEXIST — ок) и
  **перезаписывает лимит-файлы** — поэтому reload с изменённой секцией
  применяет новые лимиты бесплатно: рестарт идёт через общий spawn-путь.
- **Никаких `#[cfg(target_os)]`-веток.** `setrlimit` — POSIX (nix
  предоставляет `RLIMIT_NOFILE`/`AS`/`CPU` на всех Unix), cgroup-код — это
  `std::fs` по путям: компилируется везде, а на не-Linux (или Linux без
  cgroup v2) настройка cgroup падает в рантайме ошибкой IO → та же честная
  политика «процесс не стартует». POST_MVP предполагал «деградацию на
  не-Linux», но существующий error-path уже даёт честную деградацию без
  отдельной ветки кода, а молчаливый no-op противоречил бы политике «не
  врать оператору»; проект и так разрабатывается и тестируется только на
  Linux (waitid/WNOWAIT уже Linux-проверенный). Зафиксировать в
  TECHNICAL_PLAN.
- **Строгий режим для гейта тестов**: переменная окружения
  `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1` превращает пропуск в панику. Это
  ответ на риск «тест стал ошибочно пропускаться и в CI, где права есть»:
  привилегированный CI выставляет переменную, и ошибочный skip = красный
  прогон. По умолчанию (локально, песочница) — пропуск с `eprintln!`-логом
  (§9.1).

## 3. Конфиг: `src/config.rs` — секции `[process.rlimit]` и `[process.cgroup]`

По прецеденту `LogConfig`: сырая структура + `validate()`, вызываемая из
`load()`; бесплатная валидация типами (`NonZero*` отсекает нули).

```rust
/// Raw, as-parsed `[process.rlimit]` section (Этап 10): per-process resource
/// limits applied via setrlimit(2) in the pre_exec hook, soft = hard = value,
/// inherited by every descendant. `Copy` on purpose: the pre_exec closure
/// captures it by value, so applying limits allocates nothing in the forked
/// child. Without privileges limits can only be lowered: a value above the
/// supervisor's own hard limit fails the spawn with EPERM (a config/environment
/// error made visible, not hidden). `Clone`+`PartialEq` keep the Этап 8 reload
/// diff working.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct RlimitConfig {
    /// RLIMIT_NOFILE: max open file descriptors.
    #[serde(default)]
    pub nofile: Option<NonZeroU64>,
    /// RLIMIT_AS: max virtual address space, bytes.
    #[serde(rename = "as-bytes", default)]
    pub as_bytes: Option<NonZeroU64>,
    /// RLIMIT_CPU: max CPU time, seconds (SIGXCPU at the soft limit).
    #[serde(rename = "cpu-secs", default)]
    pub cpu_secs: Option<NonZeroU64>,
}

impl RlimitConfig {
    /// At least one of the three keys — an empty section is meaningless and
    /// would mask a typo in a key name (the LogConfig precedent).
    pub fn validate(&self) -> Result<(), String>;
}

/// Raw, as-parsed `[process.cgroup]` section (Этап 10): cgroup v2 limits for
/// the whole supervised tree. The supervisor creates `<cgroup-root>/<name>`,
/// writes the limit files and attaches the child right after spawn; a setup
/// failure fails the spawn of this process (no silent "no limits" degradation).
/// The cgroup is NOT used for teardown — killpg (Этап 4) remains the only
/// stop mechanism, by the user's decision.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CgroupConfig {
    /// cpu.max as a percentage of one CPU (50 = half a CPU, 200 = two CPUs),
    /// over a fixed 100 ms period. No upper bound: the kernel accepts any quota.
    #[serde(rename = "cpu-max-percent", default)]
    pub cpu_max_percent: Option<NonZeroU32>,
    /// memory.max, bytes.
    #[serde(rename = "memory-max-bytes", default)]
    pub memory_max_bytes: Option<NonZeroU64>,
}

impl CgroupConfig {
    /// At least one of the two keys, same rationale as RlimitConfig.
    pub fn validate(&self) -> Result<(), String>;
}
```

- `ProcessConfig` получает поля **последними** (после `log`):

```rust
    /// Optional per-process rlimits (Этап 10), applied in pre_exec.
    #[serde(default)]
    pub rlimit: Option<RlimitConfig>,
    /// Optional cgroup v2 limits for the whole tree (Этап 10).
    #[serde(default)]
    pub cgroup: Option<CgroupConfig>,
```

  Derived `Clone`/`PartialEq` подхватывают поля автоматически → reload-diff
  Этапа 8 видит изменение секций как «конфиг изменился» → принудительный
  рестарт существующей веткой `changed`. Проверено чтением `plan_reload`
  (`src/supervise.rs` — сравнивает `ProcessConfig` целиком); **новых веток в
  reload не нужно**, подтверждается тестом (§9.4), не декларацией.
- `load()` после существующих проверок: для каждого `proc.rlimit` и
  `proc.cgroup` — `validate()`, `Err(msg)` → `ConfigError::Invalid` с
  префиксом `process "<name>": ` (прецедент health/log). Плюс новая
  проверка **пригодности имени как имени директории**, только для процессов
  с cgroup-секцией (минимальное ужесточение — остальных не касается): имя
  не содержит `/` и NUL, не равно `.`/`..`, не пусто → иначе `Invalid`
  (`process name "<name>" is not usable as a cgroup directory name`).
  Doc-комментарий `ConfigError::Invalid` дополнить примером.
- Дефолтов у полей нет (все ключи опциональны, обязательность — «хотя бы
  один»), поэтому новых `DEFAULT_*`-констант в config.rs не появляется.
  Единственная константа этапа — дефолтный root (§4), живёт в `limits.rs`.

Механическая правка: **все литералы `ProcessConfig { … }` получают
`rlimit: None, cgroup: None`** — `grep -rn "ProcessConfig {" src tests`;
сейчас 46 вхождений (включая само объявление структуры) в 10 файлах. Плюс два
существующих теста полноты полей расширяются новыми вариантами:
`process_config_equality_notices_every_field` (`src/config.rs`) и
`plan_reload_notices_each_field` (`src/supervise.rs`) получают по паре на
каждую секцию: «None → Some» и «Some → Some с другим значением».

## 4. Новый модуль `src/limits.rs` — механика cgroup v2

`pub mod limits;` в `lib.rs`. Модуль ничего не знает о `SupervisorLoop` и
`Clock`: чистая файловая механика cgroupfs, юнит-тестируемая на tempdir
(`std::fs::write` создаёт обычные файлы там, где на настоящем cgroupfs ядро
подставляет свои, — весь код записи покрывается без привилегий, §9.2).

```rust
/// Default root of the supervisor's cgroup subtree. Overridable with
/// --cgroup-root (test isolation; deployments with a delegated subtree).
pub const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup/supervisor-rs";

pub fn default_root() -> PathBuf;

/// Formats the cgroup v2 `cpu.max` line for a percentage of one CPU over a
/// fixed 100 ms period: 50 → "50000 100000". Pure; unit-tested directly.
fn cpu_max_line(percent: u32) -> String;

/// A created per-process cgroup directory. Owning handle: `Drop` removes the
/// directory best-effort (`rmdir`; EBUSY/ENOENT → debug-log, keep going) — one
/// mechanism covers the daemon-exit epilogue, the reload prune (the entry is
/// dropped) and the respawn handle replacement (the new instance is already
/// attached, so rmdir of the same path fails EBUSY and is a no-op). A daemon
/// killed with SIGKILL leaves the directory behind — the same class as a stale
/// state file; the next start reuses it and rewrites the limits.
#[derive(Debug)]
pub struct ProcessCgroup {
    path: PathBuf,
}

/// Creates (or reuses) `<root>/<name>` and writes its limit files:
/// 1. create_dir_all(root)                        — EEXIST is fine;
/// 2. append the needed controllers to <root>/cgroup.subtree_control
///    ("+cpu" iff cpu_max_percent, "+memory" iff memory_max_bytes) —
///    incremental, idempotent writes; requires the root's *ancestor* to have
///    delegated these controllers (its own subtree_control), which is outside
///    the supervisor's control — a violation surfaces as a write error;
/// 3. create_dir(<root>/<name>)                   — EEXIST is fine (respawn
///    reuses the directory; a leftover from a SIGKILLed daemon is adopted);
/// 4. write cpu.max / memory.max for the configured keys (rewritten on every
///    spawn, which is what makes a reloaded config's new limits apply).
/// Any error aborts the whole setup: the caller fails the process spawn.
pub fn setup(root: &Path, name: &str, cfg: &CgroupConfig) -> std::io::Result<ProcessCgroup>;

impl ProcessCgroup {
    /// Writes `pid` into `<path>/cgroup.procs` — called from the PARENT right
    /// after spawn (formatting a pid allocates; allocation is banned in the
    /// forked child while the Этап 9 reader threads exist — plan §2 p.4).
    pub fn attach(&self, pid: u32) -> std::io::Result<()>;

    pub fn path(&self) -> &Path;
}

/// Best-effort removal of the root directory, for the run() epilogue: rmdir,
/// ENOENT (never created) and EBUSY/ENOTEMPTY (another daemon's dirs or an
/// escaped orphan's cgroup still inside) are debug-logged and ignored.
pub fn remove_root_best_effort(root: &Path);
```

Все записи — `std::fs::write`/`OpenOptions::append` (для `subtree_control`
append семантически не отличается от write — записи инкрементальны; выбрать
`fs::write` как более простой и зафиксировать комментарием, что cgroupfs
трактует каждую запись как команду, а не содержимое). Никакого нового крейта
и никакого `nix` тут не нужно.

## 5. `src/process.rs` — rlimit в `pre_exec`, cgroup вокруг `spawn`

### Сигнатура

```rust
/// `cgroup_root`: where to create the per-process cgroup if the config has a
/// `[process.cgroup]` section. `None` with such a section is an error (the
/// config demands isolation the runtime cannot provide) — main.rs always
/// resolves a root, so this only guards hand-built test configs.
pub fn spawn(cfg: &ProcessConfig, cgroup_root: Option<&Path>) -> Result<Spawned, SpawnError>
```

`Spawned` получает четвёртое поле:

```rust
    /// The created cgroup of this instance (Этап 10); `Some` exactly when the
    /// config has a `[process.cgroup]` section. Owning handle: dropping it
    /// best-effort-removes the directory.
    pub cgroup: Option<limits::ProcessCgroup>,
```

`SpawnError` получает два варианта (Display/Error в стиле существующих):

```rust
    /// Этап 10: creating/configuring the process's cgroup failed before spawn.
    CgroupSetup { name: String, source: std::io::Error },
    /// Этап 10: the child spawned but could not be attached to its cgroup; it
    /// has been SIGKILLed and reaped before this error was returned.
    CgroupAttach { name: String, source: std::io::Error },
```

(`(Some(section), None-root)` — это `CgroupSetup` с `io::Error` вида
`Unsupported`/сообщением «no cgroup root configured».)

### Порядок внутри `spawn` (существующий порядок Этапа 9 не ломается)

1. **cgroup setup — до всего** (решение §2 п.4: директория и лимиты готовы до
   fork): `let cgroup = match (&cfg.cgroup, cgroup_root) { … limits::setup(root,
   &cfg.name, cc)? … };` — ошибка здесь возвращается до того, как создан хоть
   один pipe или процесс.
2. Pipe-ы захвата (Этап 9) — без изменений.
3. `pre_exec` — расширяется: `setsid()` как раньше, затем rlimits.

```rust
    // Capture by value: RlimitConfig is Copy, so the closure owns plain
    // integers and the hook still allocates nothing.
    let rlimit = cfg.rlimit;
    // SAFETY: pre_exec runs in the forked child between fork and exec, where
    // only async-signal-safe calls are allowed. setsid() and setrlimit() both
    // qualify: single syscalls, no allocation, no locks — the invariant that
    // keeps this hook safe alongside the Этап 9 reader threads of the parent.
    unsafe {
        cmd.pre_exec(move || {
            nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::from)?;
            if let Some(rl) = rlimit {
                use nix::sys::resource::{setrlimit, Resource};
                for (resource, value) in [
                    (Resource::RLIMIT_NOFILE, rl.nofile),
                    (Resource::RLIMIT_AS, rl.as_bytes),
                    (Resource::RLIMIT_CPU, rl.cpu_secs),
                ] {
                    if let Some(v) = value {
                        // soft = hard = v (plan §2.1); EPERM (raising above the
                        // supervisor's own hard limit) fails the spawn loudly.
                        setrlimit(resource, v.get(), v.get())
                            .map_err(std::io::Error::from)?;
                    }
                }
            }
            Ok(())
        });
    }
```

   Ошибка `setrlimit` всплывает как ошибка `cmd.spawn()` → существующий
   `SpawnError::Spawn` — нового error-пути для rlimit нет вообще.
4. `cmd.spawn()` — как раньше.
5. **attach — сразу после spawn, в родителе**: `cgroup.attach(child.id())`;
   ошибка → `child.kill()` + `child.wait()` (реапнуть обязательно — прецедент
   exec-пробы Этапа 7) → `SpawnError::CgroupAttach`. Именно `child.kill()`
   (SIGKILL по pid), не `signal_group`: `setsid` в ребёнке мог ещё не
   выполниться, pgid может не существовать; успевший форкнуться потомок в
   этом суб-миллисекундном окне осиротеет — корнер error-пути, зафиксировать
   комментарием (тот же класс, что окно «сигнал до setsid» у
   `signal_group`).

Окно «exec → attach»: процесс уже исполняется вне cgroup микросекунды до
записи pid; fork, сделанный в этом окне, остаётся вне cgroup навсегда.
Известное ограничение (§12), альтернатива `clone3(CLONE_INTO_CGROUP)`
отклонена (§2 п.4). Обязательный комментарий у attach-вызова.

### Правка вызовов

`process::spawn` вызывается из: `src/supervise.rs` (`start_instance`,
юнит-тест give-up), `src/process.rs` (юнит-тесты), `tests/spawn.rs`. Все
существующие вызовы получают `, None` механически (лимит-секций там нет —
поведение не меняется); supervise — §7.

## 6. CLI и main: флаг `--cgroup-root`

- `src/cli.rs`: `Command::Run` получает поле `cgroup_root: Option<PathBuf>`;
  разбор флага — по образцу `--state-file` (значение обязательно, последний
  побеждает); применимость — **только `run`**: `--cgroup-root` на
  `status`/`start`/`stop`/`restart` — usage-ошибка (прецедент
  `--state-file` на `stop`). `usage()` дополняется строкой с дефолтом.
- `src/main.rs`: `run()` получает параметр; резолв —
  `cgroup_root.unwrap_or_else(limits::default_root)`; передаётся в
  конструктор (§7) **всегда** (`Some(root)`) — создание чего-либо на диске
  ленивое, при отсутствии cgroup-секций демон к cgroupfs не прикасается.
  Никакой pre-flight-проверки root'а в main нет: ошибка настройки —
  per-process, в момент спавна (политика §2.1).

Существующие cli-тесты с литералом `Command::Run { … }` получают
`cgroup_root: None` механически.

## 7. Интеграция в `SupervisorLoop` (`src/supervise.rs`) — перечень правок

Правок семантики супервизии нет; все изменения — протаскивание
`cgroup_root` до `process::spawn` и владение хендлом. Поимённо (это
одновременно граница разрешённого):

1. Поле `cgroup_root: Option<PathBuf>` на `SupervisorLoop`.
2. Конструктор: `pub fn new_with_cgroup_root(configs, clock, cgroup_root:
   Option<PathBuf>) -> Self` — полный; существующий `new(configs, clock)`
   становится тонкой обёрткой с `None` (все существующие тесты и их
   вызовы `new` не меняются). main.rs зовёт полный.
3. `start_instance(config: &ProcessConfig, cgroup_root: Option<&Path>)` —
   передаёт root в `process::spawn`, возвращает тройку
   `(Running, Option<LogState>, Option<limits::ProcessCgroup>)`. Логика
   reader-threads Этапа 9 внутри — без изменений; на её error-пути
   (`kill_and_reap_instance`) cgroup-хендл дропается возвратом `Err` →
   `Drop` убирает уже пустую директорию — отдельного кода не нужно
   (зафиксировать комментарием).
4. `start_supervised(config, now, cgroup_root)` + поле `cgroup` в литерале
   `Supervised`.
5. `Supervised` получает поле `cgroup: Option<limits::ProcessCgroup>`
   (после `log`; instance-derived, как `LogState`).
6. `tick()`, респавн-ветка: `Ok((running, log_state, cgroup))` и строка
   `proc.cgroup = cgroup;` (старый хендл дропается присваиванием — новый
   инстанс уже attach-нут, rmdir того же пути даёт EBUSY, no-op; см. §2.1).
   Единственная правка `tick()`.
7. Вызовы `start_supervised` в `new_…` и в added-пути `apply_config`
   получают третий аргумент — **механическое протаскивание сигнатуры, ноль
   изменений reload-логики** (diff/классификация/prune не трогаются; это
   уточнение к границе «reload — ноль правок»: одна строка вызова меняется,
   семантика — нет).
8. Эпилог `run()` (рядом с удалением state-файла и сокета): если
   `cgroup_root` задан — `for proc { drop(proc.cgroup.take()) }` (все дети к
   этому моменту реапнуты, директории пусты — кроме случая живого орфана,
   тогда EBUSY → debug и директория остаётся), затем
   `limits::remove_root_best_effort(root)`.

**НЕ меняются**: `poll_child`, `begin_shutdown`, `escalate_to_kill`,
`handle_poll_error` (cgroup-хендл give-up-процесса остаётся до эпилога —
осознанно: там может жить орфан), `join_log_readers`, `plan_reload`,
`apply_config`-логика, `prune_removed`, `handle_command`,
`run_due_health_check`, весь `signal.rs`, `control.rs`, `health.rs`,
`state.rs`, `clock.rs`, `logs.rs`.

## 8. Наблюдаемость и state-файл — решения

- **Схема state-файла НЕ меняется, `version` остаётся 1** (прецедент
  health/log): лимиты и cgroup-путь в снапшот и `status` не попадают —
  лимиты наблюдаемы самим cgroupfs (`cat <root>/<name>/memory.max`) и
  `/proc/<pid>/limits`.
- Логи `tracing`: `info!` один раз на созданный cgroup процесса (путь) при
  спавне — это событие настройки, не рутина; `error!` — на
  `CgroupSetup`/`CgroupAttach` (через существующий лог «failed to spawn
  process»); `debug!` — на rmdir-неудачи в `Drop`/эпилоге. Новых каналов
  нет.
- `handle_command`/control-socket — ноль правок: лимиты — не команда сокета.

## 9. Тест-план (поимённо)

Общие правила SKILL.md обязательны: стабы через `/usr/bin/env sh -c`,
готовность по содержимому маркер-файла (не по существованию), поллинг с
дедлайном, изоляция `--state-file`/`--control-socket` в tempdir у каждого
e2e-запуска, новые тесты на процессы прогнать 10 раз подряд.

Ключевой наблюдательный приём этапа: **rlimit-ы читаются `ulimit`-builtin'ом
самого стаба** (`sh -c 'ulimit -n > marker'`) — детерминированно, без
привилегий и без умирающих процессов; `ulimit -n` печатает soft-лимит fd,
`ulimit -v` — адресное пространство в КиБ (конфиг-значение брать кратным
1024), `ulimit -t` — CPU-секунды.

### 9.1 Паттерн гейтинга cgroup-тестов на привилегии — ОБЯЗАТЕЛЬНЫЙ, первый в проекте

Любой тест, реально создающий cgroup-поддерево, обязан сначала выяснить,
может ли он это сделать здесь, и **явно, громко пропуститься**, если нет —
не падать, не виснуть, не притворяться прошедшим. На текущей dev-машине
cgroup2 смонтирован, но uid 1000 не может писать в `/sys/fs/cgroup` — тесты
там обязаны пропускаться; в привилегированном окружении — обязаны бежать.
Хелпер (копируется в `tests/limits.rs` и `tests/limits_e2e.rs` — осознанное
дублирование, конвенция проекта):

```rust
/// Finds a writable cgroup-v2 root for this test, or None — in which case the
/// test MUST return early ("conditionally skipped", the first such class in
/// this project). Never silent: the reason goes to stderr (visible under
/// `cargo test -- --nocapture` and in the 10× loop), and setting
/// SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1 (privileged CI does) turns every skip
/// into a panic — so a bug that made the gate skip erroneously where
/// privileges DO exist cannot silently mask a regression.
///
/// Discovery: read our own cgroup from /proc/self/cgroup ("0::<path>") and walk
/// from /sys/fs/cgroup/<path> up towards /sys/fs/cgroup, trying at each
/// ancestor A to: (1) mkdir A/suptest-<pid>-<tag>; (2) ensure "cpu memory" are
/// available inside it (its cgroup.controllers), enabling them in A's
/// cgroup.subtree_control if needed (allowed only while A has no direct member
/// processes — the "no internal processes" rule; failure → clean up, next
/// ancestor). Under systemd user delegation this succeeds somewhere below
/// user@<uid>.service; without delegation nothing is writable and we skip.
/// The returned directory is passed to the daemon as --cgroup-root (e2e) or
/// via new_with_cgroup_root (in-process); the test removes it in its epilogue
/// (best-effort: the daemon's own cleanup usually already has).
fn cgroup_root_for_test(tag: &str) -> Option<std::path::PathBuf> {
    match try_find_writable_cgroup_root(tag) {
        Ok(root) => Some(root),
        Err(reason) => {
            if std::env::var_os("SUPERVISOR_RS_REQUIRE_CGROUP_TESTS").is_some() {
                panic!(
                    "cgroup test cannot run here ({reason}), but \
                     SUPERVISOR_RS_REQUIRE_CGROUP_TESTS demands it"
                );
            }
            eprintln!("CGROUP-TEST SKIPPED ({tag}): {reason}");
            None
        }
    }
}
```

Использование в каждом гейтящемся тесте — единообразное и первой строкой:

```rust
#[test]
fn cgroup_attaches_pid_and_writes_limits() {
    let Some(root) = cgroup_root_for_test("attach") else {
        return; // skipped LOUDLY inside the helper; never a silent pass
    };
    // ... the actual test body ...
}
```

Обоснование, зафиксировать в преамбуле `tests/limits.rs`: (а) гейт проверяет
**ровно предусловие продукта** (та же цепочка mkdir/subtree_control, что у
`limits::setup`) — «тест бежит ⟺ демон здесь смог бы»; (б) пропуск не тихий:
stderr-маркер `CGROUP-TEST SKIPPED` + строгий режим для сред, где права
гарантированы; (в) пробная директория одноразовая (`suptest-<pid>-<tag>`),
убирается самим тестом — параллельные тесты не делят путь (прецедент
изоляции `--state-file`). Голое `test -w /sys/fs/cgroup` как гейт
отвергнуто: writability верхнего уровня не эквивалентна возможности создать
поддерево с контроллерами (делегирование может быть на глубине), проверять
надо действие, а не право.

### 9.2 Юниты `src/config.rs` (mod tests)

- `parses_rlimit_section_full` — все три ключа.
- `parses_rlimit_with_single_key` — только `nofile`; остальные `None`;
  `validate()` ок.
- `parses_cgroup_section_full` и `parses_cgroup_with_only_memory`.
- `rejects_empty_rlimit_section` / `rejects_empty_cgroup_section` — через
  `load()`: `ConfigError::Invalid`, Display содержит `process "web"` и имя
  недостающих ключей.
- `rejects_zero_limit_values` — цикл по `nofile = 0`, `as-bytes = 0`,
  `cpu-secs = 0`, `cpu-max-percent = 0`, `memory-max-bytes = 0` — ошибка
  парсинга (`NonZero*`), образец
  `rejects_zero_port_interval_timeout_and_threshold`.
- `rejects_snake_case_limit_keys` — `as_bytes`/`cpu_max_percent` молча
  игнорируются парсером → секция становится пустой → `load()` отвергает
  (пин kebab-case контракта, образец `rejects_snake_case_stop_grace`).
- `rejects_cgroup_process_name_unfit_for_directory` — имена `a/b`, `..` при
  наличии cgroup-секции → `Invalid`; те же имена **без** секции проходят
  (ужесточение точечное).
- `process_without_limit_sections_parses` — оба поля `None`.
- Дополнение `process_config_equality_notices_every_field` — четыре новых
  мутации: `rlimit` None→Some и Some→другой `nofile`; `cgroup` None→Some и
  Some→другой `memory-max-bytes`.

### 9.3 Юниты `src/limits.rs` (mod tests) — tempdir как фейковый cgroupfs

Приём: на tempdir `fs::write` создаёт обычные файлы там, где ядро подставило
бы свои, — весь код записи (`setup`/`attach`/`Drop`) тестируется без
привилегий; ядерная семантика остаётся гейтящимся тестам §9.4/§9.5.

- `cpu_max_line_formats_percent` — 50 → `"50000 100000"`, 250 →
  `"250000 100000"`, 1 → `"1000 100000"`.
- `setup_creates_dirs_and_writes_limit_files` — root и `<name>` созданы;
  `cpu.max`/`memory.max` содержат ожидаемое.
- `setup_enables_only_needed_controllers` — только memory-лимит →
  `subtree_control` получил `+memory` и не получил `+cpu`.
- `setup_reuses_existing_dir_and_rewrites_limits` — второй `setup` по тому
  же пути с другим значением проходит и перезаписывает файл (контракт
  респавна/reload).
- `attach_writes_pid_to_cgroup_procs`.
- `drop_removes_empty_cgroup_dir` — после дропа директории нет.
- `drop_keeps_nonempty_cgroup_dir` — в директорию положен файл → дроп не
  паникует, директория на месте (best-effort контракт).
- `remove_root_ignores_missing_and_nonempty`.

### 9.4 Юниты `src/process.rs` (mod tests)

- `spawn_with_rlimit_applies_limits_via_ulimit` — конфиг: `nofile` (например
  123), `as_bytes` (кратно 1024, например 512 MiB), `cpu_secs` (например
  111); стаб `sh -c 'echo "$(ulimit -n) $(ulimit -v) $(ulimit -t)" > marker'`;
  поллинг содержимого маркера → ровно `123 524288 111` (`ulimit -v` — в
  КиБ). Один тест на все три поля — они применяются одним циклом.
- `spawn_without_rlimit_leaves_limits_inherited` — стаб пишет `ulimit -n`;
  значение равно текущему soft-лимиту теста (прочитать через
  `nix::sys::resource::getrlimit` — фича уже включена), не 123.
- `rlimit_cpu_kills_spinning_child` — `cpu_secs = 1`, стаб
  `sh -c 'while :; do :; done'`; `child.wait()` с реальным дедлайном (~10 с
  запас) → статус «убит сигналом» (SIGXCPU или SIGKILL — ядро может добить
  жёстко, ассертить `signaled`, не конкретный номер). Единственный
  реально-временной тест (~1 с CPU) — класса существующих shutdown-тестов.
- `spawn_with_cgroup_without_root_is_an_error` — `spawn(cfg_with_cgroup,
  None)` → `SpawnError::CgroupSetup`; ничего не заспавнено.
- `spawn_with_unwritable_cgroup_root_fails_before_fork` — root указывает
  внутрь обычного *файла* (`<tempfile>/sub` → ENOTDIR) → `CgroupSetup`;
  негативный путь без привилегий и без гейта.

### 9.5 In-process тесты — новый файл `tests/limits.rs`

`FakeClock`, `tick()` руками; хелперы `cfg`/`sh`/`wait_for_pid`/`is_alive`/
`tick_until_reaped`/`tick_until_new_pid` скопировать из `tests/logs.rs`
(осознанное дублирование — пометить в преамбуле; там же — обоснование
гейт-паттерна из §9.1). Новые хелперы: `with_rlimit(...)`/`with_cgroup(...)`
(секции через `toml::from_str`, образец `with_exec_health`),
`wait_for_marker(path, pred, timeout)` (реальный дедлайн — стаб пишет из
другого процесса асинхронно к `FakeClock`; тот же двухрежимный принцип, что
в Этапе 9).

Без гейта (rlimit и негативные пути привилегий не требуют):

- `rlimit_marker_shows_configured_limits` — процесс через
  `SupervisorLoop::new`, маркер с `ulimit`-числами, `wait_for_marker`.
- `restarted_instance_keeps_rlimits` — `handle_command(Restart)` →
  `tick_until_new_pid` → маркер нового инстанса несёт те же значения.
- `changed_rlimit_forces_restart_with_new_limits` — §7 п.7 и reload-контракт:
  `apply_config` с изменённым только `nofile` → процесс `stopping`, после
  `tick_until_new_pid` `restart_count == 1` (адресация по имени — правило
  Этапа 8) → маркер нового инстанса несёт новое значение. Доказывает «reload
  видит новые секции без единой правки reload-кода».
- `cgroup_spawn_failure_is_per_process` — политика §2.1 без привилегий:
  `new_with_cgroup_root(&[c_bad_cgroup, c_plain], clock,
  Some(<путь-внутри-файла>))` → `had_start_errors() == true`, снапшот
  содержит только `plain` (по имени), `plain` доходит до `running`.

С гейтом §9.1:

- `cgroup_attaches_pid_and_writes_limits` — root от хелпера; процесс с
  memory+cpu лимитами; после `wait_for_pid`: `<root>/<name>/cgroup.procs`
  содержит pid (поллинг с реальным дедлайном — attach идёт после спавна),
  `memory.max`/`cpu.max` — сконфигурированные значения (на настоящем
  cgroupfs ядро нормализует запись — сравнивать разобранные числа, не сырую
  строку).
- `respawned_instance_lands_in_same_cgroup` — рестарт → новый pid в том же
  `cgroup.procs`, директория одна.
- `stopped_process_leaves_empty_cgroup_dir` — оператор `stop` →
  `tick_until_reaped` → `cgroup.procs` пуст (поллингом: перемещение из
  cgroup асинхронно смерти), директория ещё существует (убирается только в
  эпилоге демона).

Ядерная семантика enforcement'а (OOM-kill по `memory.max`, троттлинг по
`cpu.max`) тестами **не** ассертится — осознанно: контракт супервизора —
корректное размещение и значения файлов, а исполнение лимита — контракт
ядра; OOM-стаб и тайминговые ассерты троттлинга — источник флейков.
Enforcement RLIMIT_CPU покрыт (`rlimit_cpu_kills_spinning_child`), потому
что там наблюдаемое — сигнал, а не тайминг. Зафиксировать в §12.

### 9.6 e2e на реальном бинарнике — новый файл `tests/limits_e2e.rs`

Хелперы `write_config`/`start_supervisor`/`wait_for_state`/
`wait_with_timeout`/`wait_until_gone` скопировать из `tests/logs_e2e.rs`;
изоляция `--state-file` и `--control-socket` обязательна.

- `rlimit_applied_end_to_end` (без гейта) — конфиг с `[process.rlimit]`,
  маркер с `ulimit`-числами в tempdir; `wait_for_state` до `running`;
  поллинг маркера; SIGTERM → exit 0.
- `invalid_limits_section_fails_run_with_config_error` (без гейта) — пустая
  `[process.rlimit]`: exit 1, лог содержит текст ошибки, state-файл не
  появился (образец `invalid_log_section_fails_run_with_config_error`).
- `cgroup_end_to_end_created_attached_and_removed` (с гейтом §9.1) —
  **критерий приёмки целиком**: root от хелпера передан `--cgroup-root`;
  `wait_for_state` до `running`; pid из снапшота присутствует в
  `<root>/<name>/cgroup.procs`; `memory.max` несёт значение; SIGTERM →
  exit 0 → `<root>/<name>` удалена и сам root удалён (артефакт рантайма —
  контракт §2.1); state-файл и сокет удалены (существующий контракт цел).

### 9.7 Существующие тесты

Все 247 обязаны остаться зелёными. Допустимые правки — только механические:
`rlimit: None, cgroup: None` в литералах `ProcessConfig` (§3), `, None` у
вызовов `process::spawn` (§5), `cgroup: None` в литерале `Supervised`
юнит-теста give-up и `cgroup_root: None` в литералах `Command::Run`
cli-тестов (§6), плюс аддитивные варианты двух тестов полноты полей (§3).
Поведенческих правок и правок ожиданий — ноль; `tests/signals.rs`,
`tests/tree.rs`, `tests/control.rs`, `tests/status.rs`, `tests/health*.rs`,
`tests/reload*.rs`, `tests/logs*.rs` не меняются вовсе (кроме — ничего).
Проверить прогоном.

## 10. Порядок работ (шаг = один связный коммит)

1. **Cargo.toml + конфиг**: фича `"resource"` у `nix`; `RlimitConfig`/
   `CgroupConfig`, `validate()`, проверки в `load()` (включая пригодность
   имени), `rlimit: None, cgroup: None` во всех литералах; юниты §9.2.
   Полный зелёный прогон (правка литералов затрагивает все крейты).
2. **`src/limits.rs`** (+`lib.rs`): `setup`/`ProcessCgroup`/`attach`/`Drop`/
   `remove_root_best_effort`/`cpu_max_line`; юниты §9.3 на tempdir.
3. **`src/process.rs`**: rlimit в `pre_exec`, cgroup setup/attach вокруг
   спавна, `Spawned.cgroup`, варианты `SpawnError`, механические `, None` у
   вызовов; юниты §9.4. supervise на этом шаге передаёт `None` и дропает
   `cgroup` из `Spawned` (безопасно — секций никто не задаёт); полный
   зелёный прогон.
4. **Интеграция**: `src/supervise.rs` (ровно перечень §7), `src/cli.rs` +
   `src/main.rs` (§6); in-process тесты §9.5 (`tests/limits.rs`).
5. **e2e** (`tests/limits_e2e.rs`, §9.6). Прогнать 10 раз подряд с видимым
   стдерром, чтобы посчитать skip-маркеры глазами:
   `for i in $(seq 10); do cargo test --test limits --test limits_e2e -- --nocapture || break; done`.
6. **Доки** (§11).

После каждого шага: `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test`.

## 11. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`: новый раздел «Этап 10 — Resource limits» по
  фактической реализации: решения пользователя из §2 (явно, как принятые —
  особенно «cgroup не для teardown, killpg остаётся единственным механизмом
  остановки»); почему две секции, а не одна; почему attach в родителе, а не
  в `pre_exec` (аллокация против fork-safety Этапа 9), и отклонённый
  `clone3(CLONE_INTO_CGROUP)`; политика «setup failed → процесс не
  стартует» с обоснованием; ленивое включение контроллеров и контракт
  делегирования предка; жизненный цикл cgroup-директорий (артефакт рантайма,
  `Drop`, эпилог, поведение после SIGKILL демона); отсутствие
  `#[cfg(target_os)]` и честная рантайм-деградация на не-Linux; паттерн
  гейтинга тестов (первый класс conditionally-skipped тестов, строгий режим
  через `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS`); схема state-файла не изменена
  (version 1); известные ограничения (§12). «Модульная структура» — добавить
  `limits.rs`.
- `docs/POST_MVP_PLAN.md`: раздел «cgroups / resource limits» пометить
  реализованным в Этапе 10 по образцу пунктов Этапов 6–9, с оговорками,
  остающимися кандидатами: **`cgroup.kill` как teardown строже `killpg`
  (осознанно НЕ сделан — решение пользователя)**; раздельные soft/hard у
  rlimit; другие контроллеры (io, pids); `cpu.max` с настраиваемым period;
  `clone3(CLONE_INTO_CGROUP)` для атомарного помещения; видимость лимитов в
  `status`. Фразу «потребует … деградации на не-Linux» снять с причиной:
  деградация есть, но рантаймовая и громкая (ошибка спавна), а не
  компайл-таймовая ветка.
- `docs/PLAN.md`: добавить Этап 10 в список; финальный абзац дополнить.
- `README.md`: строка статуса этапов; абзац про лимиты с примером обеих
  секций и флагом `--cgroup-root` (упомянуть требование делегирования).
- `examples/supervisor.toml`: закомментированные примеры обеих секций с
  пояснениями (прецедент health/log).
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`, в «грабли» (по
  фактическим находкам; ожидаемые кандидаты): pid в `cgroup.procs` пишет
  родитель после `spawn()`, не `pre_exec` (форматирование pid — аллокация в
  форкнутом ребёнке при живых потоках); `setrlimit` в `pre_exec` безопасен
  (один syscall, без аллокаций) — расширять существующий хук, не заводить
  второй; tempdir как фейковый cgroupfs для юнитов записи; `ulimit`-builtin
  стаба как детерминированный наблюдатель rlimit-ов (`-v` — в КиБ!);
  гейт-паттерн cgroup-тестов (громкий skip + строгий env-режим; проверять
  действие, а не `test -w`); на реальном cgroupfs сравнивать разобранные
  значения лимит-файлов, а не сырые строки (ядро нормализует).
- `grep -rn "TODO(Этап 10)" src/` пуст; финальную редакцию формулировок
  делает основная сессия — здесь достаточно фактической точности.

## 12. Границы — что НЕ трогать, и известные ограничения

Не трогать:

- **Механика process groups / teardown Этапа 4** — вовсе (решение
  пользователя §2 п.2): `poll_child` (peek → sweep → reap), `begin_shutdown`,
  `escalate_to_kill`, `signal_group`, backoff, бюджет poll-ошибок. В
  `handle_poll_error` — ноль вставок (в отличие от Этапа 9).
- `tick()` — ровно одна правка: тройка + `proc.cgroup = cgroup;` в
  респавн-ветке (§7 п.6).
- Reload Этапа 8: `plan_reload` / логика `apply_config` / `prune_removed` —
  ноль правок; единственное касание — третий аргумент у вызова
  `start_supervised` (§7 п.7), семантика нетронута. Подтверждается тестом
  `changed_rlimit_forces_restart_with_new_limits`, не декларацией.
- `src/signal.rs`, `src/control.rs`, `handle_command`, `src/health.rs`,
  `src/state.rs`, `src/clock.rs`, `src/logs.rs` — без правок.
- Схема state-файла (`version = 1`) — без новых полей.
- `Cargo.toml` — ровно одна правка: `"resource"` в фичах `nix`. Никаких
  новых крейтов; Docker Compose не добавлять.
- `src/cli.rs` / `src/main.rs` — правки разрешены (единственный такой этап),
  но строго в объёме §6: один флаг, его применимость, usage-текст, резолв
  дефолта и передача в конструктор.
- Известные ограничения Этапов 4–9 — принятое поведение, не «чинить».
  Отдельно: «сбежавший setsid-орфан» НЕ чинится через cgroup (он остаётся в
  cgroup и был бы достижим для `cgroup.kill` — но это отклонено решением
  пользователя; кандидат POST_MVP).

Известные ограничения Этапа 10 (записать в TECHNICAL_PLAN как осознанные):

1. Окно «exec → attach»: первые микросекунды процесс исполняется вне cgroup;
   fork, сделанный в этом окне, остаётся вне cgroup навсегда. Атомарная
   альтернатива `clone3(CLONE_INTO_CGROUP)` отклонена (§2 п.4).
2. cgroup не участвует в teardown: `cgroup.kill` не используется, сбежавший
   орфан переживает остановку (как в Этапе 4) и, оставаясь в cgroup, мешает
   `rmdir` (директория остаётся с debug-логом).
3. soft = hard у rlimit-ов; поднять лимит выше hard-лимита самого
   супервизора без привилегий нельзя — EPERM → ошибка спавна (видимая, не
   тихая).
4. cgroup требует v2, писабельного (делегированного) root'а и контроллеров,
   включённых предком; иначе — ошибка спавна процесса, не тихая деградация
   (политика §2.1). На не-Linux — тот же путь; компайл-таймовых веток нет.
5. Enforcement cgroup-лимитов (OOM-kill, троттлинг) не ассертится тестами —
   контракт супервизора: размещение pid и значения файлов; поведение ядра
   при OOM наблюдаемо оператором как signal-exit → `Failure` → обычная
   restart-policy (нового кода не требует).
6. Демон, убитый SIGKILL, оставляет cgroup-директории (класс осиротевшего
   state-файла); следующий старт переиспользует их и перезаписывает лимиты.
7. Контроллеры, дописанные в `<root>/cgroup.subtree_control`, обратно не
   выключаются.
8. Лимиты не видны в `status`/state-файле; изменение секций через reload —
   рестарт процесса (как любое изменение конфига).
9. Один uid — один default root: два демона без явных `--cgroup-root` делят
   `/sys/fs/cgroup/supervisor-rs`; коллизий директорий нет, пока уникальны
   имена процессов между конфигами (не проверяется — класс существующего
   ограничения дефолтных путей state/socket).
10. `cpu-max-percent` с фиксированным period 100 ms; сырые quota/period не
    экспортируются (кандидат POST_MVP).

## 13. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` —
   зелёные в непривилегированной среде (cgroup-тесты видимо пропущены);
   `cargo test --test limits --test limits_e2e -- --nocapture` — зелёные
   10 прогонов подряд, skip-маркеры `CGROUP-TEST SKIPPED` присутствуют в
   стдерре там, где прав нет.
2. Все 247 существующих тестов проходят; их единственные правки —
   механические из §9.7.
3. rlimit-контракт закрыт `spawn_with_rlimit_applies_limits_via_ulimit`,
   `rlimit_applied_end_to_end` и enforcement'ом
   `rlimit_cpu_kills_spinning_child`; сохранность через рестарт —
   `restarted_instance_keeps_rlimits`.
4. cgroup-контракт закрыт юнитами §9.3 (вся запись — без привилегий) и
   гейтящимися `cgroup_attaches_pid_and_writes_limits` /
   `cgroup_end_to_end_created_attached_and_removed` (там, где среда
   позволяет); политика «setup failed → процесс не стартует, соседи целы» —
   негативными `cgroup_spawn_failure_is_per_process` и
   `spawn_with_unwritable_cgroup_root_fails_before_fork` (без привилегий).
5. Гейт-паттерн реализован по §9.1: громкий skip + строгий режим
   `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1` (проверить руками: с выставленной
   переменной в непривилегированной среде cgroup-тесты падают паникой с
   причиной).
6. Взаимодействие с reload закрыто
   `changed_rlimit_forces_restart_with_new_limits` — при нетронутой
   reload-логике (§7 п.7).
7. `git diff --stat`: `src/signal.rs`, `src/control.rs`, `src/health.rs`,
   `src/state.rs`, `src/clock.rs`, `src/logs.rs` не изменены; в
   `Cargo.toml` — только фича `"resource"`; функции teardown Этапа 4 —
   без диффа.
8. Доки из §11 актуализированы, включая пометку раздела POST_MVP
   реализованным с оговоркой «`cgroup.kill`-teardown осознанно не сделан —
   решение пользователя» и фиксацию решений пользователя в TECHNICAL_PLAN.
