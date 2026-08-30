//! Getting a control on screen before judging it.
//!
//! # Why this exists
//!
//! A sweep that plans from one snapshot of whatever surface the application
//! happened to open on, and drops any button whose box is `0x0`, measures a
//! fraction of the window and says nothing about the rest. Against a real
//! application that is not a detail: in the run that motivated this module,
//! 286 buttons were in the tree, 64 had a box, and 222 were quietly discarded.
//! None of the 222 were hidden. They were the per-row controls, on a surface
//! the sweep never visited.
//!
//! A skip is indistinguishable from a pass in the output, so a run that touched
//! a fifth of the window reported "every button acted". That is the whole bug.
//! Coverage was one screen deep and the report did not say so.
//!
//! # What this does instead
//!
//! Reaching is a first-class step with its own verdict. Before a control is
//! judged it is *brought into view*: its surface is opened, its section is
//! expanded, its row is hovered, and it is scrolled to. Only then is it clicked.
//! If none of that gives it a box it is counted as *unreachable* and printed,
//! rather than dropped, because a control the harness cannot reach is either a
//! real defect or a gap in this file, and both need to be visible.
//!
//! The rule the whole module turns on: **never silently skip.** Every button in
//! the tree ends in exactly one bucket, and the buckets are printed.

use std::collections::HashMap;

use blitz_control_protocol::SemanticNode;

/// A surface the sweep must visit, named by the control that opens it.
///
/// Defined by the application in its profile; this alias keeps the call sites
/// reading as they did when the list was hardcoded here.
pub use crate::app::SurfaceSpec as Surface;

/// Every top-level surface, in the order they are swept.
///
/// Read from the application's profile. The order is the application's to
/// choose and it matters: a surface that owns destructive row controls belongs
/// last, or visiting it first deletes the rows the other surfaces are reached
/// through. A harness cannot know which surface that is.
///
/// Every surface names an opener, including the one the app happens to launch
/// on. An empty opener meant "wherever we already are", which held only until
/// the first control that changed pane: the run pressed one button, navigated,
/// and counted the remaining 169 as vanished.
pub fn surfaces() -> &'static [crate::app::SurfaceSpec] {
    &profile().surfaces
}

/// Stands in for "the first document tab in the strip", resolved when the sweep
/// runs because a document's own name is user data and varies per profile.
pub const DYNAMIC_DOCUMENT: &str = "\u{0}dynamic-document";

/// A control that opens a document, either its tab or its row in a list.
///
/// Prefers a tab already in the strip, because activating one is a pane switch
/// rather than a load. A fresh profile may have no document tabs open at all,
/// so the fallback is a row in the root surface's list.
///
/// A row is recognised by the summary the list renders beside its name, which
/// the application states as `document_row_markers`. Matching the document's own
/// name is not possible: it is user data and differs per profile.
pub fn document_opener(nodes: &[SemanticNode]) -> Option<String> {
    let closes: Vec<String> = nodes
        .iter()
        .filter(|n| n.role == "button" && onscreen(n))
        .filter_map(|n| {
            profile()
                .close_prefixes
                .iter()
                .find_map(|prefix| n.name.strip_prefix(prefix.as_str()))
                .map(str::to_owned)
        })
        .collect();
    /*
     * A document tab, which is any doubled label that is not a permanent
     * surface.
     *
     * This used to filter on `!navigates(name)`, which was correct until
     * `navigates` was taught that document tabs are navigation - after that it
     * excluded every candidate and the project surface could never be opened.
     * The two need different questions: `navigates` asks "does pressing this
     * leave the surface I am sweeping", and this asks "is this the way in".
     */
    let tab = nodes
        .iter()
        .filter(|n| n.role == "button" && onscreen(n))
        .filter(|n| !profile().is_permanent(&n.name))
        .find(|n| {
            doubled(&n.name).is_some_and(|label| !profile().is_permanent(label))
                || closes
                    .iter()
                    .any(|subject| n.name == format!("{subject}{subject}"))
        });
    if let Some(tab) = tab {
        return Some(tab.name.clone());
    }
    /*
     * Otherwise a row in the root surface's list, which is what a person
     * clicks to open one. A fresh profile may have no document tabs in the
     * strip at all, so without this the surface is unreachable on exactly the
     * runs that matter.
     *
     * Preferring a document that has something in it: an empty one renders its
     * panes as a row of empty headers with every per-item control absent, so
     * the pane a person cares about most would be on screen with nothing to
     * press.
     */
    let rows = || {
        nodes
            .iter()
            .filter(|n| n.role == "button" && onscreen(n))
            .filter(|n| {
                let skip = |prefixes: &Vec<String>| {
                    prefixes.iter().any(|p| n.name.starts_with(p.as_str()))
                };
                !skip(&profile().close_prefixes) && !skip(&profile().row_action_prefixes)
            })
    };
    /*
     * Preferring a row whose summary carries a non-zero count.
     *
     * The first marker is treated as the one a count precedes, because a row
     * with something in it opens a populated pane and an empty one opens four
     * empty headers. If no marker parses a count this falls through to plain
     * marker matching, which is the honest behaviour for an application whose
     * rows carry no counts at all.
     */
    let markers = &profile().document_row_markers;
    let populated = markers.first().and_then(|first| {
        rows().find(|n| {
            n.name
                .split(first.as_str())
                .next()
                .and_then(|head| head.rsplit(')').next())
                .and_then(|count| count.trim().rsplit(' ').next())
                .and_then(|count| count.parse::<u32>().ok())
                .is_some_and(|open| open > 0)
        })
    });
    if let Some(row) = populated {
        return Some(row.name.clone());
    }
    rows()
        .find(|n| {
            markers
                .iter()
                .any(|marker| n.name.contains(marker.as_str()))
        })
        .map(|n| n.name.clone())
}

/// Whether a node is on screen well enough to click.
///
/// Both dimensions, because a control laid out at zero width is one no pointer
/// can land on even though the tree lists a box for it.
pub fn onscreen(node: &SemanticNode) -> bool {
    node.visible && node.bounds.is_some_and(|b| b[2] > 0.0 && b[3] > 0.0)
}

/// Semantic nodes to reveal from the outer container down to the target.
///
/// Calling `scrollIntoView` on only a deeply nested target can exhaust its
/// local scroller while the containing panel remains below the viewport. The
/// outer-to-inner chain exposes each nesting boundary without coordinates.
pub fn reveal_chain(nodes: &[SemanticNode], target: u64) -> Vec<u64> {
    let by_id: HashMap<u64, &SemanticNode> = nodes.iter().map(|node| (node.id, node)).collect();
    let mut inner_to_outer = Vec::new();
    let mut cursor = Some(target);
    for _ in 0..32 {
        let Some(id) = cursor else { break };
        let Some(node) = by_id.get(&id) else { break };
        if node.role == "main" {
            break;
        }
        inner_to_outer.push(id);
        cursor = node.parent;
    }
    inner_to_outer.reverse();
    inner_to_outer
}

/// Whether a semantic node is an interactive component a person can operate.
///
/// This intentionally keys off roles rather than tags. Applications may build
/// a switch or menu item from a generic element with an explicit ARIA role;
/// omitting those would make a component audit silently button-only.
pub fn interactive(node: &SemanticNode) -> bool {
    (node.role == "option" && node.visible)
        || matches!(
            node.role.as_str(),
            "button"
                | "checkbox"
                | "combobox"
                | "link"
                | "menuitem"
                | "menuitemcheckbox"
                | "menuitemradio"
                | "radio"
                | "slider"
                | "spinbutton"
                | "switch"
                | "tab"
                | "textbox"
                | "treeitem"
        )
}

/// Whether pressing this leaves the surface, invalidating the rest of the plan.
///
/// A sweep that presses a navigation control first loses the rest of its plan:
/// every remaining control belongs to the surface it just
/// navigated away from. A control that changes surface has to be swept last, or
/// it takes the plan with it.
///
/// Matched on the tab-strip and nav entries by name. Deliberately a small,
/// explicit list rather than a guess about which names look like navigation:
/// over-matching here silently drops controls from the sweep, which is the
/// failure this whole module exists to end.
pub fn navigates(name: &str) -> bool {
    // The application's own permanent tabs. `is_permanent` accepts the doubled
    // doubled form some strips render, which an exact match would miss and a
    // substring match would over-match into a close action.
    if profile().is_permanent(name) {
        return true;
    }
    if profile()
        .navigation_controls
        .iter()
        .any(|control| name.eq_ignore_ascii_case(control))
    {
        return true;
    }
    /*
     * A tab in the strip, whose label the strip doubles.
     *
     * Clicking a document tab switches panes, and the root surface's remaining controls go to
     * `visible=false` while staying in the retained DOM. They were reported as
     * vanished when the sweep had simply walked off the surface.
     *
     * A project tab is swept as the opener of the project surface, so skipping
     * it here loses no coverage.
     */
    doubled(name).is_some()
}

/// Whether this button opens the document represented by its surrounding row.
///
/// User-owned document names cannot live in an application profile. A row
/// action can still identify them without guessing: if `Rename Report` is a
/// configured row-action form, the sibling button named exactly `Report` is
/// the row opener. The action itself is never classified as navigation.
pub fn opens_document_row(nodes: &[SemanticNode], id: u64) -> bool {
    opens_document_row_for(profile(), nodes, id)
}

fn opens_document_row_for(
    profile: &crate::app::AppProfile,
    nodes: &[SemanticNode],
    id: u64,
) -> bool {
    let Some(candidate) = nodes
        .iter()
        .find(|node| node.id == id && node.role == "button" && onscreen(node))
    else {
        return false;
    };
    profile.row_action_prefixes.iter().any(|prefix| {
        nodes.iter().any(|action| {
            action.id != candidate.id
                && action.role == "button"
                && onscreen(action)
                && action
                    .name
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|subject| subject == candidate.name)
        })
    })
}

/// The single label behind a doubled tab-strip name, if it is one.
///
/// `"ee"` -> `"e"`, `"DashboardDashboard"` -> `"Dashboard"`. An odd length or a mismatched
/// half is not a tab.
fn doubled(name: &str) -> Option<&str> {
    let characters = name.chars().count();
    if characters == 0 || !characters.is_multiple_of(2) {
        return None;
    }
    let midpoint = name.char_indices().nth(characters / 2)?.0;
    let (left, right) = name.split_at(midpoint);
    (left == right && !left.trim().is_empty()).then_some(left)
}

/// Whether the window is still showing the surface a plan was made against.
///
/// Each surface is recognised by a control only it renders. That is enough to
/// answer the one question the sweep needs - "did the last click take us
/// somewhere else" - without a route or a title to read, neither of which the
/// semantic tree exposes.
pub fn on_surface(nodes: &[SemanticNode], surface: &Surface) -> bool {
    on_surface_for_profile(nodes, surface, profile())
}

fn on_surface_for_profile(
    nodes: &[SemanticNode],
    surface: &Surface,
    profile: &crate::app::AppProfile,
) -> bool {
    if profile.is_permanent(&surface.opener) {
        let doubled = format!("{}{}", surface.opener, surface.opener);
        let selected = nodes.iter().any(|node| {
            node.role.eq_ignore_ascii_case("button")
                && node.selected
                && onscreen(node)
                && (node.name.eq_ignore_ascii_case(&surface.opener)
                    || node.name.eq_ignore_ascii_case(&doubled))
        });
        if !selected {
            return false;
        }
    }
    let Some(marker) = surface.marker.as_deref() else {
        return true;
    };
    nodes.iter().any(|n| onscreen(n) && n.name.contains(marker))
}

/// The controls that belong to the surface in front, by ancestry.
///
/// # Why not position, and not visibility
///
/// Both were tried against a running app and both are wrong. A retained root
/// sits behind an open document pane and its rows keep real boxes in the same
/// horizontal band as panel controls, so a coordinate cut cannot separate them. Worse, the
/// retained rows still report `visible` with a non-zero box, so filtering on
/// visibility keeps every one of them too.
///
/// The consequence is not a small error: one surface's row controls can be swept
/// as though they belonged to another, crowding the actual controls out of the
/// plan and reporting coverage for the wrong surface.
///
/// Ancestry is the one thing that does separate them: a pane is a subtree, and
/// the marker control that identifies a surface lives inside it. Walking up
/// from the marker to the pane root and then taking that root's descendants
/// gives exactly the controls a person is looking at.
pub fn on_surface_subtree(nodes: &[SemanticNode], surface: &Surface) -> Vec<u64> {
    let Some(marker) = surface.marker.as_deref() else {
        return nodes.iter().map(|n| n.id).collect();
    };
    let by_id: HashMap<u64, &SemanticNode> = nodes.iter().map(|n| (n.id, n)).collect();
    let Some(anchor) = nodes
        .iter()
        .find(|n| onscreen(n) && n.name.contains(marker))
    else {
        return Vec::new();
    };

    /*
     * Up a fixed number of levels, not to the document root.
     *
     * Walking all the way up lands on the window, whose subtree is every
     * surface at once - which is the situation this exists to end. Eight is
     * deep enough to clear a control's own chrome and reach the pane, and
     * shallow enough not to swallow its neighbour; it is the same depth
     * an in-place editor's notes use for "an input that is merely hidden still
     * walks eight levels to the window root".
     */
    /*
     * The shallowest ancestor that holds most of what is on screen.
     *
     * A fixed climb cannot work for every surface: the same depth can land above
     * one list and correctly identify another pane. So the depth is chosen by
     * measurement - climb one level at a time and keep the first ancestor whose
     * subtree covers a majority of the on-screen controls. That is the pane,
     * whichever surface it belongs to, and it stops before the window root,
     * whose subtree is every surface at once.
     */
    let onscreen_total = nodes
        .iter()
        .filter(|n| n.role == "button" && onscreen(n))
        .count();
    /*
     * Descended from the root, not climbed from every node.
     *
     * The per-node climb needed a hop limit to stay bounded, and any limit is
     * wrong: this tree runs to 8317 nodes and a project row sits deeper than
     * thirty-two ancestors, so the cap silently dropped exactly the controls
     * the sweep exists to press and the surface reports zero buttons. Walking down
     * from the root visits each node once and has no depth to guess at.
     */
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node.parent {
            children.entry(parent).or_default().push(node.id);
        }
    }
    let subtree_of = |root: u64| -> Vec<u64> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Some(kids) = children.get(&id) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    };

    /*
     * Prefer the semantic pane boundary: the child subtree directly beneath
     * `main`. Retained application panes are siblings there, so this includes
     * the complete active surface without swallowing Home, Settings, and
     * documents together. It also works when the active pane owns every
     * currently visible button, a case where the coverage heuristic below
     * mistakes the pane itself for the shared window root.
     */
    let mut cursor = anchor.id;
    for _ in 0..nodes.len() {
        let Some(parent) = by_id.get(&cursor).and_then(|node| node.parent) else {
            break;
        };
        if by_id.get(&parent).is_some_and(|node| node.role == "main") {
            return subtree_of(cursor);
        }
        cursor = parent;
    }

    /*
     * The ancestor holding the most on-screen controls, chosen over the whole
     * climb rather than at the first one to clear a threshold.
     *
     * A majority test looks reasonable and fails exactly when it matters: after
     * a full run the window holds several retained panes, no single ancestor
     * reaches half the on-screen buttons, the loop exhausts, and it returns
     * whatever the last ancestor happened to be. A surface can report zero
     * buttons that way while finding many when swept alone, a coverage hole that only
     * appeared in the run that was supposed to cover everything.
     *
     * Taking the maximum has no threshold to be wrong about. The climb stops
     * short of the document root, whose subtree is every surface at once.
     */
    let mut cursor = anchor.id;
    let mut best: Vec<u64> = Vec::new();
    let mut best_covered = 0usize;
    // A component tree can legitimately put the marker dozens of wrappers
    // below its pane. Bound the climb by the tree itself, not by a guessed
    // framework depth; parent links strictly move upward and the root break
    // below prevents the shared window subtree from being selected.
    for _ in 0..nodes.len() {
        let Some(parent) = by_id.get(&cursor).and_then(|n| n.parent) else {
            break;
        };
        cursor = parent;
        let kept = subtree_of(cursor);
        let covered = kept
            .iter()
            .filter(|id| {
                by_id
                    .get(id)
                    .is_some_and(|n| n.role == "button" && onscreen(n))
            })
            .count();
        // Everything on screen means this is the root, not a pane.
        if onscreen_total > 0 && covered >= onscreen_total {
            break;
        }
        if covered > best_covered {
            best_covered = covered;
            best = kept;
        }
    }
    best
}

/// Whether the application reserves this control for its manual release pass.
///
/// The profile owns the exact prefixes and records why each one is manual. The
/// common cases are a native chooser the semantic tree cannot dismiss, an
/// external destination, a process-ending action that would prevent cleanup,
/// or an authenticated paid-provider action CI cannot honestly exercise.
/// Ordinary in-app dialogs are not exempt: the sweep opens and closes them.
/// Every exception is counted and printed, so manual never means silently
/// treated as passing.
pub fn requires_manual_release_check(name: &str) -> bool {
    profile()
        .manual_controls
        .iter()
        .any(|exception| name.starts_with(exception.label.as_str()))
}

/// Whether the window is showing a modal that has to be dismissed to continue.
///
/// A modal is the one thing a sweep cannot treat as ordinary: every control
/// behind it is unreachable until it closes, so a dialog that will not dismiss
/// does not fail one button, it ends the run. The check is therefore not "did
/// this button act" but "can I still get out of here".
pub fn modal_open(nodes: &[SemanticNode]) -> bool {
    nodes.iter().any(|node| {
        onscreen(node)
            && (matches!(node.role.as_str(), "dialog" | "alertdialog")
                || (node.role == "button" && profile().dismisses_dialog(&node.name)))
    })
}

/// The controls that would dismiss the modal in front, best first.
///
/// Preference order comes from the application profile. A dialog often offers
/// both a footer action and a header icon, and the application knows which one
/// gives the clearest outcome evidence.
pub fn dismissers(nodes: &[SemanticNode]) -> Vec<(u64, String)> {
    let mut found: Vec<(u64, String)> = nodes
        .iter()
        .filter(|n| n.role == "button" && onscreen(n))
        .filter(|n| profile().dismisses_dialog(&n.name))
        .map(|n| (n.id, n.name.clone()))
        .collect();
    found.sort_by_key(|(_, name)| {
        profile()
            .dismiss_controls
            .iter()
            .position(|control| name.eq_ignore_ascii_case(control))
            .unwrap_or(usize::MAX)
    });
    found
}

/// Whether this closes a surface the sweep still has to stand on.
///
/// Closing a navigation tab can retire the surface later routes depend on, so
/// such controls run after the plan that stands on them.
///
/// The tab is still exercised - `Close` on a project tab is swept - but the
/// three permanent surfaces keep theirs.
pub fn closes_a_surface(name: &str) -> bool {
    /*
     * Every `Close` in the strip, not a fixed list of three.
     *
     * A project tab's close was left in the sweep on the grounds that it is an
     * ordinary control, but closing a document tab can fall the window back to
     * the root and retire a pane later surfaces are reached through.
     * The close controls are worth pressing; they are not worth pressing in the
     * middle of a plan that stands on what they remove.
     */
    profile()
        .close_prefixes
        .iter()
        .any(|prefix| name.starts_with(prefix.as_str()))
}

/// Whether the application declares this control semantically inert.
///
/// Prefixes live in the application profile. The harness cannot infer that a
/// refresh talks to a backend or that a toggle changes paint only, and product
/// labels never belong in this crate.
pub fn is_inert_control(name: &str) -> bool {
    profile()
        .inert_controls
        .iter()
        .any(|prefix| name.starts_with(prefix.as_str()))
}

/// Whether this control needs its own disposable application session.
///
/// The names are application data. The harness supplies only the scheduling
/// rule: never press a session-ending or fixture-resetting action in the middle
/// of a shared sweep.
pub fn requires_isolated_outcome(name: &str) -> bool {
    profile()
        .isolated_controls
        .iter()
        .any(|control| name.eq_ignore_ascii_case(control))
}

/// The subtree of the dialog that owns this dismiss control.
///
/// A modal does not remove the surface behind it: that surface stays in the
/// tree, `visible` and sized, the same way a retained pane does. So "everything
/// on screen" is not "everything in the dialog", and sweeping the former made
/// a dialog pass can press retained-surface controls that are not in the dialog
/// at all, including a native-panel opener.
///
/// Found by climbing from the dismiss control until the subtree stops growing
/// quickly, which is the dialog's own container: a modal is a small, self
/// contained box next to a large surface, so the first ancestor holding more
/// than a handful of controls is already too big.
pub fn enclosing_dialog(nodes: &[SemanticNode], dismiss_id: u64) -> Vec<u64> {
    const MAX_PLAUSIBLE_DIALOG_BUTTONS: usize = 12;
    let by_id: HashMap<u64, &SemanticNode> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node.parent {
            children.entry(parent).or_default().push(node.id);
        }
    }
    let subtree_of = |root: u64| -> Vec<u64> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Some(kids) = children.get(&id) {
                stack.extend(kids.iter().copied());
            }
        }
        out
    };

    let mut cursor = dismiss_id;
    let mut best = vec![dismiss_id];
    for _ in 0..8 {
        let Some(parent) = by_id.get(&cursor).and_then(|n| n.parent) else {
            break;
        };
        cursor = parent;
        let kept = subtree_of(cursor);
        let buttons = kept
            .iter()
            .filter(|id| {
                by_id
                    .get(id)
                    .is_some_and(|n| n.role.eq_ignore_ascii_case("button"))
            })
            .count();
        /*
         * A dialog holds a handful of controls. Past that this is the pane
         * behind it, and taking that would sweep the whole surface again from
         * inside the modal.
         */
        if buttons > MAX_PLAUSIBLE_DIALOG_BUTTONS {
            break;
        }
        best = kept;
    }
    best
}

/// Whether this is a panel section header, which folds the rows beneath it.
///
/// Some panel headers are named for their contents and count, such as
/// `Records1` or `Activity22`, rather than with a collapse prefix. A prefix-only
/// rule misses those disclosures and folds their descendant row controls early.
///
/// Deferring configured section headers prevents descendant controls from
/// reading as vanished before the sweep reaches them.
pub fn folds_a_section(name: &str) -> bool {
    /*
     * Read from the application's own profile, falling back to nothing.
     *
     * These are six section names from one product, and a harness has no
     * business knowing them. An application states its own in `ps-qa.ron`; one
     * that ships no profile gets an empty list, which means the sweep presses
     * sections in plan order rather than last. That is a worse sweep, not a
     * wrong one, and it beats hunting for another product's headers.
     *
     * Read once: this is called per button per surface, and a file read per
     * call would dominate the sweep.
     */
    profile().folds_a_section(name)
}

/// The application profile, read once.
///
/// This is called per button per surface, so a file read per call would
/// dominate the sweep.
pub fn profile() -> &'static crate::app::AppProfile {
    static PROFILE: std::sync::OnceLock<crate::app::AppProfile> = std::sync::OnceLock::new();
    // Broad driving commands validate their required profile before this
    // cache is reached. Targeted diagnostics deliberately do not require
    // product navigation rules; an empty profile makes their explicit
    // selector the whole policy instead of panicking inside lookup.
    PROFILE.get_or_init(|| crate::app::AppProfile::load(None).unwrap_or_default())
}

/// The disclosure controls that must be opened before a sweep of this surface.
///
/// Collapsed sections are the second-largest source of unreached controls after
/// the wrong surface: one section alone may hide a row of controls per record.
pub fn expanders(nodes: &[SemanticNode], expand_prefixes: &[String]) -> Vec<(u64, String)> {
    let prefixes: Vec<_> = expand_prefixes
        .iter()
        .map(|prefix| prefix.to_lowercase())
        .collect();
    nodes
        .iter()
        .filter(|node| node.role.eq_ignore_ascii_case("button") && onscreen(node))
        .filter(|node| {
            let name = node.name.to_lowercase();
            prefixes.iter().any(|prefix| name.starts_with(prefix))
        })
        .map(|node| (node.id, node.name.clone()))
        .collect()
}

/// Semantic ids of rows worth hovering.
///
/// Row actions do not exist until `pointerenter`, so a sweep that never moves
/// over their semantic nodes cannot see them at all. Hovering the row rather
/// than the control is the only order that works: the control is not in the
/// tree to be addressed until the row is hovered.
///
/// Only rows whose middle is inside the window are returned. A transcript keeps
/// hundreds of rows at negative coordinates; asking the runtime to hover those
/// nodes reveals nothing while costing a round trip each. Bounds decide which
/// semantic ids are eligible, but never become an input action.
pub fn hover_row_ids(nodes: &[SemanticNode], row_role: &str, window: (f64, f64)) -> Vec<u64> {
    nodes
        .iter()
        .filter(|node| node.role.eq_ignore_ascii_case(row_role) && onscreen(node))
        .filter_map(|node| {
            let b = node.bounds?;
            let (x, y) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
            (y >= window.0 && y <= window.1 && x >= 0.0).then_some(node.id)
        })
        .collect()
}

/// How every button in the tree was accounted for.
///
/// Printed at the end of a run so a coverage regression is visible as a number
/// rather than as silence. `swept + unreachable + hidden` must equal the button
/// count in the tree; if it does not, this file has a hole in it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Coverage {
    pub in_tree: usize,
    pub swept: usize,
    /// Already driven and judged by a named rendered outcome.
    ///
    /// `cover --unmapped-only` still inventories every concrete instance, but
    /// does not destructively replay controls whose component contract already
    /// passed in the ordered outcome suite.
    pub outcome_declared: usize,
    pub unreachable: usize,
    /// Planned, then gone by the time its turn came.
    ///
    /// Not a fault and not a skip: closing one tab removes its neighbours'
    /// close buttons, so a working control legitimately retires others. It gets
    /// its own bucket so it cannot be confused with a control that was never
    /// tried.
    pub vanished: usize,
    /// Leaves the surface, so it is exercised as an opener instead.
    pub navigation: usize,
    /// Hands the screen to a native modal, so it is never pressed unattended.
    pub manual: usize,
    /// Requires a disposable application session and a dedicated outcome check.
    pub isolated: usize,
    /// Left unreachable behind a dialog that would not dismiss.
    ///
    /// Its own bucket because it is neither a pass nor a skip: these controls
    /// were planned, are on the surface, and could not be reached because one
    /// bug upstream of them traps the window. Counting them as anything else
    /// hides the blast radius of that bug.
    pub blocked: usize,
    /// Swept, but not present when `in_tree` was counted.
    ///
    /// A dialog's own controls do not exist until it opens, and a row's actions
    /// do not exist until it is hovered, so both are pressed without ever having
    /// been in the snapshot the total came from. Charged to `swept`, they pushed
    /// the buckets past the total and the consistency check reported a *negative*
    /// `UNACCOUNTED` - a surplus - which read as "more than covered" when the
    /// real meaning was that the denominator was wrong.
    ///
    /// Separate from `swept` so the surplus cannot hide a genuine gap: a run can
    /// now be short of coverage and full of dialogs at the same time and still
    /// report both.
    pub revealed: usize,
}

impl Coverage {
    /// Every button ended in a bucket.
    pub fn accounted(&self) -> bool {
        self.bucketed() == self.total()
    }

    /// Everything a run was responsible for: the surface's own buttons plus the
    /// ones that only came into existence once it started pressing things.
    pub fn total(&self) -> usize {
        self.in_tree + self.revealed
    }

    fn bucketed(&self) -> usize {
        self.swept
            + self.outcome_declared
            + self.unreachable
            + self.vanished
            + self.navigation
            + self.manual
            + self.isolated
            + self.blocked
            + self.revealed
    }

    pub fn line(&self) -> String {
        format!(
            "{} buttons{}: {} swept, {} outcome-declared, {} unreachable, {} vanished, {} nav, {} manual, {} isolated, {} blocked{}",
            self.total(),
            if self.revealed > 0 {
                format!(" ({} on open, {} revealed)", self.in_tree, self.revealed)
            } else {
                String::new()
            },
            self.swept,
            self.outcome_declared,
            self.unreachable,
            self.vanished,
            self.navigation,
            self.manual,
            self.isolated,
            self.blocked,
            if self.accounted() {
                String::new()
            } else {
                format!(
                    " (UNACCOUNTED {})",
                    self.total() as i64 - self.bucketed() as i64
                )
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, role: &str, name: &str, bounds: Option<[f64; 4]>) -> SemanticNode {
        SemanticNode {
            dom_id: None,
            id,
            parent: None,
            role: role.to_owned(),
            name: name.to_owned(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds,
            slot: None,
        }
    }

    #[test]
    fn a_zero_box_control_is_not_onscreen() {
        // The exact shape of the 222 discarded controls: in the tree, not
        // hidden, no box.
        assert!(!onscreen(&node(1, "button", "Row action", Some([0.0; 4]))));
        assert!(onscreen(&node(
            2,
            "button",
            "Row action",
            Some([10.0, 10.0, 20.0, 20.0])
        )));
    }

    #[test]
    fn a_retained_marker_does_not_make_an_unselected_permanent_surface_current() {
        let surface = Surface {
            name: "settings".into(),
            opener: "Settings".into(),
            marker: Some("Search settings".into()),
            reveal_with: None,
        };
        let profile = crate::app::AppProfile {
            permanent_surfaces: vec!["Settings".into(), "Analytics".into()],
            ..Default::default()
        };
        let mut settings_tab = node(
            1,
            "button",
            "SettingsSettings",
            Some([0.0, 0.0, 100.0, 30.0]),
        );
        let marker = node(
            2,
            "textbox",
            "Search settings",
            Some([0.0, 50.0, 200.0, 30.0]),
        );
        let mut analytics_tab = node(
            3,
            "button",
            "AnalyticsAnalytics",
            Some([100.0, 0.0, 100.0, 30.0]),
        );
        analytics_tab.selected = true;

        assert!(!on_surface_for_profile(
            &[settings_tab.clone(), marker.clone(), analytics_tab],
            &surface,
            &profile,
        ));
        settings_tab.selected = true;
        assert!(on_surface_for_profile(
            &[settings_tab, marker],
            &surface,
            &profile,
        ));
    }

    #[test]
    fn reveal_walks_nested_containers_from_outer_to_inner() {
        let mut root = node(1, "main", "", Some([0.0, 0.0, 200.0, 200.0]));
        root.parent = None;
        let mut panel = node(2, "group", "", Some([0.0, 0.0, 200.0, 400.0]));
        panel.parent = Some(1);
        let mut list = node(3, "list", "", Some([0.0, 0.0, 200.0, 600.0]));
        list.parent = Some(2);
        let mut button = node(4, "button", "More", Some([0.0, 560.0, 80.0, 20.0]));
        button.parent = Some(3);

        assert_eq!(reveal_chain(&[root, panel, list, button], 4), vec![2, 3, 4]);
    }

    #[test]
    fn surface_scope_reaches_a_pane_deeper_than_twelve_wrappers() {
        let mut nodes = vec![node(1, "main", "window", Some([0.0, 0.0, 800.0, 600.0]))];

        let mut pane = node(2, "generic", "", Some([0.0, 0.0, 600.0, 600.0]));
        pane.parent = Some(1);
        nodes.push(pane);

        let mut parent = 2;
        for id in 3..18 {
            let mut wrapper = node(id, "generic", "", Some([0.0, 0.0, 600.0, 600.0]));
            wrapper.parent = Some(parent);
            nodes.push(wrapper);
            parent = id;
        }
        let mut marker = node(
            18,
            "button",
            "Surface marker",
            Some([20.0, 20.0, 100.0, 24.0]),
        );
        marker.parent = Some(parent);
        nodes.push(marker);

        let mut owned = node(
            19,
            "button",
            "Owned action",
            Some([20.0, 60.0, 100.0, 24.0]),
        );
        owned.parent = Some(2);
        nodes.push(owned);

        let mut other_pane = node(20, "generic", "", Some([600.0, 0.0, 200.0, 600.0]));
        other_pane.parent = Some(1);
        nodes.push(other_pane);
        let mut foreign = node(
            21,
            "button",
            "Foreign action",
            Some([620.0, 20.0, 100.0, 24.0]),
        );
        foreign.parent = Some(20);
        nodes.push(foreign);

        let surface = Surface {
            name: "deep".to_owned(),
            opener: "Deep".to_owned(),
            marker: Some("Surface marker".to_owned()),
            reveal_with: None,
        };
        let scope = on_surface_subtree(&nodes, &surface);

        assert!(scope.contains(&18));
        assert!(scope.contains(&19));
        assert!(!scope.contains(&21));
    }

    #[test]
    fn component_inventory_is_not_button_only() {
        for role in [
            "button", "checkbox", "combobox", "link", "menuitem", "radio", "slider", "switch",
            "tab", "textbox", "treeitem",
        ] {
            assert!(interactive(&node(
                1,
                role,
                "Named",
                Some([0.0, 0.0, 20.0, 20.0])
            )));
        }
        assert!(!interactive(&node(
            2,
            "heading",
            "Not interactive",
            Some([0.0, 0.0, 20.0, 20.0])
        )));
        // A visible custom option is directly operable. Closed native
        // selectors retain hidden options, which stay outside inventory.
        assert!(interactive(&node(
            3,
            "option",
            "Choice",
            Some([0.0, 0.0, 20.0, 20.0])
        )));
        let mut hidden_option = node(4, "option", "Retained choice", Some([0.0, 0.0, 20.0, 20.0]));
        hidden_option.visible = false;
        assert!(!interactive(&hidden_option));
    }

    #[test]
    fn only_onscreen_expanders_are_offered() {
        let nodes = vec![
            node(1, "button", "Expand Records", Some([0.0, 0.0, 20.0, 20.0])),
            node(2, "button", "Expand Hidden", Some([0.0; 4])),
            node(
                3,
                "button",
                "Collapse Running",
                Some([0.0, 0.0, 20.0, 20.0]),
            ),
        ];
        let found = expanders(&nodes, &["Expand ".to_owned()]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "Expand Records");
    }

    #[test]
    fn navigation_is_recognised_without_swallowing_its_neighbours() {
        /*
         * The half of `navigates` that needs no profile.
         *
         * A document tab is recognised by its doubled label, which is a fact
         * about how a tab strip renders rather than about any one product's
         * surfaces. The permanent-tab half is the application's to state, and
         * is covered by `a_permanent_surface_is_recognised_doubled` in `app`.
         *
         * Permanent-tab names belong to an application profile, not this test.
         */
        // Document tabs are doubled, and clicking one leaves the surface.
        assert!(navigates("ee"));
        assert!(navigates("delta/east/cobaltdelta/east/cobalt"));
        assert!(navigates("éé"));
        assert!(navigates("設定設定"));
        // A `contains` would catch these, and dropping a Close from the sweep
        // is the silent skip this module exists to end.
        assert!(!navigates("Close Dashboard"));
        assert!(!navigates("Import data"));
        assert!(!navigates("Rename document"));
        // Not every even-length name is a doubled label.
        assert!(!navigates("Send"));
        assert!(!navigates("Copy"));
    }

    #[test]
    fn a_row_opener_is_derived_from_profile_owned_action_prefixes() {
        let profile = crate::app::AppProfile {
            row_action_prefixes: vec!["Rename ".to_owned()],
            ..Default::default()
        };
        let nodes = vec![
            node(1, "button", "Report", Some([0.0, 0.0, 80.0, 20.0])),
            node(2, "button", "Rename Report", Some([90.0, 0.0, 20.0, 20.0])),
        ];
        assert!(opens_document_row_for(&profile, &nodes, 1));
        assert!(!opens_document_row_for(&profile, &nodes, 2));
    }

    #[test]
    fn a_permanent_tab_navigates_when_the_profile_names_it() {
        // The mechanism, without hardcoding a product: whatever the application
        // calls its permanent tabs, both the plain and doubled forms navigate.
        let profile = crate::app::AppProfile {
            permanent_surfaces: vec!["Dashboard".to_owned()],
            ..Default::default()
        };
        assert!(profile.is_permanent("Dashboard"));
        assert!(profile.is_permanent("DashboardDashboard"));
        assert!(!profile.is_permanent("Close Dashboard"));
    }

    #[test]
    fn only_application_documented_controls_are_exempt() {
        /*
         * The exemption is exactly the controls the application named, matched
         * by prefix, and every entry says what it raises.
         *
         * Two failure modes this guards, both of which have happened:
         *
         * - Too broad. An exemption list that grows into a general posture of
         *   not opening things is how a dialog ships with a Cancel that does
         *   nothing: no run ever opened it. In-app modals must still be swept.
         * - Named wrong. An entry carrying the *row's* label rather than the
         *   *control's* matches nothing, and the sweep raises a panel it cannot
         *   dismiss while the list claims it is exempt. That failure is silent:
         *   the report shows the control as swept.
         */
        let profile = crate::app::AppProfile {
            manual_controls: vec![
                crate::app::ManualControl {
                    label: "Import data".to_owned(),
                    command: "open_native_picker".to_owned(),
                },
                crate::app::ManualControl {
                    label: "Open http".to_owned(),
                    command: "openExternal".to_owned(),
                },
            ],
            ..Default::default()
        };
        let exempt = |name: &str| {
            profile
                .manual_controls
                .iter()
                .any(|e| name.starts_with(e.label.as_str()))
        };
        assert!(exempt("Import data"));
        // A prefix, so a label that carries its subject still matches.
        assert!(exempt("Open https://example.invalid/pull/1"));
        // Ordinary controls are still swept.
        assert!(!exempt("Send"));
        assert!(!exempt("Cancel"));
        assert!(!exempt("Create record"));

        // Every exception names what it opens, so the printed worklist is
        // actionable rather than a list of bare labels.
        assert!(
            profile
                .manual_controls
                .iter()
                .all(|e| !e.command.is_empty())
        );

        // An application that names none exempts nothing.
        assert!(crate::app::AppProfile::default().manual_controls.is_empty());
    }

    #[test]
    fn a_modal_is_recognised_and_offers_its_dismissers() {
        let dialog = vec![node(1, "dialog", "Setup", Some([0.0, 0.0, 200.0, 200.0]))];
        assert!(modal_open(&dialog));

        let ordinary = vec![node(1, "button", "Send", Some([0.0, 0.0, 20.0, 20.0]))];
        assert!(!modal_open(&ordinary));
    }

    #[test]
    fn surface_tabs_are_not_closed_out_from_under_the_sweep() {
        /*
         * The prefixes come from the application, so this asserts the matching
         * rule against a made-up vocabulary rather than one product's labels.
         *
         * A tab's close counts even for a document tab, which it did not used
         * to. It is still pressed - the caller defers these to the end of the
         * plan rather than skipping them - but closing any tab retires a pane
         * the rest of the plan stands on. Exempting document tabs is what left
         * the root surface reporting zero buttons in a full run.
         */
        let profile = crate::app::AppProfile {
            close_prefixes: vec!["Dismiss ".to_owned()],
            ..Default::default()
        };
        let closes = |name: &str| {
            profile
                .close_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
        };
        assert!(closes("Dismiss Preferences"));
        assert!(closes("Dismiss some/document/name"));
        // Not a close at all.
        assert!(!closes("Collapse Recent"));
        assert!(!closes("Rename thing"));
        // And an application that names no close prefix has none.
        let bare = crate::app::AppProfile::default();
        assert!(bare.close_prefixes.is_empty());
    }

    #[test]
    fn rows_off_the_top_of_a_transcript_are_not_hovered() {
        // The first run moved the pointer to y=-9726 eight times over. A row
        // above the window reveals nothing and costs a round trip.
        let nodes = vec![
            node(
                1,
                "listitem",
                "visible row",
                Some([10.0, 100.0, 200.0, 40.0]),
            ),
            node(
                2,
                "listitem",
                "scrolled off",
                Some([10.0, -9726.0, 200.0, 40.0]),
            ),
            node(
                3,
                "listitem",
                "below the fold",
                Some([10.0, 5000.0, 200.0, 40.0]),
            ),
        ];
        let ids = hover_row_ids(&nodes, "listitem", (0.0, 900.0));
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn coverage_reports_an_unaccounted_gap() {
        // Every bucket carries at least one control, so a bucket dropped from
        // `bucketed()` shows up here as an unaccounted gap rather than passing
        // on a zero that proves nothing. One dialog put real controls in
        // `blocked`, which is why it counts.
        let full = Coverage {
            in_tree: 10,
            swept: 3,
            outcome_declared: 0,
            unreachable: 2,
            vanished: 1,
            navigation: 1,
            manual: 1,
            isolated: 1,
            blocked: 1,
            revealed: 0,
        };
        assert!(full.accounted());
        assert!(!full.line().contains("UNACCOUNTED"));

        let declared = Coverage {
            in_tree: 2,
            outcome_declared: 2,
            ..Default::default()
        };
        assert!(declared.accounted());
        assert!(declared.line().contains("2 outcome-declared"));

        let leaky = Coverage {
            in_tree: 10,
            swept: 6,
            ..Default::default()
        };
        assert!(!leaky.accounted());
        assert!(leaky.line().contains("UNACCOUNTED 4"));
    }

    #[test]
    fn a_dialogs_own_controls_do_not_overflow_the_total() {
        /*
         * The regression: `cover home` reported `UNACCOUNTED -47`.
         *
         * A dialog's buttons do not exist when the surface is counted, so
         * pressing them used to increment `swept` against a total that never
         * included them. The buckets overran the denominator and the check went
         * negative - reporting a surplus, which reads as "better than covered"
         * when it actually means the number is meaningless.
         */
        let with_a_dialog = Coverage {
            in_tree: 10,
            swept: 10,
            revealed: 4,
            ..Default::default()
        };
        assert!(
            with_a_dialog.accounted(),
            "revealed controls extend the total: {}",
            with_a_dialog.line()
        );
        assert_eq!(with_a_dialog.total(), 14);
        assert!(with_a_dialog.line().contains("(10 on open, 4 revealed)"));
    }

    #[test]
    fn a_surplus_cannot_hide_a_real_gap() {
        // Ten on the surface, four of them never reached, plus four dialog
        // controls pressed. The old accounting cancelled one against the other
        // and reported a clean run; the gap has to survive the dialog.
        let both = Coverage {
            in_tree: 10,
            swept: 6,
            revealed: 4,
            ..Default::default()
        };
        assert!(!both.accounted());
        assert!(both.line().contains("UNACCOUNTED 4"), "{}", both.line());
    }
}
