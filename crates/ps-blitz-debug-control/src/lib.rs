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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    let mut active_session: Option<String> = None;
    for connection in listener.incoming() {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match connection {
            Ok(mut stream) => {
                let _ = stream.set_read_timeout(Some(COMMAND_TIMEOUT));
                let _ = stream.set_write_timeout(Some(COMMAND_TIMEOUT));
                let response = match read_request(&mut stream) {
                    Ok(request) => route(request, token, &mut active_session, &command_tx, waker),
                    Err(error) => webdriver_error("invalid argument", error.to_string()),
                };
                let _ = write_response(&mut stream, response);
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
    let mut bytes = Vec::with_capacity(4096);
    let header_end = loop {
        if bytes.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request is too large",
            ));
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before headers",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
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
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .unwrap_or(0);
    if header_end + content_length > MAX_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request is too large",
        ));
    }
    while bytes.len() < header_end + content_length {
        let remaining = header_end + content_length - bytes.len();
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
        serde_json::from_slice(&bytes[header_end..header_end + content_length])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    };
    Ok(HttpRequest { method, path, body })
}

fn route(
    request: HttpRequest,
    token: &str,
    active_session: &mut Option<String>,
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
        if active_session.is_some() {
            return webdriver_error("session not created", "only one session is supported");
        }
        let supplied_token = request
            .body
            .pointer("/capabilities/alwaysMatch/blitz:token")
            .and_then(Value::as_str);
        if !token_matches(supplied_token, token) {
            return webdriver_error("invalid argument", "invalid blitz:token capability");
        }
        let session_id = match random_hex(16) {
            Ok(value) => value,
            Err(error) => return webdriver_error("unknown error", error.to_string()),
        };
        *active_session = Some(session_id.clone());
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
    if active_session.as_deref() != Some(session_id) {
        return webdriver_error("invalid session id", "session is not active");
    }
    if request.method == "DELETE" && command_path.is_empty() {
        *active_session = None;
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
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    fn snapshot_request() -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            path: "/session/test-session/blitz/getDomSnapshot".into(),
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

        let mut active_session = Some("test-session".to_string());
        let response = route(
            snapshot_request(),
            "token",
            &mut active_session,
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

        let mut active_session = Some("test-session".to_string());
        let response = route(
            snapshot_request(),
            "token",
            &mut active_session,
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
        let mut active_session = Some("test-session".to_string());

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
            &mut active_session,
            &command_tx,
            &waker,
        );

        servicer.join().unwrap();
        assert_eq!(response["value"]["serviced"], true);
    }
}
