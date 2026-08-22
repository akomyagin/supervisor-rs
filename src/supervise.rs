//! Supervision loop (Этап 2): watches spawned processes, applies the restart
//! policy on exit and respawns with exponential backoff.
//!
//! Этап 3 adds a shutdown mode: a SIGTERM/SIGINT caught by the process-wide
//! handler is forwarded to every child, and from that point on the restart
//! policy is suppressed — otherwise the forwarded signal would kill a child,
//! `tick()` would see the exit and an `always`/`on-failure` policy would
//! respawn it, and the supervisor would never terminate.
//!
//! Этап 4 makes the unit of supervision the process *group* rather than the
//! direct child, and turns the SIGTERM → grace → SIGKILL escalation into
//! per-process state advanced by the ordinary `tick()` (see [`StopPhase`]).
//! A blocking `terminate_tree(pgid, grace)` was rejected: it would serialise
//! the grace period across processes (N × grace to shut down N processes), it
//! is invisible from the outside, and its deadline could not be exercised on
//! the injected `FakeClock`.

use crate::clock::Clock;
use crate::config::{ExitOutcome, HealthProbe, ProcessConfig};
use crate::control::{ControlServer, Request, Response};
use crate::health;
use crate::process;
use crate::state::{self, ProcState, ProcessState, StateSnapshot, STATE_VERSION};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_FACTOR: u32 = 2;
/// If a process stays up at least this long, its backoff resets to initial.
pub const STABLE_RESET: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(50);
/// How many consecutive failed status polls a process may accumulate before it
/// is abandoned. ~1 s at the 50 ms tick: any error here is already anomalous
/// (`SA_RESTART` rules EINTR out), a second of retries covers every conceivable
/// transient case, and keeping the process forever would mean an endless loop
/// flooding the log — the Этап 3 debt this closes.
const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 20;
/// How long to wait before re-sending a SIGKILL that the kernel refused. Slow
/// on purpose: a kill that fails once will almost certainly fail again, so
/// retrying every 50 ms tick would only flood the log, while never retrying at
/// all would hang the shutdown forever.
const KILL_RETRY: Duration = Duration::from_secs(1);
/// How many times, and how long apart, the give-up path retries reaping a child
/// it has just SIGKILLed. See `handle_poll_error` for why a single attempt is
/// not enough.
const REAP_RETRIES: u32 = 10;
const REAP_RETRY_DELAY: Duration = Duration::from_millis(10);
/// How often the daemon republishes the state snapshot (Этап 5). One second:
/// fresh enough for a human-facing `status` command, and 20× less IO than
/// writing on every 50 ms tick. The cost is the documented staleness bound —
/// what `status` prints is at most `STATE_WRITE_INTERVAL` old.
pub const STATE_WRITE_INTERVAL: Duration = Duration::from_secs(1);

pub struct Backoff {
    current: Duration,
    initial: Duration,
    max: Duration,
    factor: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration, factor: u32) -> Self {
        Self {
            current: initial,
            initial,
            max,
            factor,
        }
    }

    /// Returns the delay to use now and advances to the next (capped) step.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(self.factor).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

fn classify(status: std::process::ExitStatus) -> ExitOutcome {
    if status.success() {
        ExitOutcome::Success
    } else {
        // Non-zero exit code or killed by a signal.
        ExitOutcome::Failure
    }
}

/// Returns true when the failure budget is exhausted and the process should be
/// abandoned. Split out as a pure function because a real `waitid` error on a
/// live direct child cannot be provoked cheaply, so only the decision is unit
/// tested.
fn record_poll_error(count: &mut u32) -> bool {
    *count += 1;
    *count >= MAX_CONSECUTIVE_POLL_ERRORS
}

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

/// What the operator asked for over the control socket (Этап 6). Orthogonal to
/// both [`StopPhase`] (the per-process TERM→KILL escalation, reused as-is) and
/// `shutting_down` (whole-supervisor shutdown): intent decides what happens
/// *after* the leader is reaped, which neither of those tracks. `StopPhase` is
/// cleared on reaping and `shutting_down` is a mode of the whole supervisor,
/// while a command is addressed to one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserIntent {
    /// Normal supervision; the restart policy applies.
    None,
    /// `stop <name>`: no respawn after exit, and `done` stays false so the
    /// daemon keeps running and `start <name>` can revive it. Cleared only by
    /// `start`. Invariant: `intent == Stopped ⟹ next_restart_at == None`.
    Stopped,
    /// Schedule an immediate respawn once the exit is reaped, then revert to
    /// `None`. Transient, unlike `Stopped`. Armed by two triggers: the operator
    /// `restart <name>` command, and the Этап 7 health-check threshold (a live
    /// but "stuck" process). Both reuse this one intent so no third variant and
    /// no extra `tick()` branch are needed; the cause is distinguished only in
    /// the log at the trigger point.
    RestartPending,
}

/// The three control verbs, factored out of [`Request`] so the transition table
/// in `handle_command` matches on `(state, verb)` without repeating the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Start,
    Stop,
    Restart,
}

/// A process's state as far as a control command is concerned. Purely local to
/// `handle_command`'s transition table (§5 of the Этап 6 plan); not the same as
/// the `status` snapshot's `ProcState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcClass {
    /// `running.is_some() && stop == Idle`.
    Running,
    /// `running.is_some() && stop != Idle` — TERM sent, waiting for the exit.
    /// Reached by an operator `stop`/`restart` or by the Этап 7 health threshold
    /// forcing a restart; all three route through `signal_terminate`.
    Stopping,
    /// `running == None && next_restart_at.is_some()` — waiting out a backoff.
    Backoff,
    /// `running == None && intent == Stopped` — stopped by the operator.
    UserStopped,
    /// `done == true` — terminal.
    Done,
}

/// Sends SIGTERM to a live process's group and arms its SIGKILL deadline, the
/// same escalation `begin_shutdown` uses — but for a single, command-targeted
/// process. The caller has already set `proc.intent`; a failed signal is logged
/// and swallowed so a command never aborts on a lost child.
fn signal_terminate(proc: &mut Supervised<'_>, now: Instant) {
    if let Some(running) = &proc.running {
        if let Err(err) = process::signal_group(running.pgid, Signal::SIGTERM) {
            tracing::error!(
                name = %proc.config.name,
                error = %err,
                "failed to signal process group for forced stop/restart"
            );
        }
        proc.stop = StopPhase::Terminating {
            deadline: now + proc.config.stop_grace(),
        };
    }
}

/// What a single status poll of a live child concluded.
///
/// The poll is expressed as a value instead of acting in place because acting
/// needs the whole `Supervised`, while the poll itself holds a mutable borrow
/// of its `child` field.
enum PollOutcome {
    Running,
    Exited(std::process::ExitStatus),
    /// The status could not be determined; carries the message to log.
    Failed(String),
}

/// Probe schedule for the *current* instance of the process (Этап 7). One
/// struct, not two `Option`s obliged to agree — the same lesson as merging
/// child+pgid into `Running` in Этап 5.
struct HealthSchedule {
    /// `restart_count` value this schedule was armed for. A mismatch means a new
    /// instance is up: re-arm afresh (fresh start-period, zeroed failures). The
    /// counter is the generation marker on purpose — it increments on every
    /// respawn, whether policy-, operator- or health-triggered.
    armed_for: u32,
    /// Next probe is due at this instant, on the injected clock.
    next_check_at: Instant,
}

/// Per-process health-check state (Этап 7). Present exactly when the config has
/// a `[process.health-check]` section.
struct HealthState {
    /// Validated once at construction; the runner never parses config.
    probe: HealthProbe,
    /// `None` until first armed for an instance.
    schedule: Option<HealthSchedule>,
    consecutive_failures: u32,
}

/// A live (or exited-but-not-yet-reaped) child together with its process group.
///
/// The two used to be separate `Option` fields obliged to stay in step; merging
/// them makes `child.is_some() ⟺ pgid.is_some()` a property of the type instead
/// of an invariant every mutation had to re-prove — and removes the `pgid: None`
/// arms that were unreachable while a child existed.
struct Running {
    child: std::process::Child,
    /// pgid == leader pid, set right after spawn (the child is made a session
    /// leader by `pre_exec` setsid). This struct exists exactly while the leader
    /// is alive or an unreaped zombie — the window in which the kernel cannot
    /// recycle the pgid, so `killpg` on it is safe. It is dropped only after the
    /// leader has been reaped. Never derive the pgid lazily from `child.id()`:
    /// once reaped, the pid may already belong to somebody else.
    pgid: Pid,
}

struct Supervised<'a> {
    config: &'a ProcessConfig,
    /// `Some` ⇔ "killpg on `pgid` is safe"; cleared only after the leader is
    /// reaped.
    running: Option<Running>,
    stop: StopPhase,
    /// What the operator last asked for over the control socket (Этап 6). Drives
    /// what happens after the leader is reaped; see [`UserIntent`].
    intent: UserIntent,
    /// Consecutive status-poll failures; the process is abandoned once
    /// `MAX_CONSECUTIVE_POLL_ERRORS` is reached.
    poll_errors: u32,
    restart_count: u32,
    started_at: Instant,
    next_restart_at: Option<Instant>,
    backoff: Backoff,
    done: bool,
    /// Active health check (Этап 7); `Some` ⇔ the config has a
    /// `[process.health-check]` section. Intervals/threshold are *not* copied
    /// here — they are read from `config.health_check`, the single source of
    /// truth.
    health: Option<HealthState>,
}

/// Polls a live child and, if it has exited, sweeps its process group with
/// SIGKILL *before* reaping it.
///
/// The order peek → sweep → reap is load-bearing. Between the exit and the
/// reaping the leader is a zombie, and a zombie still occupies its pgid, so the
/// kernel cannot have handed that id to an unrelated group: `killpg` is
/// guaranteed to hit our tree. Reaping first would open a TOCTOU window in
/// which the sweep kills innocent processes.
///
/// Sweeping on *every* leader exit — during shutdown and on the restart path
/// alike — is deliberate. The grace period protects the shutdown of the
/// *leader*; once it is gone, the remaining group members are stragglers with
/// nobody left to shepherd them, and without cgroups there is no race-free way
/// to wait for a group to empty (a zombie leader makes even `killpg(pgid, 0)`
/// succeed forever). A leader that wants its own children stopped gracefully
/// must wait for them before exiting — the standard contract. On the restart
/// path the sweep is what keeps a new instance from coming up on top of the
/// old tree; for a childless process it hits a group holding one zombie and is
/// a harmless no-op.
fn poll_child(running: &mut Running, name: &str) -> PollOutcome {
    match process::peek_exited(&running.child) {
        Ok(false) => PollOutcome::Running,
        Ok(true) => {
            if let Err(err) = process::signal_group(running.pgid, Signal::SIGKILL) {
                tracing::error!(
                    name = %name,
                    error = %err,
                    "failed to sweep process group after leader exit"
                );
            }
            match running.child.try_wait() {
                Ok(Some(status)) => PollOutcome::Exited(status),
                // Unreachable after a positive peek — the status is there to be
                // collected. Reported as a poll failure rather than panicking.
                Ok(None) => {
                    PollOutcome::Failed("child reported exited but has no status".to_string())
                }
                Err(err) => PollOutcome::Failed(err.to_string()),
            }
        }
        Err(errno) => PollOutcome::Failed(errno.to_string()),
    }
}

/// Counts a failed status poll and, once the budget is spent, abandons the
/// process. Before Этап 4 the failure was only logged and the process stayed
/// `child = Some` / `done = false` forever, so `run()` spun at 20 iterations a
/// second flooding the log — the Этап 3 debt this closes. The supervisor's exit
/// code is deliberately untouched: giving up shows in the log only.
fn handle_poll_error(proc: &mut Supervised<'_>, err: &str) {
    let exhausted = record_poll_error(&mut proc.poll_errors);
    tracing::error!(
        name = %proc.config.name,
        error = %err,
        consecutive_errors = proc.poll_errors,
        "failed to poll process status"
    );
    if !exhausted {
        return;
    }
    // Best-effort teardown, in the usual order: the leader has not been reaped
    // (we never learned its status), so its pgid is still ours to kill.
    let killed = match &proc.running {
        Some(running) => process::signal_group(running.pgid, Signal::SIGKILL).is_ok(),
        None => true,
    };
    // Reaping needs a short real-time retry, not a single attempt: SIGKILL was
    // sent microseconds ago and the kernel has almost certainly not moved the
    // process to zombie yet, so one try_wait() would return Ok(None) and we
    // would drop the Child — which neither reaps nor kills — leaking a zombie
    // for the lifetime of the daemon. The wait is real (thread::sleep, not
    // clock.sleep) because what we wait for is the kernel, not logical time,
    // so a FakeClock must not skip it. Blocking the loop for up to
    // REAP_RETRIES * REAP_RETRY_DELAY is acceptable here and nowhere else:
    // this path means something is already deeply wrong, and it runs at most
    // once per process.
    let mut reaped = false;
    if let Some(running) = proc.running.as_mut() {
        for _ in 0..REAP_RETRIES {
            match running.child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(REAP_RETRY_DELAY),
                // Still failing, exactly as the polls that got us here did.
                Err(_) => break,
            }
        }
    }
    proc.running = None;
    proc.stop = StopPhase::Idle;
    proc.done = true;
    if killed && reaped {
        tracing::error!(
            name = %proc.config.name,
            consecutive_errors = proc.poll_errors,
            "giving up on process after repeated poll errors; killed its group and reaped it"
        );
    } else {
        // Say plainly what was left behind. This is the one path on which the
        // stage's acceptance criterion ("no descendant left alive, no zombies
        // accumulating") can fail, and a silent give-up would make the leak
        // undiagnosable from the log.
        tracing::error!(
            name = %proc.config.name,
            consecutive_errors = proc.poll_errors,
            group_killed = killed,
            reaped,
            "giving up on process after repeated poll errors; it may be left \
             behind as a zombie or, if the kill failed, as a live orphan"
        );
    }
}

/// Where and when the daemon publishes its state snapshot.
struct StateWriter {
    path: PathBuf,
    /// `None` until the first write, so the very first `maybe_write_state`
    /// fires immediately and `status` works right after the daemon starts.
    next_write_at: Option<Instant>,
}

pub struct SupervisorLoop<'a, C: Clock> {
    procs: Vec<Supervised<'a>>,
    clock: C,
    had_start_errors: bool,
    /// `None` unless a state file was requested. Opt-in so the tests that do
    /// not care about it keep the plain two-argument construction and never
    /// touch the filesystem.
    state_writer: Option<StateWriter>,
    /// `None` unless a control socket was requested (Этап 6). Opt-in like
    /// `state_writer`, so tests that do not exercise the socket keep the plain
    /// construction and never touch it.
    control_server: Option<ControlServer>,
    /// Set once a shutdown signal has been received; suppresses restarts.
    /// Deliberately per-instance state rather than a global: tests can drive
    /// `begin_shutdown()` directly without touching process-wide signal state.
    shutting_down: bool,
    shutdown_signal: Option<Signal>,
}

impl<'a, C: Clock> SupervisorLoop<'a, C> {
    /// Spawns every configured process, best-effort: a process that fails to
    /// spawn is logged and skipped rather than aborting the whole startup —
    /// aborting would drop already-spawned `Child`s without waiting or
    /// killing them, orphaning healthy processes over a single bad one.
    /// Skipped processes are not tracked, so indices into per-process
    /// accessors (`restart_count`, `is_done`, ...) refer to the subsequence
    /// of `configs` that spawned successfully, not `configs` itself.
    pub fn new(configs: &'a [ProcessConfig], clock: C) -> Self {
        let mut procs = Vec::with_capacity(configs.len());
        let mut had_start_errors = false;
        for config in configs {
            match process::spawn(config) {
                Ok(child) => {
                    tracing::info!(name = %config.name, pid = child.id(), "process spawned");
                    let pgid = Pid::from_raw(child.id() as i32);
                    // Build the health state from the (already load-validated)
                    // config. `probe()` cannot fail after `load()`, but in-process
                    // tests construct `ProcessConfig` by hand, so a bad section is
                    // logged and dropped (defensive, in the spirit of Этап 2's
                    // best-effort startup) rather than panicking.
                    let health = config
                        .health_check
                        .as_ref()
                        .and_then(|hc| match hc.probe() {
                            Ok(probe) => Some(HealthState {
                                probe,
                                schedule: None,
                                consecutive_failures: 0,
                            }),
                            Err(err) => {
                                tracing::error!(
                                    name = %config.name,
                                    error = %err,
                                    "invalid health-check config; probing disabled for this process"
                                );
                                None
                            }
                        });
                    procs.push(Supervised {
                        config,
                        running: Some(Running { child, pgid }),
                        stop: StopPhase::Idle,
                        intent: UserIntent::None,
                        poll_errors: 0,
                        restart_count: 0,
                        started_at: clock.now(),
                        next_restart_at: None,
                        backoff: Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR),
                        done: false,
                        health,
                    });
                }
                Err(err) => {
                    tracing::error!(name = %config.name, error = %err, "failed to spawn process");
                    had_start_errors = true;
                }
            }
        }
        Self {
            procs,
            clock,
            had_start_errors,
            state_writer: None,
            control_server: None,
            shutting_down: false,
            shutdown_signal: None,
        }
    }

    /// Publishes the state snapshot to `path` while the loop runs, and removes
    /// the file when the loop exits cleanly.
    pub fn with_state_file(mut self, path: PathBuf) -> Self {
        self.state_writer = Some(StateWriter {
            path,
            next_write_at: None,
        });
        self
    }

    /// Listens for `start`/`stop`/`restart` commands on `server` while the loop
    /// runs, and removes the socket file when it exits cleanly. Opt-in, like
    /// [`with_state_file`](Self::with_state_file).
    pub fn with_control_server(mut self, server: ControlServer) -> Self {
        self.control_server = Some(server);
        self
    }

    /// Whether any process failed to spawn during startup.
    pub fn had_start_errors(&self) -> bool {
        self.had_start_errors
    }

    /// Builds a snapshot of the supervised processes as they are right now.
    ///
    /// A pure read of live supervision state; `pub` so in-process tests can
    /// assert on it without going through the filesystem.
    ///
    /// Note what is *not* here: a process whose very first spawn failed was
    /// never tracked (the best-effort startup of Этап 2), so it appears in
    /// neither `procs` nor the snapshot. Its failure is reported by the
    /// daemon's exit code and its startup log instead.
    pub fn snapshot(&self) -> StateSnapshot {
        let process = self
            .procs
            .iter()
            .map(|proc| {
                let (state, pid, uptime_secs) = match (&proc.running, proc.stop, proc.done) {
                    (Some(running), StopPhase::Idle, _) => (
                        ProcState::Running,
                        Some(running.child.id()),
                        Some(self.uptime_of(proc)),
                    ),
                    // Terminating or Killing: the child is still there, we are
                    // waiting for it to go.
                    (Some(running), _, _) => (
                        ProcState::Stopping,
                        Some(running.child.id()),
                        Some(self.uptime_of(proc)),
                    ),
                    (None, _, false) if proc.next_restart_at.is_some() => {
                        (ProcState::Restarting, None, None)
                    }
                    // Everything else is terminal: policy said no restart, the
                    // shutdown reaped it, or the poll-error budget gave up.
                    _ => (ProcState::Stopped, None, None),
                };
                ProcessState {
                    name: proc.config.name.clone(),
                    state,
                    pid,
                    restart_count: proc.restart_count,
                    uptime_secs,
                }
            })
            .collect();

        StateSnapshot {
            version: STATE_VERSION,
            daemon_pid: std::process::id(),
            // Wall clock, deliberately not routed through `Clock`: this field
            // is informational only, and widening the Clock trait for it would
            // make every FakeClock user carry a fake wall clock too.
            written_at_unix_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default(),
            process,
        }
    }

    /// Uptime of the current instance, on the injected clock — so it is
    /// deterministic under `FakeClock`.
    fn uptime_of(&self, proc: &Supervised<'a>) -> u64 {
        self.clock
            .now()
            .saturating_duration_since(proc.started_at)
            .as_secs()
    }

    /// Republishes the snapshot if `STATE_WRITE_INTERVAL` has elapsed on the
    /// injected clock; a no-op without a state file.
    ///
    /// Called from `run()` after `tick()`. `pub` because in-process tests drive
    /// `tick()` themselves and never enter `run()`. A failed write is a `warn`
    /// and nothing more: supervision comes first, `status` second, and the
    /// write interval already caps the log to one line per second.
    pub fn maybe_write_state(&mut self) {
        let now = self.clock.now();
        let due = match &self.state_writer {
            None => return,
            Some(writer) => writer.next_write_at.is_none_or(|at| now >= at),
        };
        if !due {
            return;
        }
        // The snapshot is taken before the writer is borrowed mutably: it reads
        // the whole of `self`.
        let snapshot = self.snapshot();
        let Some(writer) = self.state_writer.as_mut() else {
            return;
        };
        writer.next_write_at = Some(now + STATE_WRITE_INTERVAL);
        if let Err(err) = state::write_atomic(&writer.path, &snapshot) {
            tracing::warn!(
                path = %writer.path.display(),
                error = %err,
                "failed to write the state file; supervision continues"
            );
        }
    }

    /// Applies one control request and returns the response line to send back
    /// (Этап 6).
    ///
    /// Pure with respect to sockets — it only mutates supervision state and, for
    /// `stop`/`restart`, sends a group signal — so in-process tests can drive
    /// commands without any IO. The actual spawn of a `start`/`restart` is left
    /// to the ordinary respawn branch of `tick()` (see below), which reuses its
    /// spawn-error handling, `restart_count` bookkeeping and `started_at`/`pgid`
    /// setup for free; the reply is therefore an asynchronous *ack* ("the signal
    /// is sent / the respawn is scheduled"), not a confirmation of the process's
    /// death or rebirth — observe progress through `status`, like systemctl's
    /// `--no-block`.
    pub fn handle_command(&mut self, req: &Request) -> Response {
        // Shutdown already owns every restart and every StopPhase; a command
        // wedged into it would race two owners. Refuse all three verbs.
        if self.shutting_down {
            return Response::Error("supervisor is shutting down".to_string());
        }

        let (name, verb) = match req {
            Request::Start(name) => (name, Verb::Start),
            Request::Stop(name) => (name, Verb::Stop),
            Request::Restart(name) => (name, Verb::Restart),
        };

        // A process whose first spawn failed was never tracked (best-effort
        // startup of Этап 2), so it is absent here and answers "no such
        // process" — the same limitation as `status`.
        let Some(proc) = self.procs.iter_mut().find(|p| &p.config.name == name) else {
            return Response::Error(format!("no such process \"{name}\""));
        };
        let now = self.clock.now();

        // Local state classification for the transition table (§5 of the plan).
        let state = if proc.done {
            ProcClass::Done
        } else if proc.running.is_some() {
            if proc.stop == StopPhase::Idle {
                ProcClass::Running
            } else {
                // Outside shutdown this is only reachable after a stop/restart
                // command, so `intent ∈ {Stopped, RestartPending}`.
                ProcClass::Stopping
            }
        } else if proc.next_restart_at.is_some() {
            ProcClass::Backoff
        } else if proc.intent == UserIntent::Stopped {
            ProcClass::UserStopped
        } else {
            // running == None, no restart scheduled, not user-stopped, not done:
            // a transient gap the ordinary respawn branch is about to close.
            // Treat it as backoff-with-immediate for command purposes.
            ProcClass::Backoff
        };

        match (state, verb) {
            // ---- RUNNING ----
            (ProcClass::Running, Verb::Stop) => {
                proc.intent = UserIntent::Stopped;
                signal_terminate(proc, now);
                Response::Ok(Some(format!("stopping \"{name}\"")))
            }
            (ProcClass::Running, Verb::Start) => {
                Response::Ok(Some(format!("\"{name}\" is already running")))
            }
            (ProcClass::Running, Verb::Restart) => {
                proc.intent = UserIntent::RestartPending;
                signal_terminate(proc, now);
                Response::Ok(Some(format!("restarting \"{name}\"")))
            }
            // ---- STOPPING (signal already sent, waiting for exit) ----
            (ProcClass::Stopping, Verb::Stop) => {
                // `stop` overrides a restart in flight: after the exit there is
                // to be no respawn.
                proc.intent = UserIntent::Stopped;
                Response::Ok(Some(format!("stopping \"{name}\"")))
            }
            (ProcClass::Stopping, Verb::Start) => {
                Response::Error(format!("\"{name}\" is stopping; retry once it has stopped"))
            }
            (ProcClass::Stopping, Verb::Restart) => {
                if proc.intent == UserIntent::RestartPending {
                    Response::Ok(Some(format!("\"{name}\" restart already in progress")))
                } else {
                    Response::Error(format!("\"{name}\" is stopping"))
                }
            }
            // ---- BACKOFF (waiting out a restart delay) ----
            (ProcClass::Backoff, Verb::Stop) => {
                proc.intent = UserIntent::Stopped;
                // Preserve the invariant Stopped ⟹ next_restart_at == None.
                proc.next_restart_at = None;
                Response::Ok(Some(format!("\"{name}\" stopped")))
            }
            (ProcClass::Backoff, Verb::Start) => {
                // Cut the wait short; the backoff *step* is not reset — the
                // command speeds one restart, it does not declare the process
                // healthy.
                proc.next_restart_at = Some(now);
                Response::Ok(Some(format!("starting \"{name}\"")))
            }
            (ProcClass::Backoff, Verb::Restart) => {
                proc.next_restart_at = Some(now);
                Response::Ok(Some(format!("restarting \"{name}\"")))
            }
            // ---- USER_STOPPED (running == None, intent == Stopped) ----
            (ProcClass::UserStopped, Verb::Stop) => {
                Response::Ok(Some(format!("\"{name}\" is already stopped")))
            }
            (ProcClass::UserStopped, Verb::Start) => {
                proc.intent = UserIntent::None;
                proc.next_restart_at = Some(now);
                Response::Ok(Some(format!("starting \"{name}\"")))
            }
            (ProcClass::UserStopped, Verb::Restart) => {
                Response::Error(format!("\"{name}\" is stopped; use 'start <name>'"))
            }
            // ---- DONE (terminal; never revived — see the note below) ----
            (ProcClass::Done, Verb::Stop) => {
                // Idempotent; intent is left untouched — `done` is terminal.
                Response::Ok(Some(format!("\"{name}\" is already stopped")))
            }
            (ProcClass::Done, Verb::Start) => {
                Response::Error(format!("\"{name}\" has finished and cannot be started"))
            }
            (ProcClass::Done, Verb::Restart) => {
                Response::Error(format!("\"{name}\" has finished and cannot be restarted"))
            }
        }
    }

    /// Polls the control socket for at most one command per tick and applies it.
    /// A no-op without a control server. Called from `run()` after `tick()`,
    /// never from `tick()` — the same discipline as `maybe_write_state`.
    fn poll_control(&mut self) {
        // Take the accepted stream out from under the server's borrow before
        // touching `self` mutably in `handle_command`.
        let stream = match &self.control_server {
            None => return,
            Some(server) => server.try_accept(),
        };
        let Some(mut stream) = stream else {
            return;
        };
        let resp = match crate::control::read_request(&mut stream) {
            Ok(req) => self.handle_command(&req),
            Err(msg) => Response::Error(msg),
        };
        crate::control::respond(&mut stream, &resp);
    }

    /// Enters shutdown mode: cancels pending restarts and sends `sig` to the
    /// whole process group of every live child, then arms the per-process
    /// SIGKILL deadline. Idempotent — a repeated signal is a no-op, so the
    /// first signal keeps ownership of the shutdown.
    pub fn begin_shutdown(&mut self, sig: Signal) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.shutdown_signal = Some(sig);
        tracing::info!(signal = ?sig, "shutdown requested, forwarding to children");

        for proc in &mut self.procs {
            // Drop any scheduled restart: a process waiting out its backoff
            // must not come back to life during shutdown.
            proc.next_restart_at = None;
            if let Some(running) = &proc.running {
                // The signal goes to the group, not the pid: a child that
                // forked its own children must take the whole tree down.
                if let Err(err) = process::signal_group(running.pgid, sig) {
                    // A failed forward must not abort the shutdown of the
                    // remaining processes; log and move on.
                    tracing::error!(
                        name = %proc.config.name,
                        error = %err,
                        "failed to signal process group"
                    );
                }
                let deadline = self.clock.now() + proc.config.stop_grace();
                proc.stop = StopPhase::Terminating { deadline };
                // `done` stays false on purpose: the child is still alive
                // and must be reaped by tick() before the loop may exit,
                // otherwise we would leave it orphaned.
            } else {
                // Nothing left to wait for: no live child and no restart.
                proc.done = true;
            }
        }
    }

    /// Immediately SIGKILLs every live child's process group, skipping whatever
    /// grace is left. Used when a second shutdown signal arrives: an operator
    /// pressing Ctrl-C twice is asking for the hard stop. Reaping and `done`
    /// still happen in the ordinary `tick()`.
    ///
    /// Precondition: call only after [`begin_shutdown`](Self::begin_shutdown).
    /// This method does not enter shutdown mode itself, so calling it on a
    /// running supervisor would kill the groups and then let the restart policy
    /// bring them straight back — "killed and immediately respawned". `run()`
    /// guards the call; the method is `pub` for in-process tests.
    pub fn escalate_to_kill(&mut self) {
        debug_assert!(
            self.shutting_down,
            "escalate_to_kill without begin_shutdown would race the restart policy"
        );
        for proc in &mut self.procs {
            let Some(running) = &proc.running else {
                continue;
            };
            let killed = match process::signal_group(running.pgid, Signal::SIGKILL) {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        name = %proc.config.name,
                        error = %err,
                        "failed to SIGKILL process group; falling back to the deadline path"
                    );
                    false
                }
            };
            // On failure, hand the process to the deadline path with an expired
            // deadline instead of parking it in the terminal Killing phase: the
            // next tick retries and keeps retrying. Parking it there would hang
            // the shutdown forever — see the escalation branch in tick().
            proc.stop = if killed {
                StopPhase::Killing
            } else {
                StopPhase::Terminating {
                    deadline: self.clock.now(),
                }
            };
        }
    }

    /// Runs at most ONE due health probe per call (the user's decision: a tick
    /// may block for up to one probe timeout, never for a sum of them). Called
    /// from `run()` after `poll_control()`, never from `tick()` — the same
    /// discipline as `maybe_write_state`. `pub` so in-process tests drive it
    /// directly with a `FakeClock`.
    ///
    /// The scheduling — when a probe is due — rides the injected clock; the
    /// probe *execution* is bounded by real OS time (`timeout-secs`), so a hung
    /// server cannot block the loop longer than one timeout. The "stuck →
    /// restart" transition reuses the existing `StopPhase`/`RestartPending`
    /// machine wholesale: on threshold this arms `RestartPending` and sends the
    /// same group TERM as an operator `restart`, then every later step (grace,
    /// SIGKILL escalation, sweep, respawn) is the untouched existing path.
    pub fn run_due_health_check(&mut self) {
        if self.shutting_down {
            // Shutdown owns every StopPhase; probes must not interfere.
            return;
        }
        let now = self.clock.now();
        for proc in &mut self.procs {
            let Some(health) = proc.health.as_mut() else {
                continue;
            };
            // Paused for a stopped or being-stopped process: no live child
            // (operator stop / backoff / done) or a stop already in flight. An
            // `intent` check is unnecessary — `Stopped`/`RestartPending` with a
            // live process always carry `stop != Idle`, and after reaping
            // `running == None`.
            if proc.running.is_none() || proc.stop != StopPhase::Idle {
                continue;
            }

            // Re-arm on a generation mismatch: a new instance is up, so the
            // schedule is rebuilt from the current `started_at` with a fresh
            // start-period and a zeroed failure count. No `continue` afterwards —
            // the freshly armed schedule is immediately checked for "is it due?"
            // (it cannot be, since start-period + interval is non-zero, but the
            // uniform path is simpler to reason about).
            let hc = proc
                .config
                .health_check
                .as_ref()
                .expect("health is Some ⟹ config has a health-check section");
            let armed = matches!(&health.schedule, Some(s) if s.armed_for == proc.restart_count);
            if !armed {
                health.schedule = Some(HealthSchedule {
                    armed_for: proc.restart_count,
                    next_check_at: proc.started_at + hc.start_period() + hc.interval(),
                });
                health.consecutive_failures = 0;
            }

            let due = health
                .schedule
                .as_ref()
                .is_some_and(|s| now >= s.next_check_at);
            if !due {
                continue;
            }

            // Due: run the one probe. This is the single blocking point, bounded
            // by `timeout-secs`.
            let result = health::run_probe(&health.probe, hc.timeout());

            // Read the clock *after* the probe: on a SystemClock the probe took
            // real time, so the next interval must start from now, not from the
            // pre-probe instant.
            let after = self.clock.now();
            if let Some(schedule) = health.schedule.as_mut() {
                schedule.next_check_at = after + hc.interval();
            }

            match result {
                Ok(()) => {
                    if health.consecutive_failures > 0 {
                        tracing::info!(
                            name = %proc.config.name,
                            recovered_after = health.consecutive_failures,
                            "health check healthy again"
                        );
                        health.consecutive_failures = 0;
                    } else {
                        // A routine success once per interval per process would
                        // flood the log at info level — keep it at debug.
                        tracing::debug!(name = %proc.config.name, "health check ok");
                    }
                }
                Err(err) => {
                    health.consecutive_failures += 1;
                    let failures = health.consecutive_failures;
                    let threshold = hc.failure_threshold.get();
                    tracing::warn!(
                        name = %proc.config.name,
                        error = %err,
                        failures,
                        threshold,
                        "health check failed"
                    );
                    if failures >= threshold {
                        tracing::warn!(
                            name = %proc.config.name,
                            failures,
                            "unhealthy after consecutive probe failures; forcing restart"
                        );
                        // Exactly what `handle_command` does on RUNNING × restart:
                        // arm the transient RestartPending intent and TERM the
                        // group. `signal_terminate` uses fresh clock time so the
                        // grace is not shortened by the probe's duration. The
                        // failure counter is left alone — the re-arm on the next
                        // generation (new `restart_count`) zeroes it.
                        proc.intent = UserIntent::RestartPending;
                        signal_terminate(proc, self.clock.now());
                    }
                }
            }
            // One probe per call: return so a second due probe waits for the
            // next tick. No starvation — the probe just run pushed its own
            // `next_check_at` out by an interval, so the next due process wins
            // the following tick.
            return;
        }
    }

    pub fn tick(&mut self) {
        for proc in &mut self.procs {
            if let Some(running) = proc.running.as_mut() {
                let outcome = poll_child(running, &proc.config.name);
                match outcome {
                    PollOutcome::Running => {
                        proc.poll_errors = 0;
                        if let StopPhase::Terminating { deadline } = proc.stop {
                            if self.clock.now() >= deadline {
                                // Grace expired: the leader ignored the
                                // shutdown signal. Its group is still pinned by
                                // the live leader, so killpg is safe here too.
                                tracing::warn!(
                                    name = %proc.config.name,
                                    "grace period expired; escalating to SIGKILL"
                                );
                                let killed =
                                    match process::signal_group(running.pgid, Signal::SIGKILL) {
                                        Ok(()) => true,
                                        Err(err) => {
                                            tracing::error!(
                                                name = %proc.config.name,
                                                error = %err,
                                                retry_in = ?KILL_RETRY,
                                                "failed to SIGKILL process group; will retry"
                                            );
                                            false
                                        }
                                    };
                                // Advance to Killing only if the signal actually
                                // went out. Killing is terminal — nothing
                                // re-examines it — so entering it after a failed
                                // kill would strand the process forever and hang
                                // the whole shutdown. Staying in Terminating with
                                // a fresh deadline retries instead, throttled to
                                // one attempt per KILL_RETRY rather than one per
                                // 50 ms tick: an unkillable process must not
                                // flood the log, which is the very Этап 3 defect
                                // the poll-error budget below exists to fix.
                                proc.stop = if killed {
                                    StopPhase::Killing
                                } else {
                                    StopPhase::Terminating {
                                        deadline: self.clock.now() + KILL_RETRY,
                                    }
                                };
                            }
                        }
                    }
                    PollOutcome::Exited(status) => {
                        proc.poll_errors = 0;
                        let outcome = classify(status);
                        tracing::info!(
                            name = %proc.config.name,
                            status = ?status,
                            outcome = ?outcome,
                            "process exited"
                        );
                        // The leader has been reaped: the kernel may recycle
                        // its pgid from now on, so the pair must never be used
                        // again. Dropped only here, after `try_wait` collected
                        // the status inside `poll_child`.
                        proc.running = None;
                        proc.stop = StopPhase::Idle;
                        if self.shutting_down {
                            // During shutdown the exit is the expected result
                            // of our own forwarded signal — the restart policy
                            // is not consulted at all.
                            proc.done = true;
                            tracing::info!(
                                name = %proc.config.name,
                                "process exited during shutdown"
                            );
                        } else {
                            match proc.intent {
                                UserIntent::RestartPending => {
                                    proc.intent = UserIntent::None;
                                    proc.next_restart_at = Some(self.clock.now());
                                    // Neutral attribution: `RestartPending` is now
                                    // armed by both `restart <name>` and the health
                                    // threshold (Этап 7), so "operator restart"
                                    // would be wrong for a health-forced respawn.
                                    // The cause is already logged at the trigger
                                    // (the "forcing restart" warn for health).
                                    tracing::info!(
                                        name = %proc.config.name,
                                        "forced restart: respawn scheduled"
                                    );
                                }
                                UserIntent::Stopped => {
                                    // Not `done`: the daemon stays up so
                                    // `start <name>` can revive it.
                                    tracing::info!(
                                        name = %proc.config.name,
                                        "stopped by operator"
                                    );
                                }
                                UserIntent::None => {
                                    if proc.config.restart.should_restart(outcome) {
                                        if self
                                            .clock
                                            .now()
                                            .saturating_duration_since(proc.started_at)
                                            >= STABLE_RESET
                                        {
                                            proc.backoff.reset();
                                        }
                                        let delay = proc.backoff.next_delay();
                                        proc.next_restart_at = Some(self.clock.now() + delay);
                                        tracing::info!(
                                            name = %proc.config.name,
                                            delay = ?delay,
                                            "restart scheduled"
                                        );
                                    } else {
                                        proc.done = true;
                                    }
                                }
                            }
                        }
                    }
                    PollOutcome::Failed(err) => handle_poll_error(proc, &err),
                }
            } else if !proc.done
                // Defense in depth: begin_shutdown() already cleared every
                // next_restart_at, but a respawn here would resurrect a
                // process we are trying to stop.
                && !self.shutting_down
                && proc
                    .next_restart_at
                    .is_some_and(|at| self.clock.now() >= at)
            {
                match process::spawn(proc.config) {
                    Ok(child) => {
                        proc.restart_count += 1;
                        proc.started_at = self.clock.now();
                        proc.next_restart_at = None;
                        proc.poll_errors = 0;
                        tracing::info!(
                            name = %proc.config.name,
                            pid = child.id(),
                            restart_count = proc.restart_count,
                            "process restarted"
                        );
                        let pgid = Pid::from_raw(child.id() as i32);
                        proc.stop = StopPhase::Idle;
                        proc.running = Some(Running { child, pgid });
                    }
                    Err(err) => {
                        // Apply the same backoff as a crashing process would get,
                        // so a permanently broken command (e.g. missing binary)
                        // doesn't hot-retry every tick.
                        let delay = proc.backoff.next_delay();
                        proc.next_restart_at = Some(self.clock.now() + delay);
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            delay = ?delay,
                            "failed to restart process; will retry with backoff"
                        );
                    }
                }
            }
        }
    }

    fn any_active(&self) -> bool {
        self.procs.iter().any(|proc| !proc.done)
    }

    pub fn run(&mut self) {
        loop {
            // Polled before the liveness check so that a signal delivered
            // while `new()` was still spawning is acted on in the very first
            // iteration, instead of after a full tick.
            if let Some(sig) = crate::signal::take_pending() {
                if !self.shutting_down {
                    self.begin_shutdown(sig);
                } else {
                    // A second signal is read as "stop waiting": skip whatever
                    // grace is left and SIGKILL every group right away.
                    tracing::warn!(
                        signal = ?sig,
                        "second signal during shutdown; escalating to SIGKILL"
                    );
                    self.escalate_to_kill();
                }
            }
            if !self.any_active() {
                break;
            }
            self.tick();
            // Publishing lives here rather than in `tick()`: `tick()` stays
            // free of side channels and of anything that could block, and the
            // integration tests that drive it directly keep touching no files.
            self.maybe_write_state();
            // Control-socket handling also lives here and not in `tick()`, for
            // the same reason: at most one accepted command per tick (Этап 6).
            self.poll_control();
            // Health probes run here too (Этап 7), after `poll_control` so a
            // `stop` accepted this tick has already armed `StopPhase` and
            // correctly suppresses the probe. At most one probe per tick.
            self.run_due_health_check();
            self.clock.sleep(TICK);
        }
        // A clean exit removes the file, so "no state file" is the plainest
        // possible answer to "is the supervisor running?". A daemon killed with
        // SIGKILL or dying in a panic leaves it behind; `status` catches that
        // case by the recorded daemon pid.
        if let Some(writer) = &self.state_writer {
            state::remove(&writer.path);
        }
        if let Some(server) = &self.control_server {
            server.cleanup();
        }
    }

    // Test-visibility accessors: integration tests drive tick() manually and
    // need to observe per-process state.

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn restart_count(&self, index: usize) -> u32 {
        self.procs[index].restart_count
    }

    pub fn is_done(&self, index: usize) -> bool {
        self.procs[index].done
    }

    /// Remaining delay until the scheduled restart, measured from `clock.now()`.
    pub fn next_restart_delay(&self, index: usize) -> Option<Duration> {
        self.procs[index]
            .next_restart_at
            .map(|at| at.saturating_duration_since(self.clock.now()))
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// The signal that started the shutdown, if any.
    pub fn shutdown_signal(&self) -> Option<Signal> {
        self.shutdown_signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR);
        let expected = [1u64, 2, 4, 8, 16, 30, 30];
        for secs in expected {
            assert_eq!(backoff.next_delay(), Duration::from_secs(secs));
        }
    }

    #[test]
    fn backoff_resets() {
        let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR);
        backoff.next_delay();
        backoff.next_delay();
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        backoff.reset();
        assert_eq!(backoff.next_delay(), INITIAL_BACKOFF);
    }

    /// Only the budget decision is unit tested: provoking a real `waitid`
    /// error on a live direct child is not worth mocking an errno for. The
    /// wiring of the give-up branch in `tick()` is covered by review.
    #[test]
    fn poll_error_budget_exhausts_after_max() {
        let mut count = 0;
        for _ in 1..MAX_CONSECUTIVE_POLL_ERRORS {
            assert!(!record_poll_error(&mut count));
        }
        assert_eq!(count, MAX_CONSECUTIVE_POLL_ERRORS - 1);
        assert!(record_poll_error(&mut count));

        // A successful poll resets the counter, so only *consecutive* failures
        // add up.
        count = 0;
        assert!(!record_poll_error(&mut count));
    }

    /// `handle_poll_error` takes `&mut Supervised` directly rather than being
    /// woven only into `tick()`'s match arms, so the give-up branch can be
    /// exercised without provoking a real `waitid` errno: build a `Supervised`
    /// around a genuinely long-lived child, call the poll-error handler
    /// `MAX_CONSECUTIVE_POLL_ERRORS` times as if every status poll had failed,
    /// and check both the resulting state *and* that the child's group was
    /// actually killed, not just that the struct fields were reset.
    #[test]
    fn poll_error_gives_up_and_kills_group_after_max() {
        let config = ProcessConfig {
            name: "stuck".to_string(),
            command: vec![
                "/usr/bin/env".to_string(),
                "sleep".to_string(),
                "60".to_string(),
            ],
            workdir: None,
            env: None,
            restart: crate::config::RestartPolicy::Never,
            stop_grace_secs: crate::config::DEFAULT_STOP_GRACE_SECS,
            health_check: None,
        };
        let child = process::spawn(&config).unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let pgid = Pid::from_raw(child.id() as i32);

        let mut proc = Supervised {
            config: &config,
            running: Some(Running { child, pgid }),
            stop: StopPhase::Idle,
            intent: UserIntent::None,
            poll_errors: 0,
            restart_count: 0,
            started_at: std::time::Instant::now(),
            next_restart_at: None,
            backoff: Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR),
            done: false,
            health: None,
        };

        for i in 1..MAX_CONSECUTIVE_POLL_ERRORS {
            handle_poll_error(&mut proc, "synthetic poll error");
            assert!(!proc.done, "abandoned too early at iteration {i}");
            assert_eq!(proc.poll_errors, i);
        }
        handle_poll_error(&mut proc, "synthetic poll error");

        assert!(proc.done, "process was not abandoned after the budget");
        assert!(
            proc.running.is_none(),
            "the child handle and its pgid were not dropped"
        );
        assert_eq!(proc.stop, StopPhase::Idle);

        // The teardown must be more than a state reset: the SIGKILL must have
        // actually reached the leader. It becomes a zombie the instant the
        // kernel processes the signal, which can be a moment after `killpg`
        // returns, so a single `try_wait` inside `handle_poll_error` can miss
        // the exit (`Ok(None)`) and drop the `Child` handle while the process
        // is still a zombie -- with `child` gone, nobody ever reaps it, so
        // `ps` shows it as `Z <defunct>` forever. That is why the reachable
        // signal here is "the leader received SIGKILL", checked via `ps`
        // STAT, not "the pid is fully gone" -- the latter is a real, separate
        // gap (see the coverage report).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stat = String::new();
        while std::time::Instant::now() < deadline {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stat.is_empty() || stat.starts_with('Z') {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            stat.is_empty() || stat.starts_with('Z'),
            "the stuck child's group was not killed (ps STAT={stat:?})"
        );
    }

    #[test]
    fn classify_success_and_failure() {
        let ok = std::process::Command::new("/usr/bin/env")
            .arg("true")
            .status()
            .unwrap();
        assert_eq!(classify(ok), ExitOutcome::Success);

        let fail = std::process::Command::new("/usr/bin/env")
            .arg("false")
            .status()
            .unwrap();
        assert_eq!(classify(fail), ExitOutcome::Failure);
    }
}
