//! The control socket the daemon listens on and the `start` / `stop` /
//! `restart` subcommands talk to (Этап 6).
//!
//! The channel is a `SOCK_STREAM` unix-domain socket, one command per
//! connection, carrying a one-line text protocol. That choice is consistent
//! with Этап 5's state file: a text line needs no new crate (`serde_json` would
//! be one for a three-verb vocabulary), it can be driven by hand with
//! `nc -U <sock>`, and the client and server are the same binary so no protocol
//! versioning is warranted.
//!
//! The socket is polled from the ordinary poll loop with a non-blocking
//! `try_accept` once per tick — the same pattern as `maybe_write_state` in
//! Этап 5 — so `src/signal.rs` keeps its atomic-flag design and no event loop
//! (self-pipe / `signalfd`) is introduced. This is the user's decision; see
//! `docs/TECHNICAL_PLAN.md` (Этап 6).

use crate::state;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Directory mode for the runtime directory that holds the socket. Same 0700 as
/// the state directory: the socket is reachable only by its owning uid, which is
/// the entire access-control model.
const SOCKET_DIR_MODE: u32 = 0o700;

/// Longest request line the server will read. A line longer than this without a
/// terminating newline is rejected with `error: malformed request` rather than
/// read unboundedly.
const MAX_REQUEST_BYTES: usize = 4096;

/// How long the server blocks reading a request from, or writing a response to,
/// an accepted connection. The accept is non-blocking, but a connection may not
/// carry its data the instant it is accepted, so the accepted stream reads with
/// this bound. A well-behaved client (our own binary) writes the whole command
/// in one `write` right after `connect`, so this timeout never fires in normal
/// life; only a malicious same-uid client could make it fire, and such a uid can
/// already SIGKILL the daemon. It is an OS-level `SO_RCVTIMEO`/`SO_SNDTIMEO`, not
/// logical time, so `Clock` is deliberately not involved.
const REQUEST_IO_TIMEOUT: Duration = Duration::from_millis(250);

/// How long the CLI client waits for the daemon to answer before giving up, so a
/// wedged daemon cannot hang the command line. The one-tick (50 ms) answer
/// latency of the poll loop fits inside this with a huge margin.
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Start(String),
    Stop(String),
    Restart(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// Renders as `ok` or `ok: <text>`.
    Ok(Option<String>),
    /// Renders as `error: <text>`.
    Error(String),
}

/// Default location of the control socket:
/// `$XDG_RUNTIME_DIR/supervisor-rs/control.sock`, falling back to
/// `/tmp/supervisor-rs-<uid>/control.sock`. The same directory as the state
/// file, so the XDG-vs-`/tmp` choice is made once in `state::runtime_dir_from`.
pub fn default_socket_path() -> PathBuf {
    default_socket_path_from(std::env::var("XDG_RUNTIME_DIR").ok().as_deref())
}

/// The pure half of [`default_socket_path`], for unit tests that must not mutate
/// the process-wide environment.
pub fn default_socket_path_from(xdg_runtime_dir: Option<&str>) -> PathBuf {
    state::runtime_dir_from(xdg_runtime_dir).join("control.sock")
}

/// Parses one request line. A trailing `\n`/`\r` is tolerated here so callers do
/// not have to strip it.
///
/// The name is everything after the first space: a configured process name may
/// contain spaces, so `split_once(' ')` is used rather than `split_whitespace`.
/// The name must be non-empty.
pub fn parse_request(line: &str) -> Result<Request, String> {
    let line = line.trim_end_matches(['\n', '\r']);
    let (verb, name) = line
        .split_once(' ')
        .ok_or_else(|| format!("malformed request: no name in \"{line}\""))?;
    if name.is_empty() {
        return Err(format!("malformed request: empty name in \"{line}\""));
    }
    match verb {
        "start" => Ok(Request::Start(name.to_string())),
        "stop" => Ok(Request::Stop(name.to_string())),
        "restart" => Ok(Request::Restart(name.to_string())),
        other => Err(format!("unknown verb \"{other}\"")),
    }
}

/// Renders a response as its protocol line, **without** the trailing newline.
pub fn render_response(resp: &Response) -> String {
    match resp {
        Response::Ok(None) => "ok".to_string(),
        Response::Ok(Some(text)) => format!("ok: {text}"),
        Response::Error(text) => format!("error: {text}"),
    }
}

/// Parses a response line back into a [`Response`]. Used by the CLI client and
/// by roundtrip tests. A line that is neither `ok`… nor `error:`… is treated as
/// an `Error` carrying the raw line, so a garbled answer still surfaces as a
/// failure rather than being lost.
pub fn parse_response(line: &str) -> Response {
    let line = line.trim_end_matches(['\n', '\r']);
    if line == "ok" {
        Response::Ok(None)
    } else if let Some(rest) = line.strip_prefix("ok: ") {
        Response::Ok(Some(rest.to_string()))
    } else if let Some(rest) = line.strip_prefix("error: ") {
        Response::Error(rest.to_string())
    } else {
        Response::Error(line.to_string())
    }
}

/// The listening half of the control socket, owned by the running daemon.
pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
}

#[derive(Debug)]
pub enum BindError {
    /// A live daemon already answers on this socket path.
    AlreadyRunning(PathBuf),
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning(path) => write!(
                f,
                "a supervisor is already running on control socket {}",
                path.display()
            ),
            BindError::Io(source) => write!(f, "failed to bind control socket: {source}"),
        }
    }
}

impl std::error::Error for BindError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BindError::AlreadyRunning(_) => None,
            BindError::Io(source) => Some(source),
        }
    }
}

impl ControlServer {
    /// Binds the control socket at `path`, creating the parent directory
    /// (mode 0700) the same way `state::write_atomic` does.
    ///
    /// A `bind` on an existing socket path fails with `EADDRINUSE` regardless of
    /// whether the owner is alive, so this probes first: if the file exists, try
    /// to `connect` to it. A successful connect means a live daemon already owns
    /// it (`AlreadyRunning`); any connect failure means the file is orphaned
    /// (`ECONNREFUSED` for a dead socket, or the path is not a socket at all), in
    /// which case it is removed and the bind proceeds. Liveness is proven by the
    /// socket itself, so this shares none of `daemon_alive`'s pid-reuse caveat.
    ///
    /// The race where two daemons start at once and both see the same orphaned
    /// file is deliberately not closed — the same "one supervisor per uid" class
    /// of limitation as in Этап 5.
    pub fn bind(path: &Path) -> Result<Self, BindError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(SOCKET_DIR_MODE)
                    .create(parent)
                    .map_err(BindError::Io)?;
            }
        }

        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_probe) => return Err(BindError::AlreadyRunning(path.to_path_buf())),
                Err(_) => {
                    // Orphaned socket file from a daemon that died without
                    // cleaning up. Remove it and bind afresh.
                    std::fs::remove_file(path).map_err(BindError::Io)?;
                }
            }
        }

        let listener = UnixListener::bind(path).map_err(BindError::Io)?;
        listener.set_nonblocking(true).map_err(BindError::Io)?;
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Non-blocking accept of at most one pending connection.
    ///
    /// Returns `None` on `WouldBlock` (no client waiting). An unexpected accept
    /// error is logged at `warn` and also answered with `None`: supervision must
    /// never be derailed by a control-socket hiccup.
    pub fn try_accept(&self) -> Option<UnixStream> {
        match self.listener.accept() {
            Ok((stream, _addr)) => Some(stream),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => None,
            Err(err) => {
                tracing::warn!(error = %err, "control socket accept failed");
                None
            }
        }
    }

    /// Best-effort removal of the socket file on a clean exit, mirroring
    /// `state::remove`. A missing file is fine; anything else is logged.
    pub fn cleanup(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "failed to remove control socket file"
                );
            }
        }
    }
}

/// Reads one request line from an accepted connection, bounded by
/// [`REQUEST_IO_TIMEOUT`] and [`MAX_REQUEST_BYTES`].
///
/// A read timeout, an over-long line, or a bad verb all become an `Err(String)`
/// the caller answers with `error: <msg>`.
pub fn read_request(stream: &mut UnixStream) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(REQUEST_IO_TIMEOUT))
        .map_err(|err| format!("failed to arm read timeout: {err}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = Vec::with_capacity(64);
    // Read one byte at a time, bounded, so an over-long or newline-less line is
    // rejected rather than buffered without limit. A slow/silent client trips
    // the socket read timeout and surfaces here as an IO error.
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => return Err("malformed request: connection closed".to_string()),
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > MAX_REQUEST_BYTES {
                    return Err("malformed request".to_string());
                }
            }
            Err(err) => return Err(format!("malformed request: {err}")),
        }
    }

    let text = String::from_utf8(line).map_err(|_| "malformed request: not UTF-8".to_string())?;
    parse_request(&text)
}

/// Writes the response line to an accepted connection.
///
/// A write error (the client already hung up — `EPIPE` and friends) is logged at
/// `debug` and swallowed: the connection is one-shot and nobody is owed the
/// answer once the client is gone. Rust masks SIGPIPE before `main`, so this is
/// an error return here rather than a process-killing signal.
pub fn respond(stream: &mut UnixStream, resp: &Response) {
    if let Err(err) = stream.set_write_timeout(Some(REQUEST_IO_TIMEOUT)) {
        tracing::debug!(error = %err, "failed to arm control socket write timeout");
        return;
    }
    let mut line = render_response(resp);
    line.push('\n');
    if let Err(err) = stream.write_all(line.as_bytes()) {
        tracing::debug!(error = %err, "failed to write control socket response");
    }
}

#[derive(Debug)]
pub enum ClientError {
    /// Connect failed — the usual "the daemon is not running" case.
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    Io(std::io::Error),
    /// The daemon answered something that is not a response line.
    Malformed(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Connect { path, source } => write!(
                f,
                "cannot connect to supervisor at {}: {source} (is the daemon running?)",
                path.display()
            ),
            ClientError::Io(source) => write!(f, "control socket IO error: {source}"),
            ClientError::Malformed(line) => {
                write!(f, "daemon returned a malformed response: {line:?}")
            }
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::Connect { source, .. } | ClientError::Io(source) => Some(source),
            ClientError::Malformed(_) => None,
        }
    }
}

/// The CLI client: connect, write one request line, read one response line.
///
/// Both directions are bounded by [`CLIENT_IO_TIMEOUT`] so a wedged daemon
/// cannot hang the command line.
pub fn send_command(socket: &Path, req: &Request) -> Result<Response, ClientError> {
    let mut stream = UnixStream::connect(socket).map_err(|source| ClientError::Connect {
        path: socket.to_path_buf(),
        source,
    })?;
    stream
        .set_read_timeout(Some(CLIENT_IO_TIMEOUT))
        .map_err(ClientError::Io)?;
    stream
        .set_write_timeout(Some(CLIENT_IO_TIMEOUT))
        .map_err(ClientError::Io)?;

    let mut line = render_request(req);
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(ClientError::Io)?;
    // The daemon closes the connection after answering, so no explicit
    // shutdown-of-write is needed for it to see the whole request.

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    let read = reader
        .read_line(&mut response_line)
        .map_err(ClientError::Io)?;
    if read == 0 {
        return Err(ClientError::Malformed(String::new()));
    }
    Ok(parse_response(&response_line))
}

/// Renders a request as its protocol line, **without** the trailing newline.
/// The inverse of [`parse_request`]; kept private since only the client needs it.
fn render_request(req: &Request) -> String {
    match req {
        Request::Start(name) => format!("start {name}"),
        Request::Stop(name) => format!("stop {name}"),
        Request::Restart(name) => format!("restart {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    // ---- protocol (pure functions) ----

    #[test]
    fn parse_request_accepts_three_verbs() {
        assert_eq!(
            parse_request("stop web").unwrap(),
            Request::Stop("web".into())
        );
        assert_eq!(
            parse_request("start web").unwrap(),
            Request::Start("web".into())
        );
        assert_eq!(
            parse_request("restart web").unwrap(),
            Request::Restart("web".into())
        );
        // A trailing newline is tolerated.
        assert_eq!(
            parse_request("stop web\n").unwrap(),
            Request::Stop("web".into())
        );
    }

    #[test]
    fn parse_request_keeps_spaces_in_name() {
        assert_eq!(
            parse_request("stop my app").unwrap(),
            Request::Stop("my app".into())
        );
    }

    #[test]
    fn parse_request_rejects_unknown_verb() {
        assert!(parse_request("reload web").is_err());
    }

    #[test]
    fn parse_request_rejects_missing_name() {
        assert!(parse_request("stop").is_err());
        assert!(parse_request("stop ").is_err());
    }

    #[test]
    fn parse_request_rejects_empty_line() {
        assert!(parse_request("").is_err());
    }

    #[test]
    fn response_roundtrips_through_render_and_parse() {
        for resp in [
            Response::Ok(None),
            Response::Ok(Some("stopping \"web\"".into())),
            Response::Error("no such process \"web\"".into()),
        ] {
            assert_eq!(parse_response(&render_response(&resp)), resp);
        }
    }

    // ---- default paths ----

    #[test]
    fn default_socket_path_prefers_xdg() {
        let sock = default_socket_path_from(Some("/run/user/1000"));
        assert_eq!(
            sock,
            PathBuf::from("/run/user/1000/supervisor-rs/control.sock")
        );
        // Same directory as the state file, so isolation of one isolates both.
        assert_eq!(
            sock.parent(),
            state::default_path_from(Some("/run/user/1000")).parent()
        );
    }

    #[test]
    fn default_socket_path_falls_back_to_tmp() {
        let sock = default_socket_path_from(None);
        assert_eq!(sock.file_name().unwrap(), "control.sock");
        assert_eq!(sock.parent(), state::default_path_from(None).parent());
        // An empty variable is as good as unset.
        assert_eq!(default_socket_path_from(Some("")), sock);
    }

    // ---- socket (real UnixListener/UnixStream, short file names: sun_path) ----

    #[test]
    fn bind_creates_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let server = ControlServer::bind(&path).unwrap();
        assert!(path.exists(), "bind did not create the socket file");
        server.cleanup();
        assert!(!path.exists(), "cleanup did not remove the socket file");
    }

    #[test]
    fn bind_removes_orphaned_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        // Bind and drop the listener, leaving the file behind — a daemon that
        // died without cleaning up.
        {
            let listener = UnixListener::bind(&path).unwrap();
            drop(listener);
        }
        assert!(path.exists(), "precondition: orphaned file present");
        let server = ControlServer::bind(&path).expect("bind over an orphaned socket must succeed");
        server.cleanup();
    }

    #[test]
    fn bind_refuses_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let _first = ControlServer::bind(&path).unwrap();
        match ControlServer::bind(&path) {
            Err(BindError::AlreadyRunning(p)) => assert_eq!(p, path),
            Err(other) => panic!("expected AlreadyRunning, got {other:?}"),
            Ok(_) => panic!("second bind on a live socket must fail"),
        }
    }

    #[test]
    fn try_accept_returns_none_without_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let server = ControlServer::bind(&path).unwrap();
        assert!(server.try_accept().is_none());
    }

    #[test]
    fn request_response_roundtrip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let server = ControlServer::bind(&path).unwrap();

        let client_path = path.clone();
        let client = thread::spawn(move || {
            send_command(&client_path, &Request::Stop("web".into())).unwrap()
        });

        // Poll the non-blocking accept with a deadline.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            if let Some(stream) = server.try_accept() {
                break stream;
            }
            assert!(Instant::now() < deadline, "no client connected in time");
            thread::sleep(Duration::from_millis(5));
        };
        let req = read_request(&mut stream).unwrap();
        assert_eq!(req, Request::Stop("web".into()));
        respond(&mut stream, &Response::Ok(Some("stopping \"web\"".into())));

        let resp = client.join().unwrap();
        assert_eq!(resp, Response::Ok(Some("stopping \"web\"".into())));
    }

    #[test]
    fn read_request_times_out_on_silent_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.sock");
        let server = ControlServer::bind(&path).unwrap();

        // Client connects and then stays silent.
        let client_path = path.clone();
        let client = thread::spawn(move || {
            let _stream = UnixStream::connect(&client_path).unwrap();
            thread::sleep(Duration::from_secs(1));
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut stream = loop {
            if let Some(stream) = server.try_accept() {
                break stream;
            }
            assert!(Instant::now() < deadline, "no client connected in time");
            thread::sleep(Duration::from_millis(5));
        };

        let start = Instant::now();
        let result = read_request(&mut stream);
        assert!(
            result.is_err(),
            "silent client must time out, got {result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "read_request did not honour its timeout ({:?})",
            start.elapsed()
        );

        client.join().unwrap();
    }
}
