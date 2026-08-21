//! The state snapshot the daemon publishes and `supervisor-rs status` reads
//! (Этап 5).
//!
//! The channel is a *file*, not a unix socket (decision taken by the user).
//! That choice is why the Этап 3 revision condition did not fire: the
//! supervision loop stays a poll loop with a periodic sleep, so `signal.rs`
//! keeps its atomic-flag design and no self-pipe/`signalfd` is needed.
//!
//! The format is TOML: `serde` + `toml` are already dependencies (so the
//! serialiser costs zero new crates), the result is human-readable, and it
//! matches the format of the project's own config. JSON was rejected because it
//! would pull in `serde_json` for nothing, a hand-rolled line format because it
//! would mean writing a parser and escaping process names by hand.
//!
//! Writing is atomic — temp file in the same directory plus `rename` — so a
//! reader either sees the whole previous snapshot or the whole new one, never
//! half of either.

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::{Pid, Uid};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// Schema version of the snapshot. `status` refuses to interpret a file that
/// does not carry exactly this version rather than guessing at unknown fields.
pub const STATE_VERSION: u32 = 1;

/// Directory mode for the state directory the daemon creates.
///
/// 0700 matters for the `/tmp` fallback: the path there is predictable, so a
/// world-writable parent would let another uid pre-create the directory and
/// intercept the snapshot. If the directory already exists and is not ours, the
/// write simply fails and is logged — the daemon keeps supervising.
const STATE_DIR_MODE: u32 = 0o700;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StateSnapshot {
    pub version: u32,
    /// Pid of the daemon that wrote this file — the key to telling "the daemon
    /// is running" from "this file was left behind by a dead one" (see
    /// [`daemon_alive`]).
    pub daemon_pid: u32,
    /// Wall-clock time of the write, for humans reading the file directly. No
    /// decision is taken from it and no test asserts on its value, so the
    /// non-monotonic clock behind it cannot make anything flaky.
    pub written_at_unix_secs: u64,
    /// Serialised as TOML `[[process]]` entries. Must stay the last field: TOML
    /// requires every scalar of a table to precede its sub-tables.
    #[serde(default)]
    pub process: Vec<ProcessState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProcessState {
    pub name: String,
    pub state: ProcState,
    /// Absent unless the process is running or stopping.
    ///
    /// `skip_serializing_if` is not cosmetic here: TOML has no null, so
    /// `toml::to_string` fails outright on a `None` field. Without the
    /// attribute the very first snapshot containing a restarting process would
    /// fail to serialise.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pid: Option<u32>,
    pub restart_count: u32,
    /// Absent unless the process is running or stopping; see the note on `pid`
    /// for why the attribute is mandatory.
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

impl std::fmt::Display for ProcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let word = match self {
            ProcState::Running => "running",
            ProcState::Restarting => "restarting",
            ProcState::Stopping => "stopping",
            ProcState::Stopped => "stopped",
        };
        f.write_str(word)
    }
}

/// Default location of the state file: `$XDG_RUNTIME_DIR/supervisor-rs/state.toml`,
/// falling back to `/tmp/supervisor-rs-<uid>/state.toml`.
pub fn default_path() -> PathBuf {
    default_path_from(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

/// The pure half of [`default_path`].
///
/// Split out so the fallback can be unit-tested without mutating the
/// process-wide environment: `cargo test` runs tests as threads of one process,
/// and `set_var` there races every other test.
pub fn default_path_from(xdg_runtime_dir: Option<&str>) -> PathBuf {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => Path::new(dir).join("supervisor-rs").join("state.toml"),
        _ => PathBuf::from(format!("/tmp/supervisor-rs-{}", Uid::current().as_raw()))
            .join("state.toml"),
    }
}

/// Writes `snapshot` to `path` atomically, creating the parent directory
/// (mode 0700) if it is missing.
///
/// The snapshot goes to `<path>.tmp` **in the same directory** first — `rename`
/// is only atomic within one filesystem — and is then renamed over `path`.
/// POSIX `rename` swaps the name in one step, so a concurrent reader sees
/// either the old file in full or the new one in full.
///
/// The temp name is fixed rather than random: there is exactly one writer (the
/// daemon), and `tempfile` is a dev-dependency that must not leak into
/// production code. No `fsync`: the point is to protect a concurrent *reader*,
/// not to survive a kernel panic — after a crash the file is stale anyway and
/// `status` detects that by the daemon pid.
pub fn write_atomic(path: &Path, snapshot: &StateSnapshot) -> std::io::Result<()> {
    let text = toml::to_string(snapshot)
        .map_err(|err| std::io::Error::new(ErrorKind::InvalidData, err.to_string()))?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(STATE_DIR_MODE)
                .create(parent)?;
        }
    }

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    fs::write(&tmp, text)?;
    if let Err(err) = fs::rename(&tmp, path) {
        // Do not leave the half-published file lying around next to the real
        // one; the rename is the only thing that makes it visible as state.
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[derive(Debug)]
pub enum ReadError {
    /// No state file at all — the ordinary "the daemon is not running" case,
    /// distinguished from other IO errors because it gets its own message.
    NotFound,
    Io(std::io::Error),
    Parse(toml::de::Error),
    UnsupportedVersion(u32),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::NotFound => write!(f, "state file not found"),
            ReadError::Io(source) => write!(f, "failed to read state file: {source}"),
            ReadError::Parse(source) => write!(f, "failed to parse state file: {source}"),
            ReadError::UnsupportedVersion(version) => write!(
                f,
                "unsupported state file version {version} (expected {STATE_VERSION})"
            ),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReadError::NotFound | ReadError::UnsupportedVersion(_) => None,
            ReadError::Io(source) => Some(source),
            ReadError::Parse(source) => Some(source),
        }
    }
}

/// Reads and validates a snapshot.
///
/// Thanks to the atomic write a parse error here means the file is genuinely
/// corrupt, not that the read raced the writer.
pub fn read(path: &Path) -> Result<StateSnapshot, ReadError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == ErrorKind::NotFound => return Err(ReadError::NotFound),
        Err(err) => return Err(ReadError::Io(err)),
    };
    let snapshot: StateSnapshot = toml::from_str(&text).map_err(ReadError::Parse)?;
    if snapshot.version != STATE_VERSION {
        return Err(ReadError::UnsupportedVersion(snapshot.version));
    }
    Ok(snapshot)
}

/// Best-effort removal of the state file when the daemon stops cleanly.
///
/// "No file" is then the plainest possible answer to "is the supervisor
/// running?". A missing file is not an error (nothing to remove); anything else
/// is logged and ignored — failing to clean up must not change how the daemon
/// exits.
pub fn remove(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            tracing::warn!(path = %path.display(), error = %err, "failed to remove state file");
        }
    }
}

/// Whether the process that wrote the state file is still around, via
/// `kill(pid, 0)`.
///
/// `EPERM` counts as alive: the pid exists, it just belongs to another user.
/// This shares the pid-reuse caveat of `wait_until_gone` in the tests — a pid
/// recycled by the kernel after a daemon crash would make a stale snapshot look
/// live. Race-free liveness needs pidfds; recorded as a known limitation of the
/// stage rather than papered over.
pub fn daemon_alive(pid: u32) -> bool {
    // pid 0 would mean "our own process group" to kill(2) and always answer
    // success; only a corrupt file can carry it, and it is not a live daemon.
    if pid == 0 {
        return false;
    }
    kill(Pid::from_raw(pid as i32), None) != Err(Errno::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StateSnapshot {
        StateSnapshot {
            version: STATE_VERSION,
            daemon_pid: 12345,
            written_at_unix_secs: 1_755_772_800,
            process: vec![
                ProcessState {
                    name: "web".to_string(),
                    state: ProcState::Running,
                    pid: Some(4242),
                    restart_count: 0,
                    uptime_secs: Some(42),
                },
                ProcessState {
                    name: "worker".to_string(),
                    state: ProcState::Restarting,
                    pid: None,
                    restart_count: 3,
                    uptime_secs: None,
                },
            ],
        }
    }

    #[test]
    fn snapshot_roundtrips_through_toml() {
        let snapshot = sample();
        let text = toml::to_string(&snapshot).unwrap();
        let parsed: StateSnapshot = toml::from_str(&text).unwrap();
        assert_eq!(parsed, snapshot);
    }

    /// TOML cannot express null, so a `None` field without
    /// `skip_serializing_if` makes `toml::to_string` fail. This pins the
    /// attribute: without it the first snapshot holding a restarting process
    /// would not serialise at all.
    #[test]
    fn serializes_restarting_without_pid_and_uptime() {
        let snapshot = StateSnapshot {
            process: vec![ProcessState {
                name: "worker".to_string(),
                state: ProcState::Restarting,
                pid: None,
                restart_count: 1,
                uptime_secs: None,
            }],
            ..sample()
        };
        let text = toml::to_string(&snapshot).unwrap();
        assert!(text.contains("state = \"restarting\""), "{text}");
        // `\npid` and not `pid`: the snapshot always carries `daemon-pid`.
        assert!(!text.contains("\npid = "), "{text}");
        assert!(!text.contains("uptime-secs"), "{text}");
    }

    #[test]
    fn write_atomic_creates_parent_dir_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.toml");
        write_atomic(&path, &sample()).unwrap();

        assert!(path.exists(), "state file was not created");
        assert_eq!(read(&path).unwrap(), sample());
    }

    #[test]
    fn write_atomic_replaces_previous_content_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        write_atomic(&path, &sample()).unwrap();

        let second = StateSnapshot {
            daemon_pid: 999,
            ..sample()
        };
        write_atomic(&path, &second).unwrap();

        assert_eq!(read(&path).unwrap(), second);
        assert!(
            !dir.path().join("state.toml.tmp").exists(),
            "the temp file survived the rename"
        );
    }

    #[test]
    fn read_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = read(&dir.path().join("absent.toml")).unwrap_err();
        assert!(matches!(err, ReadError::NotFound), "{err:?}");
    }

    #[test]
    fn read_rejects_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let snapshot = StateSnapshot {
            version: 99,
            ..sample()
        };
        write_atomic(&path, &snapshot).unwrap();

        let err = read(&path).unwrap_err();
        assert!(matches!(err, ReadError::UnsupportedVersion(99)), "{err:?}");
    }

    #[test]
    fn read_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        fs::write(&path, "this is not toml =\n").unwrap();

        let err = read(&path).unwrap_err();
        assert!(matches!(err, ReadError::Parse(_)), "{err:?}");
    }

    #[test]
    fn default_path_prefers_xdg_runtime_dir() {
        assert_eq!(
            default_path_from(Some("/run/user/1000")),
            PathBuf::from("/run/user/1000/supervisor-rs/state.toml")
        );
    }

    #[test]
    fn default_path_falls_back_to_tmp() {
        let expected = PathBuf::from(format!("/tmp/supervisor-rs-{}", Uid::current().as_raw()))
            .join("state.toml");
        assert_eq!(default_path_from(None), expected);
        // An empty variable is as good as unset — the XDG spec's own advice.
        assert_eq!(default_path_from(Some("")), expected);
    }

    #[test]
    fn daemon_alive_for_self_and_dead_pid() {
        assert!(daemon_alive(std::process::id()));

        // A direct child we reaped ourselves: ESRCH is immediate and
        // deterministic, no polling window as there would be for a grandchild.
        let mut child = std::process::Command::new("/usr/bin/env")
            .arg("true")
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!daemon_alive(pid));

        assert!(
            !daemon_alive(0),
            "pid 0 is the caller's group, not a daemon"
        );
    }
}
