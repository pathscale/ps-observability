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
            .stderr(Stdio::null())
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
}
