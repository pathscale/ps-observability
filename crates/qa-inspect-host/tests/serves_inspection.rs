//! The host serves a real page over a real socket.
//!
//! # Why this exists
//!
//! Every other test in this stack is a unit test. `ps-qa` cannot cover the host
//! at all, because it is forbidden from linking blitz, so until now nothing in
//! CI ever launched a host or checked that a page reaches the socket. The whole
//! path was verified by hand, once, and would have broken silently.
//!
//! The fixture is deliberately self-contained: a page, a stylesheet and a
//! script under `tests/fixture`, with no bundler and no other repository
//! involved. A test that needs a sibling checkout built first is a test that
//! does not run.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use blitz_control_protocol::{JsonRpcId, MessageStream, TransportStream, framed_json};
use tauri_runtime_blitz::control_protocol::{
    AgentAction, AgentControlRequest, DebugEvent, DebugResponse, DebugStream, DiagnosticsRequest,
    decode_diagnostics_event, decode_response, encode_agent_request, encode_diagnostics_request,
};

async fn request(
    stream: &mut dyn MessageStream,
    next_id: &mut i64,
    request: &AgentControlRequest,
) -> DebugResponse {
    *next_id += 1;
    let id = JsonRpcId::Number(*next_id);
    stream
        .send(encode_agent_request(id.clone(), request).expect("encode agent request"))
        .await
        .expect("send agent request");
    loop {
        let message = stream
            .recv()
            .await
            .expect("the host keeps serving")
            .expect("read agent response");
        if let Ok((response_id, response)) = decode_response(message)
            && response_id == id
        {
            return response;
        }
    }
}

async fn observe_paint(stream: &mut dyn MessageStream, next_id: &mut i64) {
    *next_id += 1;
    let id = JsonRpcId::Number(*next_id);
    stream
        .send(
            encode_diagnostics_request(
                id.clone(),
                &DiagnosticsRequest::Observe {
                    streams: vec![DebugStream::Paint],
                },
            )
            .expect("encode observe request"),
        )
        .await
        .expect("send observe request");
    loop {
        let message = stream
            .recv()
            .await
            .expect("the host keeps serving")
            .expect("read observe response");
        if let Ok((response_id, DebugResponse::Ack)) = decode_response(message)
            && response_id == id
        {
            return;
        }
    }
}

/// Kill the host however the test ends, including on a panic.
struct Host(std::process::Child);

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serves_a_page_over_the_inspection_socket() {
    let page = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture/page.html");
    let binary = env!("CARGO_BIN_EXE_qa-inspect-host");

    let mut host = Host(
        Command::new(binary)
            .env("QA_INSPECT_PAGE", page)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the host binary should start"),
    );

    // The descriptor path, which the host prints once it is serving. Read on a
    // thread with a deadline around it: a host that dies before announcing
    // would otherwise block for ever on a pipe that will never produce a line.
    let stdout = host.0.stdout.take().expect("stdout was piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line);
    });

    let announced = receiver
        .recv_timeout(Duration::from_secs(60))
        .expect("the host should announce a descriptor");
    let descriptor = std::path::PathBuf::from(announced.trim());

    assert!(
        descriptor.is_file(),
        "the announced descriptor should exist: {}",
        descriptor.display()
    );

    // The socket lives beside the descriptor, and the host writes the
    // descriptor before it binds, so a connection can lose that race.
    let socket = descriptor.with_extension("sock");
    let deadline = Instant::now() + Duration::from_secs(30);
    let connected = loop {
        match std::os::unix::net::UnixStream::connect(&socket) {
            Ok(stream) => break Some(stream),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break None,
        }
    };

    assert!(
        connected.is_some(),
        "the inspection socket should accept a connection at {}",
        socket.display()
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let socket = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect async client");
        let mut stream = TransportStream::new(framed_json(socket));
        let mut next_id = 0;
        let snapshot = request(
            &mut stream,
            &mut next_id,
            &AgentControlRequest::Inspect {
                root: None,
                max_depth: 20,
            },
        )
        .await;
        let DebugResponse::AgentSnapshot(snapshot) = snapshot else {
            panic!("inspect should return a semantic snapshot");
        };
        let button = snapshot
            .nodes
            .iter()
            .find(|node| node.role.eq_ignore_ascii_case("button"))
            .unwrap_or_else(|| panic!("fixture button missing from {:?}", snapshot.nodes))
            .id;
        let input = snapshot
            .nodes
            .iter()
            .find(|node| node.role.eq_ignore_ascii_case("textbox"))
            .unwrap_or_else(|| panic!("fixture input missing from {:?}", snapshot.nodes))
            .id;

        assert!(matches!(
            request(
                &mut stream,
                &mut next_id,
                &AgentControlRequest::Act(AgentAction::SetValue {
                    node_id: input,
                    value: "after".into(),
                }),
            )
            .await,
            DebugResponse::Ack
        ));
        let changed = request(
            &mut stream,
            &mut next_id,
            &AgentControlRequest::Inspect {
                root: Some(input),
                max_depth: 1,
            },
        )
        .await;
        let DebugResponse::AgentSnapshot(changed) = changed else {
            panic!("inspect should return the changed input");
        };
        assert!(
            changed
                .nodes
                .iter()
                .any(|node| node.id == input && node.value.as_deref() == Some("after")),
            "SetValue must be observable before its Ack: {:?}",
            changed.nodes
        );

        observe_paint(&mut stream, &mut next_id).await;
        assert!(matches!(
            request(
                &mut stream,
                &mut next_id,
                &AgentControlRequest::Act(AgentAction::ScrollIntoView { node_id: button }),
            )
            .await,
            DebugResponse::Ack
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), stream.recv())
                .await
                .is_err(),
            "a supported no-op must not fabricate a paint event"
        );

        assert!(matches!(
            request(
                &mut stream,
                &mut next_id,
                &AgentControlRequest::Act(AgentAction::ScrollBy {
                    node_id: button,
                    delta_x: 0.0,
                    delta_y: 10.0,
                }),
            )
            .await,
            DebugResponse::Error(error) if error.code == "unsupported"
        ));

        assert!(matches!(
            request(
                &mut stream,
                &mut next_id,
                &AgentControlRequest::Act(AgentAction::Hover { node_id: button }),
            )
            .await,
            DebugResponse::Ack
        ));
        let event = tokio::time::timeout(Duration::from_millis(250), stream.recv())
            .await
            .expect("a real hover repaint should emit an event")
            .expect("the host keeps serving")
            .expect("read paint event");
        assert!(matches!(
            decode_diagnostics_event(event),
            Ok(DebugEvent::PaintCommitted { .. })
        ));
    });
}
