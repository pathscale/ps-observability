//! Inspection commands for the colours Blitz resolved at paint time.
//!
//! These operate on the running renderer. They do not infer outcomes from CSS
//! classes and do not render a synthetic component tree.

use std::collections::HashMap;

use blitz_control_protocol::{DebugResponse, DiagnosticsRequest, SemanticNode, SnapshotRequest};
use eyre::{Result, bail};

use crate::inspector::{Client, inspect};
use crate::paint_color::{Rgba, composite, contrast_ratio, luminance, parse};

/// Print the resolved foreground, background, opacity, and visibility of
/// matching painted boxes, largest first.
pub(crate) async fn report(client: &mut Client, want: &str, min_area: f64) -> Result<()> {
    let answer = client
        .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
            include_dom: true,
            include_layout: true,
            include_computed_style: true,
            node_ids: Vec::new(),
        }))
        .await?;
    let DebugResponse::Snapshot(snapshot) = answer.response else {
        bail!("asked for a paint snapshot, got {:?}", answer.response);
    };

    let styles = computed_styles(snapshot.computed_style.as_ref());
    if styles.is_empty() {
        bail!("the snapshot carried no computed styles; is this build's diagnostics feature on?");
    }

    let bounds: HashMap<u64, (f64, f64, f64, f64)> = snapshot
        .layout
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            (
                row.node_id,
                (
                    row.bounds.x,
                    row.bounds.y,
                    row.bounds.width,
                    row.bounds.height,
                ),
            )
        })
        .collect();
    let nodes = snapshot
        .dom
        .as_ref()
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let mut rows: Vec<(f64, String)> = Vec::new();
    for node in &nodes {
        let Some(id) = node.get("id").and_then(|value| value.as_u64()) else {
            continue;
        };
        let name = node
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let role = node
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        if !want.is_empty() && !name.contains(want) && !role.contains(want) {
            continue;
        }
        let (Some(style), Some(&(x, y, width, height))) = (styles.get(&id), bounds.get(&id)) else {
            continue;
        };
        let area = width * height;
        if area < min_area {
            continue;
        }
        let field = |key: &str| {
            style
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or("-")
                .to_owned()
        };
        let opacity = style
            .get("opacity")
            .and_then(|value| value.as_f64())
            .unwrap_or(f64::NAN);
        rows.push((
            area,
            format!(
                "  {id:>11}  {role:<12} {width:>7.1}x{height:<7.1} at {x:.0},{y:.0}  bg={:<10} fg={:<10} opacity={opacity:.2} {:<12} {name}",
                field("backgroundColor"),
                field("color"),
                field("visibility"),
            ),
        ));
    }

    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "{} nodes, {} with computed styles, showing boxes of {min_area}px2 or more",
        nodes.len(),
        styles.len()
    );
    for (_, row) in &rows {
        println!("{row}");
    }
    if rows.is_empty() {
        println!("nothing matched");
    }
    Ok(())
}

/// Fail when named, visible paint does not meet its configured contrast floor.
///
/// Translucent ancestor films are composited in tree order before measuring,
/// so this judges the paint Blitz intended rather than a stylesheet token.
pub(crate) async fn contrast(
    client: &mut Client,
    want: &str,
    text_ratio: f64,
    control_ratio: f64,
) -> Result<()> {
    let (semantic, elapsed) = inspect(client).await?;
    let answer = client
        .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
            include_dom: false,
            include_layout: false,
            include_computed_style: true,
            node_ids: Vec::new(),
        }))
        .await?;
    let DebugResponse::Snapshot(snapshot) = answer.response else {
        bail!("asked for a contrast snapshot, got {:?}", answer.response);
    };
    let styles = computed_styles(snapshot.computed_style.as_ref());
    if styles.is_empty() {
        bail!("the snapshot carried no computed styles; is this build's diagnostics feature on?");
    }
    let by_id: HashMap<u64, &SemanticNode> =
        semantic.nodes.iter().map(|node| (node.id, node)).collect();

    // A transparent window has no deterministic wallpaper colour beneath it.
    // Infer a conservative black/white base from the full-window text colour.
    let root_ink = semantic
        .nodes
        .iter()
        .filter_map(|node| {
            let bounds = node.bounds?;
            let color = parse(styles.get(&node.id)?.get("color")?.as_str()?)?;
            Some((bounds[2] * bounds[3], color))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, color)| color)
        .unwrap_or(Rgba::WHITE);
    let base = if luminance(root_ink) >= 0.25 {
        Rgba::BLACK
    } else {
        Rgba::WHITE
    };

    let mut failures = Vec::new();
    let mut audited = 0usize;
    for node in &semantic.nodes {
        if !auditable(node, want) {
            continue;
        }
        let Some(foreground) = styles
            .get(&node.id)
            .and_then(|style| style.get("color"))
            .and_then(|value| value.as_str())
            .and_then(parse)
        else {
            continue;
        };
        if foreground.alpha <= f64::EPSILON {
            continue;
        }

        let mut chain = Vec::new();
        let mut next = Some(node.id);
        for _ in 0..128 {
            let Some(id) = next else { break };
            chain.push(id);
            next = by_id.get(&id).and_then(|ancestor| ancestor.parent);
        }
        // First resolve what sits beneath this node. A control's own fill is
        // its visible chrome, not the surface that chrome must contrast with.
        // Including the node in this fold made every opaque thumb and swatch
        // compare inherited text `color` against its own fill instead.
        let under = chain.iter().skip(1).rev().fold(base, |under, id| {
            styles
                .get(id)
                .and_then(|style| style.get("backgroundColor"))
                .and_then(|value| value.as_str())
                .and_then(parse)
                .map_or(under, |film| composite(film, under))
        });
        let own_background = styles
            .get(&node.id)
            .and_then(|style| style.get("backgroundColor"))
            .and_then(|value| value.as_str())
            .and_then(parse);
        let border = styles.get(&node.id).and_then(|style| {
            let width = style
                .get("borderWidth")?
                .as_str()?
                .strip_suffix("px")?
                .parse::<f64>()
                .ok()?;
            (width > 0.0)
                .then(|| style.get("borderColor")?.as_str())
                .flatten()
                .and_then(parse)
                .filter(|color| color.alpha > f64::EPSILON)
        });
        let background = own_background.map_or(under, |film| composite(film, under));
        let has_text_content = styles
            .get(&node.id)
            .and_then(|style| style.get("hasTextContent"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let form_chrome = is_graphical_control(&node.role);
        let graphical_control = uses_graphical_contrast(&node.role, has_text_content);
        let (ink, substrate) = if form_chrome && let Some(border) = border {
            (border, under)
        } else if form_chrome
            && let Some(fill) = own_background
            && fill.alpha > f64::EPSILON
        {
            (fill, under)
        } else {
            (foreground, background)
        };
        let ratio = contrast_ratio(composite(ink, substrate), substrate);
        let required = if graphical_control {
            control_ratio
        } else {
            text_ratio
        };
        audited += 1;
        if ratio + 0.01 < required {
            failures.push((
                ratio,
                required,
                node.id,
                node.role.clone(),
                node.name.clone(),
            ));
        }
    }

    failures.sort_by(|a, b| a.0.total_cmp(&b.0));
    println!(
        "audited {audited} named painted nodes in {elapsed:.1}ms; text {text_ratio:.2}:1, controls {control_ratio:.2}:1"
    );
    if audited == 0 {
        bail!("no visible painted node matched {want:?}");
    }
    for (ratio, required, id, role, name) in failures.iter().take(80) {
        println!("  {ratio:>5.2}:1 < {required:.2}:1  {id:>11}  {role:<12} {name}");
    }
    if failures.len() > 80 {
        println!("... and {} more", failures.len() - 80);
    }
    if !failures.is_empty() {
        bail!(
            "{} named painted node(s) fall below their contrast floor",
            failures.len()
        );
    }
    println!("all named painted text meets the contrast floor");
    Ok(())
}

fn computed_styles(value: Option<&serde_json::Value>) -> HashMap<u64, serde_json::Value> {
    value
        .and_then(serde_json::Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| Some((row.get("nodeId")?.as_u64()?, row.clone())))
                .collect()
        })
        .unwrap_or_default()
}

fn auditable(node: &SemanticNode, want: &str) -> bool {
    !node.name.is_empty()
        && (node.enabled || !is_interactive(&node.role))
        && (want.is_empty() || node.name.contains(want) || node.role.contains(want))
        && node.visible
        && node
            .bounds
            .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
}

fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "tab"
            | "checkbox"
            | "switch"
            | "slider"
            | "radio"
            | "option"
            | "combobox"
            | "menuitem"
            | "textbox"
    )
}

fn is_graphical_control(role: &str) -> bool {
    matches!(role, "checkbox" | "switch" | "slider" | "radio")
}

fn uses_graphical_contrast(role: &str, has_text_content: bool) -> bool {
    is_graphical_control(role)
        || (!has_text_content && matches!(role, "button" | "tab" | "link" | "menuitem"))
}

#[cfg(test)]
mod tests {
    use super::{auditable, is_graphical_control, uses_graphical_contrast};
    use blitz_control_protocol::SemanticNode;

    #[test]
    fn only_non_text_form_chrome_uses_the_graphical_control_floor() {
        assert!(is_graphical_control("slider"));
        assert!(is_graphical_control("radio"));
        assert!(!is_graphical_control("button"));
        assert!(!is_graphical_control("combobox"));
    }

    #[test]
    fn icon_only_buttons_use_graphical_contrast_but_text_buttons_do_not() {
        assert!(uses_graphical_contrast("button", false));
        assert!(!uses_graphical_contrast("button", true));
    }

    #[test]
    fn inactive_controls_are_exempt_from_contrast_auditing() {
        let disabled = SemanticNode {
            dom_id: Some("prompt".into()),
            id: 1,
            parent: None,
            role: "textbox".into(),
            name: "Task manager prompt".into(),
            value: None,
            enabled: false,
            visible: true,
            selected: false,
            bounds: Some([0.0, 0.0, 200.0, 24.0]),
            slot: None,
        };
        assert!(!auditable(&disabled, ""));
    }
}
