//! Health-check probe runner (Этап 7).
//!
//! Deliberately decoupled from supervision: this module knows nothing about
//! `SupervisorLoop` or `Clock`. It is a pure "run one probe to completion within
//! a bounded amount of *real* time" function, unit-tested against real sockets
//! and real commands, mirroring how `control.rs` is tested.
//!
//! The timeout here is OS time (an `Instant` deadline / `connect_timeout` /
//! `SO_RCVTIMEO`), not logical `Clock` time — the user's decision, see the Этап 7
//! plan §2. Only the *schedule* of when a probe is due rides the injected clock;
//! its *execution* is bounded by the real wall clock so a hung server cannot
//! block the loop past `timeout-secs`.
//!
//! The HTTP probe is a hand-rolled minimal HTTP/1.1 GET over `std::net`: it
//! writes one request line and reads only the status line. No HTTP crate, no
//! TLS, no redirects, no chunked, no keep-alive — see the plan §2 for why a
//! dependency is not warranted for a one-line request and a three-digit code.

use crate::config::HealthProbe;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// Longest HTTP status line the probe will read before giving up. A server that
/// produces no '\n' within this many bytes is not speaking HTTP/1.x.
const MAX_STATUS_LINE_BYTES: usize = 256;
/// How often the exec probe re-polls its child while waiting for the exit.
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// The smallest non-zero socket timeout we ever set. `std` rejects
/// `set_read_timeout(Some(Duration::ZERO))` with an error, so a budget that has
/// shrunk to zero is clamped to this instead of being passed through verbatim.
const MIN_SOCKET_TIMEOUT: Duration = Duration::from_millis(1);

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

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Spawn(err) => write!(f, "failed to spawn probe command: {err}"),
            ProbeError::Failed(status) => write!(f, "probe command failed: {status}"),
            ProbeError::Timeout { after } => write!(f, "probe timed out after {after:?}"),
            ProbeError::Connect(err) => write!(f, "probe connect failed: {err}"),
            ProbeError::Io(err) => write!(f, "probe IO error: {err}"),
            ProbeError::HttpStatus(code) => write!(f, "probe got non-2xx HTTP status {code}"),
            ProbeError::Malformed(line) => {
                write!(f, "probe got malformed HTTP status line: {line:?}")
            }
        }
    }
}

impl std::error::Error for ProbeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProbeError::Spawn(err) | ProbeError::Connect(err) | ProbeError::Io(err) => Some(err),
            _ => None,
        }
    }
}

/// Runs one probe to completion, bounded by `timeout` of *real* time (an OS
/// deadline, deliberately not routed through `Clock` — see the Этап 7 plan §2).
pub fn run_probe(probe: &HealthProbe, timeout: Duration) -> Result<(), ProbeError> {
    let deadline = Instant::now() + timeout;
    let result = match probe {
        HealthProbe::Exec { command } => run_exec(command, deadline),
        HealthProbe::Tcp { addr } => run_tcp(addr, deadline),
        HealthProbe::Http { addr, path } => run_http(addr, path, deadline),
    };
    // The inner runners don't carry `timeout` around, only the `deadline`
    // instant, so a `Timeout` they build reports "how far past the deadline
    // we noticed" (near zero) rather than "how long we waited" — fix that up
    // here where the configured budget is in scope, since that's what the log
    // message actually wants to say.
    result.map_err(|err| match err {
        ProbeError::Timeout { .. } => ProbeError::Timeout { after: timeout },
        other => other,
    })
}

/// Remaining budget until `deadline`, or `None` if it has already passed.
fn remaining(deadline: Instant) -> Option<Duration> {
    let now = Instant::now();
    if now >= deadline {
        None
    } else {
        Some(deadline - now)
    }
}

fn run_exec(command: &[String], deadline: Instant) -> Result<(), ProbeError> {
    // All three stdio go to /dev/null: inheriting them would pollute the daemon
    // log, and the probe's output is not interpreted — only its exit code is.
    let mut child = std::process::Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(ProbeError::Spawn)?;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(ProbeError::Failed(status.to_string()))
                };
            }
            Ok(None) => {}
            Err(err) => return Err(ProbeError::Io(err)),
        }
        if remaining(deadline).is_none() {
            // Budget spent: kill the command and reap it. The wait after kill is
            // mandatory — the probe child is a direct child of the daemon, so
            // dropping the handle without reaping would leak a zombie for the
            // lifetime of the daemon.
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProbeError::Timeout {
                after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
            });
        }
        std::thread::sleep(EXEC_POLL_INTERVAL);
    }
}

fn run_tcp(addr: &std::net::SocketAddr, deadline: Instant) -> Result<(), ProbeError> {
    let budget = remaining(deadline).ok_or(ProbeError::Timeout {
        after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
    })?;
    match TcpStream::connect_timeout(addr, budget) {
        // A successful connect is health enough; the stream is dropped here.
        Ok(_stream) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => Err(ProbeError::Timeout {
            after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
        }),
        Err(err) => Err(ProbeError::Connect(err)),
    }
}

fn run_http(addr: &std::net::SocketAddr, path: &str, deadline: Instant) -> Result<(), ProbeError> {
    let budget = remaining(deadline).ok_or(ProbeError::Timeout {
        after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
    })?;
    let mut stream = match TcpStream::connect_timeout(addr, budget) {
        Ok(stream) => stream,
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
            return Err(ProbeError::Timeout {
                after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
            });
        }
        Err(err) => return Err(ProbeError::Connect(err)),
    };

    // Arm write/read timeouts to the remaining budget (never zero — std rejects
    // a zero timeout).
    set_stream_timeouts(&stream, deadline)?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\n\r\n",
        ip = addr.ip(),
        port = addr.port(),
    );
    stream
        .write_all(request.as_bytes())
        .map_err(map_io_timeout)?;

    // Re-arm before reading: the write consumed part of the budget.
    set_stream_timeouts(&stream, deadline)?;

    // Read the status line one byte at a time, bounded — the same pattern as
    // `read_request` in control.rs. We stop at the first '\n'; the rest of the
    // response (headers, body) is never read — `Connection: close` was asked
    // for, but there is no reason to wait for the close, the socket is dropped.
    let mut line = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => {
                // EOF before a newline: a reset or a server that closed early.
                return Err(ProbeError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before HTTP status line",
                )));
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                // Ignore a lone '\r'; the split below tolerates trailing CR too.
                line.push(byte[0]);
                if line.len() > MAX_STATUS_LINE_BYTES {
                    return Err(ProbeError::Malformed(
                        "HTTP status line exceeded the byte limit".to_string(),
                    ));
                }
            }
            Err(err) => return Err(map_io_timeout(err)),
        }
    }

    parse_status_line(&line)
}

/// Parses an HTTP/1.x status line: `HTTP/1.x <code> [reason]`. Tolerates a
/// missing reason phrase and either minor version; 2xx is success.
fn parse_status_line(line: &[u8]) -> Result<(), ProbeError> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end_matches('\r');
    let mut tokens = text.split(' ');
    let version = tokens.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(ProbeError::Malformed(text.to_string()));
    }
    let code = match tokens.next().and_then(|tok| tok.parse::<u16>().ok()) {
        Some(code) => code,
        None => return Err(ProbeError::Malformed(text.to_string())),
    };
    if (200..=299).contains(&code) {
        Ok(())
    } else {
        // 3xx counts as a failure: there are no redirects by design.
        Err(ProbeError::HttpStatus(code))
    }
}

/// Arms both read and write timeouts on the stream to the remaining budget,
/// clamped to a minimum non-zero value (std rejects a zero timeout). A drained
/// budget surfaces as a `Timeout`.
fn set_stream_timeouts(stream: &TcpStream, deadline: Instant) -> Result<(), ProbeError> {
    let budget = remaining(deadline)
        .map(|b| b.max(MIN_SOCKET_TIMEOUT))
        .ok_or(ProbeError::Timeout {
            after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
        })?;
    stream
        .set_read_timeout(Some(budget))
        .map_err(ProbeError::Io)?;
    stream
        .set_write_timeout(Some(budget))
        .map_err(ProbeError::Io)?;
    Ok(())
}

/// Maps an IO error to `Timeout` if it is a socket timeout, else to `Io`. A
/// `SO_RCVTIMEO`/`SO_SNDTIMEO` expiry shows up as `WouldBlock` or `TimedOut`
/// depending on the platform.
fn map_io_timeout(err: std::io::Error) -> ProbeError {
    match err.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => ProbeError::Timeout {
            after: Duration::ZERO, // overwritten by `run_probe` with the configured budget
        },
        _ => ProbeError::Io(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    use std::net::{TcpListener, TcpStream as StdTcpStream};
    use std::thread;

    fn exec(command: &[&str]) -> HealthProbe {
        HealthProbe::Exec {
            command: command.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ---- exec ----

    #[test]
    fn exec_probe_succeeds_on_zero_exit() {
        let probe = exec(&["/usr/bin/env", "true"]);
        assert!(run_probe(&probe, Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn exec_probe_fails_on_nonzero_exit() {
        let probe = exec(&["/usr/bin/env", "false"]);
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::Failed(_)), "{err:?}");
    }

    #[test]
    fn exec_probe_reports_unspawnable_command() {
        let probe = exec(&["/nonexistent/definitely/not/here"]);
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::Spawn(_)), "{err:?}");
    }

    #[test]
    fn exec_probe_times_out_and_kills_the_command() {
        // A command that would run for 30s, with a 1s probe timeout: the probe
        // must time out well before that and kill+reap the child.
        let probe = exec(&["/usr/bin/env", "sh", "-c", "echo $$ > /dev/null; sleep 30"]);
        let start = Instant::now();
        let err = run_probe(&probe, Duration::from_secs(1)).unwrap_err();
        let elapsed = start.elapsed();
        assert!(matches!(err, ProbeError::Timeout { .. }), "{err:?}");
        assert!(
            elapsed < Duration::from_secs(3),
            "probe did not honour its timeout: {elapsed:?}"
        );
    }

    /// Extra check that the timed-out probe actually reaps its child, so no
    /// zombie is leaked. The probe command writes its pid to a file, we time it
    /// out, then confirm the pid is gone (the probe is a direct child and
    /// `wait()`s it, so `ESRCH` is deterministic once the probe returns).
    #[test]
    fn exec_probe_reaps_the_killed_command() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!(r#"echo $$ > "{}"; sleep 30"#, pidfile.display());
        let probe = exec(&["/usr/bin/env", "sh", "-c", &script]);
        let err = run_probe(&probe, Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, ProbeError::Timeout { .. }), "{err:?}");

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The probe child is reaped by the probe itself, so it is fully gone.
        assert_eq!(
            kill(Pid::from_raw(pid), None),
            Err(Errno::ESRCH),
            "the timed-out probe command was not reaped"
        );
    }

    // ---- tcp ----

    #[test]
    fn tcp_probe_succeeds_on_listening_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let probe = HealthProbe::Tcp { addr };
        assert!(run_probe(&probe, Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn tcp_probe_fails_on_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let probe = HealthProbe::Tcp { addr };
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::Connect(_)), "{err:?}");
    }

    // ---- http ----

    /// Spawns a one-shot HTTP stub: accepts one connection, reads the request
    /// (up to the blank line), then writes `response`. Returns the bound address
    /// and a handle that yields the raw request text.
    fn http_stub(response: &'static str) -> (std::net::SocketAddr, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read whatever the client sends until it stops or we have the
            // request headers.
            let mut buf = [0u8; 1024];
            let mut request = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = stream.write_all(response.as_bytes());
            String::from_utf8_lossy(&request).to_string()
        });
        (addr, handle)
    }

    #[test]
    fn http_probe_accepts_2xx() {
        let (addr, handle) = http_stub("HTTP/1.1 200 OK\r\n\r\n");
        let probe = HealthProbe::Http {
            addr,
            path: "/health".to_string(),
        };
        assert!(run_probe(&probe, Duration::from_secs(5)).is_ok());
        let request = handle.join().unwrap();
        let first_line = request.lines().next().unwrap();
        assert_eq!(first_line, "GET /health HTTP/1.1");
        assert!(request.contains("Host:"), "{request:?}");
        assert!(request.contains("Connection: close"), "{request:?}");
    }

    #[test]
    fn http_probe_accepts_204() {
        // A 2xx boundary, and HTTP/1.0 in the response — the parser tolerates
        // the minor version.
        let (addr, handle) = http_stub("HTTP/1.0 204 No Content\r\n\r\n");
        let probe = HealthProbe::Http {
            addr,
            path: "/".to_string(),
        };
        assert!(run_probe(&probe, Duration::from_secs(5)).is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn http_probe_rejects_500() {
        let (addr, handle) = http_stub("HTTP/1.1 500 Internal Server Error\r\n\r\n");
        let probe = HealthProbe::Http {
            addr,
            path: "/".to_string(),
        };
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::HttpStatus(500)), "{err:?}");
        handle.join().unwrap();
    }

    #[test]
    fn http_probe_rejects_garbage_status_line() {
        let (addr, handle) = http_stub("not http at all\r\n");
        let probe = HealthProbe::Http {
            addr,
            path: "/".to_string(),
        };
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::Malformed(_)), "{err:?}");
        handle.join().unwrap();
    }

    #[test]
    fn http_probe_fails_on_immediate_disconnect() {
        // Accept and immediately close, writing nothing.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            drop(stream);
        });
        let probe = HealthProbe::Http {
            addr,
            path: "/".to_string(),
        };
        let err = run_probe(&probe, Duration::from_secs(5)).unwrap_err();
        assert!(matches!(err, ProbeError::Io(_)), "{err:?}");
        handle.join().unwrap();
    }

    #[test]
    fn http_probe_times_out_on_silent_server() {
        // Accept and never write anything back.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Hold the connection open past the probe timeout.
            thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let probe = HealthProbe::Http {
            addr,
            path: "/".to_string(),
        };
        let start = Instant::now();
        let err = run_probe(&probe, Duration::from_secs(1)).unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            matches!(err, ProbeError::Timeout { .. } | ProbeError::Io(_)),
            "{err:?}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "silent server probe did not honour its timeout: {elapsed:?}"
        );
        handle.join().unwrap();
    }

    // A sanity check that the stub helper's own connect path is reachable — not
    // a probe test, just guards against a broken fixture in this file.
    #[test]
    fn http_stub_is_connectable() {
        let (addr, handle) = http_stub("HTTP/1.1 200 OK\r\n\r\n");
        let mut stream = StdTcpStream::connect(addr).unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        handle.join().unwrap();
    }
}
