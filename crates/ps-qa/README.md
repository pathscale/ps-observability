# ps-qa

Drive a running [Blitz](https://github.com/DioxusLabs/blitz) app through its MCP
control socket and assert what the renderer actually did.

Not a benchmark and not a unit-test runner. It connects to a live application,
addresses controls by semantic node id, and reads back the layout boxes the
engine computed — so it can tell a control that works from one that is present
in the tree, correctly named, and `0x0` on screen. Explicit pointer commands
remain available for diagnosing hit-testing itself.

## Why it exists

A DOM-only test environment cannot answer the questions that matter for a
desktop UI. It has no compositor, no hit-testing, and no layout, so it reports a
node as fine while the user sees nothing. Three failure classes make the point,
all of them found in a real app whose unit suite was green at the time:

- **A dialog that could not be dismissed.** Its root was re-parented with
  `document.body.append`, the engine reallocated the slot, and the node that
  painted was no longer the node the handlers were bound to. Every exit was
  inert, with no error. 68 of that surface's 84 controls sat unreachable behind
  it.
- **A rename pencil that never opened its editor.** The row around it was a
  `role="button"` that folded on `click`, and the framework delegates `click`,
  so the pencil's `stopPropagation` lost the race.
- **Icons that laid out perfectly and drew nothing.** Correct box, correct
  stroke colour, no artwork — because the `<use href="#sprite">` resolved
  against a document the rasteriser never saw.

None of those are visible without the running renderer.

## How it connects

The app writes a descriptor when built with its inspector feature:

```json
{
  "protocolVersion": 1,
  "pid": 62904,
  "address": "unix:///path/to/blitz-control.sock",
  "renderer": "blitz"
}
```

`ps-qa` reads `--descriptor <path>`, or falls back to
`target/blitz-control.json` and then the newest reachable descriptor advertised
in the temporary directory. It probes the Unix socket itself rather than only
checking the pid, because operating systems reuse pids while stale descriptor
files remain. It then speaks **MCP** over that socket: `initialize`,
`tools/list`, then `tools/call`
against `blitz.agent.control` (drive) and `blitz.diagnostics` (inspect).

The framing is length-delimited, **not** newline-delimited and not WebSocket:

```
u32 BE length | u8 kind (0=Text, 1=Binary, 2=Ping, 3=Pong, 4=Close) | payload
```

`length` counts the kind byte. `Text` payloads are UTF-8 JSON-RPC. Responses are
matched by JSON-RPC id, because the server also pushes notifications on the same
socket — a client that returns the next frame it sees will eventually hand back
a console message as though it were the answer.

One connection serves a whole run.

## Usage

```sh
ps-qa list                       # every check, what it drives, what it asserts
ps-qa qa                         # run them all
ps-qa qa dialog                  # one group
ps-qa qa dialog-cancel-dismisses # one check, by id
ps-qa inventory                  # fast reachability counts on every surface
ps-qa inventory --require-outcomes # fail every reachable control with no named verdict
ps-qa reconcile cover.txt        # map a saved CI inventory to current checks, no GUI
```

`list` needs no running app. Everything else does. Exit code is 1 if any check
fails, so it drops into CI unchanged.

`--trace` prints the node each check activates, which is how you tell "the
control is broken" from "the check pressed the wrong thing".

### Diagnosing, without writing a check

```sh
ps-qa layout "<name>"     # live boxes: x, y, w, h per matching node
ps-qa dom "<name>" 6      # attributes plus the ancestor chain
ps-qa paint "<name>"      # the colours the renderer resolved
ps-qa press "<name>"      # a real pointer: move, down, up
ps-qa find "<name>" --role button # semantic matches and their node ids
ps-qa click --id 1842     # activate one exact semantic node
ps-qa click "<name>"      # activate the first matching semantic node
ps-qa nodes               # tree size and a role histogram
```

`press` remains a generic, explicit pointer-path diagnostic. Application suites
use semantic activation by default: resolve a name with `find`, retain the node
id, and act on that id. When repeated rows intentionally share an accessible
name, `click --id` selects the intended row without coordinates.

`inventory` navigates, expands, materializes profile-declared deferred rows,
and hovers configured surfaces with semantic node-id actions, then emits every
interactive component (buttons, links, fields, menus, switches, sliders, tabs,
and ARIA equivalents) with its surface,
role, node id, exact accessible name, and classification. Reachable controls
remain `reachable-unverified` until an outcome check proves them. Native-dialog
and external-link exceptions are `excluded-manual`; unreachable or anonymous
controls are failures. Nothing is silently counted as a pass. `cover` is the
slower, mutating sweep used when generic activation evidence is useful.
Session-ending or fixture-resetting controls are counted as `isolated` and
named for dedicated outcome runs; a shared sweep never claims a socket-closing
click passed merely because the process disconnected.

`dom` is usually the fastest way to the answer: a control that writes its state
but never appears is nearly always a hidden or zero-sized *ancestor*, which the
chain shows immediately.

## Writing a check

A check is a precondition, an action, and an assertion about the state after it:

```ron
(
    id: "confirmation-cancel-dismisses",
    group: "dialog",
    what: "the confirmation dialog's Cancel actually dismisses it",
    hover: None,
    click: Some("Cancel"),
    subject: "Confirm operation",
    expect: Vanishes,
)
```

Omitting `press` is deliberate and selects semantic node activation. Set
`press: true` only when a suite explicitly tests coordinate hit-testing.
`click` resolves every painted interactive semantic role by accessible name,
including buttons, menu items, options, checkboxes, switches, sliders and
tabs, then activates the exact semantic node id.

| Expectation | Passes when |
| --- | --- |
| `Paints` | the subject exists and has a **non-zero rendered box** |
| `Enabled` | a painted subject accepts input after the action |
| `Disabled` | a painted subject refuses input after the action |
| `Vanishes` | nothing matching is on screen (it may remain in the tree) |
| `PaintsMore` | more matching nodes are on screen than before |
| `Grows` | more matching nodes are in the tree than before |
| `Holds` | the count did not change |
| `Absent` | no matching node at all |
| `TargetPaints` | the exact accessible name selected for the click still paints |
| `Above` | the subject's painted box is above `compare` |
| `ValueChanges` | the same semantic node exposes a different value after activation |
| `SelectionChanges` | the same semantic node changes selected/pressed state |
| `DistinctPositions` | every painted member of a semantic family has a distinct center |
| `NameChanges` | the same semantic node exposes a different accessible name after the action |

Outcome checks can continue past activation with literal semantic input:
`type_into: Some("New item"), text: Some("qa audit newest"), key: Some("Enter")`.
Text is focused, selected, and exactly replaced by node id; no coordinate pointer is involved.
When the expectation is `ValueChanges`, ps-qa carries that exact text-field
node id through `SetValue` and compares its semantic value before and after.
This prevents a neighbouring same-name editor from satisfying the check.

Use `prepare` when a check needs a known semantic state before its measured
action. ps-qa activates the preparation control by node id, waits for the
renderer, and only then records the `before` snapshot. That keeps the check
independently rerunnable and prevents a default-selected control from passing
without changing anything:

```ron
prepare: Some("Models"),
click: Some("Value"),
subject: "tab:Value",
expect: SelectionChanges,
```

For a repeated family rendered by one component from one data array, declare
the shared contract explicitly with `covers`, while keeping the action and
verdict exact:

```ron
click: Some("Offer Default"),
covers: ["checkbox:Offer "],
subject: "Offer Default",
expect: ValueChanges,
```

`covers` only affects inventory reconciliation. It never broadens activation
or lets a sibling satisfy the verdict: the live check still follows the exact
clicked node id. `ps-qa list` prints every family selector so this credit is
visible during review.

`Paints` is the one that earns its keep. A node can be in the tree, correctly
named, and invisible; that is what a dead control looks like from the outside.

Prefer `Vanishes` to `Absent` for anything that closes — a dismissed dialog is
usually still in the tree at `0x0`, so asking for absence reports a working
control as broken.

### Mutation-test every check

A check that has only ever passed proves nothing. Reintroduce the bug, confirm
it goes red, restore the fix, confirm it goes green. Two of the checks here were
wrong when first written and passed anyway:

- A `Paints` assertion on `textbox` stayed green while the control was dead,
  because other textboxes on the surface always paint.
- A name-based subject was satisfied by the *pencil*, since the control and the
  editor it opens share an accessible name.

Both were caught by breaking the app on purpose. Neither would have been caught
by running the check.

## Gotchas that cost real time

- **A dirty instance poisons a delta.** `PaintsMore` and `Grows` compare against
  a baseline, so an editor left open by an earlier press is already counted.
  Restore a pristine profile before a run.
- **Cargo will not rebuild if the source mtime lands in the same minute.** The
  build reports `Finished` in 0.4s having compiled nothing, and you test a stale
  binary while believing you tested the fix. Check for a `Compiling` line.
- **Retained views keep real boxes.** A pane held behind the visible one reports
  `visible` nodes with sensible geometry, so a name can resolve to the wrong
  surface. Filter by the pane, or resolve the node id through the surface
  subtree.
- **Click cost proves nothing.** "Acknowledged in 0.01ms" reads like a detached
  handler; a control that works reports the same.

## Building

```sh
cargo build --release
cargo test
```

Nothing here may pull in tauri, winit, wgpu or blitz. The protocol types come
from `blitz-control-protocol` precisely so this binary can speak the wire
without building the renderer that serves it — depending on the runtime for the
same types would build a browser engine to send a wheel event.
`cargo tree` is the check.

## Pointing it at an application

Nothing in `src/` knows what any one application calls its controls. Two files
supply that, and both belong to the application under test:

**`ps-qa.ron`** — what the harness cannot infer. The surfaces to sweep and the
control that opens each, the permanent tabs, the collapsible section headers,
an optional per-surface `reveal_with` search field for lazily mounted rows,
the prefixes of controls that close or fold something, the region a transcript
scrolls inside, the exact native/external controls reserved for a manual pass,
controls to defer until a surface is otherwise covered, session-ending controls
that require an isolated outcome run, and controls whose successful effect is
outside the semantic tree. Found by `--app`, or
`ps-qa.ron` in the working directory. A profile that does not parse names the
file, line and column rather than degrading to empty in silence.

**`tests/ps-qa/*.ron`** — the checks. A check is a precondition, an action and
an assertion with no behaviour of its own, so it is data: editing a selector is
an edit and a re-run, not a recompile. Found by `--checks`, or
`tests/ps-qa/`. Files are read in name order, so the group order is the filename
order.

`reconcile` decodes the emitted TOON directly, including nested control rows
with per-control check arrays. Do not flatten or scrape that report before
feeding it back to the tool. Isolated controls remain listed as unverified but
do not make offline reconciliation fail: the application must run and gate
their disposable-process outcome separately. Ordinary unmapped controls and
previously recorded failures still return a nonzero exit status.

```sh
ps-qa list                      # every check, no application needed
ps-qa qa                        # run them all
ps-qa qa dialog                 # one group
ps-qa qa --checks path/to/dir   # from somewhere else
```

An application that ships neither still gets every diagnostic command — `layout`,
`dom`, `paint`, `spill`, `ghost`, `drift` — because those ask the renderer
questions that need no vocabulary.

Still application-shaped, and worth knowing before pointing this at something
new: the sweep assumes a tab strip that doubles a tab's label in its accessible
name, and `DYNAMIC_DOCUMENT` resolves "the first document tab" by that doubling. An
application that names its tabs differently will need that rule widened.
Outcome suites should also list deterministic fixture document names in
`document_openers`; this lets repeated checks reuse the open document without
guessing that an unrelated control such as `New document` is a document name.
Paged lists can similarly declare `pagination_controls` name fragments. The
inventory activates those exact semantic controls until all pages are mounted
before it counts component instances.
