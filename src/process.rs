//! Process spawning and group primitives for supervisor-rs.
//!
//! Spawn (Этап 1), signal forwarding (Этап 3) and, since Этап 4, process
//! groups: every supervised process is the leader of its own group, so a
//! signal can be delivered to the whole tree it forked rather than to the
//! direct child alone.

use crate::config::ProcessConfig;
use nix::errno::Errno;
use nix::sys::signal::{killpg, Signal};
use nix::sys::wait::{waitid, Id, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

#[derive(Debug)]
pub enum SpawnError {
    EmptyCommand {
        name: String,
    },
    Spawn {
        name: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyCommand { name } => {
                write!(f, "process '{name}': command is empty")
            }
            SpawnError::Spawn { name, source } => {
                write!(f, "process '{name}': failed to spawn: {source}")
            }
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SpawnError::EmptyCommand { .. } => None,
            SpawnError::Spawn { source, .. } => Some(source),
        }
    }
}

pub fn spawn(cfg: &ProcessConfig) -> Result<Child, SpawnError> {
    if cfg.command.is_empty() {
        return Err(SpawnError::EmptyCommand {
            name: cfg.name.clone(),
        });
    }

    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..]);
    if let Some(dir) = &cfg.workdir {
        cmd.current_dir(dir);
    }
    if let Some(map) = &cfg.env {
        // Adds to the inherited environment; deliberately no env_clear.
        cmd.envs(map);
    }

    // The child becomes a session (and hence process-group) leader, so its own
    // forks stay in one group that killpg can tear down as a unit. setsid over
    // setpgid(0, 0) for two reasons: in this position it cannot fail (a freshly
    // forked child is never already a group leader, the single error condition
    // of setsid), and it detaches the child from the controlling terminal — so
    // a Ctrl-C in the shell no longer reaches children directly and forwarding
    // by the supervisor becomes the only path, which makes manual testing
    // honest. Cost: supervised children have no controlling terminal, the same
    // trade systemd makes.
    //
    // SAFETY: pre_exec runs in the forked child between fork and exec, where
    // only async-signal-safe calls are allowed. setsid() qualifies: a single
    // syscall, no allocation, no locks.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }

    cmd.spawn().map_err(|source| SpawnError::Spawn {
        name: cfg.name.clone(),
        source,
    })
}

/// Sends `sig` to the whole process group `pgid`.
///
/// ESRCH is success: the group is already gone, a legitimate outcome during
/// shutdown. It also covers a sub-millisecond window right after spawn, before
/// the child's `pre_exec` has run `setsid` and the group exists at all. That
/// window cannot break the invariant — exit is tracked per-pid by
/// [`peek_exited`], never by probing the group — but it is not free either: a
/// child signalled inside it receives no SIGTERM at all and is hard-killed once
/// the escalation deadline expires, losing its chance to shut down gracefully.
/// Deliberately not fixed: closing it would mean setting the group from the
/// parent as well, and the window needs a signal to land in the same
/// sub-millisecond as a spawn.
///
/// Call only while the group's leader has not been reaped: an unreaped leader
/// (alive or zombie) pins the pgid, so the kernel cannot have recycled it for
/// somebody else's group. Reaping first would open a TOCTOU window in which
/// this call kills innocent processes.
pub fn signal_group(pgid: Pid, sig: Signal) -> Result<(), Errno> {
    match killpg(pgid, sig) {
        Err(Errno::ESRCH) => {
            tracing::debug!(pgid = pgid.as_raw(), signal = ?sig, "signal target group already gone");
            Ok(())
        }
        other => other,
    }
}

/// Non-destructive exit check for a directly spawned child.
///
/// Uses `waitid(P_PID, WEXITED | WNOHANG | WNOWAIT)`: it reports whether the
/// child has exited *without* reaping it, so the zombie keeps pinning its pgid
/// and the group can still be swept with [`signal_group`]. `waitpid(2)` cannot
/// do this — on Linux `WNOWAIT` is only valid for `waitid(2)`, and `wait4`
/// rejects it with EINVAL.
///
/// `Ok(true)` — exited and still unreaped (idempotent until someone reaps it);
/// `Ok(false)` — still running; `Err` — errno from `waitid`.
pub fn peek_exited(child: &Child) -> Result<bool, Errno> {
    let pid = Pid::from_raw(child.id() as i32);
    let flags = WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT;
    match waitid(Id::Pid(pid), flags) {
        Ok(WaitStatus::StillAlive) => Ok(false),
        // Exited vs Signaled is not distinguished here: the classification is
        // made from the status that `Child::try_wait` returns when reaping.
        Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => Ok(true),
        // Unreachable with the flags above, which request exits only. Spelled
        // out rather than folded into a catch-all `Ok(_) => Ok(true)`: adding
        // WSTOPPED/WCONTINUED later would silently turn "child stopped" into
        // "leader exited", and the caller answers that by SIGKILLing a live
        // process group. Better a loud errno than a quiet massacre.
        Ok(other) => {
            tracing::error!(pid = pid.as_raw(), status = ?other, "unexpected waitid status");
            Err(Errno::EINVAL)
        }
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RestartPolicy, DEFAULT_STOP_GRACE_SECS};
    use std::time::{Duration, Instant};

    fn cfg(name: &str, command: &[&str]) -> ProcessConfig {
        ProcessConfig {
            name: name.to_string(),
            command: command.iter().map(|s| s.to_string()).collect(),
            workdir: None,
            env: None,
            restart: RestartPolicy::default(),
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            health_check: None,
        }
    }

    #[test]
    fn spawn_rejects_empty_command() {
        let cfg = cfg("empty", &[]);
        let err = spawn(&cfg).unwrap_err();
        assert!(matches!(err, SpawnError::EmptyCommand { .. }));
    }

    /// The whole stage rests on this: without its own group, killpg on the
    /// child's pid would hit the supervisor's group instead of the child tree.
    #[test]
    fn spawned_child_leads_own_process_group() {
        let cfg = cfg("group", &["/usr/bin/env", "sleep", "60"]);
        let mut child = spawn(&cfg).unwrap();
        let pid = Pid::from_raw(child.id() as i32);

        // setsid happens in the child after fork, so poll rather than assume it
        // has already run by the time spawn() returned in the parent.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pgid = nix::unistd::getpgid(Some(pid)).unwrap();
        while pgid != pid && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            pgid = nix::unistd::getpgid(Some(pid)).unwrap();
        }
        assert_eq!(pgid, pid, "child is not the leader of its own group");

        signal_group(pgid, Signal::SIGKILL).unwrap();
        child.wait().unwrap();
    }

    /// The hybrid reaping invariant: peek reports the exit but leaves the
    /// zombie in place (pinning the pgid), and `Child::try_wait` still finds a
    /// status to reap afterwards.
    #[test]
    fn peek_exited_does_not_reap() {
        let cfg = cfg("quick", &["/usr/bin/env", "sh", "-c", "exit 0"]);
        let mut child = spawn(&cfg).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !peek_exited(&child).unwrap() {
            assert!(Instant::now() < deadline, "stub did not exit in time");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Idempotent: nobody reaped it, so the status is still there.
        assert!(peek_exited(&child).unwrap());

        assert!(
            child.try_wait().unwrap().is_some(),
            "peek consumed the status: the child was reaped behind Child's back"
        );
    }
}
