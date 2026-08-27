# qa-headless-host

Host a Blitz document over the inspection socket, with no window.

```sh
QA_HOST_DIST=/path/to/one/built/page qa-headless-host
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

`Inspect`, `Click`, `DoubleClick` and `Key`. Anything else returns `unsupported`
rather than a plausible `Ack`, because a check that silently did nothing reports
a working component as broken.

Pointer and wheel events are not implemented: those carry pointer position and
button state on the runtime itself, which a windowless host has nothing to
attach to.
