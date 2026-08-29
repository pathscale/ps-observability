//! Drive a repeatable interaction against a running Blitz application and
//! measure it.
//!
//! The point is to remove the human from the measurement. Hand-scrolling
//! produces numbers nobody can reproduce or compare; this sends a fixed number
//! of identical events at a fixed cadence and reports the frame window that
//! resulted.
//!
//! Two levels, as built into the bundle:
//!   `blitz.agent.control`  -> Inspect / Click / SetValue / ScrollIntoView / Key
//!   `blitz.diagnostics`    -> Observe / Snapshot / Metrics / WaitForIdle
//!
//! Requests are encoded from `blitz-control-protocol`, which is the server's
//! own definition of the wire. The previous client hand-wrote this JSON and got
//! the adjacent tagging of `AgentAction` wrong, which presented as a hung app.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::{Duration, Instant};

use blitz_control_protocol::{
    AgentAction, AgentControlRequest, AgentSnapshot, CaptureRequest, CapturedImage, DebugResponse,
    DebugStream, DiagnosticsRequest, InputCommand, Modifiers, PointerPhase, RendererMetrics,
    SemanticNode, SnapshotRequest,
};
use eyre::{Context, Result, bail, eyre};

use crate::computed_style::{
    font_size, opaque_background, transparent_background, wait_for_larger_font,
};
use crate::diagnostics::{dom, metrics, nodes, panes, spill, transcript};
use crate::inspector::{Client, inspect, inspect_subtree};
use crate::interaction::{
    click_named, hover_over, press_key, scroll, scroll_events, type_keys, type_text,
};
use crate::layout_report::layout;
use crate::target::{
    locate_control, name_matches, offscreen, painted_bounds, painted_named, resolved_action_target,
    selector_matches_node, viewport_of,
};
use crate::timing::{check_timeout, sleep_pace};
use crate::{app, audit, cli, inspector, paint_audit, qa, reach, report, sweep};

/// Subpixel edge coverage can vary by a few 8-bit levels between equivalent
/// GPU captures. Four levels is the first difference treated as authored ink.
const CAPTURE_CHANNEL_TOLERANCE: u8 = 3;

/// Wait only for the destination a navigation check declared.
///
/// Background counters and provider refreshes can keep the whole semantic tree
/// changing indefinitely. They are irrelevant to whether the requested panel
/// arrived, so navigation gets the same sub-second budget as every other QA
/// action and polls its exact marker.
async fn wait_for_arrival(
    client: &mut Client,
    destination: Option<&reach::Surface>,
    want_here: &str,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + check_timeout(900);
    let mut painted_streak = 0;
    let mut root = None;
    loop {
        let (tree, scoped) = if let Some(node_id) = root {
            match inspect_subtree(client, node_id).await {
                Ok((tree, _)) => (tree, true),
                // A reconciliation may replace the node between samples. One
                // full read reacquires it; a stale id is not a failed check.
                Err(_) => {
                    root = None;
                    painted_streak = 0;
                    (inspect(client).await?.0, false)
                }
            }
        } else {
            (inspect(client).await?.0, false)
        };
        let arrived = arrival_sample_matches(&tree.nodes, destination, want_here, scoped);
        if arrived && root.is_none() {
            root = arrival_anchor(&tree.nodes, destination, want_here);
        } else if !arrived && root.is_some() {
            root = None;
        }
        if stable_arrival(&mut painted_streak, arrived) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn arrival_sample_matches(
    nodes: &[SemanticNode],
    destination: Option<&reach::Surface>,
    want_here: &str,
    scoped: bool,
) -> bool {
    if scoped {
        // The anchor subtree intentionally excludes permanent chrome such as
        // the selected tab. The preceding full snapshot already proved that
        // chrome state; scoped polls only need to prove that the destination
        // marker remains painted. Reapplying the whole-surface rule here made
        // every permanent surface oscillate full -> subtree -> full until its
        // deadline.
        arrival_anchor(nodes, destination, want_here).is_some()
    } else {
        destination.map_or_else(
            || painted_named(nodes, want_here),
            |surface| reach::on_surface(nodes, surface),
        )
    }
}

/// The smallest node that proves an arrival condition.
///
/// Its id is used only as the next snapshot root. If the framework remounts it,
/// the caller clears the id and reacquires from one full snapshot.
fn arrival_anchor(
    nodes: &[SemanticNode],
    destination: Option<&reach::Surface>,
    want_here: &str,
) -> Option<u64> {
    let marker = destination.and_then(|surface| surface.marker.as_deref());
    nodes
        .iter()
        .find(|node| {
            reach::onscreen(node)
                && marker.map_or_else(
                    || selector_matches_node(node, want_here),
                    |marker| node.name.contains(marker),
                )
        })
        .map(|node| node.id)
}

/// Wait for the specific document tab, not merely any document-shaped pane.
///
/// Switching between projects leaves the outgoing project surface painted
/// while the incoming pane reconciles. A generic project marker therefore
/// reports arrival hundreds of milliseconds too early. Permanent surfaces do
/// not share that ambiguity and keep the cheaper ordinary arrival path.
async fn wait_for_navigation_arrival(
    client: &mut Client,
    destination: Option<&reach::Surface>,
    want_here: &str,
    named_document: bool,
    document_name: &str,
) -> Result<bool> {
    if !named_document {
        return wait_for_arrival(client, destination, want_here).await;
    }
    let deadline = tokio::time::Instant::now() + check_timeout(900);
    let mut painted_streak = 0;
    let mut selected_tab = None;
    loop {
        let (tree, scoped) = if let Some(node_id) = selected_tab {
            match inspect_subtree(client, node_id).await {
                Ok((tree, _)) => (tree, true),
                Err(_) => {
                    selected_tab = None;
                    painted_streak = 0;
                    (inspect(client).await?.0, false)
                }
            }
        } else {
            (inspect(client).await?.0, false)
        };
        // The exact selected tab disambiguates the incoming document from any
        // retained outgoing pane. Do not require the final action target here:
        // it may intentionally live in a collapsed or search-deferred section
        // that can only be materialized after navigation has completed.
        let arrived = named_document_is_active(&tree.nodes, document_name)
            && (scoped
                || destination.is_some_and(|surface| reach::on_surface(&tree.nodes, surface)));
        if arrived && selected_tab.is_none() {
            let tab_name = format!("{document_name}{document_name}");
            selected_tab = tree
                .nodes
                .iter()
                .find(|node| {
                    node.role.eq_ignore_ascii_case("button")
                        && node.name.eq_ignore_ascii_case(&tab_name)
                        && node.selected
                        && reach::onscreen(node)
                })
                .map(|node| node.id);
        } else if !arrived && selected_tab.is_some() {
            selected_tab = None;
        }
        if stable_arrival(&mut painted_streak, arrived) {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A newly mounted overlay or hover action can paint for one renderer snapshot,
/// reconcile, and immediately receive a different node id. Treating that first
/// frame as ready races the actual action against the remount. Three consecutive
/// painted snapshots cost only 50ms in the stable case and prove the control a
/// user sees is still present when the harness drives it.
fn stable_arrival(painted_streak: &mut u8, arrived: bool) -> bool {
    if arrived {
        *painted_streak = painted_streak.saturating_add(1);
    } else {
        *painted_streak = 0;
    }
    *painted_streak >= 3
}

/// Drive every panel check and report what the renderer did.
///
/// Each check runs against the live tree, and the three steps are separated on
/// purpose: hovering is what makes the row controls exist at all, and a check
/// that skips it reports a missing feature rather than a test driving the app
/// wrongly. That mistake is why the hover regression shipped.
///
/// Returns the number of failures, so the caller can set an exit code.
/// A spawned host that is killed when it goes out of scope.
///
/// `kill` on the happy path is not enough: anything that leaves the loop early
/// (a panic, a `?`, a break) orphans the process, and an orphaned host holds a
/// socket and a document for the rest of the session. One leaked during a sweep
/// that panicked on a missing profile, and it had to be found and killed by
/// hand.
struct HostProcess(std::process::Child);

impl Drop for HostProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A node to park the pointer on between hovers.
///
/// The largest painted node, which is the window or its root container: it is
/// always present, and it is never the small control a check is hovering.
async fn away_target(client: &mut Client) -> Result<Option<u64>> {
    let (snapshot, _) = inspect(client).await?;
    Ok(snapshot
        .nodes
        .iter()
        .filter_map(|node| node.bounds.map(|b| (node.id, b[2] * b[3])))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(id, _)| id))
}

/// Move the pointer to the document root and let authored hover state settle.
async fn park_pointer(client: &mut Client) -> std::result::Result<(), String> {
    if let Some(root) = away_target(client)
        .await
        .map_err(|error| error.to_string())?
    {
        client
            .agent(&AgentControlRequest::Act(AgentAction::Hover {
                node_id: root,
            }))
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Enter a named control repeatedly, leaving between entries.
///
/// Kept separate from the QA loop because overlays need the same abuse after
/// their trigger has prepared them, while ordinary hover-revealed controls
/// need it before preparation.
async fn repeat_hover(
    client: &mut Client,
    hover: &qa::Hover,
    leave_after: bool,
) -> std::result::Result<(), String> {
    let want = hover.target();
    let node_id = scroll_hover_target_into_view(client, want).await?;

    // A previous check may have left the pointer on this exact node. Sending
    // Hover to it again is then a move within the same target, not a new enter,
    // so hover-revealed row actions never mount. Every declared hover cycle
    // starts from the same neutral state, including the first one.
    if let Some(root) = away_target(client)
        .await
        .map_err(|error| error.to_string())?
    {
        client
            .agent(&AgentControlRequest::Act(AgentAction::Hover {
                node_id: root,
            }))
            .await
            .map_err(|error| error.to_string())?;
    }

    for turn in 0..hover.times() {
        if turn > 0
            && let Some(root) = away_target(client)
                .await
                .map_err(|error| error.to_string())?
        {
            client
                .agent(&AgentControlRequest::Act(AgentAction::Hover {
                    node_id: root,
                }))
                .await
                .map_err(|error| error.to_string())?;
        }
        client
            .agent(&AgentControlRequest::Act(AgentAction::Hover { node_id }))
            .await
            .map_err(|error| error.to_string())?;
    }

    if leave_after
        && let Some(root) = away_target(client)
            .await
            .map_err(|error| error.to_string())?
    {
        client
            .agent(&AgentControlRequest::Act(AgentAction::Hover {
                node_id: root,
            }))
            .await
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

/// Put a future hover target in its final viewport position without entering it.
///
/// Pixel checks must do this before their baseline capture. Letting the hover
/// helper scroll after that capture compares two viewport positions and reports
/// movement as a paint regression.
async fn scroll_hover_target_into_view(
    client: &mut Client,
    want: &str,
) -> std::result::Result<u64, String> {
    let explicit_role = want.split_once(':').map(|(role, _)| role);
    let any_role = ["*"];
    let roles = explicit_role.as_slice();
    let roles = if roles.is_empty() { &any_role } else { roles };
    locate_control(client, want, roles)
        .await
        .map(|(node_id, _)| node_id)
        .map_err(|error| format!("could not hover {want:?}: {error}"))
}

/// Capture one declared rendered region through the renderer's own paint path.
fn capture_node_id(nodes: &[SemanticNode], selector: &str) -> std::result::Result<u64, String> {
    nodes
        .iter()
        .filter(|node| {
            selector_matches_node(node, selector)
                && node.visible
                && node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0)
        })
        .max_by(|a, b| {
            let area = |node: &&SemanticNode| {
                node.bounds
                    .map(|bounds| bounds[2] * bounds[3])
                    .unwrap_or_default()
            };
            area(a).total_cmp(&area(b))
        })
        .map(|node| node.id)
        .ok_or_else(|| format!("no painted capture region matching {selector:?}"))
}

async fn capture_region(
    client: &mut Client,
    selector: &str,
) -> std::result::Result<CapturedImage, String> {
    let (tree, _) = inspect(client).await.map_err(|error| error.to_string())?;
    let node_id = capture_node_id(&tree.nodes, selector)?;
    capture_node_region(client, node_id, selector).await
}

/// Capture a node whose identity has already been resolved.
///
/// Stability checks take several frames of the same region. Re-inspecting the
/// whole semantic tree before every one made the check mostly a tree-transfer
/// benchmark and could not fit four frames inside its own interaction budget.
async fn capture_node_region(
    client: &mut Client,
    node_id: u64,
    selector: &str,
) -> std::result::Result<CapturedImage, String> {
    // CPU rendering is diagnostic work, not a control interaction. Keep the
    // sub-second action contract for clicks and keys, but do not kill a valid
    // capture merely because shaping/rasterising its first frame is slower.
    client.set_request_timeout(Duration::from_secs(15));
    let started = std::time::Instant::now();
    let answer = client
        .diagnostics(&DiagnosticsRequest::Capture(CaptureRequest {
            node_id: Some(node_id),
            scale: 1.0,
        }))
        .await;
    client.set_request_timeout(check_timeout(900));
    if std::env::var_os("PS_QA_TRACE_CAPTURE").is_some() {
        eprintln!("capture {selector:?}: request took {:?}", started.elapsed());
    }
    let answer = answer.map_err(|error| format!("could not capture {selector:?}: {error}"))?;
    match answer.response {
        DebugResponse::Captured(image) => Ok(image),
        DebugResponse::Error(error) => Err(format!(
            "frame capture refused: {} ({})",
            error.message, error.code
        )),
        other => Err(format!("asked for a rendered frame, got {other:?}")),
    }
}

/// Capture once the rendered region stops changing, without guessing how long
/// its authored transition lasts.
async fn capture_stable_region(
    client: &mut Client,
    selector: &str,
) -> std::result::Result<CapturedImage, String> {
    // Two adjacent samples are not a settled animation. A delayed RAF or one
    // quiet interval between keyframes can produce the same pixels twice and
    // then keep moving; fast regional capture exposed that old assumption.
    // Four matching samples span three refresh intervals, long enough to
    // cross an ordinary frame scheduling gap without baking any component's
    // authored transition duration into the harness.
    const QUIET_SAMPLES: usize = 4;
    let (tree, _) = inspect(client).await.map_err(|error| error.to_string())?;
    let node_id = capture_node_id(&tree.nodes, selector)?;
    let mut previous = capture_node_region(client, node_id, selector).await?;
    // The deadline budgets authored motion and quiet intervals, not the
    // renderer's capture work. A CPU-backed regional capture can legitimately
    // take longer than one refresh interval; charging that work to the quiet
    // deadline made an unchanged frame fail before four samples existed.
    let mut deadline = tokio::time::Instant::now() + check_timeout(900);
    let mut matching = 1;
    loop {
        tokio::time::sleep(Duration::from_millis(16)).await;
        let capture_started = tokio::time::Instant::now();
        let current = capture_node_region(client, node_id, selector).await?;
        deadline += tokio::time::Instant::now().duration_since(capture_started);
        let held =
            captured_pixels_hold(&previous, &current, CAPTURE_CHANNEL_TOLERANCE).unwrap_or(false);
        if std::env::var_os("PS_QA_TRACE_CAPTURE").is_some() {
            let delta = captured_pixel_delta(&previous, &current)
                .map(|(pixels, max)| format!("{pixels} pixel(s), max {max}"))
                .unwrap_or_else(|error| error);
            eprintln!(
                "capture {selector:?}: {}x{} -> {}x{}, held={held}, streak={matching}, {delta}",
                previous.width, previous.height, current.width, current.height
            );
        }
        if held {
            matching += 1;
            if matching >= QUIET_SAMPLES {
                return Ok(current);
            }
        } else {
            matching = 1;
        }
        if tokio::time::Instant::now() >= deadline {
            let detail = if previous.width != current.width || previous.height != current.height {
                format!(
                    "; last frame changed size from {}x{} to {}x{}",
                    previous.width, previous.height, current.width, current.height
                )
            } else {
                captured_pixel_delta(&previous, &current)
                    .map(|(pixels, max)| {
                        format!("; last frame changed {pixels} pixel(s), max channel delta {max}")
                    })
                    .unwrap_or_else(|error| format!("; {error}"))
            };
            return Err(format!(
                "rendered region {selector:?} did not settle within the interaction deadline{detail}"
            ));
        }
        previous = current;
    }
}

fn captured_pixel_delta(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(usize, u8), String> {
    use base64::Engine as _;

    let decode = |encoded: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("captured frame is not valid base64: {error}"))
    };
    let before_rgba = decode(&before.rgba_base64)?;
    let after_rgba = decode(&after.rgba_base64)?;
    if before_rgba.len() != after_rgba.len() {
        return Err(format!(
            "last frame buffer changed length from {} to {} bytes",
            before_rgba.len(),
            after_rgba.len()
        ));
    }
    let expected_len = before.width as usize * before.height as usize * 4;
    if before_rgba.len() != expected_len {
        return Err(format!(
            "captured {}x{} frame has {} bytes instead of {expected_len}",
            before.width,
            before.height,
            before_rgba.len()
        ));
    }
    let mut changed = 0;
    let mut max_delta = 0;
    for (before, after) in before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
    {
        let pixel_delta = before
            .iter()
            .zip(after.iter())
            .map(|(left, right)| left.abs_diff(*right))
            .max()
            .unwrap_or_default();
        if pixel_delta > 0 {
            changed += 1;
            max_delta = max_delta.max(pixel_delta);
        }
    }
    Ok((changed, max_delta))
}

/// Whether two same-sized captures differ only by bounded raster rounding.
///
/// GPU antialias coverage may move a few least-significant channel values
/// between otherwise identical captures. That is not visible motion and must
/// not keep a stability wait alive forever. Size or colour changes beyond the
/// small shared bound still fail.
fn captured_pixels_hold(
    before: &CapturedImage,
    after: &CapturedImage,
    channel_tolerance: u8,
) -> std::result::Result<bool, String> {
    use base64::Engine as _;

    if before.width != after.width || before.height != after.height {
        return Ok(false);
    }
    if before.rgba_base64 == after.rgba_base64 {
        return Ok(true);
    }
    let decode = |encoded: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("captured frame is not valid base64: {error}"))
    };
    let before_rgba = decode(&before.rgba_base64)?;
    let after_rgba = decode(&after.rgba_base64)?;
    Ok(before_rgba.len() == after_rgba.len()
        && before_rgba
            .iter()
            .zip(&after_rgba)
            .all(|(left, right)| left.abs_diff(*right) <= channel_tolerance))
}

/// Require two captures of the same authored state to be pixel-identical.
fn pixels_hold(before: &CapturedImage, after: &CapturedImage) -> std::result::Result<(), String> {
    use base64::Engine as _;

    if before.width != after.width || before.height != after.height {
        return Err(format!(
            "rendered frame changed size from {}x{} to {}x{}",
            before.width, before.height, after.width, after.height
        ));
    }
    if captured_pixels_hold(before, after, CAPTURE_CHANNEL_TOLERANCE)? {
        return Ok(());
    }

    let decode = |encoded: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("captured frame is not valid base64: {error}"))
    };
    let before_rgba = decode(&before.rgba_base64)?;
    let after_rgba = decode(&after.rgba_base64)?;
    let changed = before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before
                .iter()
                .zip(after.iter())
                .any(|(left, right)| left.abs_diff(*right) > CAPTURE_CHANNEL_TOLERANCE)
        })
        .count();
    Err(format!(
        "{changed} rendered pixel(s) changed after the pointer returned to the same state"
    ))
}

/// Require the visible colour placement to survive the first invalidation.
///
/// A node-scoped capture has a transparent backdrop, so rerasterizing the
/// exact same antialiased circle can legitimately change edge coverage in the
/// alpha byte. The first-paint regression this guards moves coloured content;
/// compare RGB and allow only subpixel rounding in those visible channels.
fn rgb_pixels_hold(
    before: &CapturedImage,
    after: &CapturedImage,
) -> std::result::Result<(), String> {
    use base64::Engine as _;

    if before.width != after.width || before.height != after.height {
        return Err(format!(
            "rendered frame changed size from {}x{} to {}x{}",
            before.width, before.height, after.width, after.height
        ));
    }
    let decode = |encoded: &str| {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("captured frame is not valid base64: {error}"))
    };
    let before_rgba = decode(&before.rgba_base64)?;
    let after_rgba = decode(&after.rgba_base64)?;
    const RGB_TOLERANCE: u8 = 8;
    let changed = before_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(after_rgba.as_chunks::<4>().0.iter())
        .filter(|(before, after)| {
            before[..3]
                .iter()
                .zip(after[..3].iter())
                .any(|(left, right)| left.abs_diff(*right) > RGB_TOLERANCE)
        })
        .count();
    if changed == 0 {
        Ok(())
    } else {
        Err(format!(
            "{changed} visibly coloured pixel(s) changed after the first hover"
        ))
    }
}

/// Require hover feedback to change pixels without changing the region's box.
fn pixels_change(before: &CapturedImage, after: &CapturedImage) -> std::result::Result<(), String> {
    if before.width != after.width || before.height != after.height {
        return Err(format!(
            "hover changed the capture region size from {}x{} to {}x{}",
            before.width, before.height, after.width, after.height
        ));
    }
    if captured_pixels_hold(before, after, CAPTURE_CHANNEL_TOLERANCE)? {
        Err("hover left every rendered pixel unchanged".to_owned())
    } else {
        Ok(())
    }
}

/// Wait only for the first authored hover frame, not for the whole animation.
///
/// Capturing immediately after pointer delivery races the renderer: a valid
/// transition still has its pre-hover pixels until the next frame. A 100 ms
/// ceiling keeps the interaction contract responsive while allowing that one
/// frame to be produced.
async fn wait_for_pixels_change(
    client: &mut Client,
    selector: &str,
    before: &CapturedImage,
) -> std::result::Result<(), String> {
    let deadline = tokio::time::Instant::now() + check_timeout(100);

    loop {
        let after = capture_region(client, selector).await?;
        if pixels_change(before, &after).is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return pixels_change(before, &after);
        }
        tokio::time::sleep(Duration::from_millis(8)).await;
    }
}

/// Launch a host for one page and wait for the descriptor it announces.
///
/// Waiting for the line rather than sleeping is what makes this reliable on a
/// loaded machine: a fixed sleep reads a slow first paint as a host that never
/// started.
fn start_host(
    host: &std::path::Path,
    page: &std::path::Path,
    startup_timeout: Duration,
) -> std::result::Result<(HostProcess, std::path::PathBuf), String> {
    use std::io::BufRead;

    let mut child = HostProcess(
        std::process::Command::new(host)
            // The page this host is to serve. `QA_INSPECT_PAGE` is
            // `qa-inspect-host`'s interface; a host with a different one can
            // read its own environment and ignore this.
            .env("QA_INSPECT_PAGE", page)
            .stdout(std::process::Stdio::piped())
            // Host diagnostics belong to the sweep artifact. Discarding them
            // turns a startup or renderer failure into only "never announced
            // a descriptor", which hides the one message that explains it.
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| format!("launching {}: {error}", host.display()))?,
    );

    // Read on a thread with a deadline around it: a host that dies before
    // announcing would otherwise block for ever on a pipe that will never
    // produce a line.
    let stdout = child.0.stdout.take().expect("stdout was piped");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
    });

    match rx.recv_timeout(startup_timeout) {
        Ok(line) if !line.trim().is_empty() => {
            Ok((child, std::path::PathBuf::from(line.trim().to_owned())))
        }
        _ => Err("the host never announced a descriptor".to_owned()),
    }
}

/// Drive a component library one component at a time.
///
/// Each component gets its own host process, its own document and its own
/// socket, and the process is torn down before the next one starts. That
/// isolation is the whole design: a shared process makes every check
/// order-dependent, so a failure caused by the previous component's leftover
/// state is indistinguishable from a real one, and a component that wedges the
/// renderer takes down every component after it.
///
/// The host is expected to print its descriptor path on stdout once it is
/// serving. Waiting for that line rather than sleeping a fixed interval is what
/// makes this reliable on a loaded machine: a slow first paint would otherwise
/// read as a component that never mounted.
async fn sweep_components(
    ids: &[String],
    host: &std::path::Path,
    dists: &std::path::Path,
    checks_dir: Option<&std::path::Path>,
    startup_timeout: Duration,
    mode: cli::CheckMode,
) -> Result<usize> {
    /*
     * A directory per component, or a page per component.
     *
     * A bundler that emits `button.html` beside `button.js` already produces
     * one page per component; requiring a directory each meant every project
     * copied its build into throwaway directories first. Both layouts are
     * discovered here so neither needs a staging step.
     */
    let ids: Vec<String> = if ids.is_empty() {
        let mut found: Vec<String> = std::fs::read_dir(dists)
            .with_context(|| format!("reading {}", dists.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    entry.file_name().into_string().ok()
                } else if path.extension().is_some_and(|ext| ext == "html") {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .map(str::to_owned)
                } else {
                    None
                }
            })
            .collect();
        found.sort();
        found.dedup();
        found
    } else {
        ids.to_vec()
    };

    if ids.is_empty() {
        bail!("no components to sweep under {}", dists.display());
    }

    let mut passed = 0_usize;
    let mut failed = 0_usize;
    let mut verdicts: Vec<(String, bool, String)> = Vec::new();

    for id in &ids {
        // A directory holding `index.html`, or `<id>.html` beside its siblings.
        // The host accepts either, so prefer whichever this build produced.
        let as_dir = dists.join(id);
        let as_page = dists.join(format!("{id}.html"));
        let dist = if as_dir.is_dir() {
            as_dir
        } else if as_page.is_file() {
            as_page
        } else {
            println!(
                "FAIL {id}: no built page at {} or {}",
                as_dir.display(),
                as_page.display()
            );
            verdicts.push((id.clone(), false, "no built page".to_owned()));
            failed += 1;
            continue;
        };

        /*
         * One host per *check*, not per component.
         *
         * Checks share a document otherwise, and a check then runs against
         * whatever its predecessor left behind. Measured on Dropdown: `-opens`
         * leaves the menu open, `-changes` presses the same trigger to prepare
         * itself, that press closes the menu, and the click lands on an item
         * that was on screen a moment earlier. Select failed the same way.
         * Both pass alone, so the sweep reported two working components as
         * broken.
         *
         * `run_qa` deliberately assumes the opposite for an application: there,
         * a later check inherits the surface an earlier one navigated to, and
         * re-navigating would be wrong. A component page has no surfaces and
         * nothing to inherit, so the isolation the checks assume has to come
         * from somewhere, and the host lifecycle is where it is cheapest.
         */
        let check_ids: Vec<String> = match qa::checks(checks_dir) {
            Ok(all) => all
                .iter()
                .filter(|check| check.group == *id)
                .map(|check| check.id.clone())
                .collect(),
            Err(error) => {
                println!("FAIL {id}: {error}");
                verdicts.push((id.clone(), false, error));
                failed += 1;
                continue;
            }
        };

        if check_ids.is_empty() {
            println!("FAIL {id}: no checks in group {id:?}");
            verdicts.push((id.clone(), false, "no checks".to_owned()));
            failed += 1;
            continue;
        }

        // One run per check, or a single run of the whole group when the caller
        // asked for the application's shared-state behaviour.
        let runs: Vec<String> = if mode == cli::CheckMode::Sweep {
            vec![id.clone()]
        } else {
            check_ids.clone()
        };

        let mut component_failed = 0_usize;
        let mut launch_error: Option<String> = None;

        for check_id in &runs {
            let started = match start_host(host, &dist, startup_timeout) {
                Ok(started) => started,
                Err(error) => {
                    launch_error = Some(error);
                    break;
                }
            };
            let (child, descriptor_path) = started;

            let outcome = run_component(&descriptor_path, check_id, checks_dir).await;

            // Dropped here, which kills this check's host before the next
            // one starts.
            drop(child);

            match outcome {
                Ok(count) => component_failed += count,
                Err(error) => {
                    launch_error = Some(error.to_string());
                    break;
                }
            }
        }

        let outcome: Result<usize> = match launch_error {
            Some(error) => Err(eyre!(error)),
            None => Ok(component_failed),
        };

        match outcome {
            Ok(0) => {
                println!("PASS {id}");
                verdicts.push((id.clone(), true, String::new()));
                passed += 1;
            }
            Ok(count) => {
                println!("FAIL {id}: {count} check(s) failed");
                verdicts.push((id.clone(), false, format!("{count} check(s) failed")));
                failed += 1;
            }
            Err(error) => {
                println!("FAIL {id}: {error}");
                verdicts.push((id.clone(), false, error.to_string()));
                failed += 1;
            }
        }
    }

    println!();
    println!("passed: {passed}  failed: {failed}  of {}", ids.len());
    Ok(failed)
}

/// Attach to one component's host and judge its checks.
async fn run_component(
    descriptor_path: &std::path::Path,
    id: &str,
    checks_dir: Option<&std::path::Path>,
) -> Result<usize> {
    let descriptor = inspector::discover(descriptor_path.to_str())?;
    let mut client = Client::connect(&descriptor.socket_path()).await?;
    client.initialize().await?;
    run_qa(&mut client, Some(id), checks_dir).await
}

async fn run_qa(
    client: &mut Client,
    group: Option<&str>,
    checks_dir: Option<&std::path::Path>,
) -> Result<usize> {
    let all = qa::checks(checks_dir).map_err(|error| eyre!(error))?;
    // A group *or* one check's id, so chasing a single failure does not mean
    // re-running its neighbours against the real app every time.
    // Stable surface buckets: keep dependent checks in manifest order while
    // avoiding repeated remounts of the same large application pane. The
    // application profile owns the surface openers; an unknown plain opener is
    // the configured dynamic document, while role-qualified openers stay in
    // the current bucket because they open a dialog within that surface.
    let surfaces = reach::surfaces();
    let dynamic = surfaces
        .iter()
        .position(|surface| surface.opener == reach::DYNAMIC_DOCUMENT)
        .unwrap_or(0);
    let mut affinity = dynamic;
    let mut selected: Vec<(usize, &qa::Check)> = Vec::new();
    for check in &all {
        if let Some(opener) = check.open.as_deref() {
            if let Some(index) = surfaces
                .iter()
                .position(|surface| surface.opener.eq_ignore_ascii_case(opener))
            {
                affinity = index;
            } else if !opener.contains(':') {
                affinity = dynamic;
            }
        }
        if group.is_none_or(|want| check.group == want || check.id == want) {
            selected.push((affinity, check));
        }
    }
    // Surface affinity amortizes large retained-pane mounts. Destructive
    // sequences outrank that optimization: deleting fixture state before a
    // later surface uses it makes an ordered shared sweep conflict with itself.
    selected.sort_by_key(|(surface, check)| (check.destructive, *surface));
    let selected: Vec<&qa::Check> = selected.into_iter().map(|(_, check)| check).collect();
    if selected.is_empty() {
        let mut names: Vec<String> = all
            .iter()
            .map(|check| format!("{} ({})", check.id, check.group))
            .collect();
        names.sort();
        bail!(
            "no check or group matching {group:?}. known:\n  {}",
            names.join("\n  ")
        );
    }

    let mut results: Vec<CheckResult<'_>> = Vec::new();
    for check in selected {
        let full_check_started = Instant::now();
        let mut retries = 0;
        // Navigation and disclosure materialization are suite setup. Give them
        // a bounded but realistic budget; the measured control action below
        // switches the transport to the sub-second contract.
        client.set_request_timeout(Duration::from_secs(15));
        /*
         * Navigate first, and snapshot *after*, or the baseline is the wrong
         * surface entirely and every count is measured against a screen the
         * check is not about.
         *
         * A failure here is the check's own setup failing, reported as such:
         * "could not open" rather than a verdict about the control, which is a
         * distinction that cost a round of chasing a control that was simply
         * on another surface.
         */
        /*
         * Navigation is best-effort: already being on the surface is success.
         *
         * Checks run in sequence, so a later one often inherits exactly the
         * screen it would have navigated to, and the control it navigates *by*
         * is no longer on it. Treating that as a failure reported "could not
         * open" for a check whose own press had already worked - the verdict
         * was right there and got overwritten by its own setup.
         *
         * A genuinely unreachable surface still fails, one step later, when
         * the check cannot find the control it is about.
         */
        let mut open_error = None;
        let mut pixel_outcome: Option<std::result::Result<(), String>> = None;
        if let Some(want) = check.open.as_deref() {
            /*
             * A permanent surface marker can answer "already there". A
             * document marker cannot: every document renders the same
             * composer, so `Send` only proves that some document is in front.
             *
             * Named documents are activated every time. Activating an already
             * active tab is idempotent, and is safer than probing a check
             * target that may legitimately be collapsed, deferred, or mounted
             * only after hover.
             */
            let want_here: &str = check
                .hover
                .as_ref()
                .map(qa::Hover::target)
                .or(check.prepare.as_deref())
                .or(check.click.as_deref())
                .or(check.type_into.as_deref())
                .or(check.key_on.as_deref())
                .unwrap_or(&check.subject);
            let destination = surface_for_opener(want);
            let named_document = is_named_document_opener(want);
            let (here, _) = inspect(client).await?;
            let active_document_matches = named_document_is_active(&here.nodes, want);
            let arrived = arrived_without_navigation(
                &here.nodes,
                destination,
                want_here,
                named_document,
                active_document_matches,
            );
            if cli::trace() {
                let surface = reach::surfaces()
                    .iter()
                    .find(|surface| reach::on_surface(&here.nodes, surface))
                    .map(|surface| surface.name.as_str())
                    .unwrap_or("none");
                println!(
                    "        arrival want={want:?} destination={} target={want_here:?} \
                     named_document={named_document} arrived={arrived} on_surface={surface}",
                    destination.map_or("dynamic", |surface| surface.name.as_str()),
                );
            }
            if !arrived {
                /*
                 * Two steps, because the opener may not be on this surface.
                 *
                 * A document can be opened from a root-surface row, and once
                 * the document is in front that row is gone, so a check that navigates by it
                 * failed with "no visible, enabled, sized button" for a
                 * surface that was one press away. The configured root opener
                 * provides the recovery path.
                 */
                /*
                 * A tab is pressed, a row is double clicked, and which one
                 * this is cannot be known from the name - so try the cheap
                 * gesture first and escalate.
                 *
                 * Double clicking a tab-strip entry may toggle it rather than
                 * navigating, while a single row click may fold it, so
                 * committing to either gesture broke the other set of checks.
                 */
                let first_click = click_opener_quiet(client, want, named_document).await;
                let mut there = wait_for_navigation_arrival(
                    client,
                    destination,
                    want_here,
                    named_document,
                    want,
                )
                .await?;
                // A surface transition can briefly remove the opener before
                // its replacement paints. Retry the same semantic click after
                // settling; escalating that transient miss straight to a
                // document-row double-click skips ordinary button handlers.
                if !there && first_click.is_err() {
                    let _ = click_named_quiet(client, want).await;
                    there = wait_for_navigation_arrival(
                        client,
                        destination,
                        want_here,
                        named_document,
                        want,
                    )
                    .await?;
                }
                if !there && open_named(client, want).await.is_ok() {
                    there = wait_for_navigation_arrival(
                        client,
                        destination,
                        want_here,
                        named_document,
                        want,
                    )
                    .await?;
                }
                if !there {
                    if let Some(home) = reach::profile().home_opener.as_deref() {
                        let _ = click_named_quiet(client, home).await;
                    }
                    if let Err(error) = open_named(client, want).await {
                        open_error = Some(format!("could not open {want:?}: {error}"));
                    } else if !wait_for_navigation_arrival(
                        client,
                        destination,
                        want_here,
                        named_document,
                        want,
                    )
                    .await?
                    {
                        open_error = Some(format!(
                            "could not open {want:?}: destination did not paint within {}ms",
                            check_timeout(900).as_millis()
                        ));
                    }
                }
            }
            // The declared outcome below is the settle condition. Waiting for
            // an unrelated whole-tree node count to stabilize makes every
            // check pay for background updates and can hide transient UI
            // acknowledgements such as "Copied".
        }

        if open_error.is_none()
            && let (Some(field), Some(value)) = (
                check.setup_type_into.as_deref(),
                check.setup_text.as_deref(),
            )
            && let Err(error) = type_text(client, field, value).await
        {
            open_error = Some(format!(
                "could not establish setup value in {field:?}: {error}"
            ));
        }

        /*
         * A check must be independent of whichever disclosure a previous
         * check left closed. The application profile already identifies its
         * collapsible sections for inventory and coverage; use that same
         * app-owned information when the check's action target is not exposed.
         *
         * Only expand when the target is missing. This preserves intentional
         * collapse/expand round trips: an `Expand …` action remains available
         * after the preceding check closed its section and is not pre-empted.
         */
        if open_error.is_none()
            && let Some(reveal) = check.reveal_before_capture.as_deref()
        {
            if let Err(error) = scroll_hover_target_into_view(client, reveal).await {
                open_error = Some(error);
            } else {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        let setup_target = check
            .hover
            .as_ref()
            .map(qa::Hover::target)
            .or(check.prepare.as_deref())
            .or(check.click.as_deref())
            .or(check.type_into.as_deref())
            .or(check.key_on.as_deref())
            .unwrap_or(&check.subject);
        let (current, _) = inspect(client).await?;
        if !painted_named(&current.nodes, setup_target)
            && let Some(surface) = reach::surfaces()
                .iter()
                .find(|surface| reach::on_surface(&current.nodes, surface))
        {
            let opened = expand_everything(client, surface).await?;
            if cli::trace() && opened > 0 {
                println!("        opened {opened} collapsed section(s) for {setup_target:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let (expanded, _) = inspect(client).await?;
        if !painted_named(&expanded.nodes, setup_target)
            && let Some(surface) = reach::surfaces()
                .iter()
                .find(|surface| reach::on_surface(&expanded.nodes, surface))
        {
            let reveals = reveal_deferred_content(client, surface, setup_target).await?;
            if cli::trace() && reveals > 0 {
                println!(
                    "        revealed deferred content for {setup_target:?} in {reveals} step(s)"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        let mut nodes_after_first_hover = None;

        client.set_request_timeout(check_timeout(check.outcome_timeout_ms.max(900)));

        // Hover first: the row actions do not exist until `pointerenter`.
        //
        // Aimed at a node inside the panel column, not merely one whose name
        // matches. Another retained surface may render the same control names,
        // so hovering by name alone can land in the wrong list.
        if open_error.is_none()
            && let Some(hover) = check.hover.as_ref()
        {
            let already_hovered = if let Some(unless) = check.hover_unless.as_deref() {
                let (snapshot, _) = inspect(client).await?;
                painted_named(&snapshot.nodes, unless)
            } else {
                false
            };
            if already_hovered {
                // The declared revealed state is already established.
            } else {
                let hovered = if hover.times() > 1 {
                    let first = qa::Hover::Once(hover.target().to_owned());
                    match repeat_hover(client, &first, false).await {
                        Err(error) => Err(error),
                        Ok(()) => {
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            nodes_after_first_hover = Some(inspect(client).await?.0.nodes.len());
                            match park_pointer(client).await {
                                Err(error) => Err(error),
                                Ok(()) => {
                                    let remaining = qa::Hover::Times(
                                        hover.target().to_owned(),
                                        hover.times() - 1,
                                    );
                                    repeat_hover(client, &remaining, false).await
                                }
                            }
                        }
                    }
                } else {
                    repeat_hover(client, hover, false).await
                };
                if let Err(error) = hovered {
                    open_error = Some(error);
                } else if let Some(next) = check.prepare.as_deref().or(check.click.as_deref()) {
                    if !wait_for_arrival(client, None, next).await? {
                        // A virtualized row can reconcile after ScrollIntoView
                        // and lose the hover that was sent to its prior node.
                        // Reacquire it once; silently continuing turns a setup
                        // race into a misleading "could not click" failure.
                        if let Err(error) = repeat_hover(client, hover, false).await {
                            open_error = Some(error);
                        } else if !wait_for_arrival(client, None, next).await? {
                            open_error = Some(format!(
                                "hovering {:?} did not reveal {next:?}",
                                hover.target()
                            ));
                        }
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }

        // Preparation may itself be a hover-mounted control (for example a
        // row action that opens a dialog). Hover must therefore happen first;
        // the baseline remains after both setup actions so only the declared
        // outcome is measured.
        if open_error.is_none()
            && let Some(want) = check.prepare.as_deref()
        {
            let already_prepared = if let Some(unless) = check.prepare_unless.as_deref() {
                let (snapshot, _) = inspect(client).await?;
                snapshot.nodes.iter().any(|node| {
                    node.visible
                        && selector_matches_node(node, unless)
                        && painted_bounds(node).is_some()
                })
            } else {
                false
            };
            let mut prepared = if already_prepared {
                Ok(())
            } else if let Some(key) = check.prepare_key.as_deref() {
                press_key(client, key, 1, want, true).await.map(|_| ())
            } else if check.prepare_press {
                press_named(client, want).await
            } else {
                click_named_quiet(client, want).await.map(|_| ())
            };
            if prepared.is_err() {
                retries += 1;
                // A retained sibling document may expose the same target name
                // and make the global precheck skip expansion on the selected
                // document. Recover against the check's declared destination,
                // then retry the exact preparation once.
                let declared_surface = check.open.as_deref().and_then(surface_for_opener);
                let live_surface = if declared_surface.is_some() {
                    declared_surface
                } else {
                    let (snapshot, _) = inspect(client).await?;
                    reach::surfaces()
                        .iter()
                        .find(|surface| reach::on_surface(&snapshot.nodes, surface))
                };
                if let Some(surface) = live_surface {
                    let _ = expand_everything(client, surface).await?;
                    let _ = reveal_deferred_content(client, surface, want).await?;
                    prepared = if let Some(key) = check.prepare_key.as_deref() {
                        press_key(client, key, 1, want, true).await.map(|_| ())
                    } else if check.prepare_press {
                        press_named(client, want).await
                    } else {
                        click_named_quiet(client, want).await.map(|_| ())
                    };
                }
            }
            if let Err(error) = prepared {
                /*
                 * Say what *is* addressable, not just what was not found.
                 *
                 * "could not prepare \"Switch\": no visible, enabled, sized
                 * semantic control matching it" is true and nearly useless: it
                 * does not distinguish a broken component from a check naming a
                 * control that never existed. Ten components failed exactly
                 * that way, every one of them a manifest guess rather than a
                 * defect, and each cost a manual measurement to tell apart.
                 *
                 * Listing the named controls that are on screen turns the
                 * common case into a one-line fix: the trigger reads
                 * "Effort: medium", not "Effort".
                 */
                let nearby = match inspect(client).await {
                    Ok((snapshot, _)) => {
                        let mut names: Vec<String> = snapshot
                            .nodes
                            .iter()
                            .filter(|node| {
                                !node.name.is_empty()
                                    && node.visible
                                    && node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0)
                            })
                            .map(|node| format!("{}:{}", node.role, node.name))
                            .collect();
                        names.sort();
                        names.dedup();
                        names.truncate(8);
                        if names.is_empty() {
                            String::new()
                        } else {
                            format!("; on screen: {}", names.join(", "))
                        }
                    }
                    Err(_) => String::new(),
                };
                open_error = Some(format!("could not prepare {want:?}: {error}{nearby}"));
            }
            if let Some(next) = check
                .click
                .as_deref()
                .or(check.type_into.as_deref())
                .or(check.key_on.as_deref())
            {
                let _ = wait_for_arrival(client, None, next).await?;
            } else {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }

        // Overlay contents do not exist until their trigger has prepared
        // them. Exercise their pointer-driven incremental resolves now, then
        // leave the pointer away so the final authored hover state is stable.
        if open_error.is_none()
            && let Some(hover) = check.after_prepare_hover.as_ref()
        {
            if check.expect == qa::Expect::PixelsHold {
                let measured = async {
                    park_pointer(client).await?;
                    // A dropdown keeps the last entered item active after the
                    // pointer leaves. Establish that authored state once so
                    // the two frames differ only if later resolves accumulate
                    // renderer paint entries.
                    let establish = qa::Hover::Once(hover.target().to_owned());
                    repeat_hover(client, &establish, true).await?;
                    let before = capture_stable_region(client, &check.subject)
                        .await
                        .map_err(|error| format!("before hover: {error}"))?;
                    repeat_hover(client, hover, true).await?;
                    let after = capture_stable_region(client, &check.subject)
                        .await
                        .map_err(|error| format!("after hover: {error}"))?;
                    pixels_hold(&before, &after)
                }
                .await;
                pixel_outcome = Some(measured);
            } else if check.expect == qa::Expect::PixelsHoldAfterHover {
                let measured = async {
                    match wait_for_arrival(client, None, hover.target()).await {
                        Ok(true) => {}
                        Ok(false) => {
                            return Err(format!(
                                "could not prepare hover target {:?}: it did not finish painting",
                                hover.target()
                            ));
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                    scroll_hover_target_into_view(client, hover.target()).await?;
                    park_pointer(client).await?;
                    let before = capture_stable_region(client, &check.subject)
                        .await
                        .map_err(|error| format!("before hover: {error}"))?;
                    repeat_hover(client, hover, true).await?;
                    let after = capture_stable_region(client, &check.subject)
                        .await
                        .map_err(|error| format!("after hover: {error}"))?;
                    rgb_pixels_hold(&before, &after)
                }
                .await;
                pixel_outcome = Some(measured);
            } else if check.expect == qa::Expect::PixelsChange {
                let measured = async {
                    let node_id = scroll_hover_target_into_view(client, hover.target()).await?;
                    park_pointer(client).await?;
                    let before = capture_region(client, &check.subject).await?;
                    let event_driven = client
                        .arm_paint_events()
                        .await
                        .map_err(|error| error.to_string())?;
                    let require_events = std::env::var_os("PS_QA_REQUIRE_PAINT_EVENTS").is_some();
                    if require_events && !event_driven {
                        return Err("the inspector does not provide paint events".to_owned());
                    }
                    client
                        .agent(&AgentControlRequest::Act(AgentAction::Hover { node_id }))
                        .await
                        .map_err(|error| error.to_string())?;

                    let paint_committed = if event_driven {
                        client
                            .wait_for_paint(check_timeout(100))
                            .await
                            .map_err(|error| error.to_string())?
                    } else {
                        false
                    };
                    if require_events && !paint_committed {
                        return Err("no paint event arrived after hover".to_owned());
                    }

                    if paint_committed {
                        let after = capture_region(client, &check.subject).await?;
                        pixels_change(&before, &after)
                    } else {
                        // Compatibility with runtimes and headless hosts that
                        // predate paint notifications. Bounded to 100 ms and
                        // removed once the fleet has adopted the stream.
                        wait_for_pixels_change(client, &check.subject, &before).await
                    }
                }
                .await;
                pixel_outcome = Some(measured);
            } else if let Err(error) = repeat_hover(client, hover, true).await {
                open_error = Some(error);
            }
        }

        /*
         * Nothing accumulated across explicitly repeated hover cycles.
         *
         * Entering a control can legitimately mount something, so compare the
         * first completed hover with the final completed hover. Both snapshots
         * have the same authored pointer state. A single-hover check has no
         * second state to compare and must not treat unrelated asynchronous
         * rendering as a leak.
         *
         * Checks request repetition explicitly because it is deliberate abuse,
         * but the accumulation assertion comes with that request automatically.
         */
        if let Some(after_first) = nodes_after_first_hover
            && open_error.is_none()
        {
            let after_repeats = inspect(client).await?.0.nodes.len();
            let retained = after_repeats as i64 - after_first as i64;
            if retained > 0 {
                open_error = Some(format!(
                    "repeated hover left {retained} more node(s) behind \
                     ({after_first} -> {after_repeats}); something later entries add is never removed"
                ));
            }
        }

        // Hover can mount the action that the check will drive. Capture the
        // baseline afterward: inspecting before hover both omitted that real
        // pre-action state and paid for an immediately discarded snapshot.
        let (before, _) = inspect(client).await?;

        if open_error.is_none() && check.expect == qa::Expect::OpaqueBackground {
            pixel_outcome = Some(opaque_background(client, &check.subject).await);
        } else if open_error.is_none() && check.expect == qa::Expect::TransparentBackground {
            pixel_outcome = Some(transparent_background(client, &check.subject).await);
        } else if open_error.is_none() && check.expect == qa::Expect::Contrast {
            pixel_outcome = Some(
                paint_audit::contrast(client, &check.subject, 4.5, 3.0)
                    .await
                    .map_err(|error| error.to_string()),
            );
        }

        let before_font_size = if open_error.is_none() && check.expect == qa::Expect::FontSizeGrows
        {
            match font_size(client, &check.subject).await {
                Ok(size) => Some(size),
                Err(error) => {
                    pixel_outcome = Some(Err(error));
                    None
                }
            }
        } else {
            None
        };

        let before_action_pixels = if open_error.is_none()
            && check.expect == qa::Expect::PixelsChange
            && check.after_prepare_hover.is_none()
        {
            Some(match capture_node_id(&before.nodes, &check.subject) {
                Ok(node_id) => capture_node_region(client, node_id, &check.subject)
                    .await
                    .map(|image| (node_id, image)),
                Err(error) => Err(error),
            })
        } else {
            None
        };

        /*
         * The tree is mostly nodes somebody can see.
         *
         * Every other assertion here names one control and asks about it, so a
         * document can fill with abandoned subtrees while every check passes:
         * the control they name is still present and still correct, and the
         * dead nodes are 0x0 and hidden, so they do not even perturb the
         * measurement. Measured on a real instance after a few hours of use,
         * 96,263 nodes where a fresh one holds 635, and 91% of them zero-box.
         * Every check passed throughout, and `ps-qa ghost`, which exists for
         * exactly this, could not finish because the tree had grown past what
         * the inspector would serve.
         *
         * So the question has to be asked for free, on every check, rather
         * than written into one somebody thinks to add. A view legitimately
         * holds hidden nodes - a closed menu, a collapsed row - so this is a
         * ratio and an absolute floor. A deliberately filtered settings
         * catalog can hold a few thousand valid zero-box descendants after a
         * full inventory; the leak this protects against grows into tens of
         * thousands. Past 5,000 dead nodes and five dead for every live one,
         * something is being retained rather than reused.
         */
        {
            let dead = before
                .nodes
                .iter()
                .filter(|node| !node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0))
                .count();
            let live = before.nodes.len().saturating_sub(dead);
            if live > 0 && dead > 5_000 && dead > live * 5 {
                open_error = Some(format!(
                    "the document holds {dead} node(s) with no box against {live} with one, \
                     out of {}; something is retaining subtrees rather than reusing them",
                    before.nodes.len()
                ));
            }
        }
        let mut check_started = (check.click.is_none()
            && check.text.is_none()
            && check.key.is_none()
            && check.scroll_over.is_none())
        .then(Instant::now);

        // Then the action, if this check is about one. A click that cannot be
        // dispatched is itself a failure, not a skip.
        let mut action_error = open_error;
        let mut action_target = None;
        let mut action_node_id = None;
        if action_error.is_none()
            && let Some(want) = check.click.as_deref()
        {
            if check.text.is_none() && check.key.is_none() {
                check_started = Some(Instant::now());
            }
            if check.expect == qa::Expect::TargetPaints && !check.press {
                let (tree, _) = inspect(client).await?;
                action_target = resolved_action_target(&tree.nodes, want);
            }
            let driven = drive_check_action(client, want, check.press).await;
            action_error = driven.as_ref().err().cloned();
            action_node_id = driven.ok().flatten();
            // The exact declared outcome below polls the renderer. A generic
            // whole-tree settle here both duplicates that work and waits on
            // unrelated background updates.
        }

        if action_error.is_none()
            && let Some(text) = check.text.as_deref()
        {
            if check.key.is_none() {
                check_started = Some(Instant::now());
            }
            if let Some(field) = check.type_into.as_deref() {
                match type_text(client, field, text).await {
                    Ok(node_id) => action_node_id = Some(node_id),
                    Err(error) => {
                        action_error = Some(format!("could not type into {field:?}: {error}"));
                    }
                }
            } else {
                action_error = Some("text requires type_into".to_owned());
            }
        }
        if action_error.is_none()
            && let Some(key) = check.key.as_deref()
        {
            check_started = Some(Instant::now());
            let target = check
                .key_on
                .as_deref()
                .or(check.type_into.as_deref())
                .unwrap_or("");
            if let Err(error) = press_key(client, key, 1, target, check.key_on.is_some()).await {
                action_error = Some(format!("could not send {key:?}: {error}"));
            }
        }
        if action_error.is_none()
            && let Some(target) = check.scroll_over.as_deref()
        {
            check_started = Some(Instant::now());
            match hover_over(client, target).await {
                Ok(true) => {
                    if let Err(error) =
                        scroll_events(client, check.scroll_ticks, check.scroll_delta).await
                    {
                        action_error = Some(format!("could not scroll over {target:?}: {error}"));
                    }
                }
                Ok(false) => action_error = Some(format!("no painted node matching {target:?}")),
                Err(error) => {
                    action_error = Some(format!("could not aim at {target:?}: {error}"));
                }
            }
        }
        let transport_timed_out = action_error
            .as_deref()
            .is_some_and(|error| error.contains("inspector did not answer within"));
        if pixel_outcome.is_none()
            && let Some(before_pixels) = before_action_pixels
        {
            pixel_outcome = Some(match before_pixels {
                Ok((node_id, before_pixels)) => {
                    match capture_node_region(client, node_id, &check.subject).await {
                        Ok(after_pixels) => pixels_change(&before_pixels, &after_pixels),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            });
        }
        if pixel_outcome.is_none()
            && let Some((node_id, before_size)) = before_font_size
        {
            pixel_outcome = Some(
                wait_for_larger_font(
                    client,
                    node_id,
                    &check.subject,
                    before_size,
                    check_timeout(if check.outcome_timeout_ms == 0 {
                        900
                    } else {
                        check.outcome_timeout_ms
                    }),
                )
                .await,
            );
        }

        let live_paint_expect = matches!(
            check.expect,
            qa::Expect::PixelsHold
                | qa::Expect::PixelsHoldAfterHover
                | qa::Expect::PixelsChange
                | qa::Expect::OpaqueBackground
                | qa::Expect::TransparentBackground
                | qa::Expect::Contrast
                | qa::Expect::FontSizeGrows
        );
        let fallback_after = || AgentSnapshot {
            nodes: before.nodes.clone(),
            ..AgentSnapshot::default()
        };
        let (after, settle_error, settle_iterations) = if live_paint_expect {
            match inspect(client).await {
                Ok((snapshot, _)) => (snapshot, None, 0),
                Err(error) => (
                    fallback_after(),
                    Some(format!("could not inspect after the action: {error}")),
                    0,
                ),
            }
        } else if action_error.is_none() || transport_timed_out {
            match settle_for_outcome(
                client,
                check,
                &before.nodes,
                action_target.as_deref(),
                action_node_id,
            )
            .await
            {
                Ok(settled) => settled,
                Err(error) => (
                    fallback_after(),
                    Some(format!("could not inspect the rendered outcome: {error}")),
                    0,
                ),
            }
        } else {
            match inspect(client).await {
                Ok((snapshot, _)) => (snapshot, None, 0),
                Err(error) => (
                    fallback_after(),
                    Some(format!(
                        "could not inspect after the failed action: {error}"
                    )),
                    0,
                ),
            }
        };
        let mut outcome = if live_paint_expect {
            match action_error {
                Some(error) => Err(error),
                None => pixel_outcome.unwrap_or_else(|| {
                    Err("the live paint expectation was not measured".to_owned())
                }),
            }
        } else {
            match action_error {
                Some(error) if transport_timed_out => outcome_verdict(
                    check,
                    &before.nodes,
                    &after.nodes,
                    action_target.as_deref(),
                    action_node_id,
                )
                .map_err(|outcome| format!("{error}; rendered outcome also failed: {outcome}")),
                Some(error) => Err(error),
                None => outcome_verdict(
                    check,
                    &before.nodes,
                    &after.nodes,
                    action_target.as_deref(),
                    action_node_id,
                ),
            }
        };
        if let Some(error) = settle_error {
            outcome = Err(match outcome {
                Ok(()) => error,
                Err(existing) => format!("{existing}; {error}"),
            });
        }
        // Preparation is not the outcome latency. Opening an editor, hovering
        // its row, and filling a field establish the precondition; the final
        // click, value commit, or key is the user action whose rendered result
        // must arrive inside the verdict budget. An overloaded runner can opt
        // into a visible multiplier; the default remains the strict contract.
        let elapsed = check_started.unwrap_or_else(Instant::now).elapsed();
        let declared_outcome = if check.outcome_timeout_ms == 0 {
            900
        } else {
            check.outcome_timeout_ms
        };
        let verdict_budget = check_timeout(1_250.max(declared_outcome.saturating_add(250)));
        if elapsed > verdict_budget {
            let timing = format!(
                "check exceeded {}ms ({:.0}ms)",
                verdict_budget.as_millis(),
                elapsed.as_secs_f64() * 1000.0
            );
            outcome = Err(match outcome {
                Ok(()) => timing,
                Err(error) => format!("{error}; {timing}"),
            });
        }
        if cli::trace() {
            match &outcome {
                Ok(()) => println!(
                    "        verdict pass: {} ({:.0}ms)",
                    check.id,
                    elapsed.as_secs_f64() * 1000.0
                ),
                Err(error) => println!(
                    "        verdict fail: {} ({:.0}ms): {error}",
                    check.id,
                    elapsed.as_secs_f64() * 1000.0
                ),
            }
        }
        results.push(CheckResult {
            check,
            outcome,
            duration_ms: full_check_started.elapsed().as_millis() as u64,
            settle_iterations,
            retries,
        });
        if check.settle_after_ms > 0 {
            tokio::time::sleep(Duration::from_millis(check.settle_after_ms)).await;
        }
    }

    let failed = results
        .iter()
        .filter(|result| result.outcome.is_err())
        .count();
    let tally_input: Vec<_> = results
        .iter()
        .map(|result| (result.check, result.outcome.clone()))
        .collect();
    let tally = qa::tally(&tally_input);
    let mut groups: Vec<_> = tally.iter().collect();
    groups.sort_by_key(|(name, _)| *name);

    /*
     * TOON, for a caller that is a program - usually a language model.
     *
     * The column format below is for a person: reading it back means splitting
     * on whitespace, which loses any field containing a space. `what` always
     * contains one and a failure message nearly always does. That mis-parse is
     * not hypothetical - it is how a node at `[0,58 0x0]` got read as painting.
     *
     * TOON rather than JSON because a uniform array declares its length and
     * field names once, then spends one line per row instead of repeating every
     * key in every object. For a run of two dozen checks that is a large
     * fraction of the tokens, and the declared `[n]` lets the reader verify it
     * received every row rather than a truncated list.
     *
     * Nesting is why this is not flat TSV: reporting wants a check to carry its
     * own detail, and TOON keeps that expressible without giving up the tabular
     * economy.
     *
     * Encoded by `toon-format` rather than by hand. Quoting is the whole
     * problem here - a check description and a failure message both routinely
     * contain the delimiter ("Edit " went 19 -> 21, expected no change) - and a
     * hand-written emitter is a guess at the specification that drifts from it
     * silently. The crate is generic over `serde::Serialize`, so anything with
     * a `Serialize` impl encodes without a bespoke path.
     */

    let report = Report {
        passed: results.len() - failed,
        failed,
        groups: groups
            .into_iter()
            .map(|(name, (passed, total))| GroupRow {
                name: name.to_string(),
                passed: *passed,
                total: *total,
            })
            .collect(),
        checks: results.iter().map(CheckRow::from).collect(),
    };
    println!(
        "{}",
        toon_format::encode_default(&report).map_err(|e| eyre!(e.to_string()))?
    );
    Ok(failed)
}

fn outcome_verdict(
    check: &qa::Check,
    before: &[SemanticNode],
    after: &[SemanticNode],
    action_target: Option<&str>,
    action_node_id: Option<u64>,
) -> std::result::Result<(), String> {
    if check.expect == qa::Expect::TargetPaints {
        let Some(subject) = action_target else {
            return Err("could not resolve the exact click target".to_owned());
        };
        let mut targeted = check.clone();
        targeted.subject = subject.to_owned();
        targeted.expect = qa::Expect::Paints;
        qa::verdict(&targeted, before, after)
    } else if check.expect == qa::Expect::ValueChanges
        && action_target.is_some_and(|target| target == check.subject)
        && let Some(node_id) = action_node_id
    {
        qa::value_changed(node_id, before, after)
    } else if check.expect == qa::Expect::SelectionChanges
        && action_target.is_some_and(|target| target == check.subject)
        && let Some(node_id) = action_node_id
    {
        qa::selection_changed(node_id, before, after)
    } else {
        qa::verdict(check, before, after)
    }
}

#[derive(Default)]
struct OutcomeStability {
    fingerprint: Option<u64>,
    since: Option<tokio::time::Instant>,
}

impl OutcomeStability {
    fn observe(
        &mut self,
        now: tokio::time::Instant,
        fingerprint: u64,
        passing: bool,
        required: Duration,
    ) -> bool {
        if !passing {
            self.fingerprint = None;
            self.since = None;
            return false;
        }
        if required.is_zero() {
            return true;
        }
        if self.fingerprint != Some(fingerprint) {
            self.fingerprint = Some(fingerprint);
            self.since = Some(now);
            return false;
        }
        self.since
            .is_some_and(|since| now.duration_since(since) >= required)
    }
}

fn semantic_fingerprint(nodes: &[SemanticNode]) -> u64 {
    let mut fingerprint = DefaultHasher::new();
    nodes.len().hash(&mut fingerprint);
    for node in nodes {
        node.dom_id.hash(&mut fingerprint);
        node.id.hash(&mut fingerprint);
        node.parent.hash(&mut fingerprint);
        node.role.hash(&mut fingerprint);
        node.name.hash(&mut fingerprint);
        node.value.hash(&mut fingerprint);
        node.enabled.hash(&mut fingerprint);
        node.visible.hash(&mut fingerprint);
        node.selected.hash(&mut fingerprint);
        node.bounds
            .map(|bounds| bounds.map(f64::to_bits))
            .hash(&mut fingerprint);
        node.slot.hash(&mut fingerprint);
    }
    fingerprint.finish()
}

/// Wait for the declared result, not merely for a tree that already contains
/// the subject.
///
/// A refresh indicator exists both before and after its button is activated.
/// Waiting for that node to paint therefore returned immediately, even when
/// its backend read was still running. QA is an interactive contract: a result
/// that cannot paint inside one second is reported as slow rather than making
/// every later check wait behind it.
async fn settle_for_outcome(
    client: &mut Client,
    check: &qa::Check,
    before: &[SemanticNode],
    action_target: Option<&str>,
    action_node_id: Option<u64>,
) -> Result<(AgentSnapshot, Option<String>, u32)> {
    let outcome_ms = if check.outcome_timeout_ms == 0 {
        900
    } else {
        check.outcome_timeout_ms
    };
    let deadline = tokio::time::Instant::now() + check_timeout(outcome_ms);
    let stable_for = Duration::from_millis(check.stable_for_ms);
    let mut stability = OutcomeStability::default();
    let mut iterations = 0;
    loop {
        let (after, _) = inspect(client).await?;
        iterations += 1;
        let now = tokio::time::Instant::now();
        let passing =
            outcome_verdict(check, before, &after.nodes, action_target, action_node_id).is_ok();
        if stability.observe(now, semantic_fingerprint(&after.nodes), passing, stable_for) {
            return Ok((after, None, iterations));
        }
        if now >= deadline {
            let error = (passing && !stable_for.is_zero()).then(|| {
                format!(
                    "rendered outcome did not remain complete and unchanged for {}ms within {}ms",
                    stable_for.as_millis(),
                    check_timeout(outcome_ms).as_millis()
                )
            });
            return Ok((after, error, iterations));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Resolve an application-owned surface opener.
///
/// Dynamic document names and ordinary controls are not interchangeable, so
/// document names are explicit profile data rather than guessed from an
/// arbitrary unknown label.
fn surface_for_opener(want: &str) -> Option<&'static reach::Surface> {
    reach::surfaces()
        .iter()
        .find(|surface| surface.opener == want)
        .or_else(|| {
            reach::profile()
                .document_openers
                .iter()
                .any(|opener| opener.eq_ignore_ascii_case(want))
                .then(|| {
                    reach::surfaces()
                        .iter()
                        .find(|surface| surface.opener == reach::DYNAMIC_DOCUMENT)
                })
                .flatten()
        })
}

/// Whether an opener names one document among several instances of a surface.
///
/// A surface marker distinguishes Settings from Home, but cannot distinguish
/// one project from another. The application's profile owns the stable fixture
/// names, so it also owns this classification.
fn is_named_document_opener(want: &str) -> bool {
    named_document_opener_for(reach::profile(), want)
}

fn named_document_opener_for(profile: &crate::app::AppProfile, want: &str) -> bool {
    profile
        .document_openers
        .iter()
        .any(|opener| opener.eq_ignore_ascii_case(want))
}

fn arrived_without_navigation(
    nodes: &[SemanticNode],
    destination: Option<&reach::Surface>,
    want_here: &str,
    named_document: bool,
    active_document_matches: bool,
) -> bool {
    if named_document {
        active_document_matches
            && destination.is_some_and(|surface| reach::on_surface(nodes, surface))
    } else {
        destination.map_or_else(
            || painted_named(nodes, want_here),
            |surface| reach::on_surface(nodes, surface),
        )
    }
}

/// Whether the exact named document is the live selected tab.
///
/// Every project surface paints the same generic marker, so only the tab can
/// distinguish which one owns it. Agency exposes `aria-current="page"`; Blitz
/// maps that live state to `selected` rather than asking the runner to remember
/// which arbitrary action may have changed documents.
fn named_document_is_active(nodes: &[SemanticNode], want: &str) -> bool {
    named_document_is_active_with_permanent(nodes, want, &reach::profile().permanent_surfaces)
}

fn named_document_is_active_with_permanent(
    nodes: &[SemanticNode],
    want: &str,
    permanent_surfaces: &[String],
) -> bool {
    let tab_name = format!("{want}{want}");
    let exact_document_selected = nodes.iter().any(|node| {
        node.role == "button"
            && node.name.eq_ignore_ascii_case(&tab_name)
            && node.selected
            && node.visible
            && painted_bounds(node).is_some()
    });
    let permanent_surface_selected = nodes.iter().any(|node| {
        node.role == "button"
            && node.selected
            && node.visible
            && painted_bounds(node).is_some()
            && permanent_surfaces.iter().any(|surface| {
                node.name.eq_ignore_ascii_case(surface)
                    || node
                        .name
                        .eq_ignore_ascii_case(&format!("{surface}{surface}"))
            })
    });
    exact_document_selected && !permanent_surface_selected
}

/// Activate a surface opener without confusing a document tab for its Close
/// button.
///
/// A tab's accessible name is the document label doubled. Prefer that exact
/// role-qualified selector, then fall back to the ordinary opener so a profile
/// with no open tab can still use its Home row.
async fn click_opener_quiet(client: &mut Client, want: &str, named_document: bool) -> Result<u64> {
    if named_document {
        let tab = format!("button:{want}{want}");
        if let Ok(node_id) = click_named_quiet(client, &tab).await {
            return Ok(node_id);
        }
    }
    click_named_quiet(client, want).await
}

async fn click_by_id(client: &mut Client, node_id: u64) -> Result<()> {
    let answer = client
        .agent(&AgentControlRequest::Act(AgentAction::Click { node_id }))
        .await?;
    if let DebugResponse::Error(error) = answer.response {
        bail!("{} ({})", error.message, error.code);
    }
    Ok(())
}

/// A finished run, as a machine reads it.
///
/// Serialized rather than printed field by field: `toon-format` is generic over
/// `serde::Serialize`, so the shape below *is* the output format, and there is
/// no second hand-written description of it to drift.
#[derive(serde::Serialize)]
struct Report {
    passed: usize,
    failed: usize,
    groups: Vec<GroupRow>,
    checks: Vec<CheckRow>,
}

struct CheckResult<'a> {
    check: &'a qa::Check,
    outcome: std::result::Result<(), String>,
    /// End-to-end check time, including setup and teardown around the measured
    /// interaction. The verdict's stricter action budget is enforced
    /// separately; this is artifact cost.
    duration_ms: u64,
    /// Semantic snapshots taken while waiting for the declared outcome.
    settle_iterations: u32,
    /// Explicitly repeated actions after a failed first attempt.
    retries: u32,
}

#[derive(serde::Serialize)]
struct GroupRow {
    name: String,
    passed: usize,
    total: usize,
}

/// One check's outcome. Uniform, so TOON emits it as a table: the field names
/// are declared once and each check costs a single line.
#[derive(serde::Serialize)]
struct CheckRow {
    verdict: &'static str,
    group: String,
    id: String,
    duration_ms: u64,
    settle_iterations: u32,
    retries: u32,
    error: String,
    what: String,
}

impl From<&CheckResult<'_>> for CheckRow {
    fn from(result: &CheckResult<'_>) -> Self {
        Self {
            verdict: if result.outcome.is_ok() {
                "pass"
            } else {
                "fail"
            },
            group: result.check.group.clone(),
            id: result.check.id.clone(),
            duration_ms: result.duration_ms,
            settle_iterations: result.settle_iterations,
            retries: result.retries,
            error: result.outcome.clone().err().unwrap_or_default(),
            what: result.check.what.clone(),
        }
    }
}

/// One control, as `find` reports it.
#[derive(serde::Serialize)]
struct FoundRow {
    id: u64,
    role: String,
    name: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// `visible`, `hidden`, `disabled`, `0x0`, `offscreen`, comma-joined. The
    /// question behind most lookups is "why can nobody press this", and the
    /// answer is a combination of those rather than any one of them.
    state: String,
}

/// Every control matching a role, a name pattern and a state.
///
/// Replaces the `layout | awk` pipeline that every non-trivial question used to
/// need. The filters are the ones that were actually reassembled by hand, over
/// and over: which buttons are off screen, what is painting at 0x0, what does
/// this surface contain.
#[expect(
    clippy::too_many_arguments,
    reason = "each one is a filter the caller asks for by name"
)]
async fn find(
    client: &mut Client,
    pattern: &str,
    roles: &[String],
    visible: bool,
    hidden: bool,
    painted: bool,
    offscreen_only: bool,
    disabled: bool,
    count_only: bool,
    limit: Option<usize>,
) -> Result<()> {
    let (snapshot, elapsed) = inspect(client).await?;
    let viewport = viewport_of(&snapshot);

    let rows: Vec<FoundRow> = snapshot
        .nodes
        .iter()
        .filter(|node| roles.is_empty() || roles.iter().any(|role| role == &node.role))
        // `@slot` addresses the part the library named rather than the text it
        // carries, so a trigger and the thing it opens are distinguishable even
        // though they share an accessible name.
        .filter(|node| match pattern.strip_prefix('@') {
            Some(slot) => node.slot.as_deref().is_some_and(|have| have == slot),
            None => name_matches(&node.name, pattern),
        })
        .filter(|node| !visible || node.visible)
        .filter(|node| !hidden || !node.visible)
        .filter(|node| !disabled || !node.enabled)
        .filter(|node| {
            !painted
                || node
                    .bounds
                    .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
        })
        .filter(|node| {
            !offscreen_only
                || node
                    .bounds
                    .is_some_and(|bounds| offscreen(bounds, viewport))
        })
        .map(|node| {
            let bounds = node.bounds.unwrap_or([0.0; 4]);
            let mut state: Vec<&str> = Vec::new();
            state.push(if node.visible { "visible" } else { "hidden" });
            if !node.enabled {
                state.push("disabled");
            }
            if node.selected {
                state.push("selected");
            }
            if bounds[2] <= 0.0 || bounds[3] <= 0.0 {
                state.push("0x0");
            } else if offscreen(bounds, viewport) {
                state.push("offscreen");
            }
            // Rounded to a tenth. Layout arrives as f64 and prints as
            // `20.8799991607666`, which is six times the tokens of `20.9` and
            // tells the reader nothing a control's geometry depends on.
            let round = |value: f64| (value * 10.0).round() / 10.0;
            FoundRow {
                id: node.id,
                role: node.role.clone(),
                name: node.name.clone(),
                x: round(bounds[0]),
                y: round(bounds[1]),
                w: round(bounds[2]),
                h: round(bounds[3]),
                state: state.join(","),
            }
        })
        .collect();

    let matched = rows.len();
    let shown: Vec<FoundRow> = match limit {
        Some(limit) => rows.into_iter().take(limit).collect(),
        None => rows,
    };

    #[derive(serde::Serialize)]
    struct Found {
        matched: usize,
        of: usize,
        inspect_ms: f64,
        controls: Vec<FoundRow>,
    }

    let report = Found {
        matched,
        of: snapshot.nodes.len(),
        inspect_ms: (elapsed * 10.0).round() / 10.0,
        controls: if count_only { Vec::new() } else { shown },
    };
    println!(
        "{}",
        toon_format::encode_default(&report).map_err(|error| eyre!(error.to_string()))?
    );
    Ok(())
}

/// Glob-style name matching.
///
/// `chat*` for a prefix, `*close*` for anywhere, `*` for everything. A pattern
/// with no `*` is a substring, because that is how a control is remembered:
/// asking for `Rename` should find `Rename project` without ceremony.
async fn locate_button(client: &mut Client, want: &str) -> Result<(u64, [f64; 4])> {
    locate_control(client, want, &["button"]).await
}

/// Double click the first visible match, for reaching a surface.
///
/// A document row may fold on a single click and open on a double; two separate
/// `press` calls are not a double click: each round-trips through the
/// inspector, so they land hundreds of milliseconds apart and the row folds
/// there and back. Navigation that used single presses appeared to work only
/// when the surface happened to be open already, which is why checks failed
/// with "no visible, enabled, sized button" for controls one gesture away.
async fn open_named(client: &mut Client, want: &str) -> Result<()> {
    let (id, _) = locate_button(client, want).await?;
    if cli::trace() {
        println!("        opening {want:?} (id {id})");
    }
    client
        .agent(&AgentControlRequest::Act(AgentAction::DoubleClick {
            node_id: id,
        }))
        .await?;
    Ok(())
}

/// Send a coordinate-pointer press to the first visible match, when requested.
///
/// This is an explicit generic hit-testing diagnostic. Application suites use
/// the node-addressed default and opt into this only when pointer hit-testing
/// itself is the behavior under test.
async fn press_named(client: &mut Client, want: &str) -> Result<()> {
    /*
     * Any role, on screen - the same resolution `click` uses.
     *
     * This was buttons only, on the reasoning that a control and the thing it
     * opens can share an accessible name, so a name-only match might select
     * the output instead of the activator. That is a real hazard for
     * `open_named`, which is looking for an activator. It is the wrong rule
     * here: the point of this diagnostic is to compare the pointer path
     * against the node-addressed one on the *same* control, and a role filter
     * that `click` does not apply makes the two incomparable.
     *
     * Concretely, a menu item has role `menuitem`, so pressing one always
     * failed with "no visible, enabled, sized node" while clicking the
     * identical node worked. That read as a dismissal-on-pointerdown defect in
     * the overlay - it was recorded as a known gap in AgencyZero's pill menu
     * checks - and it was this filter the whole time. Selecting from a menu by
     * pointer, the gesture a person actually makes, had no coverage at all.
     */
    // `role:name`, as `painted_named` and the check subjects already accept.
    // Without it a bare name falls back to substring matching over every node:
    // pressing "low" in this application matched a Keychain warning containing
    // "allow" and moved the pointer to the top of the window. A check that
    // asserts a menu selection has to be able to say which node it means.
    let (role, name) = want.split_once(':').unwrap_or(("", want));
    let roles: &[&str] = if role.is_empty() { &[] } else { &[role] };
    let (id, b) = locate_control(client, name, roles).await?;
    let (x, y) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
    if cli::trace() {
        println!("        pressing {want:?} (id {id}) at {x:.0},{y:.0}");
    }
    for phase in [PointerPhase::Move, PointerPhase::Down, PointerPhase::Up] {
        let answer = client
            .agent(&AgentControlRequest::Act(AgentAction::Input(
                InputCommand::Pointer {
                    phase,
                    x,
                    y,
                    button: 0,
                    modifiers: Modifiers::default(),
                },
            )))
            .await?;
        if let DebugResponse::Error(error) = answer.response {
            bail!("{} ({})", error.message, error.code);
        }
    }
    Ok(())
}

/// `click_named` without the metrics report, for the QA runner's inner loop.
async fn click_named_quiet(client: &mut Client, want: &str) -> Result<u64> {
    // Resolve through the same painted, on-screen semantic path as navigation.
    // `visible` alone is insufficient: retained subtrees can expose an old
    // semantic node at 0x0, and activating that id reports a working control
    // dead while its current painted replacement remains untouched.
    let (target_id, _) = locate_control(client, want, &[]).await?;
    if cli::trace() {
        println!("        activating {want:?} (id {target_id})");
    }
    client
        .agent(&AgentControlRequest::Act(AgentAction::Click {
            node_id: target_id,
        }))
        .await?;
    Ok(target_id)
}

/// Drive one declared QA action and normalize its optional semantic node id.
///
/// Coordinate presses are an explicit generic harness diagnostic, so they do
/// not produce a semantic action id. The normal application path is a node-id
/// click and returns the exact node used for outcome attribution.
async fn drive_check_action(
    client: &mut Client,
    want: &str,
    coordinate_press: bool,
) -> std::result::Result<Option<u64>, String> {
    if coordinate_press {
        press_named(client, want)
            .await
            .map(|()| None)
            .map_err(|error| format!("could not press {want:?}: {error}"))
    } else {
        click_named_quiet(client, want)
            .await
            .map(Some)
            .map_err(|error| format!("could not click {want:?}: {error}"))
    }
}

/// What a captured frame contains, in the terms a person would use.
///
/// "Did it draw" is not answerable from a pixel count alone: an icon filled
/// black on a near-black surface is fully opaque and completely invisible, and
/// that exact failure shipped. What separates ink from background is contrast,
/// so that is what this measures.
struct Ink {
    /// Pixels that differ enough from the most common colour to be seen.
    visible: usize,
    total: usize,
    /// The colour occupying the most pixels, taken as the background.
    background: (u8, u8, u8),
}

impl Ink {
    fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.visible as f64 / self.total as f64
        }
    }
}

/// Measure the visible ink in a captured frame.
///
/// The background is discovered rather than assumed, so this works against any
/// app's surface colour without being told what it is. Contrast is relative
/// luminance, because that is what decides whether a person can see the mark:
/// raw channel distance calls black-on-near-black "different" while being the
/// one case worth catching.
fn measure_ink(image: &CapturedImage) -> Result<Ink> {
    use base64::Engine as _;

    let rgba = base64::engine::general_purpose::STANDARD
        .decode(&image.rgba_base64)
        .map_err(|error| eyre::eyre!("the capture was not valid base64: {error}"))?;
    let expected = (image.width as usize) * (image.height as usize) * 4;
    if rgba.len() != expected {
        bail!(
            "capture is {} bytes, expected {expected} for {}x{}",
            rgba.len(),
            image.width,
            image.height
        );
    }

    let mut histogram: HashMap<(u8, u8, u8), usize> = HashMap::new();
    // `as_chunks::<4>()` rather than `chunks_exact(4)`: the width is a constant,
    // so this hands back `[u8; 4]` and the indexing below is bounds-checked at
    // compile time instead of per pixel.
    for pixel in rgba.as_chunks::<4>().0 {
        *histogram.entry((pixel[0], pixel[1], pixel[2])).or_default() += 1;
    }
    let background = histogram
        .iter()
        .max_by_key(|(_, count)| **count)
        .map(|(colour, _)| *colour)
        .unwrap_or((0, 0, 0));

    let luminance = |(r, g, b): (u8, u8, u8)| {
        0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
    };
    let background_luminance = luminance(background);

    let visible = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| {
            // Transparent pixels are not ink whatever their colour.
            if pixel[3] < 32 {
                return false;
            }
            (luminance((pixel[0], pixel[1], pixel[2])) - background_luminance).abs() > 24.0
        })
        .count();

    Ok(Ink {
        visible,
        total: (image.width as usize) * (image.height as usize),
        background,
    })
}

/// Ask the app for a frame: the whole window, or one named node.
fn write_capture_ppm(image: &CapturedImage, output: &std::path::Path) -> Result<()> {
    use base64::Engine as _;
    use std::io::Write as _;

    let rgba = base64::engine::general_purpose::STANDARD
        .decode(&image.rgba_base64)
        .map_err(|error| eyre!("the capture was not valid base64: {error}"))?;
    let expected = image.width as usize * image.height as usize * 4;
    if rgba.len() != expected {
        bail!(
            "capture is {} bytes, expected {expected} for {}x{}",
            rgba.len(),
            image.width,
            image.height
        );
    }

    let mut file = std::fs::File::create(output)
        .wrap_err_with(|| format!("could not create {}", output.display()))?;
    write!(file, "P6\n{} {}\n255\n", image.width, image.height)?;
    for pixel in rgba.as_chunks::<4>().0 {
        file.write_all(&pixel[..3])?;
    }
    println!("saved {}", output.display());
    Ok(())
}

async fn capture(
    client: &mut Client,
    want: &str,
    scale: f32,
    output: Option<&std::path::Path>,
) -> Result<()> {
    let node_id = if want.is_empty() {
        None
    } else {
        let (snapshot, _) = inspect(client).await?;
        let node = snapshot
            .nodes
            .iter()
            .filter(|node| selector_matches_node(node, want) && node.visible)
            .filter_map(|node| node.bounds.map(|bounds| (node, bounds)))
            .filter(|(_, bounds)| bounds[2] > 0.0 && bounds[3] > 0.0)
            .max_by(|a, b| {
                (a.1[2] * a.1[3])
                    .partial_cmp(&(b.1[2] * b.1[3]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(node, _)| node);
        let Some(node) = node else {
            bail!("no visible node with a box matching {want:?}");
        };
        println!(
            "capturing {} role={} name={}",
            node.id,
            node.role,
            report::py_repr(&node.name.chars().take(50).collect::<String>())
        );
        Some(node.id)
    };

    let answer = client
        .diagnostics(&DiagnosticsRequest::Capture(CaptureRequest {
            node_id,
            scale,
        }))
        .await?;
    let image = match answer.response {
        DebugResponse::Captured(image) => image,
        DebugResponse::Error(error) => bail!("capture refused: {} ({})", error.message, error.code),
        other => bail!("asked for a capture, got {other:?}"),
    };

    let ink = measure_ink(&image)?;
    let fingerprint = {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};
        let mut hasher = DefaultHasher::new();
        image.width.hash(&mut hasher);
        image.height.hash(&mut hasher);
        image.rgba_base64.hash(&mut hasher);
        hasher.finish()
    };
    println!(
        "{}x{} at {scale}x, background #{:02x}{:02x}{:02x}",
        image.width, image.height, ink.background.0, ink.background.1, ink.background.2
    );
    println!("rgba fingerprint: {fingerprint:016x}");
    println!(
        "visible ink: {} of {} pixels ({:.2}%)",
        ink.visible,
        ink.total,
        ink.fraction() * 100.0
    );
    if ink.visible == 0 {
        println!("nothing was drawn: every pixel is the background colour");
    }
    if let Some(output) = output {
        write_capture_ppm(&image, output)?;
    }
    Ok(())
}

/// Audit every button in the running application.
///
/// Reads the renderer's own paint output rather than capturing each control.
/// The paint snapshot reports, per node, the style the renderer resolved and
/// the box it drew into, which answers the same question a capture does and
/// answers it for the whole window in one call.
///
/// Capturing per button was tried first and was worse in both directions. It
/// took 19 seconds against well under one, and it reported false faults: a crop
/// taken from a full-document paint cannot see content a clipping scroller drew
/// into its own layer, so three `Edit` buttons were flagged that the paint
/// output proved were drawn, with colours identical to their working siblings.
async fn run_audit(client: &mut Client, family: Option<&str>) -> Result<usize> {
    use audit::{Audited, Verdict};

    let (snapshot, _) = inspect(client).await?;

    /*
     * The viewport, read from the tree rather than assumed.
     *
     * A control below the fold is clipped, not broken. Lower section headers
     * can sit outside a short window and draw perfectly once
     * scrolled to, so a hardcoded bound reported them as faults. Reading the
     * `main` box also keeps this right when the window is resized.
     */
    let viewport = snapshot
        .nodes
        .iter()
        .filter(|node| node.role == "main")
        .filter_map(|node| node.bounds)
        .map(|b| (b[1], b[1] + b[3]))
        .max_by(|a, b| {
            (a.1 - a.0)
                .partial_cmp(&(b.1 - b.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0.0, f64::MAX));

    let painted = painted_nodes(client).await?;

    let mut rows: Vec<Audited> = Vec::new();
    for node in audit::buttons(&snapshot.nodes) {
        let family_name = audit::family_of(&node.name);
        if family.is_some_and(|want| family_name != want) {
            continue;
        }
        let (width, height) = node.bounds.map(|b| (b[2], b[3])).unwrap_or((0.0, 0.0));

        let verdict = if !node.visible {
            Verdict::Hidden
        } else if width <= 0.0 || height <= 0.0 {
            Verdict::NoBox
        } else if node
            .bounds
            .is_some_and(|b| b[1] + b[3] < viewport.0 || b[0] + b[2] < 0.0 || b[1] > viewport.1)
        {
            Verdict::Offscreen
        } else if painted.contains(&node.id) {
            Verdict::Drawn
        } else {
            Verdict::Blank
        };

        rows.push(Audited {
            name: node.name.clone(),
            family: family_name,
            width,
            height,
            verdict,
        });
    }

    if rows.is_empty() {
        bail!("no buttons matched {family:?}");
    }
    println!("auditing {} buttons in the running app\n", rows.len());

    let faults: Vec<&Audited> = rows.iter().filter(|row| row.verdict.is_fault()).collect();
    if faults.is_empty() {
        println!("no faults: every visible button was painted");
    } else {
        println!("{} button(s) nobody can see:\n", faults.len());
        for row in &faults {
            println!(
                "  {:<8} {:<52} {:.0}x{:.0}",
                row.verdict.label(),
                row.name.chars().take(52).collect::<String>(),
                row.width,
                row.height
            );
        }
        println!();
    }

    let mut families: Vec<_> = audit::by_family(&rows).into_iter().collect();
    families.sort_by_key(|(name, _)| *name);
    for (name, (passed, total)) in families {
        let mark = if passed == total { " " } else { "!" };
        println!("{mark} {name:<12} {passed}/{total}");
    }
    println!("\n{} audited, {} faults", rows.len(), faults.len());
    Ok(faults.len())
}

/// The nodes the renderer resolved and drew, from one paint snapshot.
///
/// A button absent from this was not painted, which is what "a person cannot
/// see it" means. Asked once for the whole window rather than per control.
async fn painted_nodes(client: &mut Client) -> Result<HashSet<u64>> {
    let answer = client
        .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
            include_dom: false,
            include_layout: false,
            include_computed_style: true,
            node_ids: Vec::new(),
        }))
        .await?;
    let DebugResponse::Snapshot(snapshot) = answer.response else {
        bail!("asked for a paint snapshot, got {:?}", answer.response);
    };
    Ok(snapshot
        .computed_style
        .as_ref()
        .and_then(|value| value.as_array())
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("nodeId")?.as_u64())
                .collect()
        })
        .unwrap_or_default())
}

/// Click every button and report the ones that did not act.
///
/// The tree is re-read after each click rather than once at the end, because a
/// click changes what is on screen and a stale node id is a click on nothing.
/// Buttons are addressed by name for the same reason: ids do not survive the
/// re-render that a working button causes.
async fn run_sweep(client: &mut Client, family: Option<&str>) -> Result<usize> {
    let (snapshot, _) = inspect(client).await?;
    let viewport = snapshot
        .nodes
        .iter()
        .filter(|node| node.role == "main")
        .filter_map(|node| node.bounds)
        .map(|b| (b[1], b[1] + b[3]))
        .max_by(|a, b| {
            (a.1 - a.0)
                .partial_cmp(&(b.1 - b.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0.0, f64::MAX));
    let planned = sweep::cases(
        &snapshot.nodes,
        family,
        audit::family_of,
        reach::is_inert_control,
    );
    if planned.is_empty() {
        bail!("no clickable buttons matched {family:?}");
    }
    println!("clicking {} buttons\n", planned.len());

    let mut outcomes: Vec<sweep::Outcome> = Vec::new();
    for case in planned {
        let (before, _) = inspect(client).await?;

        /*
         * Re-resolved by id, freshly, every time.
         *
         * Clicking by name cannot work here: fifteen task-log rows are all
         * called "Show the whole command", so after the first click the lookup
         * is ambiguous and every later one reports a failure that is the
         * harness's, not the application's. That produced 24 false failures in
         * the first run and buried whatever was real.
         *
         * The id is re-checked against the current tree rather than trusted
         * from the plan, because a working button re-renders its own row and a
         * stale id is a click on nothing.
         */
        let Some(node) = before.nodes.iter().find(|node| node.id == case.id) else {
            // Gone since the plan was made, which a working button often
            // causes: closing one tab removes the close buttons of its
            // neighbours. Not a failure.
            continue;
        };
        if !node.visible || !node.enabled {
            continue;
        }
        /*
         * Off the viewport is not clickable, and clicking it anyway tests the
         * harness rather than the application. A transcript keeps hundreds of
         * controls at negative coordinates and the panel's lower sections sit
         * below the fold; both reported as failures until they were skipped.
         */
        if node
            .bounds
            .is_some_and(|b| b[1] + b[3] < viewport.0 || b[0] + b[2] < 0.0 || b[1] > viewport.1)
        {
            continue;
        }

        if let Err(error) = click_by_id(client, case.id).await {
            outcomes.push(sweep::Outcome {
                case,
                failure: Some(format!("could not be clicked: {error}")),
            });
            continue;
        }
        // Long enough for a synchronous handler and its re-render. A backend
        // round trip is slower, and a button that only fails under that delay
        // is reported rather than waited for: a person sees the same thing.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let (after, _) = inspect(client).await?;
        let failure = sweep::judge(&case, &before.nodes, &after.nodes);
        outcomes.push(sweep::Outcome { case, failure });
    }

    let failures: Vec<&sweep::Outcome> = outcomes.iter().filter(|o| o.failure.is_some()).collect();
    if failures.is_empty() {
        println!("every button acted");
    } else {
        println!("{} button(s) did not act:\n", failures.len());
        for outcome in &failures {
            println!(
                "  {:<48} {}",
                outcome.case.name.chars().take(48).collect::<String>(),
                outcome.failure.as_deref().unwrap_or("")
            );
        }
        println!();
    }

    let mut by_family: HashMap<&'static str, (usize, usize)> = HashMap::new();
    for outcome in &outcomes {
        let entry = by_family.entry(outcome.case.family).or_insert((0, 0));
        entry.1 += 1;
        if outcome.failure.is_none() {
            entry.0 += 1;
        }
    }
    let mut families: Vec<_> = by_family.into_iter().collect();
    families.sort_by_key(|(name, _)| *name);
    for (name, (passed, total)) in families {
        let mark = if passed == total { " " } else { "!" };
        println!("{mark} {name:<12} {passed}/{total}");
    }
    println!(
        "\n{} clicked, {} did not act",
        outcomes.len(),
        failures.len()
    );
    Ok(failures.len())
}

/// Open every collapsed section on the current surface.
///
/// Repeated until nothing more opens, because expanding one section reveals
/// disclosure controls inside it: a collection may hold a row per record, each with
/// its own. A single pass stops one level short of the controls that matter.
async fn expand_everything(client: &mut Client, surface: &reach::Surface) -> Result<usize> {
    let mut opened = 0;
    // Bounded: a disclosure that reports "Expand" after being expanded would
    // otherwise spin here forever, and that is a bug worth finishing the run to
    // report rather than hanging on.
    for _ in 0..6 {
        let (tree, _) = inspect(client).await?;
        // This surface's disclosures only: a retained pane keeps its own, and
        // pressing one of those navigates out of the surface being planned.
        let mine: std::collections::HashSet<u64> = reach::on_surface_subtree(&tree.nodes, surface)
            .into_iter()
            .collect();
        let mut todo: Vec<String> = reach::expanders(&tree.nodes)
            .into_iter()
            .filter(|(id, _)| mine.contains(id))
            .map(|(_, name)| name)
            .collect();
        // Retained panes can expose dozens of identical disclosure names. One
        // semantic activation on the active surface is the user action; driving
        // every retained node toggles unrelated panes and can leave the current
        // one exactly where it started.
        todo.sort();
        todo.dedup();
        if todo.is_empty() {
            break;
        }
        for name in todo {
            if click_named_quiet(client, &name).await.is_ok() {
                opened += 1;
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        }
    }
    Ok(opened)
}

/// Reveal lazily mounted content on the active surface without guessing a
/// coordinate or knowing an application's section names.
///
/// Some long settings pages render their section shells first and mount each
/// body only when scrolling approaches it. If a check names a control in a
/// deferred body, that control cannot be addressed yet. The deepest rendered
/// node is the semantic equivalent of dragging the page toward its end; after
/// each node-ID reveal the tree is inspected again and the requested control
/// wins as soon as it has a real box.
async fn reveal_deferred_content(
    client: &mut Client,
    surface: &reach::Surface,
    want: &str,
) -> Result<usize> {
    let materialized = materialize_deferred_content(client, surface, want).await?
        + materialize_paginated_content(client, surface).await?;
    if materialized > 0 {
        let (snapshot, _) = inspect(client).await?;
        if painted_named(&snapshot.nodes, want) {
            return Ok(materialized);
        }
    }
    for step in 0..8 {
        let (snapshot, _) = inspect(client).await?;
        if painted_named(&snapshot.nodes, want) {
            return Ok(step);
        }
        let scope: HashSet<u64> = reach::on_surface_subtree(&snapshot.nodes, surface)
            .into_iter()
            .collect();
        let Some(target) = snapshot
            .nodes
            .iter()
            .filter(|node| scope.contains(&node.id))
            .filter_map(|node| {
                node.bounds
                    .filter(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
                    .map(|bounds| (node.id, bounds[1] + bounds[3]))
            })
            .max_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        else {
            return Ok(step);
        };
        client
            .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id: target.0,
            }))
            .await?;
        tokio::time::sleep(Duration::from_millis(75)).await;
    }
    Ok(8)
}

/// Ask an application-declared search field to mount the control this check
/// needs and keep that filtered result alive for the action which follows.
async fn materialize_deferred_content(
    client: &mut Client,
    surface: &reach::Surface,
    want: &str,
) -> Result<usize> {
    let Some(field) = surface.reveal_with.as_deref() else {
        return Ok(0);
    };
    let query = want.split_once(':').map_or(want, |(_, name)| name);
    type_text(client, field, query).await?;
    let _ = wait_for_arrival(client, None, want).await?;
    Ok(1)
}

/// Traverse a virtualized surface until scrolling stops mounting controls.
///
/// This drives no application action: it only reveals the deepest existing
/// semantic node. Settings/catalog screens commonly keep section shells in the
/// tree and mount their interactive bodies as the reader approaches them, so
/// one snapshot from the opening viewport is not an inventory of the surface.
async fn materialize_scrolled_content(
    client: &mut Client,
    surface: &reach::Surface,
) -> Result<usize> {
    let mut previous_count = None;
    let mut stable_passes = 0usize;
    let mut scrolls = 0usize;

    // Start from a known edge. If a prior focused check left the surface at
    // its current bottom, revealing that same bottom again emits no scroll
    // event and a scroll-driven virtualizer never gets a chance to admit its
    // remaining sections.
    let (initial, _) = inspect(client).await?;
    let initial_scope: HashSet<u64> = reach::on_surface_subtree(&initial.nodes, surface)
        .into_iter()
        .collect();
    if let Some(top) = initial
        .nodes
        .iter()
        .filter(|node| initial_scope.contains(&node.id))
        .filter(|node| node.role == "heading" || reach::interactive(node))
        .filter_map(|node| {
            node.bounds
                .filter(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
                .map(|bounds| (node.id, bounds[1]))
        })
        .min_by(|left, right| left.1.total_cmp(&right.1))
    {
        client
            .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id: top.0,
            }))
            .await?;
        scrolls += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for _ in 0..16 {
        let (tree, _) = inspect(client).await?;
        let scope: HashSet<u64> = reach::on_surface_subtree(&tree.nodes, surface)
            .into_iter()
            .collect();
        let interactive = tree
            .nodes
            .iter()
            .filter(|node| scope.contains(&node.id) && reach::interactive(node))
            .count();
        if cli::trace() {
            println!(
                "        inventory materialize surface={} interactive={} stable={}",
                surface.name, interactive, stable_passes
            );
        }
        if previous_count == Some(interactive) {
            stable_passes += 1;
            if stable_passes >= 2 {
                return Ok(scrolls);
            }
        } else {
            previous_count = Some(interactive);
            stable_passes = 0;
        }

        let Some(target) = tree
            .nodes
            .iter()
            .filter(|node| scope.contains(&node.id))
            .filter_map(|node| {
                node.bounds
                    .filter(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
                    .map(|bounds| (node.id, bounds[1]))
            })
            .max_by(|left, right| left.1.total_cmp(&right.1))
        else {
            return Ok(scrolls);
        };
        if cli::trace() {
            println!(
                "        inventory reveal deepest node={} y={:.1}",
                target.0, target.1
            );
        }
        client
            .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id: target.0,
            }))
            .await?;
        scrolls += 1;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(scrolls)
}

/// Reveal every page the application explicitly identifies as pagination.
///
/// This is semantic node-id activation. Pager labels are application data, so
/// the profile supplies fragments such as ` more records`; a generic `Show`
/// heuristic would also activate unrelated disclosure controls.
fn is_pagination_control(node: &SemanticNode, scope: &HashSet<u64>, patterns: &[String]) -> bool {
    scope.contains(&node.id)
        && node.role == "button"
        && node.enabled
        && node.visible
        && node
            .bounds
            .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
        && patterns
            .iter()
            .any(|pattern| name_matches(&node.name, pattern))
}

type SemanticShape = (String, String, Option<String>);

fn semantic_shapes(nodes: &[SemanticNode], scope: &HashSet<u64>) -> HashSet<SemanticShape> {
    nodes
        .iter()
        .filter(|node| scope.contains(&node.id))
        .map(|node| (node.role.clone(), node.name.clone(), node.value.clone()))
        .collect()
}

fn pagination_advanced(
    previous_shapes: &HashSet<SemanticShape>,
    previous_name: &str,
    current_shapes: &HashSet<SemanticShape>,
    current: &SemanticNode,
) -> bool {
    current.name != previous_name || current_shapes.len() > previous_shapes.len()
}

async fn materialize_paginated_content(
    client: &mut Client,
    surface: &reach::Surface,
) -> Result<usize> {
    let patterns = &reach::profile().pagination_controls;
    if patterns.is_empty() {
        return Ok(0);
    }
    let mut revealed = 0;
    let mut retired_pagers = HashSet::new();
    for _ in 0..32 {
        let (tree, _) = inspect(client).await?;
        let scope: HashSet<u64> = reach::on_surface_subtree(&tree.nodes, surface)
            .into_iter()
            .collect();
        let target = tree.nodes.iter().find(|node| {
            !retired_pagers.contains(&node.id) && is_pagination_control(node, &scope, patterns)
        });
        let Some(target) = target else {
            return Ok(revealed);
        };
        let node_id = target.id;
        let mut pager_name = target.name.clone();
        let mut pager_shapes = semantic_shapes(&tree.nodes, &scope);
        client
            .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id,
            }))
            .await?;

        // A pager keeps its semantic identity while only its remaining count
        // changes. The protocol deliberately treats activation as a delivered
        // input even after the DOM node has gone away, so an error is not a
        // disappearance signal. Read the semantic tree after each activation
        // and stop as soon as this exact id is no longer an enabled, painted
        // pager. Without that check a one-page Home button is clicked 128 times
        // as a no-op and turns a two-second inventory into a multi-minute run.
        let mut removed = false;
        for _ in 0..128 {
            if click_by_id(client, node_id).await.is_err() {
                removed = true;
                break;
            }
            revealed += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
            let (after, _) = inspect(client).await?;
            let after_scope: HashSet<u64> = reach::on_surface_subtree(&after.nodes, surface)
                .into_iter()
                .collect();
            let after_shapes = semantic_shapes(&after.nodes, &after_scope);
            let still_present = after.nodes.iter().find(|node| {
                node.id == node_id && is_pagination_control(node, &after_scope, patterns)
            });
            match still_present {
                Some(current)
                    if pagination_advanced(&pager_shapes, &pager_name, &after_shapes, current) =>
                {
                    pager_name.clone_from(&current.name);
                    pager_shapes = after_shapes;
                }
                _ => {
                    // A reveal-more control must expose semantic progress.
                    // Blitz can retain the removed DOM node for one or more
                    // snapshots, including its old geometry and visible bit;
                    // repeatedly activating that unchanged identity is a no-op
                    // loop. Retire it and scan for the next real pager.
                    retired_pagers.insert(node_id);
                    removed = true;
                    break;
                }
            }
        }
        if !removed {
            bail!("pagination node {node_id} did not disappear after 128 activations");
        }
    }
    bail!("pagination controls did not terminate after 32 semantic identities")
}

/// Hover every row on the surface, so hover-revealed controls enter the tree.
///
/// The row controls users asked about - rename, delete, pin - do not exist
/// until `pointerenter`. Hovering is what puts them in the tree at all, so this
/// runs before the plan is made rather than per-click.
async fn hover_all_rows(client: &mut Client) -> Result<usize> {
    let (tree, _) = inspect(client).await?;
    // The window band, taken from the largest `main`, so rows scrolled far off
    // the top of a transcript are not hovered at negative coordinates.
    let window = tree
        .nodes
        .iter()
        .filter(|node| node.role == "main")
        .filter_map(|node| node.bounds)
        .map(|b| (b[1], b[1] + b[3]))
        .max_by(|a, b| {
            (a.1 - a.0)
                .partial_cmp(&(b.1 - b.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or((0.0, 4000.0));
    let mut revealed = 0;
    for node_id in reach::hover_row_ids(&tree.nodes, "listitem", window) {
        if client
            .agent(&AgentControlRequest::Act(AgentAction::Hover { node_id }))
            .await
            .is_ok()
        {
            // The protocol reply is the synchronization point. Sleeping after
            // every row adds no evidence and turns a large paginated table into
            // minutes of idle time.
            revealed += 1;
        }
    }
    Ok(revealed)
}

/// Inventory semantic reachability across every configured application
/// surface without pressing the controls being counted.
///
/// `cover` answers a different and much more expensive question: it activates
/// every eligible button and performs multiple full-tree reads per button.
/// An agent asking which components are unreachable should not pay that cost or
/// mutate the profile merely to obtain counts. Navigation, disclosure expansion
/// and hover are still necessary preconditions, and all three are node-id
/// actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InventoryClass {
    MissingId,
    UnstableId,
    DuplicateId,
    Manual,
    Isolated,
    Anonymous,
    Unreachable,
    Disabled,
    Reachable,
}

fn duplicate_dom_ids(nodes: &[SemanticNode]) -> std::collections::HashSet<String> {
    let mut counts = std::collections::HashMap::<&str, usize>::new();
    for dom_id in nodes
        .iter()
        .filter_map(|node| node.dom_id.as_deref())
        .filter(|dom_id| !dom_id.trim().is_empty())
    {
        *counts.entry(dom_id).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(dom_id, _)| dom_id.to_owned())
        .collect()
}

fn generated_dom_id(dom_id: &str) -> bool {
    let mut pieces = dom_id.split('-');
    match (pieces.next(), pieces.next()) {
        (Some("cl"), Some(instance)) => instance.parse::<u64>().is_ok(),
        (Some(slot), Some(instance)) => {
            slot.parse::<u64>().is_ok() && instance.parse::<u64>().is_ok()
        }
        _ => false,
    }
}

fn inventory_class(
    node: &SemanticNode,
    manual: bool,
    isolated: bool,
    duplicate_ids: &std::collections::HashSet<String>,
) -> InventoryClass {
    if node.dom_id.as_deref().is_none_or(|id| id.trim().is_empty()) {
        InventoryClass::MissingId
    } else if node.dom_id.as_deref().is_some_and(generated_dom_id) {
        InventoryClass::UnstableId
    } else if node
        .dom_id
        .as_ref()
        .is_some_and(|id| duplicate_ids.contains(id))
    {
        InventoryClass::DuplicateId
    } else if manual {
        InventoryClass::Manual
    } else if isolated {
        InventoryClass::Isolated
    } else if node.name.trim().is_empty() {
        InventoryClass::Anonymous
    } else if !reach::onscreen(node) {
        InventoryClass::Unreachable
    } else if !node.enabled {
        InventoryClass::Disabled
    } else {
        InventoryClass::Reachable
    }
}

/// Whether a selector from an outcome check addresses this exact semantic
/// control shape.
///
/// `role:name` is the precise spelling used by checks when a button and the
/// field it opens share a name. Everything else follows the same substring or
/// glob matching as the live driver, so coverage cannot claim a selector the
/// runner itself would never resolve.
/// The library part this selector names, if it names one.
///
/// `@inline-edit-field` addresses the slot rather than the text, which is what
/// separates a trigger from the thing it opens: those routinely share an
/// accessible name, so a selector written against the name reaches whichever
/// paints and never the other.
fn outcome_check_ids(node: &SemanticNode, checks: &[qa::Check]) -> Vec<String> {
    checks
        .iter()
        .filter(|check| {
            let driven = [
                check.open.as_deref(),
                check.hover.as_ref().map(qa::Hover::target),
                check.click.as_deref(),
                check.type_into.as_deref(),
                check.key_on.as_deref(),
            ]
            .into_iter()
            .flatten()
            .chain(check.covers.iter().map(String::as_str))
            .any(|selector| selector_matches_node(node, selector));
            let disabled_outcome = !node.enabled
                && matches!(check.expect, qa::Expect::Disabled)
                && selector_matches_node(node, &check.subject);
            driven || disabled_outcome
        })
        .map(|check| check.id.clone())
        .collect()
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
struct SavedControl {
    surface: String,
    #[serde(default)]
    dom_id: Option<String>,
    #[serde(default)]
    slot: Option<String>,
    role: String,
    name: String,
    classification: String,
}

#[derive(serde::Deserialize)]
struct SavedInventory {
    controls: Vec<SavedControl>,
}

fn saved_controls(report: &str) -> Result<Vec<SavedControl>, String> {
    toon_format::decode_default::<SavedInventory>(report)
        .map(|inventory| inventory.controls)
        .map_err(|error| format!("inventory report is not valid TOON: {error}"))
}

fn saved_control_node(control: &SavedControl) -> SemanticNode {
    SemanticNode {
        dom_id: control.dom_id.clone(),
        id: 0,
        parent: None,
        role: control.role.clone(),
        name: control.name.clone(),
        value: None,
        enabled: !control.classification.contains("disabled"),
        visible: !control.classification.contains("unreachable"),
        selected: false,
        bounds: Some([0.0, 0.0, 1.0, 1.0]),
        slot: control.slot.clone(),
    }
}

fn reconcile_inventory(
    inventory: &std::path::Path,
    checks_dir: Option<&std::path::Path>,
) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct MissingRow {
        surface: String,
        dom_id: Option<String>,
        slot: Option<String>,
        role: String,
        name: String,
        classification: String,
        checks: Vec<String>,
    }

    #[derive(serde::Serialize)]
    struct ReconcileReport {
        components: usize,
        outcome_declared: usize,
        excluded_manual: usize,
        failed_existing: usize,
        unverified: usize,
        controls: Vec<MissingRow>,
    }

    let input = std::fs::read_to_string(inventory)?;
    let controls = saved_controls(&input).map_err(eyre::Report::msg)?;
    let checks = qa::checks(checks_dir).map_err(eyre::Report::msg)?;
    let mut outcome_declared = 0;
    let mut excluded_manual = 0;
    let mut failed_existing = 0;
    let mut missing = Vec::new();

    for control in &controls {
        let node = saved_control_node(control);
        let matched = outcome_check_ids(&node, &checks);
        if control.classification == "excluded-manual" {
            excluded_manual += 1;
        } else if control.classification.starts_with("failed-") {
            failed_existing += 1;
            missing.push(MissingRow {
                surface: control.surface.clone(),
                dom_id: control.dom_id.clone(),
                slot: control.slot.clone(),
                role: control.role.clone(),
                name: control.name.clone(),
                classification: control.classification.clone(),
                checks: matched,
            });
        } else if control.classification.contains("isolated") || matched.is_empty() {
            missing.push(MissingRow {
                surface: control.surface.clone(),
                dom_id: control.dom_id.clone(),
                slot: control.slot.clone(),
                role: control.role.clone(),
                name: control.name.clone(),
                classification: if control.classification.contains("isolated") {
                    "isolated-unverified".into()
                } else if control.classification.contains("disabled") {
                    "state-disabled-unverified".into()
                } else {
                    "outcome-unverified".into()
                },
                checks: matched,
            });
        } else {
            outcome_declared += 1;
        }
    }

    let unverified = missing
        .iter()
        .filter(|row| !row.classification.starts_with("failed-"))
        .count();
    let report = ReconcileReport {
        components: controls.len(),
        outcome_declared,
        excluded_manual,
        failed_existing,
        unverified,
        controls: missing,
    };
    println!(
        "{}",
        toon_format::encode_default(&report).map_err(|error| eyre!(error.to_string()))?
    );
    // An offline report cannot execute controls that deliberately terminate
    // the shared process or its inspector. Keep them visible in `unverified`,
    // but let the application-owned isolated lifecycle gate decide them.
    // Otherwise a successful disposable-process check can never make the
    // combined workflow green.
    let blocking_unverified = report
        .controls
        .iter()
        .filter(|row| reconciliation_gap_blocks(&row.classification))
        .count();
    Ok(report.failed_existing + blocking_unverified)
}

fn reconciliation_gap_blocks(classification: &str) -> bool {
    !classification.starts_with("isolated-") && !classification.starts_with("failed-")
}

fn inventory_outcome_failures(unverified: usize, isolated: usize, required: bool) -> usize {
    if required {
        // Isolated controls are deliberately proven by a disposable-process
        // lifecycle gate after this shared sweep. Keep them visible in the
        // report, but do not make `--require-outcomes` impossible to satisfy.
        unverified.saturating_sub(isolated)
    } else {
        0
    }
}

async fn run_inventory(
    client: &mut Client,
    only: Option<&str>,
    require_outcomes: bool,
) -> Result<usize> {
    #[derive(serde::Serialize)]
    struct SurfaceRow {
        surface: String,
        opened: bool,
        components: usize,
        reachable: usize,
        unreachable: usize,
        state_hidden: usize,
        anonymous: usize,
        missing_id: usize,
        unstable_id: usize,
        duplicate_id: usize,
        disabled: usize,
        manual: usize,
        isolated: usize,
        outcome_declared: usize,
        unverified: usize,
        sections_opened: usize,
        rows_hovered: usize,
    }

    #[derive(serde::Serialize)]
    struct ControlRow {
        surface: String,
        id: u64,
        dom_id: Option<String>,
        slot: Option<String>,
        role: String,
        name: String,
        classification: String,
        reason: String,
        checks: Vec<String>,
    }

    #[derive(serde::Serialize)]
    struct RoleRow {
        role: String,
        components: usize,
        reachable: usize,
        unreachable: usize,
        state_hidden: usize,
        anonymous: usize,
        missing_id: usize,
        unstable_id: usize,
        duplicate_id: usize,
        disabled: usize,
        manual: usize,
        isolated: usize,
        outcome_declared: usize,
        unverified: usize,
    }

    #[derive(serde::Serialize)]
    struct InventoryReport {
        components: usize,
        reachable: usize,
        unreachable: usize,
        state_hidden: usize,
        anonymous: usize,
        missing_id: usize,
        unstable_id: usize,
        duplicate_id: usize,
        disabled: usize,
        manual: usize,
        isolated: usize,
        outcome_declared: usize,
        unverified: usize,
        surfaces: Vec<SurfaceRow>,
        roles: Vec<RoleRow>,
        controls: Vec<ControlRow>,
    }

    let checks = if std::path::Path::new("tests/ps-qa").is_dir() {
        qa::checks(None).map_err(eyre::Report::msg)?
    } else {
        Vec::new()
    };
    let mut rows = Vec::new();
    let mut controls = Vec::new();
    let mut role_counts: std::collections::BTreeMap<String, [usize; 13]> =
        std::collections::BTreeMap::new();
    for surface in reach::surfaces() {
        if only.is_some_and(|want| want != surface.name) {
            continue;
        }
        if !open_surface(client, surface).await? {
            rows.push(SurfaceRow {
                surface: surface.name.clone(),
                opened: false,
                components: 0,
                reachable: 0,
                unreachable: 0,
                state_hidden: 0,
                anonymous: 0,
                missing_id: 0,
                unstable_id: 0,
                duplicate_id: 0,
                disabled: 0,
                manual: 0,
                isolated: 0,
                outcome_declared: 0,
                unverified: 0,
                sections_opened: 0,
                rows_hovered: 0,
            });
            continue;
        }

        let sections_opened = expand_everything(client, surface).await?;
        if !open_surface(client, surface).await? {
            rows.push(SurfaceRow {
                surface: surface.name.clone(),
                opened: false,
                components: 0,
                reachable: 0,
                unreachable: 0,
                state_hidden: 0,
                anonymous: 0,
                missing_id: 0,
                unstable_id: 0,
                duplicate_id: 0,
                disabled: 0,
                manual: 0,
                isolated: 0,
                outcome_declared: 0,
                unverified: 0,
                sections_opened,
                rows_hovered: 0,
            });
            continue;
        }
        // Inventory needs the whole surface, not whichever filtered slice a
        // preceding focused check left behind. Hold the application-declared
        // reveal field empty until after the snapshot; the ordinary
        // materializer restores immediately because a single check only needs
        // one target, which is exactly the wrong lifetime for an inventory.
        let held_filter = if let Some(field) = surface.reveal_with.as_deref() {
            let (tree, _) = inspect(client).await?;
            let previous = tree
                .nodes
                .iter()
                .find(|node| selector_matches_node(node, field))
                .and_then(|node| node.value.clone())
                .unwrap_or_default();
            if !previous.is_empty() {
                type_text(client, field, "").await?;
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Some((field.to_owned(), previous))
        } else {
            None
        };
        materialize_scrolled_content(client, surface).await?;
        materialize_paginated_content(client, surface).await?;
        let rows_hovered = hover_all_rows(client).await?;
        let (tree, _) = inspect(client).await?;
        let mine: std::collections::HashSet<u64> = reach::on_surface_subtree(&tree.nodes, surface)
            .into_iter()
            .collect();
        let components: Vec<_> = tree
            .nodes
            .iter()
            .filter(|node| reach::interactive(node) && mine.contains(&node.id))
            .collect();
        let duplicate_ids = duplicate_dom_ids(&tree.nodes);
        let classes: Vec<_> = components
            .iter()
            .map(|node| {
                inventory_class(
                    node,
                    reach::requires_manual_release_check(&node.name),
                    reach::requires_isolated_outcome(&node.name),
                    &duplicate_ids,
                )
            })
            .collect();
        let declared: Vec<Vec<String>> = components
            .iter()
            .map(|node| outcome_check_ids(node, &checks))
            .collect();
        let count = |class| classes.iter().filter(|found| **found == class).count();
        let reachable = count(InventoryClass::Reachable);
        // A component can own a real semantic field that is intentionally
        // hidden until its trigger opens it. A named check which reveals and
        // judges that field is proof; an equally hidden node with no such
        // check remains a reachability failure.
        let unreachable = classes
            .iter()
            .zip(&declared)
            .filter(|(class, matches)| **class == InventoryClass::Unreachable && matches.is_empty())
            .count();
        let state_hidden = classes
            .iter()
            .zip(&declared)
            .filter(|(class, matches)| {
                **class == InventoryClass::Unreachable && !matches.is_empty()
            })
            .count();
        let anonymous = count(InventoryClass::Anonymous);
        let missing_id = count(InventoryClass::MissingId);
        let unstable_id = count(InventoryClass::UnstableId);
        let duplicate_id = count(InventoryClass::DuplicateId);
        let disabled = count(InventoryClass::Disabled);
        let manual = count(InventoryClass::Manual);
        let isolated = count(InventoryClass::Isolated);
        let outcome_declared = classes
            .iter()
            .zip(&declared)
            .filter(|(class, matches)| {
                matches!(
                    class,
                    InventoryClass::Reachable
                        | InventoryClass::Disabled
                        | InventoryClass::Unreachable
                ) && !matches.is_empty()
            })
            .count();
        let unverified = classes
            .iter()
            .zip(&declared)
            .filter(|(class, matches)| match class {
                InventoryClass::Reachable | InventoryClass::Disabled => matches.is_empty(),
                // These must run in disposable processes; declaration in the
                // shared suite cannot turn them green.
                InventoryClass::Isolated => true,
                InventoryClass::Manual
                | InventoryClass::MissingId
                | InventoryClass::UnstableId
                | InventoryClass::DuplicateId
                | InventoryClass::Anonymous
                | InventoryClass::Unreachable => false,
            })
            .count();
        for (node, matched_checks) in components.iter().zip(&declared) {
            let manual = reach::requires_manual_release_check(&node.name);
            let isolated = reach::requires_isolated_outcome(&node.name);
            let class = inventory_class(node, manual, isolated, &duplicate_ids);
            let counts = role_counts.entry(node.role.clone()).or_default();
            counts[0] += 1;
            match class {
                InventoryClass::Reachable => counts[1] += 1,
                InventoryClass::Unreachable if matched_checks.is_empty() => counts[2] += 1,
                InventoryClass::Unreachable => counts[12] += 1,
                InventoryClass::Anonymous => counts[3] += 1,
                InventoryClass::MissingId => counts[4] += 1,
                InventoryClass::UnstableId => counts[5] += 1,
                InventoryClass::DuplicateId => counts[6] += 1,
                InventoryClass::Disabled => counts[7] += 1,
                InventoryClass::Manual => counts[8] += 1,
                InventoryClass::Isolated => counts[9] += 1,
            }
            if !matched_checks.is_empty() {
                counts[10] += 1;
            }
            let is_unverified = match class {
                InventoryClass::Reachable | InventoryClass::Disabled => matched_checks.is_empty(),
                InventoryClass::Isolated => true,
                InventoryClass::Manual
                | InventoryClass::MissingId
                | InventoryClass::UnstableId
                | InventoryClass::DuplicateId
                | InventoryClass::Anonymous
                | InventoryClass::Unreachable => false,
            };
            if is_unverified {
                counts[11] += 1;
            }
            let (classification, reason) = match class {
                InventoryClass::MissingId => ("failed-missing-id", "no stable DOM id"),
                InventoryClass::UnstableId => (
                    "failed-unstable-id",
                    "framework-generated creation-order DOM id",
                ),
                InventoryClass::DuplicateId => ("failed-duplicate-id", "DOM id is not unique"),
                InventoryClass::Manual => ("excluded-manual", "native-dialog-or-external"),
                InventoryClass::Isolated => (
                    "isolated-unverified",
                    "requires disposable-process outcome check",
                ),
                InventoryClass::Anonymous => ("failed-anonymous", "no accessible name"),
                InventoryClass::Unreachable if !matched_checks.is_empty() => (
                    "outcome-declared-hidden",
                    "matched named reveal/outcome check",
                ),
                InventoryClass::Unreachable if !node.visible => ("failed-reachability", "hidden"),
                InventoryClass::Unreachable => ("failed-reachability", "no-box"),
                InventoryClass::Disabled if matched_checks.is_empty() => {
                    ("state-disabled-unverified", "no outcome check matched")
                }
                InventoryClass::Disabled => {
                    ("outcome-declared-disabled", "matched named outcome check")
                }
                InventoryClass::Reachable if matched_checks.is_empty() => {
                    ("reachable-unverified", "no outcome check matched")
                }
                InventoryClass::Reachable => ("outcome-declared", "matched named outcome check"),
            };
            controls.push(ControlRow {
                surface: surface.name.clone(),
                id: node.id,
                dom_id: node.dom_id.clone(),
                slot: node.slot.clone(),
                role: node.role.clone(),
                name: node.name.clone(),
                classification: classification.to_owned(),
                reason: reason.to_owned(),
                checks: matched_checks.clone(),
            });
        }
        rows.push(SurfaceRow {
            surface: surface.name.clone(),
            opened: true,
            components: components.len(),
            reachable,
            unreachable,
            state_hidden,
            anonymous,
            missing_id,
            unstable_id,
            duplicate_id,
            disabled,
            manual,
            isolated,
            outcome_declared,
            unverified,
            sections_opened,
            rows_hovered,
        });
        if let Some((field, previous)) = held_filter
            && !previous.is_empty()
        {
            type_text(client, &field, &previous).await?;
        }
    }

    let report = InventoryReport {
        components: rows.iter().map(|row| row.components).sum(),
        reachable: rows.iter().map(|row| row.reachable).sum(),
        unreachable: rows.iter().map(|row| row.unreachable).sum(),
        state_hidden: rows.iter().map(|row| row.state_hidden).sum(),
        anonymous: rows.iter().map(|row| row.anonymous).sum(),
        missing_id: rows.iter().map(|row| row.missing_id).sum(),
        unstable_id: rows.iter().map(|row| row.unstable_id).sum(),
        duplicate_id: rows.iter().map(|row| row.duplicate_id).sum(),
        disabled: rows.iter().map(|row| row.disabled).sum(),
        manual: rows.iter().map(|row| row.manual).sum(),
        isolated: rows.iter().map(|row| row.isolated).sum(),
        outcome_declared: rows.iter().map(|row| row.outcome_declared).sum(),
        unverified: rows.iter().map(|row| row.unverified).sum(),
        surfaces: rows,
        roles: role_counts
            .into_iter()
            .map(|(role, counts)| RoleRow {
                role,
                components: counts[0],
                reachable: counts[1],
                unreachable: counts[2],
                state_hidden: counts[12],
                anonymous: counts[3],
                missing_id: counts[4],
                unstable_id: counts[5],
                duplicate_id: counts[6],
                disabled: counts[7],
                manual: counts[8],
                isolated: counts[9],
                outcome_declared: counts[10],
                unverified: counts[11],
            })
            .collect(),
        controls,
    };
    let failures = report.unreachable
        + report.anonymous
        + report.missing_id
        + report.unstable_id
        + report.duplicate_id
        + inventory_outcome_failures(report.unverified, report.isolated, require_outcomes);
    println!(
        "{}",
        toon_format::encode_default(&report).map_err(|error| eyre!(error.to_string()))?
    );
    Ok(failures)
}

/// Navigate to a surface, and say whether it opened.
async fn open_surface(client: &mut Client, surface: &reach::Surface) -> Result<bool> {
    if surface.opener.is_empty() {
        return Ok(true);
    }
    // Do not activate the current tab again. Inventory intentionally calls
    // this before and after expanding a surface; re-clicking the same opener
    // remounts expensive application trees and adds no evidence. The marker is
    // the same destination proof used after a real navigation.
    let (current, _) = inspect(client).await?;
    if reach::on_surface(&current.nodes, surface) {
        return Ok(true);
    }
    // A dynamic document surface has no fixed name to aim at: the profile may be scrubbed,
    // so the tab is found by shape (a tab is the button whose `Close` twin the
    // strip renders beside it) rather than by a string that would differ per
    // profile.
    let opener = if surface.opener == reach::DYNAMIC_DOCUMENT {
        /*
         * Via the configured root surface, because that is where a dynamic
         * document can be opened from on a fresh profile.
         *
         * The strip holds no project tab until one has been opened, and the
         * sweep may reach the document while standing on another surface, where there is
         * no row to press either. Looking from where we happen to be found
         * nothing every time and the pane went unswept on exactly the runs that
         * matter. Cheap when a tab already exists: the lookup prefers it.
         */
        let (here, _) = inspect(client).await?;
        if reach::document_opener(&here.nodes).is_none() {
            if let Some(home) = reach::profile().home_opener.as_deref() {
                let _ = click_named_quiet(client, home).await;
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        let (tree, _) = inspect(client).await?;
        match reach::document_opener(&tree.nodes) {
            Some(name) => name,
            None => return Ok(false),
        }
    } else {
        surface.opener.to_owned()
    };
    /*
     * Confirmed by looking, not assumed from the click succeeding.
     *
     * A dispatched click is not proof of navigation: a control can acknowledge
     * activation while the current surface remains unchanged. Always confirm
     * the destination marker before attributing controls to that surface.
     */
    /*
     * A dynamic document row uses the profile contract's double-click gesture;
     * a fixed surface opener uses an ordinary semantic click.
     */
    if surface.opener == reach::DYNAMIC_DOCUMENT {
        /*
         * Scrolled into view before it is aimed at.
         *
         * A box is not a position on screen. The first row this picked sat at
         * y=1152 in a 900px window - laid out, `visible`, and below the fold -
         * so the gesture went to a point with nothing under it and the project
         * surface stayed unreachable while the report said only "could not be
         * opened".
         */
        let (tree, _) = inspect(client).await?;
        let Some(id) = tree
            .nodes
            .iter()
            .find(|n| n.name == opener && reach::onscreen(n))
            .map(|n| n.id)
        else {
            return Ok(false);
        };
        client
            .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id: id,
            }))
            .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;

        let (tree, _) = inspect(client).await?;
        if !tree.nodes.iter().any(|n| n.id == id && reach::onscreen(n)) {
            return Ok(false);
        }
        client
            .agent(&AgentControlRequest::Act(AgentAction::DoubleClick {
                node_id: id,
            }))
            .await?;
    } else if click_named_quiet(client, &opener).await.is_err() {
        return Ok(false);
    }
    if settle_on(client, surface).await? {
        return Ok(true);
    }
    // One retry through the configured root, which can recover when
    // a modal on the current one is swallowing the direct hop.
    // Not for the project surface: its opener is a gesture, and repeating it as
    // a single click would fold the row rather than open it.
    if surface.opener == reach::DYNAMIC_DOCUMENT {
        return Ok(false);
    }
    if let Some(home) = reach::profile().home_opener.as_deref() {
        let _ = click_named_quiet(client, home).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    if click_named_quiet(client, &opener).await.is_err() {
        return Ok(false);
    }
    settle_on(client, surface).await
}

/// Sweep the controls inside an open modal, then prove it can be dismissed.
///
/// Returns `true` when the dialog would not close by any means a person has:
/// its own dismiss controls, then Escape. That is a trap rather than a failed
/// button - every control behind it is unreachable while it is up - so the
/// caller stops rather than reporting hundreds of downstream failures that are
/// really one bug.
///
/// The dialog's own controls are pressed *before* the dismiss is tried, because
/// a `Cancel` that works would take them all off screen and they would never be
/// tested. Destructive-sounding ones are pressed last for the same reason.
async fn sweep_modal(
    client: &mut Client,
    opener: &str,
    here: &mut reach::Coverage,
    failures: &mut Vec<(String, String, String)>,
    surface: &str,
) -> Result<bool> {
    let (tree, _) = inspect(client).await?;
    let dismiss_ids: Vec<u64> = reach::dismissers(&tree.nodes)
        .into_iter()
        .map(|(id, _)| id)
        .collect();

    /*
     * The dialog's own controls, not the window's.
     *
     * A modal leaves the surface behind it in the tree, still `visible` and
     * still sized, exactly as a retained pane does. Sweeping everything on
     * screen can press retained-surface controls that are not in the dialog,
     * including a native-panel opener. The subtree that holds the dismiss
     * control is the dialog.
     */
    let scope = dismiss_ids
        .first()
        .map(|id| reach::enclosing_dialog(&tree.nodes, *id))
        .unwrap_or_default();
    if cli::trace() {
        for node in tree.nodes.iter().filter(|node| {
            scope.contains(&node.id)
                && (reach::profile()
                    .deferred_controls
                    .iter()
                    .any(|name| node.name.eq_ignore_ascii_case(name))
                    || reach::requires_isolated_outcome(&node.name))
        }) {
            println!(
                "      [modal] deferred to an outcome check: {:?}",
                node.name
            );
        }
    }
    let inner: Vec<(u64, String)> = tree
        .nodes
        .iter()
        .filter(|n| n.role == "button" && reach::onscreen(n) && n.enabled)
        .filter(|n| !n.name.trim().is_empty())
        .filter(|n| !dismiss_ids.contains(&n.id))
        .filter(|n| scope.contains(&n.id))
        .filter(|n| !reach::requires_manual_release_check(&n.name))
        .filter(|n| !reach::requires_isolated_outcome(&n.name))
        .filter(|n| {
            !reach::profile()
                .deferred_controls
                .iter()
                .any(|name| n.name.eq_ignore_ascii_case(name))
        })
        .map(|n| (n.id, n.name.clone()))
        .collect();

    for (id, name) in inner {
        let (before, _) = inspect(client).await?;
        if !before
            .nodes
            .iter()
            .any(|n| n.id == id && reach::onscreen(n))
        {
            continue;
        }
        if click_by_id(client, id).await.is_err() {
            continue;
        }
        // `revealed`, not `swept`: this control did not exist when the surface's
        // buttons were counted, so charging it to `swept` pushes the buckets
        // past the total and turns the consistency check negative.
        here.revealed += 1;
        if cli::trace() {
            println!("      [modal] clicked: {name:?}");
        }
        tokio::time::sleep(Duration::from_millis(180)).await;
        let (after, _) = inspect(client).await?;
        let case = sweep::Case {
            id,
            name: name.clone(),
            family: audit::family_of(&name),
            expect: sweep::expectation_for(&name, reach::is_inert_control(&name)),
        };
        if let Some(why) = sweep::judge(&case, &before.nodes, &after.nodes) {
            failures.push((
                surface.to_owned(),
                format!("{name} (in {opener} dialog)"),
                why,
            ));
        }
        // A control inside the dialog may itself have closed it.
        let (now, _) = inspect(client).await?;
        if !reach::modal_open(&now.nodes) {
            return Ok(false);
        }
    }

    // Every way out, in the order a person would reach for them.
    let (tree, _) = inspect(client).await?;
    for (id, name) in reach::dismissers(&tree.nodes) {
        if click_by_id(client, id).await.is_err() {
            continue;
        }
        // A dismisser belongs to the dialog, not to the surface underneath it.
        here.revealed += 1;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (after, _) = inspect(client).await?;
        if !reach::modal_open(&after.nodes) {
            return Ok(false);
        }
        failures.push((
            surface.to_owned(),
            format!("{name} (in {opener} dialog)"),
            "the dialog is still open; it did not dismiss".to_owned(),
        ));
    }

    // Escape, which `AppModal` binds and which is the last thing a person has.
    let _ = press_key(client, "escape", 1, "", false).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (after, _) = inspect(client).await?;
    if !reach::modal_open(&after.nodes) {
        return Ok(false);
    }
    failures.push((
        surface.to_owned(),
        format!("{opener} dialog"),
        "TRAPPED: no dismiss control and no Escape closes it".to_owned(),
    ));
    Ok(true)
}

/// Wait until the surface is actually showing, up to a few seconds.
///
/// Polled rather than slept: Analytics takes longer to build than the others
/// and a fixed 500ms wait declared it unopened while it was still on its way,
/// which sent the whole sweep down the retry path and then swept the wrong
/// surface twice. Polling costs nothing when the pane is already up.
async fn settle_on(client: &mut Client, surface: &reach::Surface) -> Result<bool> {
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let (tree, _) = inspect(client).await?;
        if reach::on_surface(&tree.nodes, surface) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Sweep every surface, and account for every button in the tree.
///
/// The difference from `sweep`: it visits more than the screen the app opened
/// on, it opens what is closed and hovers what reveals on hover before planning,
/// and it reports what it could not reach instead of dropping it. A run that
/// covers a fifth of the window now says so.
///
/// `SWEEP_TRACE=1` names every control as it is pressed and every one that goes
/// missing, with the surface state at that moment. Every coverage bug found so
/// far looked identical from the summary - a large `vanished` count - and was
/// only separable from this: a stale id after a re-sort, a tab click walking off
/// the surface, and a collapse hiding its own neighbours all read the same until
/// you can see which click preceded them.
async fn run_cover(
    client: &mut Client,
    only: Option<&str>,
    unmapped_only: bool,
    checks_dir: Option<&std::path::Path>,
) -> Result<usize> {
    let mut total = reach::Coverage::default();
    let mut failures: Vec<(String, String, String)> = Vec::new();
    // Named, so the manual worklist at the end is what this run actually met.
    let mut skipped_manual: Vec<String> = Vec::new();
    // Named separately: these remain automated work, but need a disposable app.
    let mut skipped_isolated: Vec<String> = Vec::new();
    let checks = if unmapped_only {
        qa::checks(checks_dir).map_err(eyre::Report::msg)?
    } else {
        Vec::new()
    };

    for surface in reach::surfaces() {
        if only.is_some_and(|want| want != surface.name) {
            continue;
        }
        if !open_surface(client, surface).await? {
            println!("- {:<10} could not be opened, skipping\n", surface.name);
            continue;
        }

        /*
         * Expanded within this surface only, and the surface re-checked after.
         *
         * `expand_everything` pressed every `Expand *` in the window, including
         * ones belonging to retained panes behind this one. Twelve such presses
         * can run before the current surface's plan is built, navigate away,
         * and leave that surface reporting zero buttons in a full run.
         */
        let opened = expand_everything(client, surface).await?;
        if !open_surface(client, surface).await? {
            println!("- {:<10} left during expansion, skipping\n", surface.name);
            continue;
        }
        materialize_deferred_content(client, surface, "").await?;
        materialize_paginated_content(client, surface).await?;
        let hovered = hover_all_rows(client).await?;

        /*
         * This surface's buttons, not the whole window's.
         *
         * A retained pane keeps its entire subtree alive and merely hidden, so
         * standing on one surface the tree can still hold every button from a
         * retained peer. Counting those here charges each surface for controls it was
         * not looking at: four surfaces reported 1106 buttons between them for
         * a window that has nowhere near that many, with 900 of them "hidden"
         * and the real coverage number buried.
         *
         * The subject is what is on screen now. A control retained from another
         * surface is that surface's to sweep, and it is swept when we stand
         * there.
         */
        let (tree, _) = inspect(client).await?;
        /*
         * Scoped by ancestry to the pane in front.
         *
         * A retained surface can keep real, `visible`, correctly-sized boxes behind an
         * open document pane, in the same horizontal band, so
         * neither position nor visibility separates them. Sweeping the union
         * means another surface's row controls are pressed under the current
         * surface's name while its own controls are crowded out of the plan.
         */
        let mine: std::collections::HashSet<u64> = reach::on_surface_subtree(&tree.nodes, surface)
            .into_iter()
            .collect();
        let buttons: Vec<&blitz_control_protocol::SemanticNode> = tree
            .nodes
            .iter()
            .filter(|n| n.role == "button" && !n.name.trim().is_empty())
            .filter(|n| n.visible)
            .filter(|n| mine.contains(&n.id))
            .collect();

        // Retained from other surfaces, reported so the difference between the
        // window's button count and this surface's is never silent.
        let retained = tree
            .nodes
            .iter()
            .filter(|n| n.role == "button" && !n.name.trim().is_empty() && !n.visible)
            .count();
        let mut here = reach::Coverage {
            in_tree: buttons.len(),
            hidden: 0,
            ..Default::default()
        };
        /*
         * Counted in a plain loop, not inside a `filter` closure.
         *
         * Written as a lazy filter that incremented the tallies, the counts were
         * still zero when the surface line printed them and only filled in as
         * the plan was consumed: the first run reported "0 swept, 0 unreachable,
         * UNACCOUNTED 106" for a surface it went on to sweep. A report that
         * undercounts itself is the exact failure this mode exists to remove.
         */
        let mut plan: Vec<(u64, String)> = Vec::new();
        let mut collapsers: Vec<(u64, String)> = Vec::new();
        let mut closers: Vec<(u64, String)> = Vec::new();
        for node in &buttons {
            if !reach::onscreen(node) {
                here.unreachable += 1;
            } else if reach::requires_isolated_outcome(&node.name) {
                here.isolated += 1;
                skipped_isolated.push(node.name.clone());
            } else if reach::requires_manual_release_check(&node.name) {
                // Never pressed unattended: a native chooser takes the user's
                // screen and cannot be dismissed from here.
                here.manual += 1;
                skipped_manual.push(node.name.clone());
            } else if unmapped_only && !outcome_check_ids(node, &checks).is_empty() {
                here.outcome_declared += 1;
            } else if reach::profile()
                .fold_prefixes
                .iter()
                .any(|p| node.name.to_lowercase().starts_with(&p.to_lowercase()))
                || reach::profile()
                    .deferred_controls
                    .iter()
                    .any(|c| node.name.eq_ignore_ascii_case(c))
                || reach::folds_a_section(&node.name)
            {
                /*
                 * Swept, but last.
                 *
                 * A collapse closes the section its neighbours live in, and
                 * every control underneath goes to `visible=false` at the
                 * section's own origin. Pressing `Collapse Recent` first took
                 * most controls on the surface off screen and the run charged
                 * them all as vanished, which read as the app losing its
                 * buttons rather than the sweep hiding them.
                 *
                 * A configured document-creation control belongs here for the
                 * opposite reason: it opens what it creates, so pressing it
                 * early walks the sweep off its surface. It is still pressed after
                 * the rest of the surface is done.
                 *
                 * A configured fold control can hide an entire side panel and
                 * every row action beneath it. Deferring fold controls keeps
                 * those descendants in the sweep plan.
                 */
                collapsers.push((node.id, node.name.clone()));
            } else if reach::navigates(&node.name)
                || reach::opens_document_row(&tree.nodes, node.id)
            {
                // Swept as the opener of its own surface, not here: pressing it
                // mid-plan navigates away and every later button on this
                // surface reads as vanished.
                here.navigation += 1;
            } else if reach::closes_a_surface(&node.name) {
                /*
                 * Swept, but after everything that stands on the tab it
                 * removes.
                 *
                 * Closing a tab can fall back to the root and retire a pane
                 * later surfaces are reached through. It is deferred rather
                 * than skipped, so the control is still pressed.
                 */
                closers.push((node.id, node.name.clone()));
            } else {
                plan.push((node.id, node.name.clone()));
            }
        }
        /*
         * Order within the plan: harmless first, then destructive, then the
         * disclosures that would hide either.
         *
         * A `Delete` removes its own row and shifts every row under it, so
         * running deletes early costs the sweep the controls it had not reached
         * yet. Sorting is stable, so controls otherwise keep tree order.
         */
        plan.sort_by_key(|(_, name)| {
            let lower = name.to_lowercase();
            u8::from(
                lower.starts_with("delete ")
                    || lower.starts_with("close ")
                    || lower.starts_with("remove ")
                    || lower.starts_with("retire "),
            )
        });
        plan.extend(collapsers);
        // Last of all: these retire the pane everything above stands on.
        plan.extend(closers);

        /*
         * Swept by name, and the surface is restored only when it has actually
         * been left.
         *
         * Re-navigating before every click looked safer and was much worse: the
         * re-render invalidated the very plan it was protecting, and the run
         * reported 98 of 125 controls vanished. Checking first costs one tree
         * read and leaves a working surface alone.
         */
        let mut done: HashMap<String, usize> = HashMap::new();
        let planned_total = plan.len();
        for (index, (planned_id, name)) in plan.into_iter().enumerate() {
            // What would be left if this control traps the window.
            let remaining = planned_total.saturating_sub(index + 1);
            let (mut before, _) = inspect(client).await?;
            if !reach::on_surface(&before.nodes, surface) {
                if !open_surface(client, surface).await? {
                    here.vanished += 1;
                    if cli::trace() {
                        println!("    left surface, could not return: {name:?}");
                    }
                    continue;
                }
                let (fresh, _) = inspect(client).await?;
                before = fresh;
            }

            /*
             * By id while it is still on screen, otherwise by name.
             *
             * Ids do not survive a re-render, and re-renders are ordinary here:
             * A sort control can reorder the list and every row can return with
             * a fresh id, the old ones retained as hidden 0x0 nodes at the
             * container's origin. Trusting the planned id after that pressed
             * nothing and charge the remaining controls as "vanished"
             * while all of them were on screen the whole time.
             *
             * Names are not unique - 161 on-screen buttons share 81 names, with
             * one label alone appearing thirty times - so the name path
             * takes the nth still-unpressed match rather than the first, which
             * is what stops one row absorbing every click aimed at its
             * neighbours.
             */
            let seen = done.entry(name.clone()).or_insert(0);
            let found = before
                .nodes
                .iter()
                .find(|n| n.id == planned_id && n.name == name && reach::onscreen(n))
                .or_else(|| {
                    before
                        .nodes
                        .iter()
                        .filter(|n| n.role == "button" && n.name == name && reach::onscreen(n))
                        .nth(*seen)
                });
            let Some(node) = found else {
                here.vanished += 1;
                if cli::trace() {
                    println!("    vanished: {name:?} (id {planned_id})");
                }
                continue;
            };
            *seen += 1;
            let id = node.id;
            if !reach::onscreen(node) || !node.enabled {
                here.vanished += 1;
                if cli::trace() {
                    let visible_buttons = before
                        .nodes
                        .iter()
                        .filter(|n| n.role == "button" && reach::onscreen(n))
                        .count();
                    println!(
                        "    offscreen/disabled: {name:?} visible={} bounds={:?} \
                         [on_surface={}, {visible_buttons} buttons on screen]",
                        node.visible,
                        node.bounds,
                        reach::on_surface(&before.nodes, surface),
                    );
                }
                continue;
            }
            let case = sweep::Case {
                id,
                name: name.clone(),
                family: audit::family_of(&name),
                expect: sweep::expectation_for(&name, reach::is_inert_control(&name)),
            };
            /*
             * Stop on a failed dispatch.
             *
             * This id came from the snapshot immediately above and names an
             * enabled, on-screen control. If the inspector cannot activate it,
             * continuing is not broader coverage: a native modal or halted UI
             * loop can leave the socket open without answering, making every
             * later control pay the full 60-second transport timeout. One run
             * then sat here for eighteen minutes after the ordinary sweep had
             * previously completed in two and a half. Preserve the exact
             * control in the error and stop before a transport failure is
             * multiplied by the rest of the plan.
             */
            let activation = tokio::time::timeout(check_timeout(900), click_by_id(client, id))
                .await
                .map_err(|_| {
                    eyre!(
                        "activation exceeded {}ms for {name:?} (id {id}) on {:?}",
                        check_timeout(900).as_millis(),
                        surface.name
                    )
                })?;
            if let Err(error) = activation {
                bail!(
                    "could not activate {name:?} (id {id}) on {:?}; stopping the sweep: {error}",
                    surface.name
                );
            }
            here.swept += 1;
            if cli::trace() {
                println!("    clicked: {name:?}");
            }
            tokio::time::sleep(Duration::from_millis(200)).await;

            /*
             * If that opened a modal, sweep it and then prove it closes.
             *
             * Judging each button against its own name cannot prove the
             * foreground dialog can be dismissed. The relevant outcome is
             * whether the application can return to the covered surface.
             *
             * A dialog that will not dismiss is reported and the run continues
             * from a restart rather than grinding on against a window nobody
             * can use.
             */
            let (after_click, _) = inspect(client).await?;
            let after = if reach::modal_open(&after_click.nodes) {
                let trapped =
                    sweep_modal(client, &name, &mut here, &mut failures, &surface.name).await?;
                if trapped {
                    /*
                     * Everything still in the plan is unreachable behind the
                     * trap, and saying so is the point: a bucket that silently
                     * loses 64 controls is how this audit missed the fork
                     * dialog in the first place.
                     */
                    println!(
                        "  ! {:?} opened a dialog that will not dismiss - \
                         the rest of this surface is unreachable behind it",
                        name
                    );
                    here.blocked += remaining;
                    break;
                }
                inspect(client).await?.0
            } else {
                after_click
            };
            if sweep::judge(&case, &before.nodes, &after.nodes).is_some() {
                // Backend-backed controls can acknowledge immediately and
                // update the semantic tree on the next task. Retry only a
                // would-be failure, so fast controls do not all pay for the
                // slowest one.
                tokio::time::sleep(Duration::from_millis(800)).await;
                let (settled, _) = inspect(client).await?;
                if let Some(why) = sweep::judge(&case, &before.nodes, &settled.nodes) {
                    failures.push((surface.name.to_owned(), name, why));
                }
            }
        }

        // Printed after the sweep, because `swept` is not known until then and
        // a coverage line that reports zero for work it is about to do is worse
        // than no line at all.
        println!(
            "= {:<10} {} ({} sections opened, {} rows hovered, {} retained elsewhere)",
            surface.name,
            here.line(),
            opened,
            hovered,
            retained
        );

        total.in_tree += here.in_tree;
        total.swept += here.swept;
        total.outcome_declared += here.outcome_declared;
        total.unreachable += here.unreachable;
        total.hidden += here.hidden;
        total.vanished += here.vanished;
        total.navigation += here.navigation;
        total.manual += here.manual;
        total.isolated += here.isolated;
        // `blocked` and `revealed` were missing here, so a control trapped
        // behind an undismissable dialog counted on its surface line and then
        // disappeared from the run total - the one line most likely to be
        // quoted as the coverage number.
        total.blocked += here.blocked;
        total.revealed += here.revealed;
    }

    println!("\n{}", total.line());
    /*
     * The exceptions, named, every run.
     *
     * A skipped control that is only a number in a bucket is a control nobody
     * remembers to test. Native dialogs that cannot be closed and controls
     * that open external destinations need a person, so the report says so
     * rather than implying the automated run covered them.
     */
    if total.manual > 0 {
        println!(
            "\n{} control(s) need the manual release pass:",
            total.manual
        );
        // Only the ones this run actually met, so the list is a worklist rather
        // than a catalogue of everything that could theoretically be skipped.
        let mut seen: Vec<&str> = skipped_manual.iter().map(String::as_str).collect();
        seen.sort_unstable();
        seen.dedup();
        for label in seen {
            let command = reach::profile()
                .manual_controls
                .iter()
                .find(|exception| label.starts_with(exception.label.as_str()))
                .map(|exception| exception.command.as_str())
                .unwrap_or("(unmapped manual control)");
            println!("  {label:<38} {command}");
        }
    }
    if total.isolated > 0 {
        println!(
            "\n{} control(s) require an isolated outcome check:",
            total.isolated
        );
        skipped_isolated.sort_unstable();
        skipped_isolated.dedup();
        for label in skipped_isolated {
            println!("  {label}");
        }
    }
    if failures.is_empty() {
        if total.isolated > 0 {
            println!("every broadly swept button acted; isolated controls remain listed above");
        } else {
            println!("every reached button acted");
        }
    } else {
        println!("\n{} did not act:\n", failures.len());
        for (surface, name, why) in &failures {
            println!(
                "  [{surface}] {:<40} {why}",
                name.chars().take(40).collect::<String>()
            );
        }
    }
    Ok(failures.len())
}

pub async fn run() -> Result<()> {
    let cli = <cli::Cli as clap::Parser>::parse();
    cli::set_trace(cli.trace);
    cli::set_pace(cli.pace);
    cli::set_timeout_scale(cli.timeout_scale);
    cli::set_app_profile(cli.app.clone());

    // The inventory reads the check list, not the application, so it answers
    // "what is covered" with nothing running. Before the descriptor lookup for
    // that reason: requiring a live instance to list the checks is what kept
    // the coverage question unanswerable.
    if let cli::Command::List { checks } = &cli.command {
        print!(
            "{}",
            qa::manifest(checks.as_deref()).map_err(|error| eyre!(error))?
        );
        return Ok(());
    }
    if let cli::Command::Reconcile { inventory, checks } = &cli.command {
        let failures = reconcile_inventory(inventory, checks.as_deref())?;
        if failures > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    // A component sweep launches its own hosts, so it attaches to something
    // different for every component and cannot use the descriptor discovered
    // once up front. Dispatched here, before that lookup, for the same reason
    // the offline commands above are: requiring a running application would
    // defeat the point.
    if let cli::Command::SweepComponents {
        ids,
        host,
        dists,
        checks,
        startup_timeout,
        mode,
    } = &cli.command
    {
        let failures = sweep_components(
            ids,
            host,
            dists,
            checks.as_deref(),
            std::time::Duration::from_secs(*startup_timeout),
            *mode,
        )
        .await?;
        if failures > 0 {
            std::process::exit(1);
        }
        return Ok(());
    }

    // Before attaching to anything: a run that drives an application against a
    // profile it does not have is worse than one that refuses to start, and
    // failing here names the missing file rather than reporting nothing found.
    app::AppProfile::load(cli.app.as_deref()).map_err(|error| eyre!(error))?;

    let descriptor = inspector::discover(cli.descriptor.as_deref().and_then(|p| p.to_str()))?;
    descriptor.warn_if_stale();

    let verbose = cli.command.is_dump();
    if verbose {
        println!("descriptor: {}", descriptor.path.display());
        println!("{}", report::dump(&descriptor.raw, usize::MAX));
    }

    let mut client = Client::connect(&descriptor.socket_path()).await?;
    let initialize = client.initialize().await?;
    if verbose {
        println!("\n== initialize ==");
        println!("{}", report::dump(&initialize, 800));
        println!("\n== tools ==");
        let tools = client.tools_list().await?;
        println!("{}", report::dump(&tools, 1200));
    }

    match cli.command {
        cli::Command::Metrics => {
            println!("\n== metrics ==");
            let answer = client.diagnostics(&DiagnosticsRequest::Metrics).await?;
            println!("{}", report::dump(&answer.envelope, 2000));
        }
        cli::Command::Watch { seconds } => {
            println!("\n== observing paint/metrics/console/runtimeErrors for {seconds}s ==");
            // Tolerates a protocol error on purpose: the server answers
            // `streamingUnavailable` because `observe` is not implemented, and
            // reporting that is more useful than exiting on it.
            let answer = client
                .diagnostics_envelope(&DiagnosticsRequest::Observe {
                    streams: vec![
                        DebugStream::Paint,
                        DebugStream::Metrics,
                        DebugStream::Console,
                        DebugStream::RuntimeErrors,
                    ],
                })
                .await?;
            println!("{}", report::dump(&answer, 400));
            for message in client.drain(seconds).await? {
                println!("{}", report::dump(&message, 400));
            }
        }
        cli::Command::Frames => report::show_frames(&metrics(&mut client).await?),
        cli::Command::Tree => {
            println!("\n== semantic tree ==");
            // The Python sent `maxDepth` here, which the server rejects: fields
            // inside protocol variants are snake_case. Encoding from the shared
            // type is what makes that unrepresentable rather than a silent
            // error nobody read.
            let answer = client
                .agent(&AgentControlRequest::Inspect {
                    root: None,
                    max_depth: 3,
                })
                .await?;
            println!("{}", report::dump(&answer.envelope, 3000));
        }
        cli::Command::Find {
            pattern,
            role,
            visible,
            hidden,
            painted,
            offscreen,
            disabled,
            count,
            limit,
        } => {
            find(
                &mut client,
                &pattern,
                &role,
                visible,
                hidden,
                painted,
                offscreen,
                disabled,
                count,
                limit,
            )
            .await?;
        }
        cli::Command::Layout { name } => {
            layout(&mut client, &name).await?;
        }
        cli::Command::Transcript => transcript(&mut client).await?,
        cli::Command::Paint { name, min_area } => {
            paint_audit::report(&mut client, &name, min_area).await?;
        }
        cli::Command::Contrast {
            name,
            text_ratio,
            control_ratio,
        } => {
            paint_audit::contrast(&mut client, &name, text_ratio, control_ratio).await?;
        }
        cli::Command::Nodes => {
            nodes(&mut client).await?;
        }
        cli::Command::Panes => panes(&mut client).await?,
        cli::Command::Dom { name, depth } => {
            dom(&mut client, &name, depth).await?;
        }
        cli::Command::Spill { axis, tolerance } => {
            spill(&mut client, &axis, tolerance).await?;
        }
        cli::Command::Idle => report::show("idle", &metrics(&mut client).await?),
        // Every counter the app publishes is cumulative since launch, and a
        // mount costs thousands of DOM writes. Reading them once and calling
        // the total a rate is how "3,698 attribute writes" got recorded as 230
        // a second when it was actually the startup mount, counted once.
        // Two reads and a subtraction is the only honest way to ask what an
        // idle app is still doing.
        cli::Command::Drift { seconds } => {
            let before = metrics(&mut client).await?;
            println!("== holding still for {seconds}s, nothing driven ==");
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            let after = metrics(&mut client).await?;
            let frames_of =
                |m: &RendererMetrics| m.frame_window.as_ref().map(|w| w.frames_total).unwrap_or(0);
            let frames = frames_of(&after).saturating_sub(frames_of(&before));
            println!(
                "frames={frames} over {seconds}s = {:.1}fps with no input",
                frames as f64 / seconds
            );
            report::show_delta(&before, &after, 0);
        }
        /*
         * The owner's blinking-rectangle repro, asserted rather than described.
         *
         * The repro is a project with 0 items and the item list expanded, which
         * is the state this reads. It exists because the fault was reported
         * four times and twice called fixed from a reading that did not
         * actually cover it: an idle window is not quiet here, and saying so
         * once in prose has not been enough.
         *
         * What it asserts, and why each is the honest form of the question:
         *
         * - `missed_refreshes` over the sample window. A blink is a frame that
         *   did not land, so this is the number that has to be zero. It is a
         *   count over a fixed 256-frame window, not a rate, so it is
         *   comparable between runs.
         * - the worst frame interval against the display's own period. A single
         *   72ms gap on a 60Hz display is four dropped refreshes and is visible
         *   as a flash; a mean of 13ms is not. The mean is what a naive reading
         *   reports and it hides exactly this.
         *
         * Both are read from one metrics response so they describe the same
         * window. Exits non-zero when the fault is present, so it can gate a
         * fix instead of being read by eye.
         *
         * Deliberately not asserted: fps. It averages over the window and a
         * blink does not move it enough to fail on, which is how "the window is
         * quiet" was concluded from a sample that contained a 477ms stall.
         */
        /*
         * Controls that are meant to be hidden and still take up space.
         *
         * The class of fault this catches has now shipped twice, and neither
         * time did a component test see it, because both are about geometry in
         * the real renderer rather than behaviour in a DOM stub. A field hidden
         * by styling the input alone leaves the library's wrapper in the
         * layout: measured beside every project name, a 101x46 box painting as
         * a black rectangle and squeezing the name next to it down to a few
         * characters.
         *
         * The rule is narrow on purpose. A node whose accessible name says it
         * belongs to an inactive control - a rename editor with no editor open
         * - must not own a painted box. Anything genuinely displayed is
         * expected to have one, so this reports only boxes that are both
         * sizeable and attached to something the tree calls hidden.
         */
        cli::Command::Ghost { min_area, max } => {
            // Second argument so `ghost 64 0` can still demand a perfectly clean
            // tree when a caller wants that, without editing this default.
            let answer = client
                .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
                    include_dom: true,
                    include_layout: true,
                    include_computed_style: false,
                    node_ids: Vec::new(),
                }))
                .await?;
            let DebugResponse::Snapshot(snapshot) = answer.response else {
                bail!("asked for a layout snapshot, got {:?}", answer.response);
            };

            let mut boxes: HashMap<u64, (f64, f64, f64, f64)> = HashMap::new();
            if let Some(rows) = snapshot.layout {
                for row in rows {
                    boxes.insert(
                        row.node_id,
                        (
                            row.bounds.x,
                            row.bounds.y,
                            row.bounds.width,
                            row.bounds.height,
                        ),
                    );
                }
            }

            let nodes = snapshot
                .dom
                .as_ref()
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let mut ghosts = Vec::new();
            for node in &nodes {
                // The snapshot spells this as `visible`, which is what `dom`
                // mode prints as HIDDEN. Reading a `hidden` key instead found
                // nothing and reported a clean run, which is the failure mode a
                // check like this must not have.
                let visible = node
                    .get("visible")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if visible {
                    continue;
                }
                let Some(id) = node.get("id").and_then(|v| v.as_u64()) else {
                    continue;
                };
                let Some(&(x, y, w, h)) = boxes.get(&id) else {
                    continue;
                };
                if w * h >= min_area {
                    let name = node
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    ghosts.push((id, name, x, y, w, h));
                }
            }
            ghosts.sort_by(|a, b| (b.4 * b.5).total_cmp(&(a.4 * a.5)));

            println!(
                "{} nodes inspected, reporting hidden boxes of {min_area}px2 or more",
                nodes.len()
            );
            for (id, name, x, y, w, h) in &ghosts {
                println!("  {id:>10}  {w:>7.1}x{h:<7.1} at {x:.0},{y:.0}  {name}");
            }
            if ghosts.is_empty() {
                println!("\nno ghosts: nothing hidden is holding a painted box");
            } else {
                println!(
                    "\nGHOSTS PRESENT: {} hidden node(s) still occupy layout",
                    ghosts.len()
                );
                // Retention is deliberate, so some ghosts are the design rather
                // than a leak: an application may deliberately retain a bounded
                // number of mounted-but-hidden panes. Failing on any ghost at
                // all therefore rejects valid retention policies.
                //
                // What a leak looks like: the 2026-08-20 window that painted one
                // flat colour and took no clicks measured 2,404 ghosts on a
                // comparable tree (5,527 nodes vs 5,098), a 41x rise. The budget
                // sits between the two, far enough above the healthy figure that
                // ordinary drift in the retained panes does not trip it.
                if ghosts.len() > max {
                    println!(
                        "over budget: {} ghosts, limit {max}. Hidden subtrees are not \
                         being unmounted.",
                        ghosts.len()
                    );
                    std::process::exit(1);
                }
                println!("within budget: limit {max}");
            }
        }
        cli::Command::Blink { allowed_missed } => {
            let reading = metrics(&mut client).await?;
            report::show("blink", &reading);

            let Some(window) = reading.frame_window.as_ref() else {
                eyre::bail!(
                    "the app published no frame window, so there is nothing to assert; \
                     launch it with --blitz-deep-profiling"
                );
            };

            // The refresh period the app itself reports, so this stays right on
            // a 120Hz panel rather than assuming 60. Unknown falls back to 60,
            // which is the more forgiving of the two.
            let period_ms = 1000.0
                / window
                    .display_refresh_hz
                    .filter(|hz| *hz > 0.0)
                    .unwrap_or(60.0);
            // Two periods: one late frame is a hiccup, two is a gap a person
            // sees. Anything under this is not the reported fault.
            let interval_budget = period_ms * 2.0;
            let worst_interval = window.interval.max_ms;

            println!();
            println!("== the reported repro: a project with 0 items, item list expanded ==");
            println!(
                "  missed refreshes : {} over {} frames (allowed {allowed_missed})",
                window.missed_refreshes, window.window_frames
            );
            println!(
                "  worst interval   : {worst_interval:.1}ms against a {period_ms:.1}ms refresh \
                 (budget {interval_budget:.1}ms)"
            );

            let mut faults = Vec::new();
            if window.missed_refreshes > allowed_missed {
                faults.push(format!(
                    "{} missed refreshes over {} frames",
                    window.missed_refreshes, window.window_frames
                ));
            }
            if worst_interval > interval_budget {
                faults.push(format!(
                    "a {worst_interval:.1}ms frame interval, {:.1}x the refresh period",
                    worst_interval / period_ms
                ));
            }

            if faults.is_empty() {
                println!("\nno blink: the window is quiet by both measures");
            } else {
                println!("\nBLINK PRESENT: {}", faults.join(", "));
                std::process::exit(1);
            }
        }
        cli::Command::Click { name, id } => {
            nodes(&mut client).await?;
            match (name, id) {
                (Some(name), None) => click_named(&mut client, &name).await?,
                (None, Some(node_id)) => {
                    click_by_id(&mut client, node_id).await?;
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    println!("activated node {node_id}");
                }
                _ => unreachable!("clap requires exactly one click selector"),
            }
        }
        cli::Command::Capture {
            name,
            scale,
            output,
        } => {
            capture(&mut client, &name, scale as f32, output.as_deref()).await?;
        }
        cli::Command::Audit { family } => {
            let faults = run_audit(&mut client, family.as_deref()).await?;
            if faults > 0 {
                std::process::exit(1);
            }
        }
        cli::Command::Sweep { family } => {
            let failures = run_sweep(&mut client, family.as_deref()).await?;
            if failures > 0 {
                std::process::exit(1);
            }
        }
        /*
         * A real pointer press, as opposed to a synthesised click.
         *
         * `click` dispatches an `AgentAction::Click` at a node id, which is not
         * the path a person's mouse takes. The owner reports controls working
         * that the sweep calls dead, so the two have to be separable: if a
         * control acts under `press` and not under `click`, the finding is the
         * harness's, and every result that rests on `click` needs re-reading.
         */
        cli::Command::Press { name } => {
            let (snapshot, _) = inspect(&mut client).await?;
            // `role:name`, the form `find` and every check subject accept. A
            // bare name still matches by substring, which is ambiguous in a
            // real application: "low" also matches a Keychain warning
            // containing "allow", and the pointer went there instead.
            let (role, bare) = name.split_once(':').unwrap_or(("", name.as_str()));
            let wanted = bare.to_lowercase();
            let Some(node) = snapshot
                .nodes
                .iter()
                .filter(|n| role.is_empty() || n.role == role)
                .filter(|n| n.name.to_lowercase().contains(&wanted))
                .filter(|n| n.visible && n.enabled)
                .find(|n| n.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0))
            else {
                bail!("no visible, enabled, sized node matching {name:?}");
            };
            let b = node.bounds.unwrap();
            let (x, y) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
            println!("pressing {:?} at {x:.0},{y:.0}", node.name);
            // Move first: a press at a point the document never saw hovered is
            // not what a mouse does, and hover state gates some controls.
            for phase in [PointerPhase::Move, PointerPhase::Down, PointerPhase::Up] {
                client
                    .agent(&AgentControlRequest::Act(AgentAction::Input(
                        InputCommand::Pointer {
                            phase,
                            x,
                            y,
                            button: 0,
                            modifiers: Modifiers::default(),
                        },
                    )))
                    .await?;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
            println!("pressed");
        }
        cli::Command::Cover {
            surface,
            unmapped_only,
            checks,
            max_seconds,
        } => {
            let result = tokio::time::timeout(
                Duration::from_secs(max_seconds),
                run_cover(
                    &mut client,
                    surface.as_deref(),
                    unmapped_only,
                    checks.as_deref(),
                ),
            )
            .await
            .map_err(|_| eyre!("coverage sweep exceeded {max_seconds}s"))?;
            let failures = result?;
            if failures > 0 {
                std::process::exit(1);
            }
        }
        cli::Command::Inventory {
            surface,
            require_outcomes,
        } => {
            let failures = run_inventory(&mut client, surface.as_deref(), require_outcomes).await?;
            if failures > 0 {
                std::process::exit(1);
            }
        }
        cli::Command::Qa { selector, checks } => {
            let failed = run_qa(&mut client, selector.as_deref(), checks.as_deref()).await?;
            if failed > 0 {
                std::process::exit(1);
            }
        }
        cli::Command::Reveal { name } => {
            let (snapshot, _) = inspect(&mut client).await?;
            let Some(target) = snapshot
                .nodes
                .iter()
                .find(|node| node.name.contains(&name) && node.bounds.is_some())
            else {
                bail!("no node named {name:?}");
            };
            let before = target.bounds.unwrap();
            let id = target.id;
            println!("{id} {:?} y={:.1}", target.role, before[1]);
            client
                .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                    node_id: id,
                }))
                .await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
            let (after_snapshot, _) = inspect(&mut client).await?;
            let after = after_snapshot
                .nodes
                .iter()
                .find(|node| node.id == id)
                .and_then(|node| node.bounds);
            match after {
                Some(b) => println!("after: y={:.1} (moved {:.1})", b[1], b[1] - before[1]),
                None => println!("after: the node is gone from the tree"),
            }
        }
        cli::Command::Key { name, count, over } => {
            // An empty `over` falls back to whatever the profile calls the
            // main scrolling region, so the common case needs no argument.
            let fallback = reach::profile()
                .transcript_region
                .clone()
                .unwrap_or_default();
            let over = if over.is_empty() { &fallback } else { &over };
            press_key(&mut client, &name, count as usize, over, false).await?;
        }
        cli::Command::Type { count, name } => {
            nodes(&mut client).await?;
            type_keys(&mut client, count as usize, &name).await?;
        }
        // Wheel events go to whatever the document last saw hovered, which an
        // injected pointer move does not reliably set, so they scroll nothing.
        // This asks the node's own scroll container to move, which is what
        // `scroll` should have been able to do all along.
        cli::Command::Drag { name, dy, steps } => {
            let fallback = reach::profile()
                .transcript_region
                .clone()
                .unwrap_or_default();
            let name = if name.is_empty() { &fallback } else { &name };
            let times = steps as usize;
            let (snapshot, _) = inspect(&mut client).await?;
            let Some(target) = snapshot
                .nodes
                .iter()
                .filter(|node| node.visible && node.name.contains(name))
                .filter_map(|node| node.bounds.map(|b| (node, b)))
                .max_by(|a, b| {
                    (a.1[2] * a.1[3])
                        .partial_cmp(&(b.1[2] * b.1[3]))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(node, _)| node.id)
            else {
                bail!("no visible node named {name:?}");
            };
            println!("scrolling node {target} by {dy} x{times}");
            for _ in 0..times {
                client
                    .agent(&AgentControlRequest::Act(AgentAction::ScrollBy {
                        node_id: target,
                        delta_x: 0.0,
                        delta_y: dy,
                    }))
                    .await?;
                sleep_pace().await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        cli::Command::Scroll { ticks, delta, over } => {
            let fallback = reach::profile()
                .transcript_region
                .clone()
                .unwrap_or_default();
            let over = if over.is_empty() { &fallback } else { &over };
            let count = nodes(&mut client).await?;
            hover_over(&mut client, over).await?;
            report::show("before", &metrics(&mut client).await?);
            scroll(&mut client, ticks as usize, delta).await?;
            report::show("after", &metrics(&mut client).await?);
            println!("tree size during run: {count} nodes");
        }
        // No catch-all: the parser rejects an unknown command, with the list of
        // real ones, before any of this runs.
        // Dispatched before the descriptor lookup, because they launch their
        // own hosts or read only files. Listed here so the match stays
        // exhaustive and a new command cannot be forgotten.
        cli::Command::SweepComponents { .. }
        | cli::Command::List { .. }
        | cli::Command::Reconcile { .. } => {
            unreachable!("handled before the client connects")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        InventoryClass, OutcomeStability, arrival_sample_matches, arrived_without_navigation,
        duplicate_dom_ids, generated_dom_id, inventory_class, is_pagination_control, name_matches,
        named_document_is_active, named_document_is_active_with_permanent,
        named_document_opener_for, outcome_check_ids, outcome_verdict, pagination_advanced,
        painted_bounds, painted_named, pixels_change, pixels_hold, resolved_action_target,
        rgb_pixels_hold, saved_control_node, saved_controls, selector_matches_node, stable_arrival,
    };
    use crate::app::{AppProfile, SurfaceSpec};
    use crate::interaction::parse_key_chord;
    use crate::qa::{Check, Expect};
    use crate::target::{exact_selector_matches_node, retain_exact_candidates, viewport_for_node};
    use blitz_control_protocol::{AgentSnapshot, CapturedImage, SemanticNode};
    use std::collections::HashSet;
    use std::time::Duration;

    #[test]
    fn outcome_stability_restarts_when_late_content_changes_the_document() {
        let start = tokio::time::Instant::now();
        let required = Duration::from_millis(150);
        let mut stability = OutcomeStability::default();

        assert!(!stability.observe(start, 7, true, required));
        assert!(!stability.observe(start + Duration::from_millis(100), 7, true, required));
        assert!(!stability.observe(start + Duration::from_millis(125), 8, true, required));
        assert!(!stability.observe(start + Duration::from_millis(250), 8, true, required));
        assert!(stability.observe(start + Duration::from_millis(275), 8, true, required));
    }

    #[test]
    fn failed_outcome_clears_a_partial_stability_window() {
        let start = tokio::time::Instant::now();
        let required = Duration::from_millis(100);
        let mut stability = OutcomeStability::default();

        assert!(!stability.observe(start, 3, true, required));
        assert!(!stability.observe(start + Duration::from_millis(75), 3, false, required));
        assert!(!stability.observe(start + Duration::from_millis(100), 3, true, required));
        assert!(stability.observe(start + Duration::from_millis(200), 3, true, required));
    }

    fn component(name: &str, enabled: bool, visible: bool) -> SemanticNode {
        SemanticNode {
            dom_id: Some(if name.is_empty() {
                "anonymous-component".into()
            } else {
                name.to_lowercase().replace(' ', "-")
            }),
            id: 1,
            parent: None,
            role: "button".into(),
            name: name.into(),
            value: None,
            enabled,
            visible,
            selected: false,
            bounds: Some([0.0, 0.0, 20.0, 20.0]),
            slot: None,
        }
    }

    #[test]
    fn surface_content_uses_main_viewport_while_chrome_uses_the_window() {
        let mut main = component("", true, true);
        main.id = 10;
        main.role = "main".into();
        main.bounds = Some([0.0, 58.0, 1344.0, 842.0]);

        let mut content = component("Row action", true, true);
        content.id = 11;
        content.parent = Some(main.id);
        content.bounds = Some([962.0, -3.0, 28.0, 28.0]);

        let mut chrome = component("Project tab", true, true);
        chrome.id = 12;
        chrome.bounds = Some([20.0, 15.0, 120.0, 35.0]);

        let snapshot = AgentSnapshot {
            nodes: vec![main, content, chrome],
            ..AgentSnapshot::default()
        };
        assert_eq!(viewport_for_node(&snapshot, 11), (58.0, 900.0));
        assert_eq!(viewport_for_node(&snapshot, 12), (0.0, 900.0));
    }

    #[test]
    fn pixel_stability_reports_a_changed_rendered_pixel() {
        use base64::Engine as _;

        let capture = |rgba: &[u8]| CapturedImage {
            width: 1,
            height: 1,
            rgba_base64: base64::engine::general_purpose::STANDARD.encode(rgba),
            node_id: Some(7),
        };
        let before = capture(&[20, 20, 20, 255]);
        assert!(pixels_hold(&before, &before).is_ok());
        assert_eq!(
            pixels_change(&before, &before).unwrap_err(),
            "hover left every rendered pixel unchanged"
        );

        let jitter = capture(&[19, 20, 20, 255]);
        assert!(pixels_hold(&before, &jitter).is_ok());
        assert_eq!(
            pixels_change(&before, &jitter).unwrap_err(),
            "hover left every rendered pixel unchanged"
        );

        let after = capture(&[16, 20, 20, 255]);
        assert_eq!(
            pixels_hold(&before, &after).unwrap_err(),
            "1 rendered pixel(s) changed after the pointer returned to the same state"
        );
        assert!(pixels_change(&before, &after).is_ok());

        let alpha_only = capture(&[20, 20, 20, 80]);
        assert!(rgb_pixels_hold(&before, &alpha_only).is_ok());
        let visible_colour = capture(&[29, 20, 20, 255]);
        assert_eq!(
            rgb_pixels_hold(&before, &visible_colour).unwrap_err(),
            "1 visibly coloured pixel(s) changed after the first hover"
        );
    }

    #[test]
    fn arrival_requires_three_consecutive_painted_snapshots() {
        let mut streak = 0;
        assert!(!stable_arrival(&mut streak, true));
        assert!(!stable_arrival(&mut streak, false));
        assert!(!stable_arrival(&mut streak, true));
        assert!(!stable_arrival(&mut streak, true));
        assert!(stable_arrival(&mut streak, true));
    }

    #[test]
    fn scoped_arrival_needs_only_its_painted_marker() {
        let marker = component("Search settings", true, true);
        let settings = SurfaceSpec {
            name: "settings".into(),
            opener: "Settings".into(),
            marker: Some("Search settings".into()),
            reveal_with: Some("Search settings".into()),
        };

        assert!(arrival_sample_matches(
            &[marker],
            Some(&settings),
            "Glass opacity",
            true
        ));
    }

    #[test]
    fn key_chords_preserve_dom_key_code_and_modifiers() {
        let (key, code, modifiers) = parse_key_chord("Cmd+2").unwrap();
        assert_eq!(key, "2");
        assert_eq!(code, "Digit2");
        assert!(modifiers.meta);
        assert!(!modifiers.control);

        let (key, code, modifiers) = parse_key_chord("Ctrl+Shift+Tab").unwrap();
        assert_eq!(key, "Tab");
        assert_eq!(code, "Tab");
        assert!(modifiers.control);
        assert!(modifiers.shift);
        assert!(!modifiers.meta);
    }

    #[test]
    fn inventory_categories_are_mutually_exclusive() {
        let duplicates = HashSet::new();
        assert_eq!(
            inventory_class(
                &component("Import data", true, true),
                true,
                false,
                &duplicates
            ),
            InventoryClass::Manual
        );
        assert_eq!(
            inventory_class(
                &component("Restart application", true, true),
                false,
                true,
                &duplicates
            ),
            InventoryClass::Isolated
        );
        assert_eq!(
            inventory_class(&component("", false, false), false, false, &duplicates),
            InventoryClass::Anonymous
        );
        assert_eq!(
            inventory_class(&component("Save", false, true), false, false, &duplicates),
            InventoryClass::Disabled
        );
        assert_eq!(
            inventory_class(&component("Hidden", true, false), false, false, &duplicates),
            InventoryClass::Unreachable
        );
        assert_eq!(
            inventory_class(
                &component("Synchronize", true, true),
                false,
                false,
                &duplicates
            ),
            InventoryClass::Reachable
        );
    }

    #[test]
    fn inventory_rejects_missing_and_duplicate_dom_ids_before_exclusions() {
        let mut missing = component("Import data", true, true);
        missing.dom_id = None;
        assert_eq!(
            inventory_class(&missing, true, false, &HashSet::new()),
            InventoryClass::MissingId
        );

        let duplicate = component("Save", true, true);
        let duplicates = HashSet::from([duplicate.dom_id.clone().unwrap()]);
        assert_eq!(
            inventory_class(&duplicate, false, false, &duplicates),
            InventoryClass::DuplicateId
        );
    }

    #[test]
    fn inventory_rejects_framework_creation_order_ids() {
        assert!(generated_dom_id("cl-0-trigger"));
        assert!(generated_dom_id("7-31"));
        assert!(generated_dom_id("7-31-trigger"));
        assert!(!generated_dom_id("composer-effort-trigger"));

        let mut generated = component("Save", true, true);
        generated.dom_id = Some("cl-22-trigger".into());
        assert_eq!(
            inventory_class(&generated, false, false, &HashSet::new()),
            InventoryClass::UnstableId
        );
    }

    #[test]
    fn duplicate_dom_ids_are_counted_from_the_live_tree() {
        let first = component("Save", true, true);
        let mut second = component("Discard", true, true);
        second.dom_id = first.dom_id.clone();
        assert_eq!(
            duplicate_dom_ids(&[first.clone(), second]),
            HashSet::from([first.dom_id.unwrap()])
        );
    }

    #[test]
    fn a_retained_hidden_pager_is_not_activated() {
        let scope = HashSet::from([1]);
        let patterns = vec![" more projects".to_owned()];
        assert!(is_pagination_control(
            &component("Show 5 more projects", true, true),
            &scope,
            &patterns
        ));
        assert!(!is_pagination_control(
            &component("Show 5 more projects", true, false),
            &scope,
            &patterns
        ));
    }

    #[test]
    fn an_unchanged_pager_requires_real_tree_progress() {
        let before = HashSet::from([
            ("button".to_owned(), "Show 5 more projects".to_owned(), None),
            ("button".to_owned(), "Open project one".to_owned(), None),
        ]);
        assert!(!pagination_advanced(
            &before,
            "Show 5 more projects",
            &before,
            &component("Show 5 more projects", true, true)
        ));
        assert!(pagination_advanced(
            &before,
            "Show 5 more projects",
            &HashSet::from([
                ("button".to_owned(), "Show 5 more projects".to_owned(), None),
                ("button".to_owned(), "Open project one".to_owned(), None),
                ("button".to_owned(), "Open project two".to_owned(), None),
            ]),
            &component("Show 5 more projects", true, true)
        ));
        assert!(!pagination_advanced(
            &before,
            "Show 5 more projects",
            &HashSet::from([
                ("button".to_owned(), "Show 5 more projects".to_owned(), None),
                (
                    "button".to_owned(),
                    "Open project replacement".to_owned(),
                    None
                ),
            ]),
            &component("Show 5 more projects", true, true)
        ));
        assert!(pagination_advanced(
            &before,
            "Show 5 more projects",
            &before,
            &component("Show 3 more projects", true, true)
        ));
    }

    fn check(id: &str, click: Option<&str>, subject: &str) -> Check {
        Check {
            id: id.into(),
            group: "coverage".into(),
            what: "a rendered outcome".into(),
            open: None,
            prepare: None,
            prepare_unless: None,
            prepare_press: false,
            prepare_key: None,
            hover: None,
            hover_unless: None,
            after_prepare_hover: None,
            reveal_before_capture: None,
            setup_type_into: None,
            setup_text: None,
            click: click.map(str::to_owned),
            type_into: None,
            text: None,
            key: None,
            key_on: None,
            scroll_over: None,
            scroll_ticks: 0,
            scroll_delta: 0.0,
            compare: None,
            expect_size: None,
            expect_count: None,
            covers: Vec::new(),
            press: false,
            settle_after_ms: 0,
            outcome_timeout_ms: 0,
            stable_for_ms: 0,
            destructive: false,
            subject: subject.into(),
            expect: Expect::Paints,
        }
    }

    #[test]
    fn role_qualified_coverage_does_not_credit_a_same_named_wrong_role() {
        let button = component("Rename project", true, true);
        assert!(!selector_matches_node(&button, "textbox:Rename project"));

        let mut textbox = button.clone();
        textbox.role = "textbox".into();
        assert!(selector_matches_node(&textbox, "textbox:Rename project"));
    }

    #[test]
    fn exact_name_priority_does_not_choose_a_longer_substring() {
        let restart = component("Restart", false, true);
        let proxy = component("Restart AgencyProxy", true, true);

        assert!(exact_selector_matches_node(&restart, "Restart"));
        assert!(!exact_selector_matches_node(&proxy, "Restart"));
        assert!(exact_selector_matches_node(&restart, "button:Restart"));
        assert!(!exact_selector_matches_node(&restart, "switch:Restart"));

        let mut candidates = vec![
            (&proxy, [0.0, 0.0, 20.0, 20.0]),
            (&restart, [0.0, 900.0, 20.0, 20.0]),
        ];
        retain_exact_candidates(&mut candidates, "button:Restart");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0.name, "Restart");
        assert!(!candidates[0].0.enabled);
    }

    #[test]
    fn check_preconditions_require_a_painted_matching_role() {
        let button = component("Rename project", true, true);
        assert!(painted_named(std::slice::from_ref(&button), "Rename"));
        assert!(!painted_named(
            std::slice::from_ref(&button),
            "textbox:Rename project"
        ));

        let mut textbox = button;
        textbox.role = "textbox".into();
        assert!(painted_named(
            std::slice::from_ref(&textbox),
            "textbox:Rename project"
        ));
        textbox.bounds = Some([0.0, 0.0, 0.0, 0.0]);
        assert!(!painted_named(
            std::slice::from_ref(&textbox),
            "textbox:Rename project"
        ));
    }

    #[test]
    fn actionability_rejects_hidden_retained_menu_items_with_stale_boxes() {
        let mut retained_menu_item = component("Opus", true, false);
        assert_eq!(
            painted_bounds(&retained_menu_item),
            retained_menu_item.bounds
        );
        assert_eq!(
            resolved_action_target(std::slice::from_ref(&retained_menu_item), "menuitem:Opus"),
            None,
            "the fixture role has not yet been made a menuitem",
        );

        retained_menu_item.role = "menuitem".into();
        assert_eq!(
            resolved_action_target(std::slice::from_ref(&retained_menu_item), "menuitem:Opus"),
            None,
            "a stale box does not make a hidden retained menu item actionable",
        );

        let mut mounted_menu_item = retained_menu_item.clone();
        mounted_menu_item.visible = true;
        assert_eq!(
            resolved_action_target(std::slice::from_ref(&mounted_menu_item), "menuitem:Opus"),
            Some("Opus".into()),
        );

        mounted_menu_item.bounds = Some([0.0, 0.0, 0.0, 0.0]);
        assert!(painted_bounds(&mounted_menu_item).is_none());
    }

    #[test]
    fn only_profile_declared_documents_require_exact_activation() {
        let profile = AppProfile {
            document_openers: vec!["Fixture project".into()],
            ..AppProfile::default()
        };

        assert!(named_document_opener_for(&profile, "fixture PROJECT"));
        assert!(!named_document_opener_for(&profile, "Settings"));
    }

    #[test]
    fn a_generic_document_marker_never_skips_named_document_activation() {
        let project = SurfaceSpec {
            name: "project".into(),
            opener: crate::reach::DYNAMIC_DOCUMENT.into(),
            marker: Some("Send".into()),
            reveal_with: None,
        };
        let nodes = [component("Send", true, true)];

        assert!(!arrived_without_navigation(
            &nodes,
            Some(&project),
            "fixture row",
            true,
            false,
        ));
        assert!(arrived_without_navigation(
            &nodes,
            Some(&project),
            "fixture row",
            true,
            true,
        ));
        assert!(arrived_without_navigation(
            &nodes,
            Some(&project),
            "fixture row",
            false,
            false,
        ));
    }

    #[test]
    fn a_named_document_is_active_only_when_its_exact_tab_is_selected() {
        let mut tab = component("Fixture projectFixture project", true, true);
        assert!(!named_document_is_active(&[tab.clone()], "Fixture project"));
        tab.selected = true;
        assert!(named_document_is_active(&[tab.clone()], "Fixture project"));

        let permanent_name = "Settings".to_owned();
        let mut permanent = component(&permanent_name, true, true);
        permanent.selected = true;
        assert!(
            !named_document_is_active_with_permanent(
                &[tab, permanent],
                "Fixture project",
                &[permanent_name],
            ),
            "a selected retained document is not in front of a selected permanent surface"
        );
    }

    #[test]
    fn target_paints_resolves_a_role_qualified_click_selector() {
        let status = component("Change the status of fixture item", true, true);

        assert_eq!(
            resolved_action_target(&[status], "button:Change the status of fixture item"),
            Some("Change the status of fixture item".into()),
        );
    }

    #[test]
    fn outcome_coverage_names_only_checks_that_drive_an_enabled_control() {
        let checks = [
            check("rename", Some("button:Rename"), "textbox:Rename project"),
            check("save", Some("Save"), "Saved"),
        ];
        assert_eq!(
            outcome_check_ids(&component("Rename project", true, true), &checks),
            vec!["rename"]
        );
        assert!(outcome_check_ids(&component("Delete project", true, true), &checks).is_empty());
        let mut editor = component("Rename project", true, true);
        editor.role = "textbox".into();
        assert!(outcome_check_ids(&editor, &checks).is_empty());
    }

    #[test]
    fn observing_disabled_is_complete_coverage_without_activation() {
        let mut disabled = component("Use the default", false, true);
        disabled.role = "button".into();
        let mut observed = check("disabled-default", None, "Use the default");
        observed.expect = Expect::Disabled;
        assert_eq!(
            outcome_check_ids(&disabled, &[observed]),
            vec!["disabled-default"]
        );
    }

    #[test]
    fn an_explicit_family_selector_credits_repeated_component_rows() {
        let mut check = check("offer-model", Some("Offer Default"), "Offer Default");
        check.covers.push("checkbox:Offer ".into());
        let mut sibling = component("Offer Sonnet", true, true);
        sibling.role = "checkbox".into();
        assert_eq!(outcome_check_ids(&sibling, &[check]), vec!["offer-model"]);
    }

    #[test]
    fn outcome_waiting_rejects_an_unchanged_refresh_indicator() {
        let mut check = check("refresh", Some("Refresh"), "Refresh generation");
        check.expect = Expect::NameChanges;
        let before = component("Refresh generation 1", true, true);
        let after = before.clone();
        assert!(outcome_verdict(&check, &[before], &[after], None, None).is_err());
    }

    #[test]
    fn outcome_waiting_accepts_the_completed_refresh_indicator() {
        let mut check = check("refresh", Some("Refresh"), "Refresh generation");
        check.expect = Expect::NameChanges;
        let before = component("Refresh generation 1", true, true);
        let after = component("Refresh generation 2", true, true);
        assert!(outcome_verdict(&check, &[before], &[after], None, None).is_ok());
    }

    #[test]
    fn value_outcome_can_belong_to_a_different_node_than_the_clicked_action() {
        let mut check = check("reset-opacity", Some("Reset to default"), "Glass opacity");
        check.expect = Expect::ValueChanges;

        let reset = component("Reset to default", true, true);
        let mut before_slider = component("Glass opacity", true, true);
        before_slider.id = 2;
        before_slider.role = "slider".into();
        before_slider.value = Some("100".into());
        let mut after_slider = before_slider.clone();
        after_slider.value = Some("55".into());

        assert!(
            outcome_verdict(
                &check,
                &[reset.clone(), before_slider],
                &[reset, after_slider],
                Some("Reset to default"),
                Some(1),
            )
            .is_ok()
        );
    }

    #[test]
    fn saved_inventory_rows_keep_quoted_control_names_with_commas() {
        let rows = saved_controls(
            "controls[1]{surface,id,role,name,classification,reason}:\n  \
             home,7,button,\"Delete alpha, beta\",reachable-unverified,none",
        )
        .expect("controls");
        let row = &rows[0];
        assert_eq!(row.surface, "home");
        assert_eq!(row.role, "button");
        assert_eq!(row.name, "Delete alpha, beta");
        assert_eq!(row.classification, "reachable-unverified");
    }

    #[test]
    fn a_saved_report_must_have_an_inventory_table() {
        assert!(saved_controls("components: 3").is_err());
        assert_eq!(
            saved_controls(
                "controls[1]{surface,id,role,name,classification,reason}:\n  home,7,button,Save,reachable-unverified,none"
            )
            .expect("controls")
            .len(),
            1
        );
    }

    #[test]
    fn nested_inventory_rows_round_trip_through_the_toon_decoder() {
        let rows = saved_controls(
            "components: 1\ncontrols[1]:\n  - surface: settings\n    id: 7\n    \
             role: switch\n    name: Enable inspection\n    \
             classification: \"isolated-unverified\"\n    reason: separate process\n    \
             checks[0]:",
        )
        .expect("nested controls");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].surface, "settings");
        assert_eq!(rows[0].role, "switch");
        assert_eq!(rows[0].name, "Enable inspection");
        assert_eq!(rows[0].classification, "isolated-unverified");
    }

    #[test]
    fn offline_inventory_keeps_slot_selector_credit() {
        let rows = saved_controls(
            "controls[1]{surface,id,dom_id,slot,role,name,classification,reason}:\n  \
             settings,7,theme-accent,complex-color-wheel,button,Accent,outcome-declared,matched",
        )
        .expect("controls");
        let mut check = check("accent-wheel", None, "@complex-color-wheel");
        check.covers.push("@complex-color-wheel".into());

        assert_eq!(rows[0].slot.as_deref(), Some("complex-color-wheel"));
        assert_eq!(
            outcome_check_ids(&saved_control_node(&rows[0]), &[check]),
            vec!["accent-wheel"]
        );
    }

    #[test]
    fn isolated_inventory_rows_remain_reported_without_blocking_reconciliation() {
        assert!(!super::reconciliation_gap_blocks("isolated-unverified"));
        assert!(super::reconciliation_gap_blocks("outcome-unverified"));
        assert_eq!(super::inventory_outcome_failures(1, 1, true), 0);
        assert_eq!(super::inventory_outcome_failures(3, 1, true), 2);
        assert_eq!(super::inventory_outcome_failures(3, 1, false), 0);
    }

    /// A bare word is a substring, because that is how a control is recalled.
    #[test]
    fn a_bare_pattern_is_a_substring() {
        assert!(name_matches("Rename project", "rename"));
        assert!(name_matches("Rename project", "project"));
        assert!(!name_matches("Rename project", "delete"));
    }

    #[test]
    fn a_trailing_star_anchors_the_front() {
        assert!(name_matches("chat with agent", "chat*"));
        assert!(!name_matches("open chat", "chat*"));
    }

    #[test]
    fn a_leading_star_anchors_the_end() {
        assert!(name_matches("open chat", "*chat"));
        assert!(!name_matches("chat with agent", "*chat"));
    }

    #[test]
    fn stars_at_both_ends_match_anywhere() {
        assert!(name_matches("the chat panel", "*chat*"));
        assert!(!name_matches("Rename project", "*chat*"));
    }

    /// Matching is case insensitive: a name is remembered by its words, not by
    /// the capitalisation a designer chose.
    #[test]
    fn matching_ignores_case() {
        assert!(name_matches("Rename Project", "rename project"));
        assert!(name_matches("CHAT", "chat*"));
    }

    #[test]
    fn a_lone_star_matches_everything() {
        assert!(name_matches("anything at all", "*"));
        assert!(name_matches("", "*"));
    }
}
