# Native-renderer QA study proposal

**Status: unadopted research outline.** The October 2026 handover is stale as an execution/status report. This plan retains the research question and source-review obligations, not its venue deadlines, private deployment details, session references, result counts, or historical claims. No paper claim is established by this document.

## Candidate question

Can a native Rust web-rendering application combine semantic control identity with committed-render outcome checks, while waiting on the application's real events instead of arbitrary settle delays? The intended study is a system description and evaluation outline. It must be grounded in the current source and a current, reproducible local acceptance process before any novelty or performance claim is made.

## Candidate contributions to verify

1. **Outcome-directed settling:** `crates/ps-qa/src/runner.rs` has a `settle_for_outcome` path. Review the current code and define exactly which outcome is awaited, how stability is established, and how timeout or unsupported capability is reported.
2. **Coverage honesty:** `crates/ps-qa/src/reach.rs` exposes `Coverage::accounted` and reports `UNACCOUNTED` when buckets do not sum to the declared total. Review all relevant paths to ensure unsupported, unreachable, and incomplete work cannot be reported as verified.
3. **Paint evidence:** `blitz-control-protocol` defines committed-paint events and a client baseline operation. Trace the event from renderer to QA verdict and show that no synthetic paint is counted.
4. **In-band inspection:** the handover proposed serialized inspection between render frames. This remains a claim to verify against the renderer and host implementation; do not claim atomicity or freedom from torn snapshots until that path is demonstrated in current source.

## Scope of event-driven waiting

Limit any event-driven claim to paths that subscribe to and await committed paint on supporting hosts. Count compatibility retries without paint events and all WebGPU outcome checks as sampled paths. The control protocol exposes WebGPU diagnostics reads, and page GPU resources, error totals, and offscreen work can change independently of committed window paint.

## Claim boundaries

- Do not claim that the system is the most advanced, the first, or a novel protocol without current source-backed comparisons to relevant browser automation and native UI testing systems.
- Keep renderer-level DOM inspection, host-level semantic identity, and committed pixels/events as distinct layers. Do not attribute a capability to the renderer when it exists only in the control or QA layer.
- A stable-looking node identifier is not a stability guarantee unless its owner and lifetime contract establish that property.
- Unsupported capability must remain explicit; it cannot become a pass by silently omitting the check.
- Do not treat merged code, inventories, or a successful build as behavioral evidence.

## Evidence required before a paper is written

- Re-review each candidate contribution against the current source revision and name the exact source path and interface that implements it.
- Trace the renderer event lifecycle and distinguish paint-event waits from sampled paths in the QA results.
- Prepare focused, reproducible local witnesses that distinguish committed paint from layout-only or unsupported observations and that fail when coverage accounting is corrupted.
- Compare the design with current browser DevTools/WebDriver interfaces, Flutter's widget testing and settling APIs, accessibility-tree testing, and GUI test-flakiness research. Record the precise difference; transport resemblance is not novelty.
- Establish a current artifact and deployment boundary, including whether the headless host runs on the target platform. Report unsupported platform behavior explicitly.
- Run the repository's local gates on the final source and paper revision. No benchmark matrix, case list, corpus, or historical result is carried in this research note.

## Ownership

The source implementation stays in its owning renderer, protocol, and QA repositories. This plan tracks the paper-level question and evidence gaps only. The paper owner decides whether to pursue a submission and where its manuscript belongs; venue and deadline choices must be checked from current official sources.
