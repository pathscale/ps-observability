//! Read-only diagnostics over a running Blitz application.

use std::collections::HashMap;

use blitz_control_protocol::{
    DebugResponse, DiagnosticsRequest, RendererMetrics, SemanticNode, SnapshotRequest,
};
use eyre::{Result, bail};

use crate::inspector::{Client, inspect};
use crate::{reach, report};

pub(crate) async fn metrics(client: &mut Client) -> Result<RendererMetrics> {
    match client
        .diagnostics(&DiagnosticsRequest::Metrics)
        .await?
        .response
    {
        DebugResponse::Metrics(metrics) => Ok(metrics),
        other => bail!("asked for metrics, got {other:?}"),
    }
}

pub(crate) async fn transcript(client: &mut Client) -> Result<()> {
    let answer = client
        .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
            include_dom: true,
            include_layout: true,
            include_computed_style: false,
            node_ids: Vec::new(),
        }))
        .await?;
    let DebugResponse::Snapshot(snapshot) = answer.response else {
        bail!("asked for a transcript snapshot, got {:?}", answer.response);
    };
    let nodes = snapshot
        .dom
        .as_ref()
        .and_then(|value| value.as_array())
        .ok_or_else(|| eyre::eyre!("snapshot omitted DOM rows"))?;
    let rows = snapshot
        .layout
        .as_ref()
        .ok_or_else(|| eyre::eyre!("snapshot omitted layout rows"))?;
    let conversation = nodes
        .iter()
        .find(|node| {
            node.get("name").and_then(|value| value.as_str())
                == reach::profile().transcript_region.as_deref()
        })
        .and_then(|node| node.get("id").and_then(|value| value.as_u64()))
        .ok_or_else(|| eyre::eyre!("configured transcript region is absent"))?;
    let parent: HashMap<u64, Option<u64>> = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.get("id")?.as_u64()?,
                node.get("parent").and_then(|value| value.as_u64()),
            ))
        })
        .collect();
    let named: HashMap<u64, (&str, &str)> = nodes
        .iter()
        .filter_map(|node| {
            Some((
                node.get("id")?.as_u64()?,
                (
                    node.get("role")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                    node.get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                ),
            ))
        })
        .collect();
    let layout: HashMap<u64, &blitz_control_protocol::LayoutDiagnosticRow> =
        rows.iter().map(|row| (row.node_id, row)).collect();
    let conversation_row = layout
        .get(&conversation)
        .ok_or_else(|| eyre::eyre!("configured transcript region has no layout row"))?;
    let viewport_bottom = conversation_row.bounds.y + conversation_row.bounds.height;
    println!(
        "transcript id={conversation} top={:.1} bottom={viewport_bottom:.1} scrollTop={:.1} max={:.1} clientHeight={:.1} scrollHeight={:.1} gapToMax={:.1}",
        conversation_row.bounds.y,
        conversation_row.scroll_offset.y,
        conversation_row.scroll_range.height,
        conversation_row.client_size.height,
        conversation_row.scroll_size.height,
        conversation_row.scroll_range.height - conversation_row.scroll_offset.y,
    );
    let is_descendant = |mut id: u64| {
        for _ in 0..512 {
            let Some(Some(next)) = parent.get(&id) else {
                return false;
            };
            if *next == conversation {
                return true;
            }
            id = *next;
        }
        false
    };
    let mut descendants: Vec<(f64, u64, f64)> = layout
        .iter()
        .filter_map(|(id, row)| {
            if *id == conversation || !is_descendant(*id) {
                return None;
            }
            let top = row.bounds.y;
            let height = row.bounds.height;
            Some((top + height, *id, top))
        })
        .collect();
    descendants.sort_by(|left, right| right.0.total_cmp(&left.0));
    for (bottom, id, top) in descendants.into_iter().take(12) {
        let (role, name) = named.get(&id).copied().unwrap_or(("", ""));
        println!(
            "  id={id} top={top:.1} bottom={bottom:.1} fromViewportBottom={:.1} role={role} name={}",
            bottom - viewport_bottom,
            name.chars().take(100).collect::<String>()
        );
    }
    Ok(())
}

/// Every box that sticks out of the box that contains it, worst first.
///
/// The reason this exists: "text spills past its container" is a claim about
/// one node's relationship to another node, and a three-element fixture cannot
/// express it. Two candidate mechanisms were built as fixtures and both were
/// refuted, which proved only that those two fixtures were wrong. The running
/// document already knows the answer; nothing asked it.
///
/// Horizontal by default, because vertical overflow is how a scroll container
/// works and would bury the signal. `spill v` includes the vertical axis.
pub(crate) async fn spill(client: &mut Client, axis: &str, tolerance: f64) -> Result<()> {
    let (snapshot, elapsed) = inspect(client).await?;
    let boxes: HashMap<u64, [f64; 4]> = snapshot
        .nodes
        .iter()
        .filter_map(|node| node.bounds.map(|bounds| (node.id, bounds)))
        .collect();

    // A spilling node is almost always an unnamed generic, so the row is
    // unreadable without saying what it sits inside. Walk up to the nearest
    // ancestor that carries a name.
    let by_id: HashMap<u64, &SemanticNode> =
        snapshot.nodes.iter().map(|node| (node.id, node)).collect();
    let describe = |mut id: u64| -> String {
        for _ in 0..12 {
            let Some(node) = by_id.get(&id) else { break };
            if !node.name.is_empty() {
                return format!(
                    "in {} \"{}\"",
                    node.role,
                    node.name.chars().take(60).collect::<String>()
                );
            }
            let Some(parent) = node.parent else { break };
            id = parent;
        }
        String::from("(no named ancestor)")
    };

    /*
     * A scroller's own offset, so scrolled-away content is not called a spill.
     *
     * Without this the tab strip reported eight children up to 1,375px left of
     * their parent, which reads as a serious layout break and is nothing at
     * all: `scroll=1186.0` against `content=2068.1` and `client=855.2` means
     * they are scrolled out of view exactly as intended. A tool that reports
     * normal scrolling as breakage costs an afternoon per reader.
     */
    let scroll_of: HashMap<u64, (f64, f64)> = {
        // A second call: the semantic snapshot carries geometry but not scroll
        // state, which only the layout snapshot has.
        let answer = client
            .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
                include_dom: false,
                include_layout: true,
                include_computed_style: false,
                node_ids: Vec::new(),
            }))
            .await?;
        match answer.response {
            DebugResponse::Snapshot(layout) => layout
                .layout
                .unwrap_or_default()
                .into_iter()
                // The *range*, not the offset. A container that can scroll on
                // an axis is one whose content is meant to exceed its box on
                // that axis, whether or not it is currently scrolled.
                .map(|row| {
                    (
                        row.node_id,
                        (row.scroll_range.width, row.scroll_range.height),
                    )
                })
                .collect(),
            // Without offsets this reports scrolled content as spill, which is
            // wrong but not silently wrong: say so rather than pretend.
            _ => {
                eprintln!("no layout snapshot: scrolled content may read as spill");
                HashMap::new()
            }
        }
    };

    let vertical = axis.starts_with('v') || axis.starts_with('a');
    let mut by_owner: HashMap<String, (usize, f64)> = HashMap::new();
    let mut rows: Vec<(f64, String)> = Vec::new();
    for node in &snapshot.nodes {
        let (Some(child), Some(parent_id)) = (node.bounds, node.parent) else {
            continue;
        };
        let Some(parent) = boxes.get(&parent_id) else {
            continue;
        };
        // A container that scrolls on this axis is one whose content is
        // *supposed* to exceed its box on that axis, so nothing inside it can
        // have escaped. Judging it anyway reported the tab strip's eight
        // offscreen tabs as a 1,375px layout break.
        let (range_x, range_y) = scroll_of.get(&parent_id).copied().unwrap_or((0.0, 0.0));
        // A zero-sized parent is a node that has not been laid out, not a
        // container something escaped from.
        if parent[2] <= 0.0 || parent[3] <= 0.0 || child[2] <= 0.0 {
            continue;
        }
        let scrolls_x = range_x > 0.5;
        let scrolls_y = range_y > 0.5;
        let mut worst = f64::NEG_INFINITY;
        let mut how = "right";
        if !scrolls_x {
            let left = parent[0] - child[0];
            let right = (child[0] + child[2]) - (parent[0] + parent[2]);
            worst = left.max(right);
            how = if right >= left { "right" } else { "left" };
        }
        if vertical && !scrolls_y {
            let top = parent[1] - child[1];
            let bottom = (child[1] + child[3]) - (parent[1] + parent[3]);
            if top > worst {
                worst = top;
                how = "top";
            }
            if bottom > worst {
                worst = bottom;
                how = "bottom";
            }
        }
        if worst <= tolerance {
            continue;
        }
        let owner = describe(parent_id);
        let entry = by_owner.entry(owner).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 = entry.1.max(worst);
        rows.push((
            worst,
            format!(
                "{:>8.1}px {how:<6} {:<11} child[{:.0},{:.0} {:.0}x{:.0}] parent[{:.0},{:.0} {:.0}x{:.0}]  {} {}",
                worst,
                node.role,
                child[0], child[1], child[2], child[3],
                parent[0], parent[1], parent[2], parent[3],
                node.name.chars().take(40).collect::<String>(),
                describe(parent_id),
            ),
        ));
    }
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "{} nodes inspected in {elapsed:.1}ms, {} axis, tolerance {tolerance}px",
        snapshot.nodes.len(),
        if vertical { "both" } else { "horizontal" }
    );
    if rows.is_empty() {
        println!("nothing sticks out of its container");
    }
    for (_, row) in rows.iter().take(40) {
        println!("{row}");
    }
    if rows.len() > 40 {
        println!("... and {} more", rows.len() - 40);
    }
    // Parent-relative overflow is blind to the case where a box and every
    // ancestor up to the pane are all too wide together: each one fits inside
    // the next and nothing reports. So also measure everything against the
    // transcript itself, which is the edge a person can see.
    if let Some((pane, pane_box)) = snapshot
        .nodes
        .iter()
        .filter(|node| {
            reach::profile()
                .transcript_region
                .as_deref()
                .is_some_and(|r| node.name.contains(r))
        })
        .filter_map(|node| node.bounds.map(|bounds| (node, bounds)))
        .max_by(|a, b| {
            (a.1[2] * a.1[3])
                .partial_cmp(&(b.1[2] * b.1[3]))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    {
        let right = pane_box[0] + pane_box[2];
        let mut out: Vec<(f64, u64, String)> = snapshot
            .nodes
            .iter()
            .filter_map(|node| node.bounds.map(|b| (node, b)))
            // Descendants of the pane only. The project panel sits to the
            // right of the transcript and every row in it is "past" the
            // transcript's edge without overflowing anything.
            .filter(|(node, _)| {
                let mut id = node.parent;
                for _ in 0..64 {
                    match id {
                        Some(current) if current == pane.id => return true,
                        Some(current) => id = by_id.get(&current).and_then(|n| n.parent),
                        None => return false,
                    }
                }
                false
            })
            .filter_map(|(node, b)| {
                let over = (b[0] + b[2]) - right;
                (over > 0.5 && b[2] > 0.0 && b[3] > 0.0).then(|| {
                    (
                        over,
                        node.id,
                        format!("{} {}", node.role, describe(node.id)),
                    )
                })
            })
            .collect();
        out.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Is the transcript actually sitting on its tail? The complaint
        // "dialogs do not push the chat up" is this number: the gap between the
        // bottom of the last thing in the pane and the bottom of the pane. A
        // pinned transcript ends flush; anything else means the newest message
        // is cut off under the chrome below.
        let pane_bottom = pane_box[1] + pane_box[3];
        let deepest = snapshot
            .nodes
            .iter()
            .filter_map(|node| node.bounds.map(|b| (node, b)))
            .filter(|(node, b)| node.id != pane.id && b[2] > 0.0 && b[3] > 0.0)
            .filter(|(node, _)| {
                let mut id = node.parent;
                for _ in 0..64 {
                    match id {
                        Some(current) if current == pane.id => return true,
                        Some(current) => id = by_id.get(&current).and_then(|n| n.parent),
                        None => return false,
                    }
                }
                false
            })
            .map(|(_, b)| b[1] + b[3])
            .fold(f64::NEG_INFINITY, f64::max);
        if deepest.is_finite() {
            println!(
                "tail: last content ends at {deepest:.1}, pane ends at {pane_bottom:.1}, gap {:.1}",
                pane_bottom - deepest
            );
        }
        println!(
            "\ntranscript pane [{:.0},{:.0} {:.0}x{:.0}], right edge {right:.0}",
            pane_box[0], pane_box[1], pane_box[2], pane_box[3]
        );
        if out.is_empty() {
            println!("  nothing reaches past it");
        }
        for (over, id, what) in out.iter().take(15) {
            println!("  {over:>8.1}px past  {id:>12}  {what}");
        }
        // The chain, for the worst few. A box in the wrong place is explained by
        // whichever ancestor it was placed against, and that ancestor is never
        // the one in the row above.
        for (_, id, _) in out.iter().take(3) {
            println!("  chain for {id}:");
            let mut current = Some(*id);
            for _ in 0..16 {
                let Some(node) = current.and_then(|id| by_id.get(&id)) else {
                    break;
                };
                let b = node.bounds.unwrap_or([f64::NAN; 4]);
                println!(
                    "    {:>12} {:<12} [{:>7.1},{:>7.1} {:>7.1}x{:>6.1}]  {}",
                    node.id,
                    node.role,
                    b[0],
                    b[1],
                    b[2],
                    b[3],
                    node.name.chars().take(40).collect::<String>()
                );
                if node.id == pane.id {
                    break;
                }
                current = node.parent;
            }
        }
    }

    // The per-row list is dominated by whichever container repeats most, so
    // the grouping is what says where to look. A `truncate` row overflows by
    // design and clips in paint; a container that appears here once, deep in
    // the transcript, does not.
    if !by_owner.is_empty() {
        let mut owners: Vec<(String, (usize, f64))> = by_owner.into_iter().collect();
        owners.sort_by(|a, b| {
            b.1.1
                .partial_cmp(&a.1.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        println!("\nby container, worst first:");
        for (owner, (count, worst)) in owners {
            println!("  {count:>4} nodes  worst {worst:>7.1}px  {owner}");
        }
    }
    Ok(())
}

/// Nodes matching `want`, each with its attributes and its ancestor chain.
///
/// `spill` says a box sticks out; it cannot say whether that is a scroller
/// doing its job or a control escaping a clip. The difference is in the
/// attributes of the ancestors — which one carries the overflow and the
/// isolation — and the semantic snapshot already reports every attribute of a
/// generic node in `value`. So this needs no new server surface: the state was
/// already on the wire and nothing printed it.
pub(crate) async fn dom(client: &mut Client, want: &str, depth: usize) -> Result<()> {
    if want.is_empty() {
        bail!("dom needs a substring to match");
    }
    let (snapshot, elapsed) = inspect(client).await?;
    let by_id: HashMap<u64, &SemanticNode> =
        snapshot.nodes.iter().map(|node| (node.id, node)).collect();

    let describe = |node: &SemanticNode| -> String {
        let bounds = node
            .bounds
            .map(|b| format!("[{:.0},{:.0} {:.0}x{:.0}]", b[0], b[1], b[2], b[3]))
            .unwrap_or_else(|| "[no box]".into());
        // The library part this element is, then its value. The second line
        // used to be labelled `attrs` and carry only the value, which reads as
        // "this element has no attributes" for every node in the tree.
        let slot = node.slot.as_deref().unwrap_or("-");
        let value = node.value.as_deref().unwrap_or("-");
        let name = format!("{:?}", node.name);
        format!(
            "{} {:<10} {name:<28} {bounds}{}\n      slot={slot} value={value}",
            node.id,
            node.role,
            if node.visible { "" } else { "  HIDDEN" },
        )
    };

    let matched: Vec<&SemanticNode> = snapshot
        .nodes
        .iter()
        .filter(|node| {
            node.name.contains(want)
                || node.role.contains(want)
                || node.value.as_deref().is_some_and(|v| v.contains(want))
        })
        .collect();

    println!(
        "{} of {} nodes match {want:?} (inspect {elapsed:.1}ms)\n",
        matched.len(),
        snapshot.nodes.len()
    );
    for node in &matched {
        println!("{}", describe(node));
        let mut parent = node.parent;
        for level in 0..depth {
            let Some(current) = parent.and_then(|id| by_id.get(&id)) else {
                break;
            };
            println!(
                "  {}^{} {}",
                "  ".repeat(level),
                level + 1,
                describe(current)
            );
            parent = current.parent;
        }
        println!();
    }
    Ok(())
}

pub(crate) async fn nodes(client: &mut Client) -> Result<usize> {
    let (snapshot, elapsed) = inspect(client).await?;
    report::show_nodes(&snapshot.nodes, elapsed);
    Ok(snapshot.nodes.len())
}

/// What each retained pane costs in nodes.
///
/// `RETAINED_PROJECT_LIMIT` keeps eight project panes mounted, and a hidden
/// pane is a full DOM subtree: it is invisible, not absent. Nobody had priced
/// one, so this walks every node to its nearest `data-retained-*` ancestor and
/// totals the subtree. The visible pane is the one a person is looking at;
/// every other line is what retention is charging for.
pub(crate) async fn panes(client: &mut Client) -> Result<()> {
    let (snapshot, elapsed) = inspect(client).await?;
    let by_id: HashMap<u64, &SemanticNode> =
        snapshot.nodes.iter().map(|node| (node.id, node)).collect();

    // The semantic snapshot carries no `data-*` attributes, so a pane cannot be
    // named by the attribute the shell stamps on it. It can still be found by
    // shape: a pane is a subtree hanging off a shared shell ancestor, and each
    // one contains exactly one of the region the profile names as its
    // transcript. Anchoring on that names panes without needing new server
    // surface.
    // The application names its own; a profile without one gets no pane report.
    let anchor = reach::profile()
        .transcript_region
        .clone()
        .unwrap_or_default();
    let anchor: &str = &anchor;

    let depth_of = |start: u64| -> usize {
        let mut cursor = Some(start);
        let mut depth = 0usize;
        for _ in 0..256 {
            let Some(current) = cursor.and_then(|id| by_id.get(&id)) else {
                break;
            };
            let Some(parent) = current.parent else { break };
            cursor = Some(parent);
            depth += 1;
        }
        depth
    };

    // Pane roots are the anchors' common-depth ancestors. Walk each anchor up a
    // fixed number of levels to the subtree the shell swaps, then total by root.
    let anchors: Vec<&SemanticNode> = snapshot
        .nodes
        .iter()
        .filter(|node| node.name.contains(anchor))
        .collect();

    let mut roots: HashMap<u64, (bool, Option<[f64; 4]>)> = HashMap::new();
    for anchor in &anchors {
        let mut cursor = anchor.parent;
        let mut root = anchor.id;
        // Climb to the highest ancestor that is not shared by every pane: the
        // shell container has many anchor descendants, a pane root has one.
        for _ in 0..12 {
            let Some(current) = cursor.and_then(|id| by_id.get(&id)) else {
                break;
            };
            let descendants = anchors
                .iter()
                .filter(|other| {
                    let mut walk = Some(other.id);
                    for _ in 0..256 {
                        let Some(step) = walk.and_then(|id| by_id.get(&id)) else {
                            return false;
                        };
                        if step.id == current.id {
                            return true;
                        }
                        walk = step.parent;
                    }
                    false
                })
                .count();
            if descendants > 1 {
                break;
            }
            root = current.id;
            cursor = current.parent;
        }
        let node = by_id.get(&root).copied();
        roots.insert(
            root,
            (
                node.map(|n| n.visible).unwrap_or(false),
                node.and_then(|n| n.bounds),
            ),
        );
    }

    let mut totals: HashMap<u64, usize> = HashMap::new();
    let mut unattributed = 0usize;
    for node in &snapshot.nodes {
        let mut cursor = Some(node.id);
        let mut found = None;
        for _ in 0..256 {
            let Some(current) = cursor.and_then(|id| by_id.get(&id)) else {
                break;
            };
            if roots.contains_key(&current.id) {
                found = Some(current.id);
                break;
            }
            cursor = current.parent;
        }
        match found {
            Some(root) => *totals.entry(root).or_default() += 1,
            None => unattributed += 1,
        }
    }

    let mut rows: Vec<(u64, usize)> = totals.into_iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1));
    println!(
        "{} nodes total, {} panes found via {anchor:?} (inspect {elapsed:.1}ms)\n",
        snapshot.nodes.len(),
        rows.len()
    );
    let mut hidden_cost = 0usize;
    for (root, count) in &rows {
        let (visible, bounds) = roots.get(root).copied().unwrap_or((false, None));
        let box_text = bounds
            .map(|b| format!("[{:.0},{:.0} {:.0}x{:.0}]", b[0], b[1], b[2], b[3]))
            .unwrap_or_else(|| "[no box]".into());
        println!(
            "  {count:>6}  node {root:<14} {:<8} depth {:<3} {box_text}",
            if visible { "VISIBLE" } else { "hidden" },
            depth_of(*root)
        );
        if !visible {
            hidden_cost += count;
        }
    }
    println!("\n  {unattributed:>6}  outside any pane (chrome, tab strip, overlays)");
    println!(
        "  {hidden_cost:>6}  in hidden panes = {:.0}% of the tree",
        100.0 * hidden_cost as f64 / snapshot.nodes.len().max(1) as f64
    );
    Ok(())
}
