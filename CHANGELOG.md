# Changelog

## ps-qa 0.9.9

- Run hosted and native-page hosts under a `--supervise-host` / `--supervise-native-host` supervisor that leads its own process group and stops the host when the runner's stdin pipe closes.
- Require a private, effective-uid-owned control directory for every hosted run, and accept only announced descriptors contained in it.
- Restrict `native-page --engine` to `ekojit`, bound the native host to 60 seconds, and report per-entry native counts and text in the outcome.

## blitz-control-protocol 0.8.0

- Breaking: drop the `ps-blitz-script` dependency. The `engine` and `capture` document functions (`DocumentControl`, `snapshot_document`, `capture_document`, `inspect_document`, `settle` and the node actions) now take any `ControlScriptBackend`, which hosts implement on their document owner. Script metrics come from `ControlScriptBackend::script_metrics`.
- Breaking: require the ps-blitz 0.7 crates. `ps-blitz-dom`, `ps-blitz-traits`, `ps-blitz-paint` and `ps-blitz-shell` move from `^0.6.2` to `^0.7.0`, and hosts must move to the same line because the public API takes `blitz_dom` types.
- Create the server control directory as a private directory (per effective uid outside macOS), validate descriptors and sockets for ownership, permissions and containment before use, and reap dead instances by pid without spawning `ps`.
- Detect hex and unaligned base64 forms of registered secrets in the guarded writer.

## ps-qa 0.9.6

- Make resource caps explicit application parameters, bound reads and latency allocations, and drain child output through the shared nagoya executor.

## blitz-control-protocol 0.7.5

- Route server work through nagoya with typed Arc ownership and concrete Send transport futures, removing per-server OS threads.

## blitz-control-protocol 0.7.4

- Route file writes through a secret-aware guard that refuses raw and encoded secret values before opening a destination.

## ps-qa 0.9.5

- Register run-plan credentials with the shared writer guard and remove file-output options for captured pixels and GPU reports.

## blitz-control-protocol 0.7.2

- Add additive tab addressing, generation and document guards, target discovery, and bounded per-target diagnostic journals while keeping `DEBUG_PROTOCOL_VERSION` at 2.

## blitz-control-protocol 0.7.1

- Add DiagnosticsRequest::WebGpuBuffer and DebugResponse::WebGpuBuffer with labeled buffer bytes, original usage, completed submit serial, writeBuffer provenance, and base64 payload.
- Advertise the additive request through the derived diagnostics schema while keeping DEBUG_PROTOCOL_VERSION at 2.
- Expose a typed pending-response classifier for clients retrying incomplete buffer samples.

## ps-qa 0.9.2

- Add target selection to ps-qa and GUI, headless, and inspection-off functional stages for tab identity, page-local input, journal replay, and lifecycle cleanup.

## ps-qa 0.9.1

- Require blitz-control-protocol ^0.7.1 and add ps-qa gpu-buffer with aligned byte ranges, little-endian typed values, stride grouping, and TOON output.
- Add GpuBufferRange, GpuBufferEquals, GpuBufferAtLeast, and GpuBufferChanges suite expectations with finite-value policy and completed-submit change evidence.
- Retry pending and failing buffer samples inside the outcome budget, honor stable_for_ms, and document the GPU inspection commands and checks.

## blitz-control-protocol 0.7.0

- Advertise inspection protocol 2 and serialize command fields as camelCase; continue accepting the protocol 1 snake_case field names on input.
- Reject mismatched descriptor versions with both versions and matching-client guidance.
- Add DiagnosticsRequest::Evaluate and DebugResponse::Evaluated for a host to return a page script's JSON completion value.
- Document host-supplied per-stage RevisionSet counters; the shared document snapshot still reports zero for counters it does not read.
- Box the private connection event without changing the public response payload types.

## ps-qa 0.9.0

- Require a protocol 2 inspector through blitz-control-protocol ^0.7.0; protocol 1 descriptors are rejected.
- Add ps-qa eval to print a page script's JSON completion value.
- Report an expired outcome deadline as OutcomeTimeout before another inspector read, instead of poisoning the connection with a zero request timeout.
