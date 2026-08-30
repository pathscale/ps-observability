# qa-inspect-host

Host a Blitz document over the inspection socket, with no window.

```zsh
QA_INSPECT_PAGE=/path/to/one/built/page qa-inspect-host
```

It prints its descriptor path on stdout once it is serving, then serves until
killed. `ps-qa sweep-components` launches one per component and waits for that
line.

## Why

A component sweep asks what happens when a control is pressed. Answering that
needs a live document behind a socket: a screenshot says only that something
painted, and a semantic tree written to a file says only what was on screen at
one instant, so every check involving a click is undecidable against either.

Hosting the socket used to require opening a window, because
`AgentControlServer::start` was private to the runtime. A sweep of 71 components
meant 71 windows over whatever the person at the machine was doing. Nothing
about that server needs a window, and this crate is what that buys: a process
that owns a document, serves inspection, and paints nothing.

## Why not part of ps-qa

`ps-qa` may not depend on blitz, tauri, winit or wgpu, so that driving a control
does not build a browser engine. A host has to link a renderer. Two crates, one
socket between them.

## What it answers

`Inspect`, `Focus`, `Hover`, `Click`, `DoubleClick`, `SetValue` and `Key`.
`ScrollIntoView` is an acknowledged no-op because a single component page is
already in view. Anything else returns `unsupported` rather than a plausible
`Ack`, because a check that silently did nothing reports a working component as
broken.

Coordinate pointer and wheel streams are not implemented: those carry window
position and button state on the runtime itself, which a windowless host has
nothing to attach to. Semantic hover targets an exact inspected node and is
fully supported.
