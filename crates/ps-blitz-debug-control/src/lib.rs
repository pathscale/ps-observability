//! Debug-only, loopback WebDriver transport for Blitz renderers.
//!
//! The server owns networking and session authentication. Renderer commands
//! cross a serialized channel and must be executed by the UI/runtime thread.
//!
//! # Security boundary
//!
//! Enabling this server grants the holder of its per-process discovery token
//! debugger-level control, including arbitrary JavaScript execution in the
//! document. It deliberately has the same trust posture as a browser remote
//! debugging port: loopback-only transport, an unpredictable token, and a
//! descriptor created with owner-only permissions on Unix. It is not a sandbox
//! or an authorization boundary against another process running as the same
//! OS user and able to read that user's files. Production builds should leave
//! the feature and its environment variables disabled.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct ActiveSession(AtomicU64);

impl ActiveSession {
    fn create(&self) -> io::Result<Option<String>> {
        let value = loop {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes).map_err(io::Error::other)?;
            let value = u64::from_ne_bytes(bytes);
            if value != 0 {
                break value;
            }
        };
        match self
            .0
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(Some(format!("{value:016x}"))),
            Err(_) => Ok(None),
        }
    }

    fn matches(&self, supplied: &str) -> bool {
        u64::from_str_radix(supplied, 16)
            .ok()
            .is_some_and(|value| value != 0 && self.0.load(Ordering::Acquire) == value)
    }

    fn clear(&self) {
        self.0.store(0, Ordering::Release);
    }
}

/// Configuration for a loopback debug-control server.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Loopback address to bind. Port zero asks the OS to choose a free port.
    pub bind_address: SocketAddr,
    /// Atomically written once the server is accepting connections.
    pub descriptor_path: PathBuf,
    /// Git revision or build identifier for the renderer.
    pub renderer_revision: String,
}

/// Wakes whichever thread services requests, so it does not have to poll for
/// them.
///
/// The server cannot know how to wake its embedder, and the embedder's event
/// loop does not exist yet when the server starts, so the callback is installed
/// later. A `OnceLock` rather than a lock because it is written exactly once
/// and read on the path of every request.
///
/// Without one installed the embedder must poll, which is what the Blitz shell
/// used to do on a 10ms timer: 100 wakeups a second while idle, up to 10ms of
/// latency on every command, and enough of both to show up in any measurement
/// taken with the driver attached.
#[derive(Clone, Default)]
pub struct ServiceWaker(Arc<OnceLock<Box<dyn Fn() + Send + Sync>>>);

impl ServiceWaker {
    /// Install the wake callback. Later calls are ignored.
    pub fn set(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.0.set(Box::new(wake));
    }

    fn wake(&self) {
        if let Some(wake) = self.0.get() {
            wake();
        }
    }
}

impl std::fmt::Debug for ServiceWaker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceWaker")
            .field("installed", &self.0.get().is_some())
            .finish()
    }
}

/// A command forwarded from the HTTP server to the renderer thread.
#[derive(Debug)]
pub struct ControlRequest {
    pub method: String,
    pub path: String,
    pub body: Value,
    reply: SyncSender<ControlResponse>,
}

impl ControlRequest {
    /// Complete this request. Failure means the client already disconnected.
    pub fn respond(self, response: ControlResponse) -> Result<(), ControlResponse> {
        self.reply.send(response).map_err(|error| error.0)
    }
}

/// A renderer response represented using W3C WebDriver success/error values.
#[derive(Debug)]
pub enum ControlResponse {
    Success(Value),
    Error {
        error: String,
        message: String,
        stacktrace: String,
    },
}

impl ControlResponse {
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Error {
            error: "unsupported operation".into(),
            message: message.into(),
            stacktrace: String::new(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor<'a> {
    pid: u32,
    address: String,
    token: &'a str,
    protocol_version: u32,
    renderer: &'static str,
    renderer_revision: &'a str,
}

/// Running server. Dropping it shuts down the listener and removes discovery.
pub struct DebugServer {
    address: SocketAddr,
    token: String,
    descriptor_path: PathBuf,
    shutdown: Arc<AtomicBool>,
    waker: ServiceWaker,
    thread: Option<JoinHandle<()>>,
}

impl DebugServer {
    /// Bind a loopback port, write the descriptor, and start the server thread.
    pub fn start(config: ServerConfig) -> io::Result<(Self, Receiver<ControlRequest>)> {
        if !config.bind_address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "debug control must bind to a loopback address",
            ));
        }
        let listener = TcpListener::bind(config.bind_address)?;
        let address = listener.local_addr()?;
        let token = random_hex(32)?;
        // Unbounded, where this was `sync_channel(1)` serviced by a poll. A
        // request that arrives before the embedder can service it (no document
        // yet, say) used to occupy the only slot, and the next one was refused
        // outright with "renderer command queue is full".
        let (command_tx, command_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let waker = ServiceWaker::default();

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_token = token.clone();
        let thread_waker = waker.clone();
        let thread = thread::Builder::new()
            .name("blitz-debug-control".into())
            .spawn(move || {
                server_loop(
                    listener,
                    &thread_token,
                    command_tx,
                    &thread_waker,
                    thread_shutdown,
                )
            })?;

        if let Err(error) = write_descriptor(&config, address, &token) {
            shutdown.store(true, Ordering::Release);
            let _ = TcpStream::connect(address);
            let _ = thread.join();
            return Err(error);
        }

        Ok((
            Self {
                address,
                token,
                descriptor_path: config.descriptor_path,
                shutdown,
                waker,
                thread: Some(thread),
            },
            command_rx,
        ))
    }

    /// Handle for telling the server how to wake the thread that services
    /// requests. Nothing wakes until a callback is installed.
    pub fn waker(&self) -> ServiceWaker {
        self.waker.clone()
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.descriptor_path);
    }
}

impl Drop for DebugServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn random_hex(byte_len: usize) -> io::Result<String> {
    let mut bytes = vec![0; byte_len];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut output = String::with_capacity(byte_len * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").unwrap();
    }
    Ok(output)
}

fn write_descriptor(config: &ServerConfig, address: SocketAddr, token: &str) -> io::Result<()> {
    let descriptor = Descriptor {
        pid: std::process::id(),
        address: address.to_string(),
        token,
        protocol_version: PROTOCOL_VERSION,
        renderer: "blitz",
        renderer_revision: &config.renderer_revision,
    };
    let bytes = serde_json::to_vec_pretty(&descriptor).map_err(io::Error::other)?;
    if let Some(parent) = config.descriptor_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = config
        .descriptor_path
        .with_extension(format!("tmp-{}", random_hex(8)?));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &config.descriptor_path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn server_loop(
    listener: TcpListener,
    token: &str,
    command_tx: Sender<ControlRequest>,
    waker: &ServiceWaker,
    shutdown: Arc<AtomicBool>,
) {
    let active_session = Arc::new(ActiveSession::default());
    let token: Arc<str> = Arc::from(token);
    for connection in listener.incoming() {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match connection {
            Ok(mut stream) => {
                let token = Arc::clone(&token);
                let active_session = Arc::clone(&active_session);
                let command_tx = command_tx.clone();
                let waker = waker.clone();
                let _ = thread::Builder::new()
                    .name("blitz-debug-connection".into())
                    .spawn(move || {
                        let _ = stream.set_write_timeout(Some(COMMAND_TIMEOUT));
                        let response = match read_request(&mut stream) {
                            Ok(request) => {
                                route(request, &token, &active_session, &command_tx, &waker)
                            }
                            Err(error) => webdriver_error("invalid argument", error.to_string()),
                        };
                        let _ = write_response(&mut stream, response);
                    });
            }
            Err(_) if shutdown.load(Ordering::Acquire) => break,
            Err(_) => continue,
        }
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: Value,
}

fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    read_request_within(stream, COMMAND_TIMEOUT)
}

fn read_request_within(stream: &mut TcpStream, within: Duration) -> io::Result<HttpRequest> {
    let deadline = Instant::now() + within;
    let mut bytes = Vec::with_capacity(4096);
    let mut scan_from = 0;
    let header_end = loop {
        if bytes.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request is too large",
            ));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out"));
        }
        stream.set_read_timeout(Some(remaining))?;
        let mut chunk = [0; 4096];
        let allowed = (MAX_REQUEST_BYTES - bytes.len()).min(chunk.len());
        let count = stream.read(&mut chunk[..allowed])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        let prior_len = bytes.len();
        bytes.extend_from_slice(&chunk[..count]);
        scan_from = scan_from.min(prior_len.saturating_sub(3));
        if let Some(index) = bytes[scan_from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            break scan_from + index + 4;
        }
        scan_from = bytes.len().saturating_sub(3);
    };

    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_string();
    let path = request_line.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid request line",
        ));
    }
    let header_fields: Vec<_> = lines.filter_map(|line| line.split_once(':')).collect();
    let content_length = header_fields
        .iter()
        .copied()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .unwrap_or(0);
    if header_fields.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && !value.trim().eq_ignore_ascii_case("identity")
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transfer-encoding is unsupported; send Content-Length",
        ));
    }
    let body_end = header_end
        .checked_add(content_length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request size overflow"))?;
    if body_end > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request is too large",
        ));
    }
    while bytes.len() < body_end {
        let deadline_remaining = deadline.saturating_duration_since(Instant::now());
        if deadline_remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "request timed out"));
        }
        stream.set_read_timeout(Some(deadline_remaining))?;
        let remaining = body_end - bytes.len();
        let mut chunk = vec![0; remaining.min(4096)];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before body",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = if content_length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..body_end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    };
    Ok(HttpRequest { method, path, body })
}

fn route(
    request: HttpRequest,
    token: &str,
    active_session: &ActiveSession,
    command_tx: &Sender<ControlRequest>,
    waker: &ServiceWaker,
) -> Value {
    if request.method == "GET" && request.path == "/status" {
        return json!({"value": {
            "ready": true,
            "message": "Blitz debug control is ready",
            "protocolVersion": PROTOCOL_VERSION,
        }});
    }

    if request.method == "POST" && request.path == "/session" {
        let supplied_token = request
            .body
            .pointer("/capabilities/alwaysMatch/blitz:token")
            .and_then(Value::as_str);
        if !token_matches(supplied_token, token) {
            return webdriver_error("invalid argument", "invalid blitz:token capability");
        }
        let session_id = match active_session.create() {
            Ok(Some(value)) => value,
            Ok(None) => {
                return webdriver_error("session not created", "only one session is supported");
            }
            Err(error) => return webdriver_error("unknown error", error.to_string()),
        };
        return json!({"value": {
            "sessionId": session_id,
            "capabilities": {
                "browserName": "blitz",
                "blitz:protocolVersion": PROTOCOL_VERSION,
            }
        }});
    }

    let Some((session_id, command_path)) = session_path(&request.path) else {
        return webdriver_error("unknown command", "unknown debug-control route");
    };
    if !active_session.matches(session_id) {
        return webdriver_error("invalid session id", "session is not active");
    }
    if request.method == "DELETE" && command_path.is_empty() {
        active_session.clear();
        return json!({"value": null});
    }

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let control_request = ControlRequest {
        method: request.method,
        path: command_path.to_string(),
        body: request.body,
        reply: reply_tx,
    };
    if command_tx.send(control_request).is_err() {
        return webdriver_error("unknown error", "renderer command channel is closed");
    }
    // Queue first, then wake: the embedder must find the request already there
    // when it comes round, or the wake is spent on an empty queue.
    waker.wake();
    match reply_rx.recv_timeout(COMMAND_TIMEOUT) {
        Ok(ControlResponse::Success(value)) => json!({"value": value}),
        Ok(ControlResponse::Error {
            error,
            message,
            stacktrace,
        }) => json!({"value": {
            "error": error,
            "message": message,
            "stacktrace": stacktrace,
        }}),
        Err(RecvTimeoutError::Timeout) => webdriver_error("timeout", "renderer command timed out"),
        Err(RecvTimeoutError::Disconnected) => {
            webdriver_error("unknown error", "renderer response channel is closed")
        }
    }
}

/// Compare an attacker-controlled capability with the fixed-size discovery
/// token without leaking how many prefix bytes matched.
fn token_matches(supplied: Option<&str>, expected: &str) -> bool {
    let supplied = supplied.unwrap_or_default().as_bytes();
    let expected = expected.as_bytes();
    let mut difference = supplied.len() ^ expected.len();
    for (index, expected_byte) in expected.iter().enumerate() {
        difference |= usize::from(*expected_byte ^ supplied.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

fn session_path(path: &str) -> Option<(&str, &str)> {
    let remainder = path.strip_prefix("/session/")?;
    let (session_id, command) = remainder.split_once('/').unwrap_or((remainder, ""));
    Some((session_id, command))
}

fn webdriver_error(error: &str, message: impl Into<String>) -> Value {
    json!({"value": {
        "error": error,
        "message": message.into(),
        "stacktrace": "",
    }})
}

fn write_response(stream: &mut TcpStream, body: Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(&body).map_err(io::Error::other)?;
    let status = match body.pointer("/value/error").and_then(Value::as_str) {
        None => "200 OK",
        Some("unknown command" | "invalid session id") => "404 Not Found",
        Some("timeout" | "unknown error") => "500 Internal Server Error",
        Some(_) => "400 Bad Request",
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    )?;
    stream.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn token_comparison_rejects_prefixes_suffixes_and_missing_values() {
        assert!(token_matches(Some("012345"), "012345"));
        assert!(!token_matches(Some("01234x"), "012345"));
        assert!(!token_matches(Some("0123456"), "012345"));
        assert!(!token_matches(Some("01234"), "012345"));
        assert!(!token_matches(None, "012345"));
    }

    fn descriptor_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("blitz-debug-{nonce}.json"))
    }

    fn request(address: SocketAddr, method: &str, path: &str, body: Value) -> Value {
        let body = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(&body).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let body_start = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        serde_json::from_slice(&response[body_start..]).unwrap()
    }

    fn raw_request(address: SocketAddr, request: &str) -> Value {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let body_start = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        serde_json::from_slice(&response[body_start..]).unwrap()
    }

    fn create_session(address: SocketAddr, token: &str) -> String {
        request(
            address,
            "POST",
            "/session",
            json!({"capabilities": {"alwaysMatch": {"blitz:token": token}}}),
        )["value"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn status_auth_session_command_and_reconnect() {
        let descriptor = descriptor_path();
        let (server, commands) = DebugServer::start(ServerConfig {
            bind_address: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            descriptor_path: descriptor.clone(),
            renderer_revision: "test-revision".into(),
        })
        .unwrap();

        let status = request(server.address(), "GET", "/status", Value::Null);
        assert_eq!(status["value"]["ready"], true);
        assert!(descriptor.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&descriptor).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let rejected = request(
            server.address(),
            "POST",
            "/session",
            json!({"capabilities": {"alwaysMatch": {"blitz:token": "wrong"}}}),
        );
        assert_eq!(rejected["value"]["error"], "invalid argument");

        let session = create_session(server.address(), server.token());
        let duplicate = request(
            server.address(),
            "POST",
            "/session",
            json!({"capabilities": {"alwaysMatch": {"blitz:token": server.token()}}}),
        );
        assert_eq!(duplicate["value"]["error"], "session not created");
        assert_eq!(
            request(
                server.address(),
                "GET",
                "/session/not-the-session/blitz/getDomSnapshot",
                Value::Null,
            )["value"]["error"],
            "invalid session id"
        );
        assert_eq!(
            request(server.address(), "GET", "/not-a-route", Value::Null)["value"]["error"],
            "unknown command"
        );
        let address = server.address();
        let command_path = format!("/session/{session}/blitz/getDomSnapshot");
        let client = thread::spawn(move || request(address, "GET", &command_path, Value::Null));
        let command = commands.recv_timeout(COMMAND_TIMEOUT).unwrap();
        assert_eq!(command.method, "GET");
        assert_eq!(command.path, "blitz/getDomSnapshot");
        command
            .respond(ControlResponse::Success(json!({"documentRevision": 7})))
            .unwrap();
        let response = client.join().unwrap();
        assert_eq!(response["value"]["documentRevision"], 7);

        let deleted = request(
            server.address(),
            "DELETE",
            &format!("/session/{session}"),
            Value::Null,
        );
        assert!(deleted["value"].is_null());
        let second_session = create_session(server.address(), server.token());
        assert_ne!(second_session, session);

        server.shutdown();
        assert!(!descriptor.exists());
    }

    #[test]
    fn rejects_non_loopback_bind_address() {
        let result = DebugServer::start(ServerConfig {
            bind_address: ([0, 0, 0, 0], 0).into(),
            descriptor_path: descriptor_path(),
            renderer_revision: "test-revision".into(),
        });
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidInput));
    }

    #[test]
    fn overflowing_content_length_is_rejected_without_killing_the_server() {
        let (server, _commands) = DebugServer::start(ServerConfig {
            bind_address: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            descriptor_path: descriptor_path(),
            renderer_revision: "test-revision".into(),
        })
        .unwrap();

        let rejected = raw_request(
            server.address(),
            &format!(
                "POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                usize::MAX
            ),
        );
        assert_eq!(rejected["value"]["error"], "invalid argument");
        let malformed_length = raw_request(
            server.address(),
            "POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: nope\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(malformed_length["value"]["error"], "invalid argument");
        let malformed_json = raw_request(
            server.address(),
            "POST /session HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{",
        );
        assert_eq!(malformed_json["value"]["error"], "invalid argument");
        assert_eq!(
            request(server.address(), "GET", "/status", Value::Null)["value"]["ready"],
            true
        );
        server.shutdown();
    }

    #[test]
    fn a_slow_client_does_not_block_other_connections_and_has_an_absolute_deadline() {
        let (server, _commands) = DebugServer::start(ServerConfig {
            bind_address: (std::net::Ipv4Addr::LOCALHOST, 0).into(),
            descriptor_path: descriptor_path(),
            renderer_revision: "test-revision".into(),
        })
        .unwrap();
        let mut slow = TcpStream::connect(server.address()).unwrap();
        slow.write_all(b"G").unwrap();
        let started = Instant::now();
        assert_eq!(
            request(server.address(), "GET", "/status", Value::Null)["value"]["ready"],
            true
        );
        assert!(started.elapsed() < Duration::from_millis(500));

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let reader = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_within(&mut stream, Duration::from_millis(40)).unwrap_err()
        });
        let mut drip = TcpStream::connect(address).unwrap();
        drip.write_all(b"G").unwrap();
        let error = reader.join().unwrap();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
        ));
        server.shutdown();
    }

    fn snapshot_request() -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            path: "/session/0000000000000001/blitz/getDomSnapshot".into(),
            body: Value::Null,
        }
    }

    /*
     * The wake has to arrive after the request is queued, never before: a wake
     * delivered to an empty queue is spent, and the embedder has no timer to
     * fall back on any more. Draining inside the callback is exactly what the
     * event loop does when it comes round, so servicing here proves the
     * ordering rather than asserting it indirectly.
     */
    #[test]
    fn wakes_the_servicer_with_the_request_already_queued() {
        let (command_tx, command_rx) = mpsc::channel::<ControlRequest>();
        let waker = ServiceWaker::default();
        let queue = Mutex::new(command_rx);
        waker.set(move || {
            let queued = queue.lock().unwrap().try_recv().unwrap();
            assert_eq!(queued.path, "blitz/getDomSnapshot");
            queued
                .respond(ControlResponse::Success(json!({"serviced": true})))
                .unwrap();
        });

        let active_session = ActiveSession(AtomicU64::new(1));
        let response = route(
            snapshot_request(),
            "token",
            &active_session,
            &command_tx,
            &waker,
        );

        assert_eq!(response["value"]["serviced"], true);
    }

    /*
     * `sync_channel(1)` refused this outright with "renderer command queue is
     * full". One unserviced request is normal, not a fault: it happens whenever
     * a command arrives before the embedder has a document to run it against.
     */
    #[test]
    fn a_second_request_queues_behind_an_unserviced_one() {
        let (command_tx, command_rx) = mpsc::channel::<ControlRequest>();
        let (reply_tx, _reply_rx) = mpsc::sync_channel(1);
        command_tx
            .send(ControlRequest {
                method: "GET".into(),
                path: "occupied".into(),
                body: Value::Null,
                reply: reply_tx,
            })
            .unwrap();

        let waker = ServiceWaker::default();
        let queue = Mutex::new(command_rx);
        waker.set(move || {
            let queue = queue.lock().unwrap();
            assert_eq!(queue.try_recv().unwrap().path, "occupied");
            queue
                .try_recv()
                .unwrap()
                .respond(ControlResponse::Success(json!({"serviced": true})))
                .unwrap();
        });

        let active_session = ActiveSession(AtomicU64::new(1));
        let response = route(
            snapshot_request(),
            "token",
            &active_session,
            &command_tx,
            &waker,
        );

        assert_eq!(response["value"]["serviced"], true);
    }

    /// Nothing installs a waker until the embedder is up, and requests that
    /// arrive in that window must still be queued rather than dropped.
    #[test]
    fn queues_requests_while_no_waker_is_installed() {
        let (command_tx, command_rx) = mpsc::channel::<ControlRequest>();
        let waker = ServiceWaker::default();
        let active_session = ActiveSession(AtomicU64::new(1));

        let sender = command_tx.clone();
        let servicer = thread::spawn(move || {
            let queued = command_rx.recv_timeout(COMMAND_TIMEOUT).unwrap();
            drop(sender);
            queued
                .respond(ControlResponse::Success(json!({"serviced": true})))
                .unwrap();
        });

        let response = route(
            snapshot_request(),
            "token",
            &active_session,
            &command_tx,
            &waker,
        );

        servicer.join().unwrap();
        assert_eq!(response["value"]["serviced"], true);
    }
}
