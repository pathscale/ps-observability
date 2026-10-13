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
  "protocolVersion": 2,
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

One connection serves a whole run. Inspector requests use a 15,000 ms
transport timeout by default. Set `PS_QA_REQUEST_TIMEOUT_MS` or pass
`--request-timeout-ms`; the flag takes precedence over the environment, and
values must be between 1 and 300,000 ms. A check's declared open or outcome
budget raises the transport timeout when it is longer. Polling still stops at
the semantic deadline.

A disconnected inspector or a transport timeout fails the current check and
aborts the suite. Later selected checks appear as `verdict="not-run"` with
zero duration, iterations, and retries. They do not count as passes or failures,
and the aborted run keeps a nonzero exit status.

### Host interface

A hosted run starts the host binary with no arguments. `QA_INSPECT_PAGE` names
the page or site to serve. `QA_INSPECT_STORAGE`, when a check sets `storage`,
contains a JSON object whose keys and values are strings. The host installs
those entries into `localStorage` for the page origin before any page script
runs. Invalid JSON is fatal, so the check cannot pass against an empty store.

`qa-hosted` serializes that object in memory and passes it in the child
environment before restarting the host. The seed is never written to a file.
Host output is suppressed while the seed is set. The next check with an empty
`storage` map restarts once without the variable.

## Usage

```sh
ps-qa list                       # every check, what it drives, what it asserts
ps-qa qa                         # run them all
ps-qa qa dialog                  # one group
ps-qa qa dialog-cancel-dismisses # one check, by id
ps-qa inventory                  # fast reachability counts on every surface
ps-qa inventory --require-outcomes # fail every reachable control with no named verdict
ps-qa reconcile inventory.toon   # map a saved CI inventory to current checks, no GUI
ps-qa run-plan tests/ps-qa/run-plan.ron --app ps-qa.ron # guest and signed-in app suites
```

`list` needs no running app. Everything else does. Exit code is 1 if any check
fails, so it drops into CI unchanged.

`--trace` prints the node each check activates, which is how you tell "the
control is broken" from "the check pressed the wrong thing".

### Running guest and signed-in suites

`run-plan` reads a strict RON plan and launches one fresh host for each ordered
guest invocation and signed-in session. Each guest names its own AppProfile and
declares either QA suites or a strict inventory step. Inventory may combine
check directories and files into one named-outcome manifest. Paths are relative
to the plan file.

The plan contains environment-variable names for credentials, never their
values; checks use `setup_secret: Some(Username)` or
`setup_secret: Some(Password)` for login fields. A typed `Assertion` binding can
provide a nonsecret check subject or form value without copying deployment
configuration into a check. `subject_secret` can assert that a runtime username
or password was not rendered; the value is resolved only inside its signed-in
session and redacted from reports, including escaped JSON-string forms. A session
may share credentials with another session only when both declare the same
`identity_group`. `any_of` accepts one of a check's explicitly listed simple
rendered outcomes; it does not add conditional actions. The runner removes inherited
environment from every child host, then sets only `QA_INSPECT_PAGE`,
`QA_INSPECT_STORAGE` as a JSON value when the check seeds storage, and a
unique `CHUZZ_CONTROL_DIR` under `target/ps-qa/run-plan-control`; it suppresses
the host's console output during the run.

```ron
(
    version: 1,
    environment: [
        (id: "host", name: "CHUZZ_HEADLESS_BIN", kind: Host),
        (id: "page", name: "QA_SITE_URL", kind: Page),
        (id: "auth_ws", name: "QA_PRODUCTION_AUTH_WS_URL", kind: Assertion,
            format: SecureWebSocket, default: Some("wss://auth.honey.id")),
        (id: "merchant_user", name: "QA_MERCHANT_USERNAME", kind: Secret),
        (id: "merchant_password", name: "QA_MERCHANT_PASSWORD", kind: Secret),
    ],
    host: "host",
    page: "page",
    login: Some((checks: ["login.ron"], selector: "login")),
    guests: [
        (
            id: "public",
            app_profile: "ps-qa.ron",
            suites: [(checks: ["checks"], selector: "*")],
        ),
        (
            id: "inventory",
            app_profile: "inventory.ron",
            inventory: Some((checks: ["checks", "isolated"], require_outcomes: true)),
        ),
    ],
    sessions: [
        (
            id: "merchant_admin",
            username: "merchant_user",
            password: "merchant_password",
            landing: "Home",
            suites: [
                (checks: ["members.ron"], selector: "app-members"),
            ],
        ),
    ],
)
```

Set `CHUZZ_HEADLESS_BIN`, `QA_SITE_URL`, and the declared credential variables
in the runner environment, then invoke `ps-qa --app <session-profile.ron> run-plan <plan.ron>` when the plan has signed-in sessions, or omit `--app` for guest-only plans. Unknown
plan fields, environment references, selectors, and login setup shapes fail
before the first host starts. `run-plan` does not accept tracing or pixel
capture options because either could expose rendered credential data.
Assertion formats validate the expected value shape, and a check names a
binding with `subject_env: Some("auth_ws")`. Guests and sessions run in
declaration order; plans do not execute conditional actions or shell commands.
Duplicate protected usernames are rejected unless the affected sessions name
the same explicit `identity_group` and credentials.

Paint assertions such as `Contrast`, `FullOpacity`, and `OpaqueBackground`
describe the state after a check's input. A check that toggles a theme and asks
for contrast measures the resulting theme. Contrast waits within the declared
`outcome_timeout_ms` for transition frames to become readable and honors
`stable_for_ms` when the check requires sustained contrast. Action-driven
`PixelsChange` captures the subject once before the click, key, or scroll,
drives that action once, and recaptures the same node against that baseline
until the pixels differ or `outcome_timeout_ms` expires. `settle_after_ms` is
the quiet window after the verdict, not a wait for those pixels. The native CLI
regression verifies immediate and delayed repairs, and a persistent contrast
regression that must fail:

```sh
QA_HOST=/absolute/path/to/chuzz-headless cargo test -p ps-qa --test cli \
  paint_verdict_uses_the_state_after_the_action -- --ignored
```

Use a font-enabled host for this test; it renders real text.

### Diagnosing, without writing a check

```sh
ps-qa layout "<name>"     # live boxes: x, y, w, h per matching node
ps-qa dom "<name>" 6      # attributes plus the ancestor chain
ps-qa paint "<name>"      # the colours the renderer resolved
ps-qa press "<name>"      # a real pointer: move, down, up
ps-qa pointer-drag "button:Drag handle" 60 30 # viewport-pixel displacement
ps-qa pointer-drag "button:Drag handle" 60 30 --cancel # abandon the gesture
ps-qa find "<name>" --role button # semantic matches and their node ids
ps-qa click --id 1842     # activate one exact semantic node
ps-qa click "<name>"      # activate the first matching semantic node
ps-qa nodes               # tree size and a role histogram
```

Check targets are accessible names, `role:name`, `#dom-id` or `@slot`. Any of
those forms can end in one exact attribute predicate:
`<target>[<attr>='<value>']`, for example
`#appName[aria-sort='ascending']` or
`#copy-button-0[data-copied='true']`. Attribute names use
lowercase ASCII letters, digits and hyphens; values use single quotes and are
matched exactly. Predicates currently read `aria-*` and `data-*` attributes
from the semantic snapshot. Malformed predicates and CSS selectors such as
`div.card` fail while loading the check. Accessible names can still contain
punctuation, including periods.

`dom` takes either a selector or free text: the same `role:name`, `#dom-id`,
`@slot` and attribute predicate forms work there, and a bare word searches
names, roles and values. A selector the document cannot answer at all, such as
a role no node carries or an id nobody declares, is reported as such rather
than as zero matches, which is the difference between "your selector" and
"your page".

`press` remains a generic, explicit pointer-path diagnostic. Application suites
use semantic activation by default: resolve a name with `find`, retain the node
id, and act on that id. When repeated rows intentionally share an accessible
name, `click --id` selects the intended row without coordinates.

For repeatable drag outcomes, declare
`pointer_drag: Some((from: "button:Drag handle", dx: 60.0, dy: 30.0, steps: 3))`
can instead use a painted destination:
`pointer_drag: Some((from: "button:Drag handle", to: Some("#target"), steps: 12))`.
ps-qa resolves both centers at run time, so a data-derived target can move
without copying its coordinates into the check file. Use either `to` or
`dx`/`dy` for one drag.
on a check, then assert the resulting state through `subject` and `expect`.
Adding `cancel: true` sends pointercancel instead of pointerup. This drives
pointer capture and movement; the older `drag` diagnostic directly scrolls a
container and cannot verify draggable controls.

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

`storage` is an optional map of string `localStorage` entries. A hosted run
writes them as a JSON object and installs them before the page's scripts.
Omit the field and the check sees whatever the standing document already stored.

Omitting `press` is deliberate and selects semantic node activation. Set
`press: true` only when a suite explicitly tests coordinate hit-testing.
`click` resolves every painted interactive semantic role by accessible name,
including buttons, menu items, options, checkboxes, switches, sliders and
tabs, then activates the exact semantic node id.

| Expectation | Passes when |
| --- | --- |
| `Paints` | the subject exists and has a **non-zero rendered box** |
| `Present` | the subject exists in the tree and was laid out, whatever it measures |
| `Enabled` | a painted subject accepts input after the action |
| `Disabled` | a painted subject refuses input after the action |
| `Vanishes` | nothing matching is on screen (it may remain in the tree) |
| `PaintsMore` | more matching nodes are on screen than before |
| `FamilyChanges` | a rendered repeated family replaces or recycles its members |
| `Grows` | more matching nodes are in the tree than before |
| `Holds` | the count did not change |
| `Count` | the matching family has exactly `expect_count` members |
| `Absent` | no matching node at all |
| `PaintsNamed` | every node matching the shared selector language paints |
| `DistinctPositions` | every painted family member has a distinct center |
| `ContainedBy` | every painted subject stays inside `compare` |
| `TargetPaints` | the exact accessible name selected for the click still paints |
| `Above` | the subject's painted box is above `compare` |
| `RightOf` | every subject paints completely right of a comparison member |
| `CenterAlignedY` | every subject shares a vertical center with a comparison member |
| `Measures` | every subject meets the `expect_size` width/height contract |
| `PixelsHold` | rendered pixels survive repeated pointer abuse unchanged |
| `PixelsHoldAfterHover` | the initial neutral frame already equals the post-hover neutral frame |
| `PixelsChange` | hover or a driven action visibly changes the declared capture region |
| `VisibleInk` | the capture contains visible pixels distinct from its backdrop |
| `InteriorInk` | visible content exists away from the control's border |
| `OpaqueBackground` | computed background alpha is fully opaque |
| `TransparentBackground` | computed background alpha is zero |
| `FullOpacity` | the whole subject paints at opacity 1 |
| `TransparentWindowTint` | native surface is transparent and its glass tint alpha is zero |
| `Contrast` | every matching painted subject meets its semantic contrast floor |
| `FontSizeGrows` | the same subject's computed font size increases |
| `ValueChanges` | the same semantic node exposes a different value after activation |
| `ValueIs` | the subject's semantic value equals `expect_value` |
| `SelectionChanges` | the same semantic node changes selected/pressed state |
| `NameChanges` | the same semantic node exposes a different accessible name after the action |

### Page WebGPU checks

`ps-qa gpu` prints page-owned devices, live resources, cumulative errors, and
sampled frames. `ps-qa gpu-capture texture:preview` or
`ps-qa gpu-capture canvas:#view` reads a texture or canvas and reports visible
ink. Capture pixels are not saved to disk because they can include secret
values. An explicit `--output` request returns a safe error, and automatic
pixel-artifact writes are skipped with a safe diagnostic.
The optional `PS_QA_GPU_DUMP` file output is also refused because diagnostic
labels and errors come from the inspected application.

Buffer inspection reads completed GPU writes through the same diagnostics
socket:

```sh
ps-qa gpu-buffer particles --as f32
ps-qa gpu-buffer "particles*" --offset 16 --length 32 --as f32 --stride 4
```

The positional argument is the GPUBuffer label. `*` matches any run of
characters, including an empty run, exactly as in `GpuHasResource`. Zero
matches fail naming the label; multiple matches fail naming the matching
labels. Offset and length are bytes and must be multiples of four. Offset
defaults to zero, omitted length reads to the end, `--as` defaults to `u32`,
and stride defaults to one. The available formats are little-endian `u32`,
`i32`, `f32`, and `u8`. A zero stride fails.

Output is TOON with the matched label, whole buffer size, original page usage
bits, completed submit serial, byte offset, and decoded element count, then
indexed values:

```toon
buffer[1]{label,size,usage,submit,offset,count}:
  particles,16,136,7,0,4
elements[4]{index,value}:
  0,1
  1,2
  2,3
  3,4
```

Element indices are relative to the whole buffer in the chosen type. Stride
groups up to that many elements per row with columns `index,value0,value1,...`;
the index names the first element in the row. A short final row is padded with
`null`, while count still reports only returned elements. Non-finite f32
values print as TOON strings `NaN`, `inf`, or `-inf`.

The host returns the newest completed write while a newer submit is pending.
If no completed write is available, it returns error code `pending`; the CLI
reports that state, and suite checks retry it within `outcome_timeout_ms`, just
as `GpuDraws` retries an incomplete frame. A submit serial of zero means no
queue submit has written the buffer, for example mappedAtCreation-only data.

| GPU expectation | Passes when |
| --- | --- |
| `GpuNoErrors` | cumulative validation and uncaptured error totals are zero |
| `GpuHasResource` | a live `kind:label` resource matches the label pattern |
| `GpuDraws(n)` | the newest completely sampled frame draws at least n times into the target |
| `GpuTextureInk` | the named texture or canvas contains visible ink |
| `GpuBufferRange` | every typed element in the byte range lies inside inclusive min/max bounds |
| `GpuBufferEquals` | one typed element at the byte offset equals value |
| `GpuBufferAtLeast` | one typed element at the byte offset is at least value |
| `GpuBufferChanges` | two snapshots inside the outcome window have different bytes and different nonzero completed submit serials |

Buffer expectations use subject `buffer:<label>` and the ordinary suite file
format. Ranges must be nonempty, offsets and lengths must be multiples of four,
and bounds must be finite. Equals and AtLeast fetch four bytes and judge one
element; for `u8` that is the first byte at the requested offset.

```ron
[
    (
        id: "particles-stay-in-bounds",
        group: "gpu",
        what: "every particle coordinate stays in the simulation bounds",
        subject: "buffer:particles",
        expect: GpuBufferRange(offset: 0, length: 32, as: f32, min: -1.0, max: 1.0),
        outcome_timeout_ms: 1000,
    ),
    (
        id: "particle-count-is-four",
        group: "gpu",
        what: "the GPU reports four live particles",
        subject: "buffer:particle-count",
        expect: GpuBufferEquals(offset: 0, as: u32, value: 4.0),
    ),
    (
        id: "compute-produced-results",
        group: "gpu",
        what: "the compute output counter is nonzero",
        subject: "buffer:result-count",
        expect: GpuBufferAtLeast(offset: 0, as: u32, value: 1.0),
    ),
    (
        id: "particles-advance",
        group: "gpu",
        what: "completed simulation submissions update particle bytes",
        subject: "buffer:particles",
        expect: GpuBufferChanges(offset: 0, length: 32),
        outcome_timeout_ms: 1000,
    ),
]
```

Typed f32 checks reject NaN and either infinity. Range alone accepts
`allow_non_finite: true`, which skips non-finite elements when judging bounds;
the default is false. Finite bounds and scalar values are rounded to f32 when
comparing f32 data. Integer scalar values must be integral and fit the chosen
type. Numeric failures name the matched label, absolute element index, value,
bound, and submit serial. Changes compares raw bytes without a type
interpretation; different bytes from the same submit, including writeBuffer
updates without a new writing submit, cannot satisfy it.

All buffer expectations retry failing samples against the declared outcome
deadline and honor `stable_for_ms`. Changes keeps its first completed submit
snapshot in that window as the baseline. Other host errors, malformed payloads,
and transport failures fail immediately.

`Paints`, `PaintsNamed`, `PaintsMore` and `Count` judge on boxes, because the
tree's visibility flag and the renderer disagree and trusting the flag once
reported a screen full of icons as painting nothing. That leaves a disclosure
unprovable in one direction: an Accordion, a Collapsible or a Tabs panel keeps
its box and flips `hidden`, so every geometry assertion is satisfied whether it
is open or closed, and only `Vanishes` could tell. Add `require_visible: true`
to ask for the flag as well, for a subject whose box does not move.

A box is also a font question, and CI runs a host with no font catalogue. Text
lays out at height zero there, so a control sized by its own label has no box
at all: a `w-full` primary call to action measures `0x44` while the same tree
carries `enabled=true`. `Present` is the answer on the geometry axis, but it
says nothing about input state, so `Enabled` and `Disabled` were unassertable
for exactly the controls a suite most wants to gate on. Add `text_sized: true`
to judge the flag on a subject that was laid out, whatever it measures:

```ron
subject: "button:Continue",
expect: Enabled,
text_sized: true,
```

It gives up the box and nothing else. A subject the document never produced, one
that never reached layout, and one carrying the wrong flag all still fail, and
declaring it on any other expectation is an error rather than a silent no-op.

Navigation has its own deadline. `open_timeout_ms` is the arrival budget for
the `open` step, separate from `outcome_timeout_ms` so that a route which
fetches can declare what it costs without weakening the interaction the check
exists to measure. Without it a live network round trip lands on either side of
900ms and fails as `could not open …`, which reads as a missing control.

`navigate` waits for a painted arrival before the assertion. When no opener is
declared that target is `subject`, which cannot succeed for a correct `Absent`
check: the forbidden name is what the harness then waits to paint. Set
`arrival_subject` to a positive marker the destination actually paints. It is
opt-in and off by default; omitting it leaves existing checks unchanged. The
assertion, including a negative `Absent` subject, still runs after that
arrival.

Outcome checks can continue past activation with literal semantic input:
`type_into: Some("New item"), text: Some("qa audit newest"), key: Some("Enter")`.
Text is focused, selected, and exactly replaced by node id; no coordinate pointer is involved.
When the expectation is `ValueChanges`, ps-qa carries that exact text-field
node id through `SetValue` and compares its semantic value before and after.
This prevents a neighbouring same-name editor from satisfying the check.

`ValueIs` asserts the exact value instead. The semantic value is
`aria-valuetext` when the control has one, otherwise `aria-valuenow`,
otherwise the control's own value. The runner polls that value until it
matches or `outcome_timeout_ms` expires:

```ron
subject: "Mana",
expect: ValueIs,
expect_value: Some("40 of 100"),
```

`key` sends one chord. A page that reacts to a key staying down, or to down
and up as separate phases, declares `keys`. The sequence runs after `key`
and before `scroll_over`. Each step goes to the focused element, or to the
document when nothing is focused:

```ron
keys: [
    Press("v"),
    Hold("ArrowRight", 400),
    Down("Shift"),
    Wait(120),
    Up("Shift"),
],
```

`Press` is down then up. `Hold` is down, wait the given milliseconds, then up.
`Down` and `Up` are one phase. `Wait` only sleeps. Names are the same chords
as `key`: `v`, `ArrowRight`, `Shift`, a space, `Meta+2`.

The check's clock starts with the sequence, so every `Hold` and `Wait`
counts against `outcome_timeout_ms`. Size it for the steps plus the outcome:
four presses 800 ms apart need well over 2400 ms before the verdict even
starts polling.

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
`tests/ps-qa/`. Files are read in name order, and checks run in the order they
are declared: a sequence written as steps runs as steps. The only thing that
moves is the destructive tail, which runs last so a check that deletes fixture
state cannot pull it out from under a later one.

A check with no `open` runs on whatever the previous check left in front of it,
which is how a sequence inside one file is written. Across files that is a
dependency nobody declared, so the first check of a file must say which surface
it starts on whenever an earlier file navigated somewhere.

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

