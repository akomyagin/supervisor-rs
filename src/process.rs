//! Process spawning, monitoring, and teardown for supervisor-rs.
//!
//! This is the heart of the supervisor and the module with the trickiest POSIX
//! mechanics. It is a skeleton in Этап 0; the real logic lands across Этапы 1–4.
//!
//! Design intent (the hard part, spelled out so it is not "discovered" late):
//! each supervised process is launched in its **own process group** via
//! `setsid`/`pre_exec(setpgid)`, so that when it forks children of its own, the
//! supervisor can signal the entire group with `killpg` rather than just the
//! direct child. Shutdown is SIGTERM to the group → grace timeout → SIGKILL to
//! the group. Exited children must be reaped (`waitpid`) so they do not linger
//! as zombies.

// TODO(Этап 1): define a `Child` handle (pid, name, config index, spawned-at)
//               and `pub fn spawn(cfg: &ProcessConfig) -> Result<Child, _>` using
//               `std::process::Command` for a plain spawn (no restart yet).
// TODO(Этап 2): add the monitor loop — reap exited children via waitpid, and
//               decide whether to restart based on RestartPolicy + exit status,
//               applying exponential backoff between restart attempts.
// TODO(Этап 3): install SIGTERM/SIGINT handlers (self-pipe or signalfd) and
//               forward the received signal to the supervised child.
// TODO(Этап 4): place each child in its own process group
//               (`Command::pre_exec` → `setsid`/`setpgid`) and implement
//               `terminate_tree(pgid, grace)`: `killpg(SIGTERM)` → wait up to
//               `grace` → `killpg(SIGKILL)`, then reap. This is the process-tree
//               teardown the whole project exists to get right.
