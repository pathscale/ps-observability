//! Resolved-style assertions against the live Blitz renderer.

use std::time::Duration;

use blitz_control_protocol::{DebugResponse, DiagnosticsRequest, SemanticNode, SnapshotRequest};

use crate::inspector::{Client, inspect};
use crate::target::selector_matches_node;

async fn value_for_node(
    client: &mut Client,
    node_id: u64,
    selector: &str,
    property: &str,
) -> Result<String, String> {
    let answer = client
        .diagnostics(&DiagnosticsRequest::Snapshot(SnapshotRequest {
            include_dom: false,
            include_layout: false,
            include_computed_style: true,
            node_ids: vec![node_id],
        }))
        .await
        .map_err(|error| error.to_string())?;
    let DebugResponse::Snapshot(snapshot) = answer.response else {
        return Err(format!(
            "asked for computed style of {selector:?}, got {:?}",
            answer.response
        ));
    };
    let resolved = snapshot
        .computed_style
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("nodeId").and_then(serde_json::Value::as_u64) == Some(node_id))
        })
        .and_then(|row| row.get(property))
        .ok_or_else(|| format!("diagnostics returned no {property} for {selector:?}"))?;
    if let Some(value) = resolved.as_str() {
        Ok(value.to_owned())
    } else if let Some(value) = resolved.as_f64() {
        Ok(value.to_string())
    } else {
        Err(format!(
            "diagnostics returned non-scalar {property} for {selector:?}"
        ))
    }
}

async fn value(client: &mut Client, selector: &str, property: &str) -> Result<String, String> {
    let (tree, _) = inspect(client).await.map_err(|error| error.to_string())?;
    let node_id = largest_painted_match(&tree.nodes, selector).ok_or_else(|| {
        format!("could not inspect {selector:?}: no visible, sized matching node")
    })?;
    value_for_node(client, node_id, selector, property).await
}

fn largest_painted_match(nodes: &[SemanticNode], selector: &str) -> Option<u64> {
    nodes
        .iter()
        .filter(|node| {
            selector_matches_node(node, selector)
                && node.visible
                && node
                    .bounds
                    .is_some_and(|bounds| bounds[2] > 0.0 && bounds[3] > 0.0)
        })
        .max_by(|left, right| {
            let area = |node: &&SemanticNode| {
                node.bounds
                    .map(|bounds| bounds[2] * bounds[3])
                    .unwrap_or_default()
            };
            area(left).total_cmp(&area(right))
        })
        .map(|node| node.id)
}

pub(crate) async fn opaque_background(client: &mut Client, selector: &str) -> Result<(), String> {
    require_opaque_background(&value(client, selector, "backgroundColor").await?)
}

pub(crate) async fn transparent_background(
    client: &mut Client,
    selector: &str,
) -> Result<(), String> {
    require_transparent_background(&value(client, selector, "backgroundColor").await?)
}

pub(crate) async fn full_opacity(client: &mut Client, selector: &str) -> Result<(), String> {
    require_full_opacity(&value(client, selector, "opacity").await?)
}

fn parse_font_size(resolved: &str) -> Result<f64, String> {
    resolved
        .strip_suffix("px")
        .ok_or_else(|| format!("resolved font size {resolved:?} is not a pixel measurement"))?
        .parse::<f64>()
        .map_err(|_| format!("resolved font size {resolved:?} is not numeric"))
}

pub(crate) async fn font_size(client: &mut Client, selector: &str) -> Result<(u64, f64), String> {
    let (tree, _) = inspect(client).await.map_err(|error| error.to_string())?;
    let node_id = largest_painted_match(&tree.nodes, selector).ok_or_else(|| {
        format!("could not inspect {selector:?}: no visible, sized matching node")
    })?;
    let resolved = value_for_node(client, node_id, selector, "fontSize").await?;
    Ok((node_id, parse_font_size(&resolved)?))
}

pub(crate) async fn wait_for_larger_font(
    client: &mut Client,
    node_id: u64,
    selector: &str,
    before: f64,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let latest =
            parse_font_size(&value_for_node(client, node_id, selector, "fontSize").await?)?;
        if latest > before + 0.1 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{selector:?} font size stayed {latest:.2}px after the interface-size action; expected more than {before:.2}px"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) fn require_opaque_background(color: &str) -> Result<(), String> {
    let Some(alpha) = color.strip_prefix('#').and_then(|hex| hex.get(6..8)) else {
        return Err(format!(
            "resolved background colour {color:?} is not #rrggbbaa"
        ));
    };
    if alpha.eq_ignore_ascii_case("ff") {
        Ok(())
    } else {
        Err(format!(
            "resolved background colour {color} is translucent; a flat surface requires alpha ff"
        ))
    }
}

pub(crate) fn require_transparent_background(color: &str) -> Result<(), String> {
    let Some(alpha) = color.strip_prefix('#').and_then(|hex| hex.get(6..8)) else {
        return Err(format!(
            "resolved background colour {color:?} is not #rrggbbaa"
        ));
    };
    if alpha.eq_ignore_ascii_case("00") {
        Ok(())
    } else {
        Err(format!(
            "resolved background colour {color} keeps a film; a zero-opacity surface requires alpha 00"
        ))
    }
}

pub(crate) fn require_full_opacity(resolved: &str) -> Result<(), String> {
    let opacity = resolved
        .parse::<f64>()
        .map_err(|_| format!("resolved opacity {resolved:?} is not numeric"))?;
    if opacity >= 0.999 {
        Ok(())
    } else {
        Err(format!(
            "resolved opacity is {opacity:.3}; the control's content must remain fully opaque"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{require_full_opacity, require_opaque_background, require_transparent_background};

    #[test]
    fn flat_backgrounds_reject_translucent_resolved_paint() {
        assert!(require_opaque_background("#17202bff").is_ok());
        assert!(require_opaque_background("#17202b66").is_err());
        assert!(require_opaque_background("transparent").is_err());
    }

    #[test]
    fn transparent_backgrounds_reject_any_remaining_film() {
        assert!(require_transparent_background("#17202b00").is_ok());
        assert!(require_transparent_background("#17202b01").is_err());
    }

    #[test]
    fn full_opacity_rejects_whole_control_fades() {
        assert!(require_full_opacity("1").is_ok());
        assert!(require_full_opacity("0.4").is_err());
    }
}
