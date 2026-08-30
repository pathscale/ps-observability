# ps-observability

The observability and QA stack for native Blitz applications. This workspace
keeps the protocol, transports, renderer host, driver, fixtures, and release
documentation together so the system has one ownership boundary.

```text
application ── tauri-runtime-blitz ── blitz-control-protocol ── ps-qa
                      ▲                         ▲
                      │                         │
               qa-inspect-host ────────────────┘

renderer embedder ── ps-blitz-debug-control ── WebDriver-style HTTP client
```

These are two deliberate alternatives, not two stacked transports.
`blitz-control-protocol` is the typed MCP/JSON-RPC inspection plane used by
`tauri-runtime-blitz`, the headless host, and `ps-qa`.
`ps-blitz-debug-control` is a smaller HTTP adapter for embedders that need a
WebDriver-shaped session and command channel; it does not depend on or duplicate
the typed protocol crate.

`endpoint-libs` owns framing and MCP/JSON-RPC wire primitives. This workspace
owns observability semantics: commands, events, revision rules, session
behaviour, discovery, diagnostics, and QA outcomes. Renderer crates expose
instrumentation hooks but do not own a control server.

## Crates

- `blitz-control-protocol`: transport-neutral observability domain types and
  their MCP wire encoding. It deliberately has no renderer dependency.
- `ps-blitz-debug-control`: loopback WebDriver-style transport adapter.
- `qa-inspect-host`: a real renderer host for headless fixtures and CI.
- `ps-qa`: the lightweight driver, audit runner, and report generator.

## Quick start

From this workspace, install the driver and build the real headless renderer
host:

```zsh
cargo install ps-qa
cargo build -p qa-inspect-host
```

Start the supplied renderer fixture in one terminal:

```zsh
QA_INSPECT_PAGE="$PWD/crates/qa-inspect-host/tests/fixture/page.html" \
  target/debug/qa-inspect-host
```

The host prints its descriptor path when ready. In a second terminal, run the
fixture's outcome check; `ps-qa` discovers the live descriptor automatically:

```zsh
ps-qa \
  --app crates/qa-inspect-host/tests/fixture/ps-qa.ron \
  qa fixture-text-entry \
  --checks crates/qa-inspect-host/tests/fixture/checks
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
macOS and Linux, not Windows. The renderer-backed `qa-inspect-host` artifact is
currently validated on macOS. Linux renderer-host packaging remains explicit
follow-up work, so “headless” here means no window or display interaction—not a
claim that the current host package has completed Linux portability.

## Releases

Crates keep independent versions. Release automation publishes only packages
whose manifest version is not already present in the registry; changing one
adapter does not force a version train across the workspace.
