//! Captured-output rotation for supervisor-rs (Этап 9).
//!
//! Optional per-process stdout/stderr capture into files with size-based
//! rotation, fully inside the supervisor (no external tools, no signals, no
//! interval timers). The mechanism is a pipe per captured stream plus one OS
//! reader thread that drains it and appends to a [`RotatingFile`]; when the
//! current file reaches its size limit the reader shifts `<path>.{keep-1}` →
//! `<path>.keep`, …, `<path>` → `<path>.1` and opens a fresh empty current
//! file.
//!
//! This is the **first use of OS threads in the project** (until now: a single
//! polling loop on `Clock`/`FakeClock`). The boundary is deliberate: scheduling
//! and supervision stay on the one-threaded clock-driven loop; output capture
//! runs a thread per pipe with **no clock at all** — rotation is a function of
//! *size*, not time, so it never touches `FakeClock` and cannot blur test
//! determinism.
//!
//! Why a pipe + reader thread and not direct child-writes-to-file: the child
//! writes to an inherited descriptor. If the supervisor renamed the file under
//! that open descriptor (`.log` → `.log.1`), the descriptor would keep writing
//! to the old inode and the new `.log` would stay empty (reopening stdout is
//! not the child's contract). Copy-truncate is out too: an open descriptor's
//! write offset is not reset by `ftruncate`, so the first write after rotation
//! would create a sparse hole the size of the old file. A pipe gives the file
//! exactly one writer — the supervisor's reader thread — which both writes and
//! rotates.

use std::io::Read;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::thread::JoinHandle;

/// Read-buffer size for the capture pipe. One pipe-buffer chunk at most.
const READ_BUF_BYTES: usize = 8192;

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
    /// Does not touch the filesystem; the first [`write`](Self::write) opens the
    /// file.
    pub fn new(path: PathBuf, max_size_bytes: u64, keep: u32) -> Self {
        RotatingFile {
            path,
            max_size_bytes,
            keep,
            file: None,
            size: 0,
        }
    }

    /// Ensures the current file is open, initialising `size` from its metadata
    /// so a respawned instance (or a daemon restart) appends to and resumes the
    /// count of an existing file rather than truncating it. The parent directory
    /// is not created — its absence is a write error handled by the drain-first
    /// policy in the reader.
    fn ensure_open(&mut self) -> std::io::Result<&mut std::fs::File> {
        if self.file.is_none() {
            let file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&self.path)?;
            self.size = file.metadata().map(|m| m.len()).unwrap_or(0);
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("just opened"))
    }

    /// Appends `chunk`, rotating *after* the write once `size >=
    /// max_size_bytes`. An `Err` means the chunk was dropped; the caller logs
    /// and keeps going — see the drain-first policy in [`spawn_reader`].
    ///
    /// The size check is after the write: the write is unconditional and the
    /// invariant is simple ("after a rotation the current file is empty"). The
    /// price is that the current file may exceed the limit by up to one chunk
    /// (`READ_BUF_BYTES`, 8 KiB against a default 10 MiB — negligible).
    /// Rotate-before was considered and rejected: it saves those bytes but adds
    /// a "first chunk is itself larger than the limit" branch (the write is
    /// still needed) and does not preserve line integrity anyway — chunks are
    /// cut on `read()` boundaries, not `\n`.
    pub fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        let file = self.ensure_open()?;
        file.write_all(chunk)?;
        self.size += chunk.len() as u64;
        if self.size >= self.max_size_bytes {
            self.rotate();
        }
        Ok(())
    }

    /// Shifts the rotated chain and moves the current file to `<path>.1`; the
    /// next [`write`](Self::write) lazily opens a fresh empty current file.
    ///
    /// Best-effort: rename failures are logged and swallowed (a bloated file
    /// beats lost output). On Unix `rename` silently replaces an existing
    /// target, so the oldest `.keep` disappears without a separate
    /// `remove_file`; a `NotFound` on an intermediate link just means the chain
    /// is not full yet.
    fn rotate(&mut self) {
        // Drop the current handle so the rename below moves the file we were
        // writing, and the next write re-opens a fresh one.
        self.file = None;
        // Shift `.{keep-1}` → `.{keep}`, …, `.1` → `.2` (newest last so we do
        // not clobber a link we still need).
        for i in (1..self.keep).rev() {
            let from = self.rotated_path(i);
            let to = self.rotated_path(i + 1);
            match std::fs::rename(&from, &to) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    tracing::warn!(
                        from = %from.display(),
                        to = %to.display(),
                        error = %err,
                        "log rotation: failed to shift rotated file"
                    );
                }
            }
        }
        // Move the current file to `.1`.
        let dot1 = self.rotated_path(1);
        match std::fs::rename(&self.path, &dot1) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "log rotation: failed to move current file to .1"
                );
            }
        }
        self.size = 0;
    }

    /// `<path>.N`.
    fn rotated_path(&self, n: u32) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }
}

/// Spawns the reader thread for one captured stream: blockingly reads the pipe
/// until EOF, appending everything to a [`RotatingFile`]. EOF arrives only once
/// EVERY holder of the pipe's write end is gone — the child and every descendant
/// that inherited the fd — which the Этап 4 killpg-sweep on leader exit already
/// guarantees to converge (see `supervise.rs::join_log_readers`). The thread
/// therefore needs no stop signal of its own: death of the tree is its shutdown
/// protocol.
///
/// Drain-first error policy (user's decision): a file-IO error never stops the
/// pipe read. The chunk is dropped with a warn log and the next write retries
/// the open (`file: None` after a failure); reading continues to EOF regardless.
/// Stopping the read would either block the child on a full pipe buffer or, if
/// the read end were dropped, kill it with SIGPIPE.
pub fn spawn_reader(
    fd: OwnedFd,
    path: PathBuf,
    max_size_bytes: u64,
    keep: u32,
    proc_name: String,
    stream: &'static str,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("log-{proc_name}-{stream}"))
        .spawn(move || {
            let mut pipe = std::fs::File::from(fd);
            let mut rotating = RotatingFile::new(path, max_size_bytes, keep);
            let mut buf = [0u8; READ_BUF_BYTES];
            // Warn only on the ok→err transition (and info on recovery), not on
            // every chunk: a full disk must not emit a log line per read.
            let mut write_failing = false;
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => {
                        // EOF: the only orderly exit — all write ends are gone.
                        tracing::debug!(
                            process = %proc_name,
                            stream,
                            "log reader reached EOF, thread exiting"
                        );
                        return;
                    }
                    Ok(n) => match rotating.write(&buf[..n]) {
                        Ok(()) => {
                            if write_failing {
                                write_failing = false;
                                tracing::info!(
                                    process = %proc_name,
                                    stream,
                                    "log writes recovered"
                                );
                            }
                        }
                        Err(err) => {
                            if !write_failing {
                                write_failing = true;
                                tracing::warn!(
                                    process = %proc_name,
                                    stream,
                                    error = %err,
                                    "log write failed, dropping output and retrying open"
                                );
                            }
                        }
                    },
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                        // Signal handlers are installed with SA_RESTART, so this
                        // is mostly theoretical — but the branch is cheap.
                        continue;
                    }
                    Err(err) => {
                        // The pipe is broken; EOF will not come, no reason to
                        // hold the thread.
                        tracing::error!(
                            process = %proc_name,
                            stream,
                            error = %err,
                            "log reader read error, thread exiting"
                        );
                        return;
                    }
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn read(path: &std::path::Path) -> Vec<u8> {
        fs::read(path).unwrap()
    }

    #[test]
    fn writes_below_limit_do_not_rotate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let mut rf = RotatingFile::new(path.clone(), 100, 3);
        rf.write(b"hello").unwrap();
        rf.write(b"world").unwrap();
        assert_eq!(read(&path), b"helloworld");
        assert!(!dir.path().join("app.log.1").exists());
    }

    #[test]
    fn reaching_limit_rotates_current_into_dot1() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let dot1 = dir.path().join("app.log.1");
        let mut rf = RotatingFile::new(path.clone(), 10, 3);
        // 12 bytes >= 10 → rotate after this write.
        rf.write(b"AAAAAAAAAAAA").unwrap();
        assert_eq!(read(&dot1), b"AAAAAAAAAAAA");
        assert!(!path.exists());
        // Next write starts the current file fresh.
        rf.write(b"BB").unwrap();
        assert_eq!(read(&path), b"BB");
        assert_eq!(read(&dot1), b"AAAAAAAAAAAA");
    }

    #[test]
    fn rotation_chain_shifts_and_drops_oldest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let dot1 = dir.path().join("app.log.1");
        let dot2 = dir.path().join("app.log.2");
        let dot3 = dir.path().join("app.log.3");
        let mut rf = RotatingFile::new(path.clone(), 3, 2);
        // Three rotations, each write is 4 bytes (>= 3).
        rf.write(b"AAAA").unwrap(); // -> .1
        rf.write(b"BBBB").unwrap(); // A -> .2, B -> .1
        rf.write(b"CCCC").unwrap(); // B -> .2, C -> .1, A dropped
        assert_eq!(read(&dot1), b"CCCC");
        assert_eq!(read(&dot2), b"BBBB");
        assert!(!dot3.exists(), "keep=2 must not create .3");
        // Exactly two rotated files.
        let rotated: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.starts_with("app.log.")
            })
            .collect();
        assert_eq!(rotated.len(), 2);
    }

    #[test]
    fn keep_one_replaces_single_rotated_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let dot1 = dir.path().join("app.log.1");
        let dot2 = dir.path().join("app.log.2");
        let mut rf = RotatingFile::new(path.clone(), 3, 1);
        rf.write(b"AAAA").unwrap(); // -> .1
        rf.write(b"BBBB").unwrap(); // -> .1, A dropped (keep=1, no .2)
        assert_eq!(read(&dot1), b"BBBB");
        assert!(!dot2.exists());
    }

    #[test]
    fn existing_file_is_appended_and_counted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let dot1 = dir.path().join("app.log.1");
        // Pre-existing file close to the limit.
        fs::write(&path, b"OLD").unwrap();
        let mut rf = RotatingFile::new(path.clone(), 5, 2);
        // Size starts at 3 (from metadata); this write brings it to 5 -> rotate.
        rf.write(b"NEW").unwrap();
        assert_eq!(read(&dot1), b"OLDNEW");
        assert!(!path.exists());
    }

    #[test]
    fn oversized_chunk_is_written_whole_then_rotated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let dot1 = dir.path().join("app.log.1");
        let mut rf = RotatingFile::new(path.clone(), 4, 2);
        // A single chunk larger than the limit: written whole, then rotated.
        rf.write(b"ABCDEFGHIJ").unwrap();
        assert_eq!(read(&dot1), b"ABCDEFGHIJ");
        assert!(!path.exists());
    }

    #[test]
    fn write_error_recovers_on_next_write() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope").join("app.log");
        let mut rf = RotatingFile::new(missing.clone(), 100, 2);
        // Parent directory does not exist -> write fails, chunk dropped.
        assert!(rf.write(b"lost").is_err());
        // Create the directory; the next write reopens lazily and succeeds.
        fs::create_dir(dir.path().join("nope")).unwrap();
        rf.write(b"kept").unwrap();
        assert_eq!(read(&missing), b"kept");
    }

    #[test]
    fn concatenation_preserves_all_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let mut rf = RotatingFile::new(path.clone(), 7, 5);
        // A series of writes across several rotations.
        let chunks: &[&[u8]] = &[b"alpha", b"beta", b"gamma", b"delta", b"eps", b"zeta"];
        let mut expected = Vec::new();
        for c in chunks {
            rf.write(c).unwrap();
            expected.extend_from_slice(c);
        }
        // Concatenate .keep … .1 + current; must equal exactly the input bytes.
        let mut got = Vec::new();
        for i in (1..=5u32).rev() {
            let mut s = path.clone().into_os_string();
            s.push(format!(".{i}"));
            let p = PathBuf::from(s);
            if p.exists() {
                got.extend_from_slice(&read(&p));
            }
        }
        if path.exists() {
            got.extend_from_slice(&read(&path));
        }
        assert_eq!(
            got, expected,
            "no bytes lost or duplicated across rotations"
        );
    }
}
