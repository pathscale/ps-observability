# ps-observability

The observability and QA stack for native Blitz applications. This workspace
keeps the protocol, transports, renderer host, driver, fixtures, and release
documentation together so the system has one ownership boundary.

```text
application / tauri-runtime-blitz
              |
              | typed events, snapshots, actions
              v
blitz-control-protocol
       |                         |
       | endpoint-libs framed    | WebDriver-compatible HTTP
       v                         v
qa-inspect-host          ps-blitz-debug-control
       |
       v
     ps-qa
```

`endpoint-libs` owns framing and MCP/JSON-RPC wire primitives. This workspace
owns observability semantics: commands, events, revision rules, session
behaviour, discovery, diagnostics, and QA outcomes. Renderer crates expose
instrumentation hooks but do not own a control server.

## Crates

- `blitz-control-protocol`: transport-neutral observability domain types and
  their MCP wire encoding. It deliberately has no renderer dependency.
- `ps-blitz-debug-control`: loopback WebDriver-compatible transport adapter.
- `qa-inspect-host`: a real renderer host for headless fixtures and CI.
- `ps-qa`: the lightweight driver, audit runner, and report generator.

## Quick start

```zsh
cargo install ps-qa
cargo run -p qa-inspect-host
ps-qa sweep --help
```

The host prints its descriptor path when ready. `ps-qa` discovers that live
descriptor automatically, or accepts an explicit descriptor path.

## Security

Observability endpoints are debugger interfaces, not application sandboxes.
They must bind only to local transports. The WebDriver adapter uses loopback,
an unpredictable per-process token, and owner-only discovery files on Unix.
The framed inspection socket has the same same-user trust boundary: any
process able to access the socket can inspect the UI and request supported
actions. Arbitrary script execution, where enabled, has the same posture as a
browser remote-debugging port and must remain disabled in production builds.

## Releases

Crates keep independent versions. Release automation publishes only packages
whose manifest version is not already present in the registry; changing one
adapter does not force a version train across the workspace.
