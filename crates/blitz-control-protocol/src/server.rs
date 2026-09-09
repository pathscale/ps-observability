use std::fs::{OpenOptions, remove_file};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use endpoint_libs::libs::ws::mcp_wire::{INVALID_REQUEST, JsonRpcError};
use endpoint_libs::libs::ws::transport::{TransportStream, framed_json};
use endpoint_libs::libs::ws::{MessageStream, StreamError, WireMessage};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, watch};

use crate::{
    AgentControlRequest, DebugDescriptor, DebugEvent, DebugResponse, DebugStream,
    DiagnosticsRequest, IncomingRequest, decode_incoming_value, decode_wire_value,
    encode_diagnostics_event, encode_initialize_response, encode_response, encode_rpc_error,
    encode_tools_list_response, peek_value_request_id,
};

pub enum ControlBridgeRequest {
    Agent(AgentControlRequest),
    Diagnostics(DiagnosticsRequest),
}

/// Who is answering, and what they can answer.
///
/// # Why the server has to be told
///
/// It used to read this off its own cargo features: the tool list advertised
/// diagnostics when the crate had been built with them, and `serverInfo` said
/// `tauri-runtime-blitz` because that is where the code lived. Both were
/// guesses that happened to hold while there was one host.
///
/// There are three now, and none of them is this crate. The window runtime,
/// the headless browser and an embedder driving a browser in-process each have
/// their own name, their own version and their own answer to whether they can
/// collect a snapshot, and a transport cannot infer any of it. A host that
/// cannot collect diagnostics says so here, and its bridge answers the request
/// with an error rather than the server failing to compile the arm.
#[derive(Debug, Clone)]
pub struct Host {
    /// What the process serving this socket is called. Reported as MCP
    /// `serverInfo.name`.
    pub name: String,
    /// That process's version. Reported as `serverInfo.version` and as the
    /// descriptor's `rendererRevision`.
    pub version: String,
    /// Whether `blitz.diagnostics` is worth advertising in `tools/list`.
    ///
    /// Advertising a tool that answers every call with an error is worse than
    /// omitting it: a client reports the application as broken instead of as a
    /// plain build.
    pub diagnostics: bool,
}

/// How the server reaches whatever is holding the document.
///
/// A plain closure, deliberately: the server binds a socket, frames requests
/// and hands them over, and knows nothing about windows, event loops or Tauri.
/// The crate's own tests have always constructed one from a bare closure with
/// nothing running, which is the proof that a host does not need a window to
/// serve inspection.
///
/// Public so a headless host can serve one. While this was `pub(crate)` the
/// only way to inspect a Blitz document from outside was to open a window,
/// which is what pushed a QA harness into screenshot and tree-file workarounds
/// that could not answer any question involving a click.
pub type ControlBridge =
    Arc<dyn Fn(ControlBridgeRequest) -> oneshot::Receiver<DebugResponse> + Send + Sync + 'static>;

#[cfg(test)]
pub(crate) static CONTROL_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct AgentControlServer {
    descriptor_path: PathBuf,
    socket_path: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    /// Holds deep-profiling collection open while a tool can attach.
    ///
    /// The consumer is out of process, so it cannot hold a session itself and
    /// the server holds one on its behalf. The server's lifetime is the right
    /// one: it exists exactly while the socket is listening, and a per-request
    /// session would be useless, because the frames a snapshot reports were
    /// presented before the request arrived.
    ///
    /// `None` when the profile does not permit sampling, which is the ordinary
    /// case: inspection without deep profiling stays free.
    ///
    /// Only a build that can collect diagnostics has anything to sample, which
    /// is why this is the one place the transport touches blitz at all, and why
    /// it is behind `capture` rather than `server`.
    #[cfg(feature = "capture")]
    _sampling: Option<blitz_shell::DeepProfilingSession>,
}

impl AgentControlServer {
    /// Bind the inspection socket and announce the descriptor.
    ///
    /// Nothing here needs a window: it creates a Unix listener, writes a
    /// descriptor file and serves frames from a thread. A headless host that
    /// owns a document can call this and be inspected exactly like the real
    /// application, which is what lets a QA sweep run with no display server
    /// and still answer questions that require clicking.
    pub fn start(bridge: ControlBridge, host: Host) -> io::Result<Self> {
        Self::start_inner(bridge, host, None)
    }

    /// Start with a bounded latest-value diagnostic event source.
    ///
    /// A watch receiver is deliberate: a slow inspector needs the newest
    /// revision, not an ever-growing queue of every frame it failed to read.
    pub fn start_with_events(
        bridge: ControlBridge,
        host: Host,
        events: watch::Receiver<Option<DebugEvent>>,
    ) -> io::Result<Self> {
        Self::start_inner(bridge, host, Some(events))
    }

    fn start_inner(
        bridge: ControlBridge,
        host: Host,
        events: Option<watch::Receiver<Option<DebugEvent>>>,
    ) -> io::Result<Self> {
        let instance_id = instance_id();
        let descriptor_path = descriptor_path(&instance_id);
        let socket_path = descriptor_path.with_extension("sock");
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = remove_file(&socket_path);

        let listener = std::os::unix::net::UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

        let descriptor = DebugDescriptor {
            protocol_version: crate::DEBUG_PROTOCOL_VERSION,
            pid: std::process::id(),
            instance_id,
            address: format!("unix://{}", socket_path.display()),
            renderer: "blitz".into(),
            renderer_revision: host.version.clone(),
        };
        write_descriptor(&descriptor_path, &descriptor)?;
        reap_dead_descriptors(&descriptor_path);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("blitz-agent-control".into())
            .spawn(move || run(listener, bridge, host, events, shutdown_rx))?;

        Ok(Self {
            descriptor_path,
            socket_path,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
            // Taken after the socket is listening, so a permitted profile
            // begins collecting for the tool that is now able to attach, and
            // stops again when this server is dropped.
            #[cfg(feature = "capture")]
            _sampling: blitz_shell::begin_deep_profiling(),
        })
    }

    /// Where this server announced itself.
    ///
    /// A headless host prints this so a client can attach to that exact
    /// instance. Searching the descriptor directory instead races every other
    /// instance on the machine, and a component sweep runs one host after
    /// another, so the newest descriptor is not reliably the right one.
    ///
    /// Was `#[cfg(test)]`, because inside this crate the runtime already knows
    /// where it wrote the descriptor and only a test needed to ask.
    pub fn descriptor_path(&self) -> &Path {
        &self.descriptor_path
    }

    /// Reacquire the sampling session after an embedder changes permission.
    ///
    /// The server can start before application settings or CLI overrides are
    /// loaded. In that order `_sampling` begins as `None`; merely granting
    /// permission later does not mutate an already-running server, so every
    /// diagnostic snapshot keeps reporting `script: null` until restart.
    ///
    /// Public because the embedder that owns the setting is no longer in this
    /// crate. It was `pub(crate)`, called by the runtime that lived beside it.
    #[cfg(feature = "capture")]
    pub fn refresh_deep_profiling(&mut self) {
        self._sampling = blitz_shell::begin_deep_profiling();
    }

    /// The socket this server is listening on.
    ///
    /// A caller that already knows the descriptor path can derive this, and
    /// several do; saying it directly is cheaper than agreeing on the
    /// extension by convention.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for AgentControlServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = remove_file(&self.socket_path);
        let _ = remove_file(&self.descriptor_path);
    }
}

fn run(
    listener: std::os::unix::net::UnixListener,
    bridge: ControlBridge,
    host: Host,
    events: Option<watch::Receiver<Option<DebugEvent>>>,
    shutdown: oneshot::Receiver<()>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
    else {
        return;
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let Ok(listener) = UnixListener::from_std(listener) else {
            return;
        };
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        let bridge = Arc::clone(&bridge);
                        let host = host.clone();
                        let events = events.clone();
                        tokio::task::spawn_local(async move {
                            handle_connection(stream, bridge, host, events).await;
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    });
}

enum ConnectionInput {
    Message(Option<Result<WireMessage, StreamError>>),
    Event(Result<(), watch::error::RecvError>),
}

fn stream_wants_event(streams: &[DebugStream], event: &DebugEvent) -> bool {
    match event {
        DebugEvent::Snapshot(_) => streams.contains(&DebugStream::Snapshots),
        DebugEvent::Metrics(_) => streams.contains(&DebugStream::Metrics),
        DebugEvent::Console(_) => streams.contains(&DebugStream::Console),
        DebugEvent::RuntimeError(_) => streams.contains(&DebugStream::RuntimeErrors),
        DebugEvent::PaintCommitted { .. } => streams.contains(&DebugStream::Paint),
    }
}

async fn handle_connection(
    stream: UnixStream,
    bridge: ControlBridge,
    host: Host,
    mut events: Option<watch::Receiver<Option<DebugEvent>>>,
) {
    let mut stream = TransportStream::new(framed_json(stream));
    let mut observed = Vec::<DebugStream>::new();
    loop {
        let input = if observed.is_empty() {
            ConnectionInput::Message(stream.recv().await)
        } else if let Some(receiver) = events.as_mut() {
            tokio::select! {
                message = stream.recv() => ConnectionInput::Message(message),
                changed = receiver.changed() => ConnectionInput::Event(changed),
            }
        } else {
            ConnectionInput::Message(stream.recv().await)
        };

        let message = match input {
            ConnectionInput::Event(changed) => {
                if changed.is_err() {
                    events = None;
                    continue;
                }
                let Some(event) = events
                    .as_ref()
                    .and_then(|receiver| receiver.borrow().clone())
                else {
                    continue;
                };
                if !stream_wants_event(&observed, &event) {
                    continue;
                }
                let Ok(frame) = encode_diagnostics_event(&event) else {
                    continue;
                };
                if stream.send(frame).await.is_err() {
                    break;
                }
                continue;
            }
            ConnectionInput::Message(Some(message)) => message,
            ConnectionInput::Message(None) => break,
        };
        let response = match message {
            Ok(message) => match decode_wire_value(message) {
                Ok(value) => {
                    // Recover the id before typed decoding consumes the parsed
                    // value. The previous path parsed the complete request once
                    // for this id and again for the request itself.
                    let request_id = peek_value_request_id(&value);
                    match decode_incoming_value(value) {
                        Ok(IncomingRequest::Initialize { id }) => {
                            encode_initialize_response(id, &host.name, &host.version)
                        }
                        Ok(IncomingRequest::Initialized) => continue,
                        Ok(IncomingRequest::ToolsList { id }) => {
                            encode_tools_list_response(id, host.diagnostics)
                        }
                        Ok(IncomingRequest::Agent { id, request }) => {
                            let response = bridge(ControlBridgeRequest::Agent(request))
                                .await
                                .unwrap_or_else(|_| {
                                    DebugResponse::Error(crate::DebugError {
                                        code: "bridgeClosed".into(),
                                        message: "the UI-thread control bridge closed".into(),
                                    })
                                });
                            encode_response(id, &response)
                        }
                        // The protocol defines diagnostics unconditionally, and
                        // so does this transport. Whether they can be collected
                        // is the host's answer, given in `Host::diagnostics`
                        // for the tool list and in its bridge for a call that
                        // arrives anyway.
                        Ok(IncomingRequest::Diagnostics {
                            id,
                            request: DiagnosticsRequest::Observe { streams },
                        }) => {
                            observed = streams;
                            // Arming observation establishes a revision baseline.
                            // Do not immediately replay a paint that happened
                            // before the action the caller is about to drive.
                            if let Some(receiver) = events.as_mut() {
                                receiver.borrow_and_update();
                            }
                            encode_response(id, &DebugResponse::Ack)
                        }
                        Ok(IncomingRequest::Diagnostics { id, request }) => {
                            let response = bridge(ControlBridgeRequest::Diagnostics(request))
                                .await
                                .unwrap_or_else(|_| {
                                    DebugResponse::Error(crate::DebugError {
                                        code: "bridgeClosed".into(),
                                        message: "the UI-thread diagnostics bridge closed".into(),
                                    })
                                });
                            encode_response(id, &response)
                        }
                        Err(error) => encode_rpc_error(
                            request_id,
                            JsonRpcError::new(INVALID_REQUEST, error.to_string()),
                        ),
                    }
                }
                Err(error) => {
                    encode_rpc_error(None, JsonRpcError::new(INVALID_REQUEST, error.to_string()))
                }
            },
            // A transport error is not a bad request: the framing is broken or
            // the peer is gone, and the next read returns the same error
            // immediately. Answering and continuing spun this task at a full
            // core for the life of the process, one per client that ever
            // disconnected, which is most of them. Measured on an idle app:
            // 0.0% CPU without the control server, 55-76% with it after a few
            // tools had connected and gone.
            //
            // So answer once, best effort, then stop reading this connection.
            Err(error) => {
                let farewell =
                    encode_rpc_error(None, JsonRpcError::new(INVALID_REQUEST, error.to_string()));
                if let Ok(farewell) = farewell {
                    let _ = stream.send(farewell).await;
                }
                break;
            }
        };
        let response = response.unwrap_or_else(|error| {
            encode_rpc_error(
                None,
                JsonRpcError::new(
                    INVALID_REQUEST,
                    format!("could not encode response: {error}"),
                ),
            )
            .expect("the fallback JSON-RPC error is serializable")
        });
        if stream.send(response).await.is_err() {
            break;
        }
    }
}

fn instance_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos:x}", std::process::id())
}

/// Whether a pid still names a live process.
///
/// `kill(pid, 0)` without a libc dependency, and `/proc` does not exist on
/// macOS. A `ps` that cannot be run at all reports "live", because deleting
/// another instance's descriptor on a bad guess is far worse than keeping a
/// stale file.
fn pid_is_live(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(true)
}

/// Delete descriptors whose process is gone.
///
/// The name carries a pid and a nanosecond stamp, so every launch leaves a new
/// pair behind and nothing ever removed them: a developer machine accumulates
/// them indefinitely, and one here had **99**. `Drop` cleans up an orderly
/// exit, but a crash, a `kill -9`, or a rebuild that unlinks the socket under a
/// running instance all skip it.
///
/// That is not merely untidy. A tool discovering the current descriptor has to
/// distinguish them, and a directory full of dead entries
/// is what makes "attached to a stale socket and reported numbers for a process
/// nobody is looking at" a routine failure rather than a rare one.
///
/// Only entries whose pid is dead are removed, so concurrent instances are left
/// strictly alone — and this one's own descriptor is skipped by path, since it
/// has just been written and its pid is obviously live.
fn reap_dead_descriptors(own: &Path) {
    reap_dead_descriptors_with(own, pid_is_live);
}

fn reap_dead_descriptors_with(own: &Path, is_live: impl Fn(u32) -> bool) {
    let Some(dir) = own.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own || path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(descriptor) = serde_json::from_str::<DebugDescriptor>(&text) else {
            continue;
        };
        if is_live(descriptor.pid) {
            continue;
        }
        let _ = remove_file(path.with_extension("sock"));
        let _ = remove_file(&path);
    }
}

fn descriptor_path(instance_id: &str) -> PathBuf {
    std::env::temp_dir()
        .join("tauri-blitz-agent")
        .join(format!("{instance_id}.json"))
}

fn write_descriptor(path: &Path, descriptor: &DebugDescriptor) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, descriptor).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use endpoint_libs::libs::ws::mcp_wire::{JsonRpcId, JsonRpcMessage, JsonRpcRequest};

    use super::*;
    use crate::{
        AgentAction, AgentControlRequest, DebugResponse, MCP_INITIALIZE, MCP_TOOLS_LIST,
        decode_response, decode_rpc, encode_agent_request, encode_rpc,
    };
    use crate::{
        DebugEvent, DebugSnapshot, DebugStream, DiagnosticsRequest, RendererMetrics, RevisionSet,
        SnapshotRequest, decode_diagnostics_event, encode_diagnostics_request,
    };

    /// A host that can do everything, for the tests that are about the
    /// transport rather than about what is on the other side of it.
    fn test_host() -> Host {
        Host {
            name: "test-host".into(),
            version: "0.0.0".into(),
            diagnostics: true,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_server_is_mcp_compatible_and_needs_no_session_or_token() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            assert!(matches!(
                request,
                ControlBridgeRequest::Agent(AgentControlRequest::Act(AgentAction::Click {
                    node_id: 42
                }))
            ));
            sender.send(DebugResponse::Ack).unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge, test_host()).unwrap();
        assert!(server.descriptor_path().is_file());
        assert_eq!(
            server
                .descriptor_path()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(1),
                    MCP_INITIALIZE,
                    serde_json::json!({"protocolVersion": "2025-06-18"}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap(),
            JsonRpcMessage::Response(_)
        ));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(2),
                    MCP_TOOLS_LIST,
                    serde_json::json!({}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap(),
            JsonRpcMessage::Response(_)
        ));
        let id = JsonRpcId::Number(42);
        stream
            .send(
                encode_agent_request(
                    id.clone(),
                    &AgentControlRequest::Act(AgentAction::Click { node_id: 42 }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let response = stream.recv().await.unwrap().unwrap();
        assert_eq!(decode_response(response).unwrap(), (id, DebugResponse::Ack));

        let second = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut second = TransportStream::new(framed_json(second));
        let id = JsonRpcId::String("second-observer".into());
        second
            .send(
                encode_agent_request(
                    id.clone(),
                    &AgentControlRequest::Act(AgentAction::Click { node_id: 42 }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_response(second.recv().await.unwrap().unwrap()).unwrap(),
            (id, DebugResponse::Ack)
        );

        drop(server);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn diagnostics_metrics_reach_the_runtime_bridge_over_mcp() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            assert!(matches!(
                request,
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Metrics)
            ));
            sender
                .send(DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(8192),
                    ..Default::default()
                }))
                .unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge, test_host()).unwrap();
        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        let id = JsonRpcId::Number(91);

        stream
            .send(encode_diagnostics_request(id.clone(), &DiagnosticsRequest::Metrics).unwrap())
            .await
            .unwrap();

        assert_eq!(
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap(),
            (
                id,
                DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(8192),
                    ..Default::default()
                })
            )
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn observe_pushes_only_requested_latest_value_events() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|_request| {
            panic!("observe is connection-local and must not reach the UI bridge")
        });
        let (events, receiver) = watch::channel(None);
        let server = AgentControlServer::start_with_events(bridge, test_host(), receiver).unwrap();
        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        let id = JsonRpcId::Number(92);

        stream
            .send(
                encode_diagnostics_request(
                    id.clone(),
                    &DiagnosticsRequest::Observe {
                        streams: vec![DebugStream::Paint],
                    },
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap(),
            (id, DebugResponse::Ack)
        );

        let event = DebugEvent::PaintCommitted { revision: 7 };
        events.send_replace(Some(event.clone()));
        assert_eq!(
            decode_diagnostics_event(stream.recv().await.unwrap().unwrap()).unwrap(),
            event
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_snapshot_keeps_the_socket_open_for_follow_up_requests() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        const LARGE_DOM_BYTES: usize = 9 * 1024 * 1024;
        let bridge: ControlBridge = Arc::new(|request| {
            let (sender, receiver) = oneshot::channel();
            let response = match request {
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Snapshot(_)) => {
                    DebugResponse::Snapshot(DebugSnapshot {
                        revisions: RevisionSet::default(),
                        active_window: Some("main".into()),
                        active_element: None,
                        dom: Some(serde_json::Value::String("x".repeat(LARGE_DOM_BYTES))),
                        layout: None,
                        computed_style: None,
                        metrics: RendererMetrics::default(),
                    })
                }
                ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Metrics) => {
                    DebugResponse::Metrics(RendererMetrics {
                        resident_bytes: Some(4096),
                        ..Default::default()
                    })
                }
                _ => panic!("unexpected request"),
            };
            sender.send(response).unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge, test_host()).unwrap();
        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));

        let snapshot_id = JsonRpcId::Number(92);
        stream
            .send(
                encode_diagnostics_request(
                    snapshot_id.clone(),
                    &DiagnosticsRequest::Snapshot(SnapshotRequest {
                        include_dom: true,
                        include_layout: false,
                        include_computed_style: false,
                        node_ids: Vec::new(),
                    }),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let (response_id, response) =
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap();
        assert_eq!(response_id, snapshot_id);
        let DebugResponse::Snapshot(snapshot) = response else {
            panic!("expected a diagnostic snapshot")
        };
        assert_eq!(
            snapshot.dom.unwrap().as_str().unwrap().len(),
            LARGE_DOM_BYTES
        );

        let metrics_id = JsonRpcId::Number(93);
        stream
            .send(
                encode_diagnostics_request(metrics_id.clone(), &DiagnosticsRequest::Metrics)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            decode_response(stream.recv().await.unwrap().unwrap()).unwrap(),
            (
                metrics_id,
                DebugResponse::Metrics(RendererMetrics {
                    resident_bytes: Some(4096),
                    ..Default::default()
                })
            )
        );
    }

    #[test]
    fn initialize_payload_uses_json_rpc() {
        let message =
            encode_initialize_response(JsonRpcId::Number(1), "test-host", "0.1.0").unwrap();
        let WireMessage::Text(payload) = message else {
            panic!("initialize response must be text")
        };
        let decoded: JsonRpcMessage = serde_json::from_str(&payload).unwrap();
        assert!(matches!(decoded, JsonRpcMessage::Response(_)));
    }

    /**
     * A dead instance's descriptor goes; a live one's stays.
     *
     * The filename carries a pid and a nanosecond stamp, so every launch leaves
     * a new pair behind and nothing removed them — one machine had 99. `Drop`
     * handles an orderly exit, but a crash, a `kill -9`, or a rebuild that
     * unlinks the socket under a running instance all skip it. A tool then has
     * to guess which is current, which is how attaching to a stale socket
     * became routine.
     *
     * The asymmetry is the whole point: reaping too eagerly would delete a
     * concurrent instance's descriptor, which is worse than leaving litter.
     */
    #[test]
    fn reaping_removes_dead_descriptors_and_spares_live_ones() {
        let dir = std::env::temp_dir().join(format!(
            "blitz-reap-{}-{:x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch directory is created");

        let write = |name: &str, pid: u32| {
            let path = dir.join(format!("{name}.json"));
            write_descriptor(
                &path,
                &DebugDescriptor {
                    protocol_version: crate::DEBUG_PROTOCOL_VERSION,
                    pid,
                    instance_id: name.into(),
                    address: format!("unix://{}", dir.join(format!("{name}.sock")).display()),
                    renderer: "blitz".into(),
                    renderer_revision: "0.0.0".into(),
                },
            )
            .expect("descriptor is written");
            std::fs::write(dir.join(format!("{name}.sock")), b"").expect("socket stub is written");
            path
        };

        // Liveness is injected here because process inspection may be denied by
        // the test sandbox. This test owns descriptor cleanup, not `ps` itself.
        let own = write("own", std::process::id());
        let live = write("live", 1);
        let dead = write("dead", 2);

        reap_dead_descriptors_with(&own, |pid| pid == std::process::id() || pid == 1);

        assert!(own.exists(), "the caller's own descriptor is never reaped");
        assert!(live.exists(), "a live instance must keep its descriptor");
        assert!(!dead.exists(), "a dead instance's descriptor is removed");
        assert!(
            !dir.join("dead.sock").exists(),
            "the orphaned socket goes with it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A gone server advertises nothing.
    ///
    /// Carried over from the browser's own control server, which is being
    /// deleted in favour of this one: it asserted that dropping the server
    /// removes both files, and losing that assertion would lose the property.
    /// Leaving either behind advertises a host that is not there, and a client
    /// that finds a stale descriptor attaches to a socket nobody is serving.
    #[tokio::test(flavor = "current_thread")]
    async fn teardown_removes_the_socket_and_the_descriptor() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|_request| {
            let (sender, receiver) = oneshot::channel();
            sender.send(DebugResponse::Ack).unwrap();
            receiver
        });
        let server = AgentControlServer::start(bridge, test_host()).unwrap();
        let descriptor = server.descriptor_path().to_path_buf();
        let socket = server.socket_path().to_path_buf();
        assert!(descriptor.is_file(), "the descriptor was not published");
        assert!(socket.exists(), "the socket was not bound");

        drop(server);

        assert!(!socket.exists(), "the socket outlived the server");
        assert!(
            !descriptor.exists(),
            "the descriptor outlived the server, so a client can still find it"
        );
    }

    /// What the tool list says is the host's answer, not this crate's.
    ///
    /// It used to be `cfg!(feature = "diagnostics")` read off the crate that
    /// happened to contain the server. There are three hosts now and the
    /// transport is in none of them.
    #[tokio::test(flavor = "current_thread")]
    async fn a_host_that_cannot_collect_diagnostics_does_not_advertise_them() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|_request| {
            let (sender, receiver) = oneshot::channel();
            sender.send(DebugResponse::Ack).unwrap();
            receiver
        });
        let server = AgentControlServer::start(
            bridge,
            Host {
                name: "plain-host".into(),
                version: "9.9.9".into(),
                diagnostics: false,
            },
        )
        .unwrap();

        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(1),
                    MCP_TOOLS_LIST,
                    serde_json::json!({}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        let JsonRpcMessage::Response(response) =
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap()
        else {
            panic!("tools/list should be a response")
        };
        let tools = response.result.unwrap()["tools"].as_array().unwrap().len();
        assert_eq!(
            tools, 1,
            "a host that cannot collect diagnostics must not offer the tool: \
             a client reports the application as broken rather than as a plain \
             build"
        );
    }

    /// The host names itself in the handshake.
    #[tokio::test(flavor = "current_thread")]
    async fn initialize_reports_the_host_rather_than_the_transport() {
        let _guard = CONTROL_TEST_LOCK.lock().await;
        let bridge: ControlBridge = Arc::new(|_request| {
            let (sender, receiver) = oneshot::channel();
            sender.send(DebugResponse::Ack).unwrap();
            receiver
        });
        let server = AgentControlServer::start(
            bridge,
            Host {
                name: "chuzz-headless".into(),
                version: "1.2.3".into(),
                diagnostics: true,
            },
        )
        .unwrap();

        let stream = UnixStream::connect(server.socket_path()).await.unwrap();
        let mut stream = TransportStream::new(framed_json(stream));
        stream
            .send(
                encode_rpc(JsonRpcMessage::Request(JsonRpcRequest::call(
                    JsonRpcId::Number(1),
                    MCP_INITIALIZE,
                    serde_json::json!({"protocolVersion": "2025-06-18"}),
                )))
                .unwrap(),
            )
            .await
            .unwrap();
        let JsonRpcMessage::Response(response) =
            decode_rpc(stream.recv().await.unwrap().unwrap()).unwrap()
        else {
            panic!("initialize should be a response")
        };
        let result = response.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "chuzz-headless");
        assert_eq!(result["serverInfo"]["version"], "1.2.3");

        let descriptor: DebugDescriptor =
            serde_json::from_str(&std::fs::read_to_string(server.descriptor_path()).unwrap())
                .unwrap();
        assert_eq!(
            descriptor.renderer_revision, "1.2.3",
            "the descriptor reports the host's version, not the protocol crate's"
        );
    }
}
