//! Semantic target selection and viewport recovery.

use blitz_control_protocol::{AgentAction, AgentControlRequest, AgentSnapshot, SemanticNode};
use eyre::{Result, bail};
use std::collections::HashSet;

use crate::inspector::{Client, inspect};
use crate::{cli, reach};

pub(crate) fn resolved_action_target(nodes: &[SemanticNode], want: &str) -> Option<String> {
    nodes
        .iter()
        .find(|node| {
            selector_matches_node(node, want)
                && node.enabled
                && node.visible
                && painted_bounds(node).is_some()
        })
        .map(|node| node.name.clone())
}

/// The renderer's painted box is the native source of truth for whether a
/// semantic node can be targeted. Blitz can report `visible = false` for a
/// frame after a control is already painted; retained hidden controls instead
/// collapse to a zero-sized box.
pub(crate) fn painted_bounds(node: &SemanticNode) -> Option<[f64; 4]> {
    node.bounds
        .filter(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
}

/// Whether a named check target currently occupies a box in the live tree.
///
/// `role:name` is accepted for precise subjects such as rename textboxes; bare
/// names retain the normal substring behavior used by application manifests.
pub(crate) fn painted_named(nodes: &[SemanticNode], want: &str) -> bool {
    nodes
        .iter()
        .any(|node| selector_matches_node(node, want) && painted_bounds(node).is_some())
}

/// Click one node by id, with no name lookup in between.
pub(crate) fn name_matches(name: &str, pattern: &str) -> bool {
    let (name, pattern) = (name.to_lowercase(), pattern.to_lowercase());
    if pattern == "*" {
        return true;
    }
    let Some(stripped) = pattern.strip_prefix('*') else {
        return match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name.contains(&pattern),
        };
    };
    match stripped.strip_suffix('*') {
        Some(middle) => name.contains(middle),
        None => name.ends_with(stripped),
    }
}

/// The viewport, read from the tree rather than assumed.
///
/// Shared by every driving mode so they agree on what is on screen. `sweep` and
/// `cover` already read it this way; `open_named` and `press_named` did not,
/// which is the bug the two helpers below exist to close.
pub(crate) fn viewport_of(snapshot: &AgentSnapshot) -> (f64, f64) {
    /*
     * The window, not `main`.
     *
     * `main` starts below the title and tab bar - measured here at y=58 in a
     * 900px window - so treating its top as the viewport's top declares the tab
     * strip off-screen. That is chrome sitting legitimately *above* the content
     * region, and rejecting it made every surface reached by a tab unnavigable.
     * Taking the top of the window keeps the below-the-fold case, which is what
     * this bound is actually for, without swallowing the header.
     */
    let bottom = snapshot
        .nodes
        .iter()
        .filter(|node| node.role == "main")
        .filter_map(|node| node.bounds)
        .map(|b| b[1] + b[3])
        .fold(f64::MIN, f64::max);
    (0.0, if bottom > f64::MIN { bottom } else { f64::MAX })
}

/// The vertical hit-test viewport that actually owns a semantic node.
///
/// App chrome lives outside `main` and legitimately uses the window viewport.
/// Surface content is a descendant of `main`; treating its negative translated
/// scroll coordinates as window-visible sends pointer events behind the tab
/// strip instead of revealing the row inside its panel.
pub(crate) fn viewport_for_node(snapshot: &AgentSnapshot, node_id: u64) -> (f64, f64) {
    let mut cursor = Some(node_id);
    for _ in 0..32 {
        let Some(id) = cursor else { break };
        let Some(node) = snapshot.nodes.iter().find(|node| node.id == id) else {
            break;
        };
        if node.role == "main"
            && let Some(bounds) = node.bounds
        {
            return (bounds[1], bounds[1] + bounds[3]);
        }
        cursor = node.parent;
    }
    viewport_of(snapshot)
}

/// Whether a node's box lies outside the window, so pressing it would land on
/// nothing.
///
/// A tab strip that overflows keeps its buttons at negative x, and the panel's
/// lower sections sit below the fold. Both are in the tree, both report
/// `visible` and `enabled`, and both have a real width and height, so the
/// obvious predicate accepts them and the press goes to a point the window
/// never had. That is why `rename-project-header` reported the application's
/// pencil dead: its project tab sat at x=-742 and the navigation press that was
/// meant to reach the surface silently hit nothing.
pub(crate) fn offscreen(bounds: [f64; 4], viewport: (f64, f64)) -> bool {
    bounds[1] + bounds[3] < viewport.0 || bounds[0] + bounds[2] < 0.0 || bounds[1] > viewport.1
}

/// Find a named semantic control, scrolling it into view when it is off-screen.
///
/// Skipping an off-screen control is honest but useless: an overflowing tab
/// strip parks every project tab at negative x, so a check that navigates to a
/// project could never reach one, and the surface behind it was untestable.
/// `ScrollIntoView` is what a person does by scrolling, so the control is
/// driven where it actually is rather than at a point outside the window.
///
/// Returns the node id and its box *after* any scroll, because the coordinates
/// the caller presses at must be the ones the renderer just settled on.
pub(crate) async fn locate_control(
    client: &mut Client,
    want: &str,
    roles: &[&str],
) -> Result<(u64, [f64; 4])> {
    /*
     * An on-screen match wins over an earlier off-screen one.
     *
     * Taking the first match in tree order and then asking whether it is
     * on-screen is wrong when a name matches more than once, which is normal: a
     * document appears in the tab strip, in the root list and in its own header.
     * Doing that can report the root unreachable while a pressable copy is on
     * screen because an overflowed copy came first in the tree.
     */
    let pick = |snapshot: &AgentSnapshot| -> Option<(u64, [f64; 4])> {
        let modal_scope: HashSet<u64> = reach::dismissers(&snapshot.nodes)
            .first()
            .map(|(id, _)| reach::enclosing_dialog(&snapshot.nodes, *id))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let surface_scope: HashSet<u64> = reach::surfaces()
            .iter()
            .find(|surface| reach::on_surface(&snapshot.nodes, surface))
            .map(|surface| reach::on_surface_subtree(&snapshot.nodes, surface))
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut candidates: Vec<_> = snapshot
            .nodes
            .iter()
            .filter(|n| {
                roles.contains(&"*")
                    || roles.is_empty() && reach::interactive(n)
                    || roles.contains(&n.role.as_str())
            })
            .filter(|n| selector_matches_node(n, want))
            // Actionability needs both semantic visibility and paint geometry.
            // The runtime rejects a Click for a hidden semantic node even when
            // that retained node still owns a stale non-zero layout box. Letting
            // geometry overrule visibility selected a closed menu's old item
            // instead of the mounted item with the same accessible name.
            .filter(|node| node.visible)
            .filter_map(|node| painted_bounds(node).map(|bounds| (node, bounds)))
            .collect();
        // An explicit accessible name excludes broader substring matches.
        // Sorting is not strong enough here: an on-screen substring can still
        // beat an exact match below the fold when the surface preference is
        // applied. `Restart` then selected visible `Restart AgencyProxy`
        // instead of scrolling the exact control into view.
        retain_exact_candidates(&mut candidates, want);
        // Preserve exact-name priority even when the exact control is
        // disabled. Filtering it first let a longer enabled substring steal
        // the action (`Send` became “Parse … before sending”).
        candidates.retain(|(node, _)| node.enabled);
        // Prefer the modal in front, then the active surface, then global
        // chrome. Retained panes can keep enabled, painted controls with the
        // same name; tree order is not a statement about which one owns the
        // interaction the caller can currently see.
        for scope in [&modal_scope, &surface_scope] {
            if let Some((node, bounds)) = candidates.iter().find(|(node, bounds)| {
                scope.contains(&node.id)
                    && !offscreen(*bounds, viewport_for_node(snapshot, node.id))
            }) {
                return Some((node.id, *bounds));
            }
        }
        if let Some((node, bounds)) = candidates
            .iter()
            .find(|(node, bounds)| !offscreen(*bounds, viewport_for_node(snapshot, node.id)))
        {
            return Some((node.id, *bounds));
        }

        let mut fallback = None;
        for (node, bounds) in candidates {
            /*
             * Of the off-screen matches, prefer one `ScrollIntoView` can
             * actually recover.
             *
             * It scrolls vertically. A control below the fold comes back; one
             * pushed off the side by an overflowing strip does not, and
             * `reveal` on such a node reports `moved 0.0`. Taking the first
             * match in tree order picked exactly that unreachable case: a
             * project name matches both its tab, parked at x=-742, and its row
             * in the list at y=2057, and only the second could be reached.
             *
             * This is a consolation prize, not a fix. The protocol's `Pointer`
             * takes x and y where `Click` takes a node id, so a real
             * press-and-release has to compute a coordinate at all, and a
             * control the window cannot show has no honest one. `ps-qa find
             * --offscreen` reports which controls are in that state.
             */
            let recoverable = bounds[0] + bounds[2] > 0.0;
            if recoverable && !fallback.is_some_and(|(_, b): (u64, [f64; 4])| b[0] + b[2] > 0.0) {
                fallback = Some((node.id, bounds));
            } else {
                fallback.get_or_insert((node.id, bounds));
            }
        }
        fallback
    };

    let (snapshot, _) = inspect(client).await?;
    let Some((id, bounds)) = pick(&snapshot) else {
        bail!("no visible, enabled, sized semantic control matching it");
    };
    let viewport = viewport_for_node(&snapshot, id);
    if !offscreen(bounds, viewport) {
        return Ok((id, bounds));
    }

    if cli::trace() {
        println!("        {want:?} is off-screen at {bounds:?}, scrolling it in");
    }
    let mut target = (id, bounds);
    let mut latest = snapshot;
    for _ in 0..4 {
        for node_id in reach::reveal_chain(&latest.nodes, target.0) {
            client
                .agent(&AgentControlRequest::Act(AgentAction::ScrollIntoView {
                    node_id,
                }))
                .await?;
        }
        // Re-inspect rather than reusing the old box: the scroll moved it, and
        // pressing at where it used to be is the bug this exists to fix. Nested
        // scrollers may need more than one semantic reveal: the first exposes
        // the node inside its local list, the next exposes that list in the
        // outer surface.
        let (settled, _) = inspect(client).await?;
        let Some(found) = pick(&settled) else {
            bail!("no visible, enabled, sized semantic control matching it");
        };
        target = found;
        let viewport = viewport_for_node(&settled, target.0);
        if !offscreen(target.1, viewport) {
            return Ok(target);
        }

        // `scrollIntoView` can finish the innermost list while its containing
        // panel is still below the viewport. Move the outermost semantic
        // ancestor's nearest scroller by exactly the remaining vertical gap;
        // the node id still selects the scroll context, never a screen point.
        let delta_y = if target.1[1] > viewport.1 {
            target.1[1] + target.1[3] - viewport.1 + 16.0
        } else if target.1[1] + target.1[3] < viewport.0 {
            target.1[1] - viewport.0 - 16.0
        } else {
            0.0
        };
        if delta_y != 0.0
            && let Some(node_id) = reach::reveal_chain(&settled.nodes, target.0).first()
        {
            client
                .agent(&AgentControlRequest::Act(AgentAction::ScrollBy {
                    node_id: *node_id,
                    delta_x: 0.0,
                    delta_y,
                }))
                .await?;
        }
        latest = settled;
    }
    bail!(
        "{want:?} is still off-screen at {:?} after four semantic reveal attempts",
        target.1
    )
}
fn selector_slot(selector: &str) -> Option<&str> {
    selector.strip_prefix('@')
}

fn selector_dom_id(selector: &str) -> Option<&str> {
    selector.strip_prefix('#').filter(|id| !id.is_empty())
}

pub(crate) fn selector_matches_node(node: &SemanticNode, selector: &str) -> bool {
    if let Some(dom_id) = selector_dom_id(selector) {
        return node.dom_id.as_deref() == Some(dom_id);
    }
    if let Some(slot) = selector_slot(selector) {
        return node.slot.as_deref() == Some(slot);
    }
    if let Some((role, name)) = selector.split_once(':')
        && role.eq_ignore_ascii_case(&node.role)
    {
        return name_matches(&node.name, name);
    }
    name_matches(&node.name, selector)
}

pub(crate) fn exact_selector_matches_node(node: &SemanticNode, selector: &str) -> bool {
    if let Some(dom_id) = selector_dom_id(selector) {
        return node.dom_id.as_deref() == Some(dom_id);
    }
    // A slot is already exact: there is no substring reading of it to narrow.
    if let Some(slot) = selector_slot(selector) {
        return node.slot.as_deref() == Some(slot);
    }
    if let Some((role, name)) = selector.split_once(':')
        && role.eq_ignore_ascii_case(&node.role)
    {
        return node.name.eq_ignore_ascii_case(name);
    }
    node.name.eq_ignore_ascii_case(selector)
}

pub(crate) fn retain_exact_candidates(
    candidates: &mut Vec<(&SemanticNode, [f64; 4])>,
    selector: &str,
) {
    if candidates
        .iter()
        .any(|(node, _)| exact_selector_matches_node(node, selector))
    {
        candidates.retain(|(node, _)| exact_selector_matches_node(node, selector));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(dom_id: Option<&str>, name: &str) -> SemanticNode {
        SemanticNode {
            dom_id: dom_id.map(str::to_owned),
            id: 1,
            parent: None,
            role: "button".into(),
            name: name.into(),
            value: None,
            enabled: true,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 20.0, 20.0]),
            slot: Some("button".into()),
        }
    }

    #[test]
    fn dom_id_is_an_exact_stable_selector() {
        let save = node(Some("settings-save"), "Save settings");
        assert!(selector_matches_node(&save, "#settings-save"));
        assert!(exact_selector_matches_node(&save, "#settings-save"));
        assert!(!selector_matches_node(&save, "#settings"));
        assert!(!selector_matches_node(&save, "#"));
    }

    #[test]
    fn dom_id_does_not_fall_back_to_the_accessible_name() {
        let anonymous = node(None, "settings-save");
        assert!(!selector_matches_node(&anonymous, "#settings-save"));
    }

    #[test]
    fn role_and_name_globs_have_one_case_insensitive_semantics() {
        let save = node(None, "Save Settings");
        assert!(selector_matches_node(&save, "BUTTON:save*"));
        assert!(selector_matches_node(&save, "button:*settings"));
        assert!(selector_matches_node(&save, "*VE SET*"));
        assert!(painted_named(&[save], "BuTtOn:*SETTINGS"));
    }

    #[test]
    fn a_bare_selector_matches_names_not_roles() {
        let save = node(None, "Save settings");
        assert!(!selector_matches_node(&save, "button"));
        assert!(selector_matches_node(&save, "save"));
    }

    #[test]
    fn a_colon_in_a_bare_name_is_not_misread_as_a_role() {
        let permission = node(None, "Default permission: Auto");
        assert!(selector_matches_node(
            &permission,
            "Default permission: Auto"
        ));
        assert!(exact_selector_matches_node(
            &permission,
            "Default permission: Auto"
        ));
    }
}
