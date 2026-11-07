# blitz-control-protocol

Typed observability commands, events, snapshots, diagnostics, and MCP wire
encoding for Blitz applications.

The crate deliberately has no renderer or window dependency. A driver can use
it to speak the exact protocol served by `izumo` without building
a browser engine. Clients must compare a discovered descriptor's
`protocolVersion` with `DEBUG_PROTOCOL_VERSION` before connecting.

The descriptor's `address` is authoritative. Current Unix servers conventionally
place a `.sock` beside the `.json` descriptor, but clients should only derive
that legacy fallback when an older descriptor omits a `unix://` address.

The transport uses `endpoint-libs` framed JSON and MCP/JSON-RPC primitives.
Client requests use one deadline across sending and receiving all frames.
Notifications and replies for other request IDs do not restart that deadline.
A timeout or terminal stream failure poisons the client connection. The
client reports these cases as `InspectorRequestTimeout` and
`InspectorDisconnected`, with the request ID, phase, and timeout or cause.
EOF while waiting for paint is a disconnect error.
`CaptureRequest::validate` defines the safe renderer allocation bounds, and
encoded responses are rejected before crossing the transport's 16 MiB frame
limit.

Public command, action, response, event, and protocol-error enums are
`#[non_exhaustive]`; consumers must retain an unknown-variant branch. Rust API
compatibility follows the crate's pre-1.0 minor version, while incompatible
wire decoding increments `DEBUG_PROTOCOL_VERSION`. Descriptor validation then
fails before a client sends an action instead of guessing across versions.

## Pin discovery tests

The pin discovery tests create private, unique child directories and remove
only those children when each fixture is dropped. Before running them, provide
`PS_OBSERVABILITY_PIN_FIXTURE_ROOT` as an absolute path to an existing
directory owned by the current user and writable by that user. The tests do not
create or clean the configured root itself. For example, after preparing an
appropriate directory at the indicated generic path:

```sh
PS_OBSERVABILITY_PIN_FIXTURE_ROOT=/path/to/preexisting/writable/owned/directory cargo test -p blitz-control-protocol pinned_discovery -- --test-threads=1
```
