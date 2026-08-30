# ps-blitz-debug-control

A debug-only, loopback HTTP control plane for Blitz renderer embedders.

It follows the useful WebDriver shape—`/status`, authenticated session creation,
session-scoped commands, and WebDriver error bodies—but it is not a complete
WebDriver implementation. Requests require `Content-Length`, connections close
after one response, and Blitz commands are forwarded to the renderer thread.

Enabling the server gives the same OS user debugger-level control, including
arbitrary document JavaScript when the embedder exposes that command. Use it
only in diagnostics builds. Discovery tokens are random, descriptor files are
owner-only on Unix, and the listener rejects non-loopback addresses.
