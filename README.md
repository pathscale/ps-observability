# ps-observability

The observability and QA stack for native Blitz applications. This workspace
keeps the protocol, transports, driver, and release documentation together so
the system has one ownership boundary.

```text
                      blitz-control-protocol
                   ┌──────────┴──────────────┐
            the vocabulary               the core
                   │                (over blitz-dom)
     ┌─────────────┴─────────────┐         │
  socket transport      in-process transport
     │        │                            │
  server    client              an embedder holding
     │        │                   the document
     │        └── ps-qa (no blitz, no window)
     │
     ├── tauri-runtime-blitz, for the application window
     └── chuzz-headless, for a page with no window
```

The headless host is not here. It used to be, as `qa-inspect-host`, and that
made two headless browsers: one a person browses with and one QA drives, with
the web platform in only the first of them. A host without `URLSearchParams`,
`matchMedia`, storage, the observers and `performance.getEntriesByType` blanks
every routed page before its first render, so every gap closed for the browser
had to be closed a second time here, by hand, or the harness measured a browser
nobody ships.

So the host is a mode of the browser now: `chuzz-headless`, in
[pathscale/chuzz](https://github.com/pathscale/chuzz), which loads through the
same loader and the same engine a tab uses and serves this protocol over the
same socket. `ps-qa` still links no renderer, because that constraint was
always about the socket rather than about which repository the host lives in.

`blitz-control-protocol` is the whole surface: one vocabulary, one core, and
two transports over that core. The core reads and drives a `blitz-dom`
document and knows nothing about sockets. The socket transport is how a harness
or an agent outside the process reaches a running application; the in-process
transport is how an embedder that already holds the document calls straight in.
Both halves of the socket are here, because a peer can be both: an application
driven by agents while it drives a browser it embeds is a client and a server
at once.

Everything that needs the renderer is behind a feature, so a client pays for
none of it. `cargo tree -p ps-qa` showing no renderer, window runtime or GPU
stack is the check, and CI fails on it.

The loopback WebDriver-shaped adapter that used to be here, `ps-blitz-debug-control`,
lives in [pathscale/ps-blitz](https://github.com/pathscale/ps-blitz) now. It is
what `blitz-script` takes behind its `debug-control` feature, so keeping it here
made ps-blitz depend on this repository while this repository needs `blitz-dom`.
Two repositories pointing at each other have no release order.

`endpoint-libs` owns framing and MCP/JSON-RPC wire primitives. This workspace
owns observability semantics: commands, events, revision rules, session
behaviour, discovery, diagnostics, and QA outcomes. Renderer crates expose
instrumentation hooks but do not own a control server.

## Crates

- `blitz-control-protocol`: the vocabulary, the core, and both transports. The
  vocabulary and the transports have no renderer dependency; the core is behind
  a feature and is the only part that does.
- `ps-qa`: the lightweight driver, audit runner, and report generator.

## Quick start

Install the driver, and build the host from the chuzz checkout beside this one:

```zsh
cargo install ps-qa
cargo build --manifest-path ../chuzz/Cargo.toml --bin chuzz-headless --release
```

Serve one page in one terminal. A directory is served as a site, on a loopback
origin, so a built application's absolute asset paths and its client routing
both work; a single file or an `http(s)` URL is taken as given:

```zsh
../chuzz/target/release/chuzz-headless ../support.cafe/dist
```

The host prints its descriptor path when ready. In a second terminal, drive it;
`ps-qa` discovers the live descriptor automatically:

```zsh
ps-qa find --role button
ps-qa audit
```

`qa-hosted` does both halves at once, launching the host, running a group of
checks and stopping it again:

```zsh
ps-qa --app tests/ps-qa/ps-qa.ron \
  qa-hosted \
  --host ../chuzz/target/release/chuzz-headless \
  --page dist \
  --checks tests/ps-qa
```

This is a renderer-backed check: it enters text through the control protocol
and verifies that the live semantic value changed. It does not use jsdom or a
mock tree. Pass `--descriptor <path>` when more than one inspectable process is
running.

See [docs/performance.md](docs/performance.md) for the measurement contract,
latency fields, and the difference between harness pacing and app throughput.

## Security

Observability endpoints are debugger interfaces, not application sandboxes.
They must bind only to local transports. The WebDriver adapter uses loopback,
an unpredictable per-process token, and owner-only discovery files on Unix.
The framed inspection socket has the same-user trust boundary: any
process able to access the socket can inspect the UI and request supported
actions. Arbitrary script execution, where enabled, has the same posture as a
browser remote-debugging port and must remain disabled in production builds.

## Platform status

The protocol, HTTP transport, and `ps-qa` driver are continuously checked on
Linux; `ps-qa` connects through a Unix-domain socket and currently supports
macOS and Linux, not Windows. The host's own platform status belongs to chuzz
now, and "headless" there means no window or display interaction rather than a
claim about which platforms the host has been packaged for.

## Releases

Crates keep independent versions. Release automation publishes only packages
whose manifest version is not already present in the registry; changing one
adapter does not force a version train across the workspace.
