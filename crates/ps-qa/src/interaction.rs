//! Pointer, keyboard, and text input against a running application.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use blitz_control_protocol::{
    AgentAction, AgentControlRequest, InputCommand, KeyPhase, Modifiers, PointerPhase,
    SemanticNode, WheelPhase,
};
use eyre::{Result, bail, eyre};

use crate::diagnostics::metrics;
use crate::inspector::{Client, inspect};
use crate::target::{locate_control, selector_matches_node};
use crate::timing::{pace, sleep_pace};
use crate::{cli, reach, report};

pub(crate) async fn hover_over(client: &mut Client, want: &str) -> Result<bool> {
    // "x,y" targets a point directly. A scroll container often has no
    // accessible name, so naming is not always enough to put the pointer inside
    // the thing you mean to scroll: a repeated label can find a button in the
    // sidebar and every wheel event went there.
    if let Some((x, y)) = want.split_once(',')
        && let (Ok(x), Ok(y)) = (x.trim().parse::<f64>(), y.trim().parse::<f64>())
    {
        {
            client
                .agent(&AgentControlRequest::Act(AgentAction::Input(
                    InputCommand::Pointer {
                        phase: PointerPhase::Move,
                        x,
                        y,
                        button: 0,
                        modifiers: Modifiers::default(),
                    },
                )))
                .await?;
            println!("pointer at {x:.0},{y:.0}");
            return Ok(true);
        }
    }
    let (snapshot, _) = inspect(client).await?;
    let Some(node) = snapshot
        .nodes
        .iter()
        .filter(|node| node.visible && selector_matches_node(node, want))
        .filter_map(|node| node.bounds.map(|bounds| (node, bounds)))
        .max_by(|a, b| {
            (a.1[2] * a.1[3])
                .partial_cmp(&(b.1[2] * b.1[3]))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(node, bounds)| (node.name.clone(), bounds))
    else {
        println!("no visible node named {want:?} to hover; wheel goes wherever it goes");
        return Ok(false);
    };
    let (name, bounds) = node;
    let (x, y) = (bounds[0] + bounds[2] / 2.0, bounds[1] + bounds[3] / 2.0);
    client
        .agent(&AgentControlRequest::Act(AgentAction::Input(
            InputCommand::Pointer {
                phase: PointerPhase::Move,
                x,
                y,
                button: 0,
                modifiers: Modifiers::default(),
            },
        )))
        .await?;
    println!("pointer over {name:?} at {x:.0},{y:.0}");
    Ok(true)
}

pub(crate) async fn scroll(client: &mut Client, ticks: usize, delta: f64) -> Result<()> {
    // Say what the pace is, every time, before any number is printed.
    //
    // The default sends a wheel event every 1/60s, so the app is asked for
    // roughly 60 frames a second and answers about 53 once the sleep overhead
    // is counted. Read without this line, that 53 looks like the application
    // failing to keep up on a 120Hz display, and the `missed_refreshes` figure
    // beside it appears to confirm it. Both are describing this loop.
    //
    // `--pace 0` sends as fast as the app accepts, which is what measures
    // the application: it reaches 120fps with no missed refreshes.
    let pace = pace();
    if pace.is_zero() {
        println!("pace: unpaced (--pace 0) - measures app throughput");
    } else {
        println!(
            "pace: {:.2}ms between events ({:.0} Hz requested); fps and missed_refreshes \
             below describe this pace, not the app's limit. Pass --pace 0 to remove it",
            pace.as_secs_f64() * 1000.0,
            1.0 / pace.as_secs_f64(),
        );
    }

    let mut latencies = scroll_events(client, ticks, delta).await?;
    report::show_latencies("wheel events", ticks, &mut latencies);
    Ok(())
}

/** Send wheel input without the benchmark report, for declarative QA actions. */
pub(crate) async fn scroll_events(
    client: &mut Client,
    ticks: usize,
    delta: f64,
) -> Result<Vec<f64>> {
    let mut latencies = Vec::with_capacity(ticks);
    for _ in 0..ticks {
        let started = Instant::now();
        client
            .agent(&AgentControlRequest::Act(AgentAction::Input(
                InputCommand::Wheel {
                    delta_x: 0.0,
                    delta_y: delta,
                    phase: WheelPhase::Moved,
                    modifiers: Modifiers::default(),
                },
            )))
            .await?;
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        sleep_pace().await;
    }
    Ok(latencies)
}

/// The painted textbox whose semantic name mentions `want` on the active layer.
fn find_text_field<'a>(nodes: &'a [SemanticNode], want: &str) -> Option<&'a SemanticNode> {
    let modal_scope: HashSet<u64> = reach::dismissers(nodes)
        .first()
        .map(|(id, _)| reach::enclosing_dialog(nodes, *id))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let surface_scope: HashSet<u64> = reach::surfaces()
        .iter()
        .find(|surface| reach::on_surface(nodes, surface))
        .map(|surface| reach::on_surface_subtree(nodes, surface))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let fields: Vec<&SemanticNode> = nodes
        .iter()
        .filter(|node| {
            matches!(node.role.as_str(), "textbox" | "textarea" | "input")
                && node.enabled
                && node
                    .bounds
                    .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
        })
        .collect();

    let matches_name = |node: &&SemanticNode| want.is_empty() || selector_matches_node(node, want);
    for scope in [&modal_scope, &surface_scope] {
        if let Some(field) = fields
            .iter()
            .find(|node| matches_name(node) && scope.contains(&node.id))
        {
            return Some(field);
        }
    }
    fields.into_iter().find(matches_name)
}

/// Drive real key events into a focused text field and price them.
///
/// Typing is the interaction the composer autosizes on: it writes
/// `style.height`, reads `scrollHeight`, and writes it again, so every
/// keystroke forces a synchronous layout resolve. Scrolling never exercises
/// that path, which is why a scroll benchmark cannot stand in for this one.
/// Send a named key, optionally repeated, after focusing something inside a
/// named container.
///
/// Wheel events could not scroll the transcript: they carry no coordinates, and
/// pointing them at the pane still moved nothing. Page Up does, because it goes
/// to the focused scroller, and without it every attempt to reach the rows the
/// owner was looking at meant asking a person to scroll. A bug that only
/// reproduces by hand is a bug that gets one measurement per message.
pub(crate) async fn press_key(
    client: &mut Client,
    name: &str,
    count: usize,
    over: &str,
    require_target: bool,
) -> Result<()> {
    // A key goes to the focused node, so focus the target first without
    // activating it. This cannot be a click: a button's click is its action,
    // and setup would submit, delete or fork before the key under test arrived.
    let (snapshot, _) = inspect(client).await?;
    // A control whose only label is `sr-only` reaches the semantic tree with an
    // empty name, so no substring can address it. The slider is one, which is
    // why targeting by node id has to be possible at all.
    let by_id = over.parse::<u64>().ok().filter(|id| {
        snapshot.nodes.iter().any(|node| {
            node.id == *id
                && node
                    .bounds
                    .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
        })
    });
    let target = if by_id.is_some() || over.is_empty() {
        by_id
    } else {
        match locate_control(client, over, &[]).await {
            Ok((node_id, _)) => Some(node_id),
            Err(error) if require_target => {
                bail!("no enabled, painted key target matching {over:?}: {error}")
            }
            Err(_) => None,
        }
    };
    if let Some(target) = target {
        client
            .agent(&AgentControlRequest::Act(AgentAction::Focus {
                node_id: target,
            }))
            .await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        println!("focused node {target} for {name} x{count}");
    } else if require_target {
        bail!("no enabled, painted key target matching {over:?}");
    } else {
        println!("no visible node named {over:?}; sending {name} to whatever has focus");
    }

    let (key, code, modifiers) = parse_key_chord(name)?;

    for _ in 0..count {
        for phase in [KeyPhase::Down, KeyPhase::Up] {
            client
                .agent(&AgentControlRequest::Act(AgentAction::Input(
                    InputCommand::Key {
                        phase,
                        key: key.clone(),
                        code: code.clone(),
                        modifiers,
                    },
                )))
                .await?;
        }
        sleep_pace().await;
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
    Ok(())
}

/// Parse the same compact chord a person would write in a QA manifest.
///
/// `key` and `code` are both what the DOM calls them. They are not
/// interchangeable and sending the wrong one is a silent no-op. Modifiers
/// belong on both phases of the key event, so the application sees the actual
/// shortcut rather than a plain character that merely resembles it.
pub(crate) fn parse_key_chord(name: &str) -> Result<(String, String, Modifiers)> {
    let mut parts = name
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .peekable();
    let mut modifiers = Modifiers::default();
    let mut key_name = None;

    while let Some(part) = parts.next() {
        let lower = part.to_ascii_lowercase();
        let is_last = parts.peek().is_none();
        if is_last {
            key_name = Some(lower);
            break;
        }
        match lower.as_str() {
            "meta" | "cmd" | "command" => modifiers.meta = true,
            "ctrl" | "control" => modifiers.control = true,
            "alt" | "option" => modifiers.alt = true,
            "shift" => modifiers.shift = true,
            other => bail!("unknown key modifier {other:?}: meta, cmd, ctrl, alt, shift"),
        }
    }

    let key_name = key_name.ok_or_else(|| eyre!("key chord is empty"))?;
    let (key, code) = match key_name.as_str() {
        "pageup" | "pgup" => ("PageUp", "PageUp"),
        "pagedown" | "pgdn" => ("PageDown", "PageDown"),
        "home" => ("Home", "Home"),
        "end" => ("End", "End"),
        "up" | "arrowup" => ("ArrowUp", "ArrowUp"),
        "down" | "arrowdown" => ("ArrowDown", "ArrowDown"),
        "left" | "arrowleft" => ("ArrowLeft", "ArrowLeft"),
        "right" | "arrowright" => ("ArrowRight", "ArrowRight"),
        "tab" => ("Tab", "Tab"),
        "enter" => ("Enter", "Enter"),
        "escape" | "esc" => ("Escape", "Escape"),
        "1" => ("1", "Digit1"),
        "2" => ("2", "Digit2"),
        other => {
            bail!(
                "unknown key {other:?}: 1, 2, pageup, pagedown, home, end, up, down, left, right, tab, enter, escape"
            )
        }
    };
    Ok((key.to_owned(), code.to_owned(), modifiers))
}

pub(crate) async fn type_keys(client: &mut Client, count: usize, want: &str) -> Result<()> {
    let (snapshot, _) = inspect(client).await?;
    let Some(field) = find_text_field(&snapshot.nodes, want) else {
        bail!("no enabled, visible text field found; open a tab with a composer");
    };
    println!(
        "typing into node {} role={} name={}",
        field.id,
        field.role,
        report::py_repr(&field.name.chars().take(40).collect::<String>())
    );
    client
        .agent(&AgentControlRequest::Act(AgentAction::Click {
            node_id: field.id,
        }))
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let before = metrics(client).await?;
    let mut latencies = Vec::with_capacity(count);
    for index in 0..count {
        let letter = (b'a' + (index % 26) as u8) as char;
        let started = Instant::now();
        for phase in [KeyPhase::Down, KeyPhase::Up] {
            client
                .agent(&AgentControlRequest::Act(AgentAction::Input(
                    InputCommand::Key {
                        phase,
                        key: letter.to_string(),
                        code: format!("Key{}", letter.to_ascii_uppercase()),
                        modifiers: Modifiers::default(),
                    },
                )))
                .await?;
        }
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
        sleep_pace().await;
    }
    let after = metrics(client).await?;

    report::show_latencies("keystrokes", count, &mut latencies);
    report::show("before", &before);
    report::show("after", &after);
    report::show_delta(&before, &after, count);
    Ok(())
}

/// Set literal text on an exact semantic text-field node.
pub(crate) async fn type_text(client: &mut Client, want: &str, text: &str) -> Result<u64> {
    let (snapshot, _) = inspect(client).await?;
    let field = find_text_field(&snapshot.nodes, want)
        .ok_or_else(|| eyre!("no enabled, visible text field matching {want:?}"))?;
    if cli::trace() {
        println!("        setting {want:?} (id {})", field.id);
    }
    client
        .agent(&AgentControlRequest::Act(AgentAction::SetValue {
            node_id: field.id,
            value: text.to_owned(),
        }))
        .await?;
    tokio::time::sleep(Duration::from_millis(25)).await;
    Ok(field.id)
}

/// Price a single click, such as switching to a tab.
///
/// A tab switch flips `display: none` to `flex` over that tab's whole subtree,
/// so taffy lays out in one pass everything the tab retained while hidden. That
/// is a different cost from typing and needs its own measurement.
pub(crate) async fn click_named(client: &mut Client, want: &str) -> Result<()> {
    let (target_id, _) = locate_control(client, want, &[]).await?;
    let (snapshot, _) = inspect(client).await?;
    let target = snapshot
        .nodes
        .iter()
        .find(|node| node.id == target_id)
        .ok_or_else(|| eyre!("node {target_id} disappeared after it was located"))?;
    println!(
        "clicking node {} role={} name={}",
        target.id,
        target.role,
        report::py_repr(&target.name.chars().take(50).collect::<String>())
    );

    // A click is dispatched at the node's coordinates, so a node scrolled out
    // of the viewport gets a `pointerdown` at a point nothing is at and no
    // click at all. "Show 12 earlier messages" sat at y=-2246 and every attempt
    // to press it read as the button doing nothing.
    let before = metrics(client).await?;
    let started = Instant::now();
    client
        .agent(&AgentControlRequest::Act(AgentAction::Click {
            node_id: target_id,
        }))
        .await?;
    let ack = started.elapsed().as_secs_f64() * 1000.0;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = metrics(client).await?;

    println!("click acked in {ack:.1}ms");
    report::show("before", &before);
    report::show("after", &after);
    report::show_delta(&before, &after, 1);
    Ok(())
}
