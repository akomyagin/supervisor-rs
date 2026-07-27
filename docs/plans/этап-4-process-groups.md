# План Этапа 4 — Process groups + деревья процессов

Ветка: `этап-4/process-groups`. Исполнителю: никаких git-коммитов — commit/push/PR
делает основная сессия. Перед началом прочитать
`.claude/skills/rust-process-supervisor-dev/SKILL.md` (конвенции и уроки
тестирования) и преамбулы `tests/signals.rs` / `tests/shutdown.rs`.

## 1. Цель и критерий приёмки

Каждый супервизируемый процесс — лидер собственной process-группы. Завершение
супервизора гасит ВСЁ дерево потомков (включая внуков, которых форкнул сам
ребёнок) с эскалацией SIGTERM → grace → SIGKILL. После завершения супервизора не
остаётся ни одного живого потомка дерева; зомби не накапливаются.

Что уже есть (Этапы 1–3) и не переделывается: парсинг конфига (`config.rs`),
restart policy + backoff на инъектируемых часах (`supervise.rs`, `clock.rs`),
перехват SIGTERM/SIGINT через `AtomicI32` (`signal.rs`), режим shutdown с
подавлением рестартов, exit-коды (usage → 2, config → 1, start errors → 1,
иначе 0 — в том числе при остановке сигналом).

## 2. Центральный инвариант: зомби-лидер пиннит pgid

**Решение принято пользователем, не пересматривать.** Реапинг — гибрид, а не
полный переход на ручной `nix::waitpid` (это осознанное отклонение от буквы
TECHNICAL_PLAN, зафиксировать в доках, см. §10):

- Владельцем pid остаётся `std::process::Child` — реапит только он, через
  `try_wait()`. Двойного реапинга нет по построению.
- Обнаружение выхода — неразрушающий peek: `waitid(P_PID, pid,
  WEXITED | WNOHANG | WNOWAIT)`. Он сообщает статус, **не реапя** процесс.

Зачем: пока лидер группы не реапнут, он существует как зомби, а pgid существующей
группы ядро переиспользовать не может. Значит `killpg(pgid, ...)` в этом окне
гарантированно бьёт по нашей группе, а не по чужому процессу с переиспользованным
id. Отсюда **обязательный порядок** в каждой точке, где лидер вышел:

```
1. peek: лидер вышел, но НЕ реапнут (зомби пиннит pgid)
2. killpg(pgid, SIGKILL) — добить остаток группы (внуков)
3. только теперь Child::try_wait() — реапнуть лидера, pgid освобождается
```

Нарушить порядок (реапнуть до killpg) — открыть TOCTOU-окно, в котором ОС отдаёт
pgid новой группе и мы убиваем невиновных.

Почему полный переход на `waitpid` не даёт ничего: `waitpid` реапит только
**прямых** детей. Внуки при смерти лидера переходят к `init`/subreaper, а не к
супервизору — их зомби реапит init, супервизор их реапить не может в принципе.
Единственный процесс, который обязан реапить супервизор, — сам лидер, и с этим
`Child::try_wait()` справляется. Гибрид берёт от `nix` ровно то, чего нет у
`std`: неразрушающий peek и `killpg`.

**Грабля Linux (не наступить):** `WNOWAIT` валиден только для `waitid(2)`.
`waitpid(2)`/`wait4(2)` вернёт `EINVAL`, если передать ему `WNOWAIT`. Поэтому
peek делается через `nix::sys::wait::{waitid, Id}` c
`WaitPidFlag::WEXITED | WNOHANG | WNOWAIT`, а не через `waitpid`. При отсутствии
изменений `waitid` с `WNOHANG` возвращает `WaitStatus::StillAlive`. Peek
идемпотентен: пока лидера не реапнули, повторный peek снова вернёт статус.

**Грабля killpg-liveness:** зомби — всё ещё существующий процесс, поэтому
`killpg(pgid, 0)` для группы, где остался только зомби-лидер, возвращает успех, а
не `ESRCH`. Отличить «в группе живые внуки» от «остался один зомби» дёшево
нельзя. Следствия: (а) примитив «живость группы» в продакшен-код **не вводим** —
его ответ не actionable и провоцирует ложную логику ожидания «опустения группы»;
(б) машина состояний ждёт только **лидера**, а не группу (см. §4).

**Проверено эмпирически (основная сессия, до начала кодинга).** Все три
утверждения выше подтверждены прогоном C-пробы на этой машине (Linux 6.8):

```
waitpid(pid, &st, WNOHANG|WNOWAIT)      -> -1, errno=22 EINVAL
waitid(P_PID, WEXITED|WNOHANG|WNOWAIT)  -> 0, si_status=7  (дважды подряд,
                                            идемпотентно, процесс не реапнут)
killpg(группа-с-одним-зомби-лидером, 0) -> 0  (не ESRCH)
killpg(та же группа, 0) ПОСЛЕ реапинга лидера -> 0  (тоже не ESRCH!)
```

Последняя строка — **дополнительная грабля, которую надо знать тестам**: группа
не исчезает и после реапинга лидера, потому что убитые внуки сами висят зомби,
пока их не реапнет init. То есть «группа мертва» не наблюдаемо через `killpg`
вообще ни в один момент — ещё один довод за (а). А главное — см. §8.1: смерть
внука наступает **раньше**, чем `kill(pid, 0)` начинает отдавать `ESRCH`, потому
что между ними стоит асинхронный реапинг со стороны init.

## 3. Постановка группы при spawn: `setsid()` в `pre_exec`

Выбор: `setsid()`, не `setpgid(0, 0)`. Обоснование:

- Оба вызова async-signal-safe и допустимы в `pre_exec`. Но `setsid()` в этой
  позиции **не может отказать**: после `fork` ребёнок никогда не лидер группы
  (его pid ≠ унаследованный pgid), а это единственное условие ошибки `setsid`.
- `setsid()` дополнительно отвязывает ребёнка от управляющего терминала. Это
  устраняет ловушку ручной проверки из Этапа 3: Ctrl-C в шелле доставлял SIGINT
  всей foreground-группе, т.е. детям напрямую, и ручная проверка ничего не
  доказывала. После `setsid` терминальные сигналы до детей не доходят вообще —
  единственный путь это форвардинг супервизора, поведение детерминировано, а
  ручной Ctrl-C становится честным тестом. С `setpgid(0,0)` дети остались бы в
  сессии с доступом к терминалу (SIGTTIN при чтении tty и т.п.) — лишняя
  вариативность без выгоды.
- Цена: у детей нет управляющего терминала (открыть `/dev/tty` нельзя). Для
  супервизируемых демонов это норма (так делает и systemd).

Реализация в `process::spawn` — ровно как в SKILL.md:

```rust
use std::os::unix::process::CommandExt;
// SAFETY: pre_exec runs in the forked child before exec; only
// async-signal-safe calls are allowed. setsid() qualifies: no allocation,
// no locks, a single syscall.
unsafe {
    cmd.pre_exec(|| nix::unistd::setsid().map(|_| ()).map_err(std::io::Error::from));
}
```

Сигнатура `spawn(cfg: &ProcessConfig) -> Result<Child, SpawnError>` не меняется.

**Где хранить pgid.** В `Supervised` появляется поле `pgid: Option<Pid>`,
заполняемое сразу после успешного spawn (`Pid::from_raw(child.id() as i32)` —
после `setsid` pgid лидера равен его pid) и обнуляемое **только после реапинга**
лидера. Нельзя вычислять pgid из `child.id()` задним числом: после реапинга
`Child` потреблён и pid уже мог быть переиспользован. `Some(pgid)` — структурная
запись инварианта «killpg по этому id безопасен» (лидер жив или зомби),
`None` — «группа больше не наша, трогать нельзя».

**Известное микроокно.** `killpg(pgid, sig)` до того, как `pre_exec` ребёнка
успел выполнить `setsid` (суб-миллисекунды после spawn), вернёт `ESRCH` — группы
ещё нет. Это не ломает инвариант: `ESRCH` при отправке трактуем как успех, факт
завершения отслеживается peek-ом по pid (не по группе), а эскалация по дедлайну
(секунды) заведомо позже окна. Отдельно не чинить, задокументировать в коде.

## 4. Машина состояний эскалации в `supervise.rs`

**Отклонение от TECHNICAL_PLAN, зафиксировать:** там описана блокирующая
`terminate_tree(pgid, grace)`. Не делаем. Блокирующий grace сериализуется по
процессам (N × grace на shutdown из N процессов), не наблюдаем изнутри и не
проверяется `FakeClock`. Вместо этого эскалация — состояние на процесс,
продвигаемое обычным `tick()` по инъектируемым часам: все группы гасятся
параллельно, дедлайны тестируются без реальных секунд.

### Семантика (решение, обосновать в коде)

- `begin_shutdown(sig)`: каждому процессу с живым ребёнком — `killpg(pgid, sig)`
  (форвардим полученный сигнал, SIGTERM или SIGINT) и дедлайн
  `clock.now() + grace` из его конфига. Рестарты подавлены (как в Этапе 3).
- Дедлайн наступил, лидер жив → `killpg(pgid, SIGKILL)`.
- **Выход лидера ⇒ немедленный SIGKILL-sweep остатка группы** (шаги 2–3 из §2),
  и в shutdown, и на обычном пути рестарта. Grace защищает завершение *лидера*;
  после его смерти оставшиеся члены группы — отставшие сироты, ждать их нечем:
  без cgroups нет race-free способа дождаться опустения группы (см. граблю
  killpg-liveness в §2 — зомби-лидер делает probe вечно-успешным, а реапнуть
  лидера до sweep запрещает инвариант). Лидер, желающий мягкой остановки своих
  детей, обязан сам дождаться их перед выходом — стандартная практика.
  Sweep при чистом выходе одиночного ребёнка бьёт по группе из одного зомби —
  это no-op (`ESRCH` или сигнал в зомби отбрасывается), вреда нет.
- **Тердаун при рестарте — тот же путь.** Ребёнок умер сам, оставив внуков →
  sweep убивает их до того, как policy решит рестартить. Новый инстанс никогда
  не поднимается поверх старого дерева. Grace внукам здесь не даём: шефствовать
  над их мягкой остановкой некому (лидер уже мёртв), а задержка рестарта на
  grace наказывала бы обычный случай без внуков. Единый код-путь sweep-а — ещё
  и меньше веток.

### Изменения структур

```rust
use nix::unistd::Pid;

/// Per-process shutdown escalation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopPhase {
    /// Not being stopped (normal supervision).
    Idle,
    /// SIGTERM/SIGINT sent to the group; escalate to SIGKILL at `deadline`.
    Terminating { deadline: Instant },
    /// SIGKILL already sent to the group; waiting for the leader to be reaped.
    Killing,
}

struct Supervised<'a> {
    config: &'a ProcessConfig,
    child: Option<std::process::Child>,
    /// Process group of the current child; pgid == leader pid (set right after
    /// spawn, the child is a session leader via pre_exec setsid). `Some` while
    /// the leader is alive or an unreaped zombie — exactly the window where the
    /// kernel cannot reuse the pgid, so killpg on it is safe. Cleared only
    /// after the leader is reaped. Never derive it from `child.id()` lazily.
    pgid: Option<Pid>,
    stop: StopPhase,
    /// Consecutive status-poll failures; the process is abandoned after
    /// MAX_CONSECUTIVE_POLL_ERRORS (see tick()).
    poll_errors: u32,
    restart_count: u32,
    started_at: Instant,
    next_restart_at: Option<Instant>,
    backoff: Backoff,
    done: bool,
}
```

Инициализация в `new()` и на рестарте: `pgid: Some(...)`, `stop: StopPhase::Idle`,
`poll_errors: 0`.

### `begin_shutdown` (правка существующего)

Для каждого процесса: `next_restart_at = None` (как сейчас); если ребёнок жив —
`process::signal_group(pgid, sig)` вместо `forward_signal`, ошибка логируется и
не прерывает остальных (как сейчас), затем
`stop = Terminating { deadline: clock.now() + config.stop_grace() }`. Если
ребёнка нет — `done = true` (как сейчас).

### `tick()` — переписанный опрос (скелет)

```rust
if let Some(child) = proc.child.as_mut() {
    match process::peek_exited(child) {
        Ok(false) => {
            proc.poll_errors = 0;
            if let StopPhase::Terminating { deadline } = proc.stop {
                if self.clock.now() >= deadline {
                    // Grace expired: the leader ignored the shutdown signal.
                    if let Some(pgid) = proc.pgid {
                        // log at warn; ESRCH inside is already Ok
                        let _ = process::signal_group(pgid, Signal::SIGKILL);
                    }
                    proc.stop = StopPhase::Killing;
                }
            }
        }
        Ok(true) => {
            proc.poll_errors = 0;
            // The leader exited but is NOT reaped yet: its zombie pins the
            // pgid, so sweeping the group now cannot hit a recycled id.
            // Order is load-bearing: sweep BEFORE try_wait().
            if let Some(pgid) = proc.pgid {
                let _ = process::signal_group(pgid, Signal::SIGKILL); // log err
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    proc.pgid = None;
                    proc.stop = StopPhase::Idle;
                    // ... существующая ветка Ok(Some(status)) Этапов 2–3 без
                    // изменений: classify → shutdown/done | policy → restart |
                    // done
                }
                // Cannot happen after a positive peek; treat as a poll error
                // rather than panicking.
                Ok(None) => { /* та же ветка, что Err ниже */ }
                Err(err) => { /* poll-error ветка, см. ниже */ }
            }
        }
        Err(errno) => { /* poll-error ветка, см. ниже */ }
    }
} else if /* существующая респавн-ветка без изменений */
```

Ветка рестарта по `next_restart_at` не меняется, кроме инициализации новых полей
после успешного spawn.

### Техдолг Этапа 3: счётчик ошибок опроса

Сейчас ошибка `try_wait()` в `tick()` логируется, а процесс остаётся
`child = Some`, `done = false` — вечный цикл на 20 итераций/с, заливающий лог
(см. TECHNICAL_PLAN, Этап 3). Чинится так: poll-error ветка инкрементирует
`proc.poll_errors`, логирует с текущим счётчиком; при достижении
`MAX_CONSECUTIVE_POLL_ERRORS` — **сдача**:

```rust
const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 20; // ~1 s at the 50 ms tick
```

Поведение при сдаче: best-effort `signal_group(pgid, SIGKILL)` (если pgid ещё
`Some`), одна финальная попытка `child.try_wait()` (реапнуть, если статус
вдруг доступен), затем `child = None`, `pgid = None`, `done = true` и лог уровня
`error` («giving up on process after N consecutive poll errors; killed its
group best-effort»). Обоснование N=20: любая ошибка здесь уже аномальна
(`SA_RESTART` исключает EINTR), секунда ретраев отсекает мыслимые transient-
случаи, а держать процесс вечно — DoS собственного лога. Счётчик сбрасывается в
0 при любом успешном опросе. Exit-код супервизора не трогаем (приоритет Этапа 3
неизменен) — сдача отражается только в логе.

### Второй сигнал — немедленный SIGKILL

В `run()` на месте `TODO(Этап 4)`:

```rust
if let Some(sig) = crate::signal::take_pending() {
    if !self.shutting_down {
        self.begin_shutdown(sig);
    } else {
        tracing::warn!(signal = ?sig, "second signal during shutdown; escalating to SIGKILL");
        self.escalate_to_kill();
    }
}
```

Новый публичный метод (pub — его дёргают in-process тесты):

```rust
/// Immediately SIGKILLs every live child's process group, skipping the
/// remaining grace. Used when a second shutdown signal arrives.
pub fn escalate_to_kill(&mut self)
```

Реализация: для каждого процесса с `child.is_some()` и `pgid = Some(pgid)` —
`signal_group(pgid, SIGKILL)` (ошибки логировать, не прерываться),
`stop = StopPhase::Killing`. Реапинг и `done` доедут обычным `tick()`.
Идемпотентность `begin_shutdown` не меняется.

## 5. Примитивы в `src/process.rs`

```rust
use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};

/// Sends `sig` to the whole process group `pgid`.
///
/// ESRCH is success: the group is already fully gone, which is a legitimate
/// outcome during shutdown (same contract as Этап 3's forward_signal). Call
/// only while the group's leader is unreaped — an unreaped leader (alive or
/// zombie) pins the pgid, so it cannot have been recycled for a foreign group.
pub fn signal_group(pgid: Pid, sig: Signal) -> Result<(), Errno>

/// Non-destructive exit check for a directly-spawned child.
///
/// Uses waitid(P_PID, WEXITED | WNOHANG | WNOWAIT): reports whether the child
/// has exited WITHOUT reaping it, so the zombie keeps pinning its pgid and the
/// group can still be killpg-ed safely. waitpid(2) cannot do this — on Linux
/// WNOWAIT is only valid for waitid(2) and wait4 rejects it with EINVAL.
///
/// Ok(true) — exited, still unreaped (idempotent until someone reaps);
/// Ok(false) — still running; Err — errno from waitid.
pub fn peek_exited(child: &Child) -> Result<bool, Errno>
```

Детали реализации:

- `signal_group`: `killpg(pgid, sig)`, ветка `Err(Errno::ESRCH) => Ok(())` с
  `tracing::debug!` — дословно перенести паттерн из `forward_signal`.
- `peek_exited`: `waitid(Id::Pid(pid), WEXITED | WNOHANG | WNOWAIT)`;
  `WaitStatus::StillAlive => Ok(false)`, любой другой `Ok(_) => Ok(true)`
  (Exited/Signaled — не различаем, классификацию делает `try_wait` при
  реапинге), `Err(e) => Err(e)`.
- `forward_signal(child, sig)` **удалить**: единственный вызов в
  `begin_shutdown` переходит на `signal_group`. Его doc-контракт («звать только
  для нереапнутого Child») наследуется `signal_group` в обобщённом виде.
- Примитив «живость группы» (`killpg(pgid, 0)`) **не вводить** — см. §2:
  зомби-лидер делает ответ бессмысленным, а машина в нём не нуждается. Тесты
  проверяют живость конкретных pid-ов напрямую (`kill(pid, 0)`).

Отображение ошибок: `Errno` наружу как есть (как в Этапе 3) — вызывающая
сторона (`supervise`) логирует и считает; в `SpawnError` ничего не добавляется
(`setsid` в `pre_exec` не может отказать; гипотетическая ошибка `pre_exec`
приходит как обычная `io::Error` из `spawn()` и уже покрыта `SpawnError::Spawn`).

## 6. Конфиг: grace-таймаут на процесс

`src/config.rs`, поле в `ProcessConfig`:

```rust
/// Default grace period between SIGTERM and SIGKILL escalation, seconds.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

#[derive(Debug, Deserialize)]
pub struct ProcessConfig {
    // ... существующие поля без изменений ...
    /// Seconds between the shutdown signal to the process group and the
    /// SIGKILL escalation. Whole seconds by design: TOML integer, no duration
    /// parser, negative values are rejected by the u64 type itself.
    #[serde(rename = "stop-grace-secs", default = "default_stop_grace_secs")]
    pub stop_grace_secs: u64,
}

fn default_stop_grace_secs() -> u64 { DEFAULT_STOP_GRACE_SECS }

impl ProcessConfig {
    pub fn stop_grace(&self) -> Duration { Duration::from_secs(self.stop_grace_secs) }
}
```

TOML: `stop-grace-secs = 10`. Kebab-имя — в стиле значений `restart`
(`on-failure`); существующие поля — одиночные слова, поэтому rename точечный, а
не `rename_all` на структуру (не менять принятые имена `workdir`/`env`).
Отвергнутые альтернативы: строка-длительность `"5s"` требует парсер (новая
зависимость или самописный — лишнее для одного поля); float-секунды требуют
валидации NaN/отрицательных. Целые секунды достаточны: grace меньше секунды в
проде не нужен, а тестам машины состояний реальное время не важно (FakeClock), в
e2e SIGKILL-тесте `stop-grace-secs = 1` даёт приемлемые ~1–2 с.

Внимание: новое поле ломает все литеральные конструкторы `ProcessConfig` в
тестах — поправить `cfg()`-хелперы в `tests/restart.rs`, `tests/shutdown.rs`,
`tests/spawn.rs` и юнит в `src/process.rs`
(`stop_grace_secs: DEFAULT_STOP_GRACE_SECS`).

## 7. Инвентаризация `TODO(Этап 4)`

| Место | TODO | Судьба |
|---|---|---|
| `src/process.rs:62` | pre_exec(setsid) | снимается — реализовано (§3) |
| `src/process.rs:63` | switch reaping to nix::waitpid | снимается с изменённым решением: не полный переход, а гибрид waitid-peek + Child::try_wait (§2); отразить в доках (§10) |
| `src/process.rs:74` | forward_signal → killpg | снимается — forward_signal удалён, введён signal_group (§5) |
| `src/supervise.rs:272` | второй сигнал → SIGKILL | снимается — escalate_to_kill (§4) |
| `src/main.rs:33-34` | process groups + teardown | снимается — комментарий удалить, doc-заголовок main.rs актуализировать на семантику Этапа 4 |
| `src/main.rs:35` | TODO(Этап 5) status CLI | остаётся |

Плюс техдолг Этапа 3 (ошибки опроса) — закрыт счётчиком (§4). Других
`TODO(Этап 4)` в репозитории нет (проверить `grep -rn "TODO(Этап 4)" src/`
перед завершением — все должны исчезнуть).

## 8. Тест-план (поимённо)

Общие правила из SKILL.md, обязательны: стабы через `/usr/bin/env sh -c '...'`
(не исполняемый файл на диске — ETXTBSY); готовность стаба — по **содержимому**
pid-файла, не по существованию; трапы взводить ДО записи pid-файла; долгий стаб —
цикл коротких `sleep 0.1` с ограничением ~600 итераций (≫ дедлайнов тестов);
ожидание супервизора только через `wait_with_timeout` (5 с, по таймауту SIGKILL +
panic); новые сигнальные тесты прогнать 10 раз подряд перед признанием зелёными
(`for i in $(seq 10); do cargo test --test tree || break; done`).

**Два правила про смерть ВНУКА (добавлено основной сессией) — нарушение любого
из них даёт либо флейк, либо тест, который ничего не проверяет:**

- **Смерть внука наблюдается только поллингом.** Прямого ребёнка реапит сам
  супервизор, поэтому для него `ESRCH` детерминирован сразу. Внука реапит init,
  асинхронно, — до этого момента он зомби, и `kill(pid, 0)` по нему возвращает
  **успех**. Любая проверка «внук мёртв» — цикл с дедлайном, никогда не
  одиночный `assert`. Касается тестов №1 и №6.
- **Фоновый `spin &` в `sh` глух к SIGINT, но не к SIGTERM.** POSIX предписывает
  шеллу выставлять асинхронно запущенной команде `SIGINT`/`SIGQUIT` в `SIG_IGN`.
  Поэтому дерево-тест на SIGINT вёл бы себя иначе, чем на SIGTERM (внуки
  пережили бы форвардинг и умерли бы только от sweep-а). Тесты этапа используют
  SIGTERM; если понадобится SIGINT-вариант — помнить об этой асимметрии и не
  списывать её на баг супервизора.

### 8.1 Новый e2e-файл `tests/tree.rs` (реальный бинарник)

Хелперы `write_config` (с подстановкой `stop-grace-secs` и скрипта),
`start_supervisor`, `wait_with_timeout`, `signal_supervisor` — продублировать из
`tests/signals.rs` с комментарием, что дублирование осознанное: каждый
интеграционный тест — отдельный крейт, а связывать их общим модулем ради ~40
строк — лишняя косвенность в тестах-документации. Новый хелпер:

```rust
/// Waits until `path` holds exactly `n` fully parseable pid lines.
fn wait_for_pids(path: &Path, n: usize, timeout: Duration) -> Vec<i32>
```

(парсить ВСЕ строки; вернуть, только когда их ровно `n` и все распарсились —
закрывает окно open/write из `tests/signals.rs`.)

Стаб-дерево (в TOML как `'''...'''`-литерал, pid-файл через env, как в
`signals.rs`):

```sh
spin() { i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; }
( trap '' TERM; spin ) &
echo $! >> "$SUP_PIDFILE"
( trap '' TERM; spin ) &
echo $! >> "$SUP_PIDFILE"
echo $$ >> "$SUP_PIDFILE"
spin
```

Родитель пишет свой pid **последним**: «в файле 3 строки» ⇒ оба внука уже
форкнуты — это и есть handshake.

**Внуки глухи к SIGTERM намеренно (правка основной сессии).** С обычным `spin`
внуки умирали бы от того же `killpg(SIGTERM)`, что и лидер, и тест был бы зелёным
даже при полностью отсутствующем SIGKILL-sweep — он доказывал бы только
групповой форвардинг. С `trap '' TERM` единственный путь к их смерти — sweep
после выхода лидера (§4), то есть тест наконец проверяет то, ради чего этап
существует. Лидер трапа не ставит и умирает от SIGTERM сразу, чем и запускает
sweep.

1. **`shutdown_kills_whole_tree`** — главный тест приёмки этапа. Конфиг: стаб-
   дерево, `restart = "never"`, grace по умолчанию. Дождаться 3 pid-ов, SIGTERM
   супервизору, `wait_with_timeout`, exit-код 0, затем для **всех трёх** pid-ов
   дождаться `kill(Pid::from_raw(pid), None) == Err(Errno::ESRCH)`.

   **`ESRCH` проверять поллингом с дедлайном, а не одним assert-ом (правка
   основной сессии).** Убитый внук сначала становится зомби и отдаёт `ESRCH`
   только после того, как его реапнет init, — а это происходит асинхронно, уже
   после выхода супервизора (эмпирическое подтверждение — в §2). Одиночный
   `assert` сразу после `wait_with_timeout` — гарантированный флейк. Нужен
   хелпер вида `wait_until_gone(&[i32], Duration) -> Result<(), Vec<i32>>`,
   поллящий с шагом ~50 мс и дедлайном 5 с; при провале сообщение должно
   перечислять именно выжившие pid-ы, иначе диагностика по красному CI
   невозможна.

   Доказывает: killpg доставил сигнал всей группе, sweep добил глухих к SIGTERM
   внуков, никто не осиротел (`ESRCH` ⇒ процесса нет вовсе — ни живого, ни
   зомби; зомби внуков реапит init, супервизору они недоступны, см. §2, поэтому
   проверка именно по pid, а не подсчёт зомби). Антифлейк: handshake по 3
   распарсенным строкам; короткие sleep в стабе; поллинг ESRCH.

2. **`sigkill_escalation_when_child_ignores_sigterm`** — путь SIGKILL. Стаб:

   ```sh
   trap '' TERM
   echo $$ >> "$SUP_PIDFILE"
   i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
   ```

   Конфиг: `restart = "never"`, `stop-grace-secs = 1`. Trap взводится до записи
   pid — handshake гарантирует, что SIGTERM попадёт в уже глухого ребёнка.
   SIGTERM супервизору → супервизор обязан выйти сам (реальный grace 1 с <
   таймаут 5 с), стаб мёртв (ESRCH), exit-код 0. Доказывает эскалацию по
   дедлайну на реальных часах. Антифлейк: grace 1 с при дедлайне 5 с — запас 4 с.

3. **`second_signal_escalates_immediately`** — снятие TODO в `run()`. Тот же
   глухой стаб, но `stop-grace-secs = 30` (заведомо больше таймаута теста —
   выйти по grace супервизор не успеет, выход возможен только через эскалацию).
   `start_supervisor` здесь пишет stderr в файл (`Stdio::from(File)`) и ставит
   `.env("RUST_LOG", "info")` для детерминизма. Handshake второго сигнала — не
   sleep, а поллинг stderr-файла на подстроку `shutdown requested` (лог
   `begin_shutdown`): первый SIGTERM гарантированно обработан. Затем второй
   SIGTERM → супервизор выходит в пределах 5 с, стаб мёртв, exit-код 0.
   Антифлейк: без поллинга лога два сигнала могли бы слиться в один
   (`PENDING` хранит только последний — коалесценция до первого `take_pending`),
   и тест флейково зависал бы. Если тест всё же провалится по таймауту,
   `wait_with_timeout` SIGKILL-нет супервизор, а стаб умрёт сам по границе 600
   итераций — утечки нет.

### 8.2 In-process тесты машины состояний (добавить в `tests/shutdown.rs`)

Работают через `begin_shutdown()`/`tick()`/`FakeClock`, без реальных хендлеров
(причина — в преамбуле файла, не менять). Новый хелпер: стаб, игнорирующий TERM
и пишущий pid, + `wait_for_pid` (перенести паттерн из `signals.rs`).

4. **`escalation_waits_for_grace_deadline`**: конфиг — глухой к TERM стаб с
   pid-файлом (env через поле `env` конфига), `stop_grace_secs: 5`. Дождаться
   pid (trap взведён!), `tick()`, `begin_shutdown(SIGTERM)`. Несколько
   `tick()`-ов с короткими реальными sleep, **не** двигая FakeClock: процесс не
   `done`, стаб жив (`kill(pid, None).is_ok()`) — SIGKILL до дедлайна не
   отправляется. Затем `clock().advance(6 s)` → `tick_until_done` → стаб мёртв
   (ESRCH). Доказывает обе стороны дедлайна на фейковых часах — то, что
   блокирующая terminate_tree не позволила бы проверить. Антифлейк: handshake по
   pid-файлу обязателен — иначе SIGTERM мог бы убить стаб до взведения trap, и
   тест зелёный даже со сломанной эскалацией.

5. **`escalate_to_kill_skips_grace`**: тот же стаб, `begin_shutdown(SIGTERM)`,
   затем `escalate_to_kill()` БЕЗ продвижения часов → `tick_until_done` → стаб
   мёртв. Машинная половина теста №3.

6. **`leader_exit_sweeps_leftover_grandchildren`** — тердаун при рестарте. Стаб:

   ```sh
   ( i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done ) &
   echo $! >> "$SUP_PIDFILE"
   exit 1
   ```

   `restart = "on-failure"`. Дождаться pid внука из файла,
   `wait_for_restart_scheduled` (паттерн из `tests/restart.rs`) — и к этому
   моменту внук получил SIGKILL: sweep случился при реапинге лидера, ДО
   планирования рестарта. Доказывает «новый инстанс не поднимается поверх
   старого дерева» и порядок peek → sweep → reap. Антифлейк: pid внука записан
   до `exit 1`, файл переживает смерть — гонки нет; **исчезновение внука ждать
   поллингом с дедлайном** (правка основной сессии — тем же хелпером, что в №1:
   внук уходит в зомби до реапинга со стороны init, и мгновенный `ESRCH`-assert
   флейкует).

7. Прогнать существующие тесты `shutdown.rs`/`signals.rs` без изменений
   семантики: они обязаны остаться зелёными (совместимость Этапа 3 — часть
   критерия готовности). Ожидаемые правки в них — только добавление поля в
   `cfg()`-хелперы.

### 8.3 Юниты

8. `src/config.rs`: **`stop_grace_defaults_to_five_secs`** (поле отсутствует в
   TOML → 5), **`parses_stop_grace_secs`** (`stop-grace-secs = 10` → 10 и
   `stop_grace()` = 10 s), **`rejects_negative_stop_grace`** (`-1` → ошибка
   парсинга, бесплатно от u64), **`rejects_snake_case_stop_grace`**
   (`stop_grace_secs = 10` игнорируется/не подхватывается — фиксируем kebab-имя
   контрактом; проверить, что значение осталось дефолтным. Если текущий парсер
   молча глотает неизвестные ключи — оставить только проверку дефолта и
   зафиксировать это поведение комментарием).
9. `src/process.rs`: **`spawned_child_leads_own_process_group`** — заспавнить
   `sleep`-стаб, `getpgid(child_pid) == child_pid` (`nix::unistd::getpgid`),
   затем убить и реапнуть; **`peek_exited_does_not_reap`** — стаб `exit 0`,
   поллить `peek_exited` до `Ok(true)`, вызвать `peek_exited` **повторно** →
   снова `Ok(true)` (не реапнут — статус на месте), затем `try_wait()` →
   `Some(status)` (реапинг Child-ом сработал после peek). Это юнит на сам
   гибрид §2.
10. `src/supervise.rs` (mod tests): счётчик ошибок опроса. Реальную ошибку
    `waitid` по живому прямому ребёнку не спровоцировать дёшево, поэтому
    выделить решение в чистую функцию и тестировать её:

    ```rust
    /// Returns true when the failure budget is exhausted and the process
    /// should be abandoned.
    fn record_poll_error(count: &mut u32) -> bool {
        *count += 1;
        *count >= MAX_CONSECUTIVE_POLL_ERRORS
    }
    ```

    Тест **`poll_error_budget_exhausts_after_max`**: 19 вызовов → false, 20-й →
    true; сброс в 0 моделирует успешный опрос. Проводку ветки сдачи в `tick()`
    покрыть ревью (в отчёте честно указать, что интеграционно она не
    тестируется — инъекция errno в waitid не стоит своего мока).

## 9. Порядок работ

1. `config.rs`: поле + дефолт + `stop_grace()` + юниты; починить все
   конструкторы `ProcessConfig` в тестах (компилятор укажет места).
2. `process.rs`: `pre_exec(setsid)` в `spawn`, `signal_group`, `peek_exited`,
   удалить `forward_signal`; юниты №9.
3. `supervise.rs`: поля `pgid`/`stop`/`poll_errors`, `StopPhase`,
   `record_poll_error`; переписать опрос в `tick()` (порядок peek → sweep →
   reap!), правка `begin_shutdown`, `escalate_to_kill`, второй сигнал в `run()`;
   юнит №10.
4. `main.rs`: снять TODO, актуализировать doc-заголовок.
5. Тесты: правки хелперов, №4–6 в `shutdown.rs`, новый `tests/tree.rs` (№1–3).
6. Прогоны: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`;
   `tests/tree.rs` и обновлённый `shutdown.rs` — 10 раз подряд.
7. Актуализация доков (см. §10).

## 10. Актуализация документации (в этой же ветке)

- `docs/TECHNICAL_PLAN.md`, раздел Этапа 4: зафиксировать три осознанных
  отклонения с обоснованиями из §2–§4 — (а) гибрид waitid-WNOWAIT вместо полного
  перехода на `waitpid` (и почему `waitpid` не помог бы с внуками), (б) машина
  состояний в `tick()` вместо блокирующей `terminate_tree(pgid, grace)`,
  (в) семантика «выход лидера ⇒ немедленный sweep». Упомянуть новое поле
  `stop-grace-secs` и закрытие техдолга Этапа 3 (счётчик, N=20). Пометить, что
  решения (а) и «grace как поле конфига» приняты пользователем.
- `.claude/skills/rust-process-supervisor-dev/SKILL.md`: раздел «Teardown
  дерева» переписать под машину состояний; в «грабли» добавить: WNOWAIT только у
  `waitid(2)` (EINVAL у waitpid), зомби-лидер пиннит pgid (инвариант порядка
  peek → sweep → reap), `killpg(pgid, 0)` бесполезен при зомби-лидере,
  коалесценция сигналов в `PENDING` и handshake второго сигнала через stderr-лог.
- `examples/supervisor.toml`: показать `stop-grace-secs` у одного из процессов.
- Финальную редакцию формулировок сделает основная сессия — здесь достаточно
  фактической точности.

## 11. Границы — что НЕ трогать

- Публичный контракт CLI: один позиционный аргумент `<config-path>`, никаких
  подкоманд (это Этап 5).
- Exit-коды и их приоритет из Этапа 3 (usage 2 → config 1 → start errors 1 → 0);
  сдача по счётчику ошибок код не меняет.
- `signal.rs` целиком: механизм `AtomicI32`/`take_pending` не пересматривается
  (условие пересмотра — событийный цикл Этапа 5). `install_handlers` не
  расширять.
- Самодельный `Clock`/`FakeClock` — API не менять.
- Зависимости: ничего нового в `Cargo.toml` (нужные `nix`-фичи `signal`,
  `process` уже объявлены; `waitid`/`killpg`/`getpgid` ими покрыты). Docker
  Compose не добавлять.
- Семантику restart policy / backoff Этапа 2 и подавление рестартов Этапа 3.
- Идемпотентность `begin_shutdown` (первый сигнал владеет shutdown) — второй
  сигнал эскалирует, но не переписывает `shutdown_signal`.

## 12. Критерий готовности

1. `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` — зелёные;
   `cargo test --test tree` и `--test shutdown` зелёные 10 прогонов подряд.
2. Все тесты Этапов 1–3 проходят без изменения своей семантики (правки —
   только конструкторы конфига).
3. `tests/tree.rs::shutdown_kills_whole_tree` доказывает критерий приёмки:
   после SIGTERM супервизору все pid-ы дерева (лидер + 2 внука) дают ESRCH,
   exit-код 0.
4. Эскалация: глухой к SIGTERM ребёнок добивается SIGKILL-ом по grace из
   конфига (e2e №2) и немедленно по второму сигналу (e2e №3); обе стороны
   дедлайна проверены на `FakeClock` (№4).
5. Рестарт не наслаивает деревья: №6 зелёный.
6. `grep -rn "TODO(Этап 4)" src/` пуст; техдолг опроса закрыт (№10 + ревью
   ветки сдачи).
7. Доки из §10 актуализированы.
