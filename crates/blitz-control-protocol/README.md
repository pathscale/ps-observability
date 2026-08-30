# blitz-control-protocol

Typed observability commands, events, snapshots, diagnostics, and MCP wire
encoding for Blitz applications.

The crate deliberately has no renderer or window dependency. A driver can use
it to speak the exact protocol served by `tauri-runtime-blitz` without building
a browser engine. Clients must compare a discovered descriptor's
`protocolVersion` with `DEBUG_PROTOCOL_VERSION` before connecting.

The descriptor's `address` is authoritative. Current Unix servers conventionally
place a `.sock` beside the `.json` descriptor, but clients should only derive
that legacy fallback when an older descriptor omits a `unix://` address.

The transport uses `endpoint-libs` framed JSON and MCP/JSON-RPC primitives.
`CaptureRequest::validate` defines the safe renderer allocation bounds, and
encoded responses are rejected before crossing the transport's 16 MiB frame
limit.

Public command, action, response, event, and protocol-error enums are
`#[non_exhaustive]`; consumers must retain an unknown-variant branch. Rust API
compatibility follows the crate's pre-1.0 minor version, while incompatible
wire decoding increments `DEBUG_PROTOCOL_VERSION`. Descriptor validation then
fails before a client sends an action instead of guessing across versions.
