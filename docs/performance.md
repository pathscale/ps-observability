# Performance measurements

`ps-qa` reports the time the application and inspection protocol actually
spent completing each check. It does not treat fixture navigation, process
startup, or an intentional event cadence as application latency.

## Per-check fields

The TOON report includes:

- `duration_ms`: setup, measured action, settling, and verdict for one check.
- `settle_iterations`: outcome snapshots required after the measured action.
- `retries`: recovery attempts needed to make the declared target reachable.

Use focused checks while diagnosing latency. A full sweep measures coverage
and interaction between surfaces; it is not a microbenchmark.

```zsh
ps-qa qa theme-base-colour
ps-qa qa --checks path/to/checks theme-base-colour
```

## Event pacing

Wheel and keyboard stress commands default to a 60 Hz input cadence. Their FPS
and missed-refresh numbers describe that requested cadence. Pass `--pace 0` to
send the next event as soon as the previous one is acknowledged and measure
maximum application throughput instead.

```zsh
ps-qa --pace 0 scroll 300 12
```

Ack latency uses finite samples only, reports an empty run without panicking,
and computes p95 using the nearest-rank definition. Outcome latency is bounded
by each check's declared timeout; the renderer host never acknowledges an
action while immediate reactive work remains queued.

## Inspection cost

Outcome polling starts at the active surface subtree. Nodes outside that scope
are retained from the action baseline, and a full-document probe is used only
when a scoped verdict fails (for example, a portal-owned toast). This keeps
serialization proportional to the active UI while preserving global outcome
semantics.

Renderer metrics are cumulative. Delta reports subtract the snapshot taken
before an interaction, so startup and earlier checks are not attributed to the
current action. `poll_hook` work caused by reading metrics is retained and
labelled as observer cost rather than silently removed.

## Reproducible comparisons

Record the app build, renderer dependency tree, viewport, profile, check id,
and `--pace` value. Compare medians across repeated fresh-profile runs; use p95
and max to investigate tail stalls, not as substitutes for the raw samples.
