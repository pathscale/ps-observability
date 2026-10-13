# Changelog

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
