use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use blitz_control_protocol::client::Client;
use blitz_control_protocol::{
    AGENT_CONTROL_TOOL, AgentSnapshot, CapturedImage, DIAGNOSTICS_TOOL, DebugDescriptor,
    DebugSnapshot, SemanticNode, TargetContext, TargetId, TargetInfo,
};
use eyre::{Result, bail, ensure, eyre};
use serde_json::{Value, json};

struct Tree {
    snapshot: AgentSnapshot,
    context: TargetContext,
}

async fn call(client: &mut Client, tool: &str, arguments: Value) -> Result<Value> {
    let response = client
        .raw_request("tools/call", json!({"name": tool, "arguments": arguments}))
        .await?;
    ensure!(
        response.get("error").is_none(),
        "JSON-RPC failed: {response}"
    );
    response
        .pointer("/result/structuredContent")
        .cloned()
        .ok_or_else(|| eyre!("missing payload: {response}"))
}

async fn checked(client: &mut Client, tool: &str, arguments: Value) -> Result<Value> {
    let response = call(client, tool, arguments).await?;
    ensure!(response["result"] != "error", "request failed: {response}");
    Ok(response)
}

async fn targets(client: &mut Client) -> Result<Vec<TargetInfo>> {
    let response = checked(
        client,
        AGENT_CONTROL_TOOL,
        json!({"command": "listTargets"}),
    )
    .await?;
    ensure!(
        response["result"] == "targets",
        "unexpected discovery response: {response}"
    );
    Ok(serde_json::from_value(response["value"].clone())?)
}

async fn tree(client: &mut Client, id: Option<u64>) -> Result<Tree> {
    let discovered = targets(client).await?;
    let mut request = json!({"command": "inspect", "params": {"root": null, "max_depth": 40}});
    match id {
        Some(id) => {
            let target = discovered
                .iter()
                .find(|target| target.id == TargetId::Tab(id))
                .ok_or_else(|| eyre!("tab {id} is not discoverable"))?;
            let document_id = target
                .document_id
                .ok_or_else(|| eyre!("tab {id} is not mounted"))?;
            request["target"] = json!({"tabId": id});
            request["expectedGeneration"] = json!(target.generation);
            request["expectedDocumentId"] = json!(document_id);
        }
        None => {
            ensure!(
                discovered
                    .iter()
                    .any(|target| target.id == TargetId::chrome()),
                "chrome target is not discoverable"
            );
            request["target"] = json!("chrome");
        }
    }
    let response = checked(client, AGENT_CONTROL_TOOL, request).await?;
    Ok(Tree {
        snapshot: serde_json::from_value(response["value"].clone())?,
        context: serde_json::from_value(response["context"].clone())?,
    })
}

fn node<'a>(tree: &'a Tree, id: &str) -> Result<&'a SemanticNode> {
    tree.snapshot
        .nodes
        .iter()
        .find(|node| node.dom_id.as_deref() == Some(id))
        .ok_or_else(|| eyre!("missing authored node #{id}"))
}

fn guarded(tree: &Tree, mut request: Value) -> Value {
    request["target"] = match &tree.context.target_id {
        TargetId::Tab(id) => json!({"tabId": *id}),
        _ => json!("chrome"),
    };
    request["expectedGeneration"] = tree.context.generation.into();
    request["expectedDocumentId"] = tree.context.document_id.into();
    request
}

async fn act(client: &mut Client, tree: &Tree, action: &str, params: Value) -> Result<()> {
    let response = checked(
        client,
        AGENT_CONTROL_TOOL,
        guarded(
            tree,
            json!({
                "command": "act", "params": {"action": action, "params": params}
            }),
        ),
    )
    .await?;
    ensure!(
        response["result"] == "ack",
        "action did not acknowledge: {response}"
    );
    Ok(())
}

async fn key(client: &mut Client, tree: &Tree, key: &str, code: &str) -> Result<()> {
    for phase in ["down", "up"] {
        act(
            client,
            tree,
            "input",
            json!({
                "input": "key", "phase": phase, "key": key, "code": code,
                "modifiers": {"shift": false, "control": false, "alt": false, "meta": false}
            }),
        )
        .await?;
    }
    Ok(())
}

async fn wait_name(client: &mut Client, id: u64, dom_id: &str, name: &str) -> Result<Tree> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(tree) = tree(client, Some(id)).await
            && node(&tree, dom_id).is_ok_and(|node| node.name == name)
        {
            return Ok(tree);
        }
        ensure!(
            Instant::now() < deadline,
            "tab {id} did not produce #{dom_id} {name:?}"
        );
        nagoya::sleep(Duration::from_millis(25)).await;
    }
}

async fn mounted(client: &mut Client, id: u64) -> Result<TargetInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(target) = targets(client)
            .await?
            .into_iter()
            .find(|target| target.id == TargetId::Tab(id) && target.document_id.is_some())
        {
            return Ok(target);
        }
        ensure!(Instant::now() < deadline, "tab {id} did not mount");
        nagoya::sleep(Duration::from_millis(25)).await;
    }
}

async fn open_tab(client: &mut Client) -> Result<u64> {
    let before = targets(client).await?;
    let chrome = tree(client, None).await?;
    let new_tab = node(&chrome, "chuzz-new-tab")?.id;
    act(client, &chrome, "click", json!({"node_id": new_tab})).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(id) = targets(client)
            .await?
            .into_iter()
            .find_map(|target| match target.id {
                TargetId::Tab(id)
                    if !before
                        .iter()
                        .any(|previous| previous.id == TargetId::Tab(id)) =>
                {
                    Some(id)
                }
                _ => None,
            })
        {
            mounted(client, id).await?;
            return Ok(id);
        }
        ensure!(
            Instant::now() < deadline,
            "new-tab action did not create a target"
        );
        nagoya::sleep(Duration::from_millis(25)).await;
    }
}

async fn navigate(client: &mut Client, id: u64, url: &str) -> Result<()> {
    checked(
        client,
        AGENT_CONTROL_TOOL,
        json!({"target": {"tabId": id}, "command": "navigate", "params": {"url": url}}),
    )
    .await?;
    Ok(())
}

async fn observe(socket: &Path, id: Option<u64>, all: bool) -> Result<Client> {
    let mut client = Client::connect(socket).await?;
    let mut request = json!({"command": "observe", "afterSequence": 0,
        "params": {"streams": ["console", "runtimeErrors"]}});
    if let Some(id) = id {
        request["target"] = json!({"tabId": id});
    }
    if all {
        request["allTargets"] = true.into();
    }
    checked(&mut client, DIAGNOSTICS_TOOL, request).await?;
    Ok(client)
}

fn records(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message["method"] == "notifications/blitz/diagnostics")
        .map(|message| &message["params"])
        .collect()
}

fn validate_records(messages: &[Value], expected: Option<&TargetContext>) -> Result<()> {
    let mut previous = 0;
    for record in records(messages) {
        if record["event"] == "diagnosticOverflow" {
            ensure!(
                record["value"]["code"] == "diagnosticOverflow",
                "overflow is unnamed: {record}"
            );
            let first = record["value"]["firstSequence"]
                .as_u64()
                .ok_or_else(|| eyre!("missing overflow start"))?;
            let last = record["value"]["lastSequence"]
                .as_u64()
                .ok_or_else(|| eyre!("missing overflow end"))?;
            ensure!(first <= last, "invalid overflow range: {record}");
            previous = last;
            continue;
        }
        ensure!(
            record["event"] != "clientNotificationOverflow",
            "client lost notifications: {record}"
        );
        let context: TargetContext = serde_json::from_value(record.clone())?;
        if let Some(expected) = expected {
            ensure!(
                &context == expected,
                "observation crossed target identity: {record}"
            );
        }
        let sequence = record["sequence"]
            .as_u64()
            .ok_or_else(|| eyre!("missing journal sequence"))?;
        ensure!(
            sequence > previous,
            "events are not ordered: {previous} then {sequence}"
        );
        previous = sequence;
        if record["event"] == "runtimeError" {
            ensure!(
                !record["value"]["stack"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty(),
                "runtime error lost its stack"
            );
        }
    }
    Ok(())
}

fn event_sequence(messages: &[Value], message: &str) -> Result<u64> {
    records(messages)
        .into_iter()
        .find(|record| record["value"]["message"] == message)
        .and_then(|record| record["sequence"].as_u64())
        .ok_or_else(|| eyre!("missing diagnostic {message:?}"))
}

fn error_code(response: &Value, code: &str) -> Result<()> {
    ensure!(
        response["result"] == "error" && response["value"]["code"] == code,
        "expected {code}, got {response}"
    );
    Ok(())
}

fn one_socket(descriptor_path: &Path, descriptor: &DebugDescriptor) -> Result<()> {
    let parent = descriptor_path
        .parent()
        .ok_or_else(|| eyre!("descriptor has no directory"))?;
    let mut count = 0;
    for entry in std::fs::read_dir(parent)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
            && let Ok(bytes) = std::fs::read(&path)
            && let Ok(other) = serde_json::from_slice::<DebugDescriptor>(&bytes)
            && other.pid == descriptor.pid
        {
            ensure!(
                other.instance_id == descriptor.instance_id,
                "tab creation allocated another instance descriptor"
            );
            count += 1;
        }
    }
    ensure!(
        count == 1,
        "expected exactly one descriptor for GUI process, got {count}"
    );
    let current: DebugDescriptor = serde_json::from_slice(&std::fs::read(descriptor_path)?)?;
    ensure!(
        &current == descriptor,
        "tab activity changed the socket descriptor"
    );
    ensure!(
        Path::new(
            descriptor
                .address
                .strip_prefix("unix://")
                .ok_or_else(|| eyre!("inspection is not local Unix transport"))?
        )
        .exists(),
        "inspection socket is missing"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(descriptor_path)?.permissions().mode() & 0o777 == 0o600,
            "descriptor is not owner-only"
        );
        ensure!(
            std::fs::metadata(parent)?.permissions().mode() & 0o777 == 0o700,
            "inspection directory is not owner-only"
        );
    }
    Ok(())
}

pub async fn gui(socket: &Path, descriptor_path: &Path, fixture_url: &str) -> Result<()> {
    let descriptor: DebugDescriptor = serde_json::from_slice(&std::fs::read(descriptor_path)?)?;
    let mut client = Client::connect(socket).await?;
    let chrome = tree(&mut client, None).await?;
    ensure!(
        chrome.context.target_id == TargetId::chrome(),
        "targeted GUI inspect did not return chrome context"
    );
    let legacy = checked(
        &mut client,
        AGENT_CONTROL_TOOL,
        json!({"command": "inspect", "params": {"root": null, "max_depth": 40}}),
    )
    .await?;
    let legacy: AgentSnapshot = serde_json::from_value(legacy["value"].clone())?;
    ensure!(
        legacy.nodes.iter().map(|node| node.id).collect::<Vec<_>>()
            == chrome
                .snapshot
                .nodes
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>(),
        "targeted and legacy chrome inspect differ"
    );

    let a = open_tab(&mut client).await?;
    let separator = if fixture_url.contains('?') { '&' } else { '?' };
    navigate(&mut client, a, &format!("{fixture_url}{separator}name=A")).await?;
    wait_name(&mut client, a, "ready", "READY A").await?;
    let b = open_tab(&mut client).await?;
    navigate(&mut client, b, &format!("{fixture_url}{separator}name=B")).await?;
    let tree_b = wait_name(&mut client, b, "ready", "READY B").await?;
    let tree_a = wait_name(&mut client, a, "ready", "READY A").await?;
    let discovered = targets(&mut client).await?;
    ensure!(
        !discovered
            .iter()
            .find(|target| target.id == TargetId::Tab(a))
            .ok_or_else(|| eyre!("missing A"))?
            .active,
        "A is not inactive"
    );
    ensure!(
        discovered
            .iter()
            .find(|target| target.id == TargetId::Tab(b))
            .ok_or_else(|| eyre!("missing B"))?
            .active,
        "B is not active"
    );
    ensure!(
        tree_a.context.document_id != tree_b.context.document_id,
        "tabs share a document"
    );
    ensure!(
        node(&tree_a, "press")?.id == node(&tree_b, "press")?.id,
        "fixture no longer exercises colliding document-local node IDs"
    );
    ensure!(
        node(&tree_a, "ready")?
            .bounds
            .is_some_and(|bounds| bounds[1].abs() < 1.0),
        "page-local coordinates include browser chrome"
    );

    let click = json!({"command": "act", "params": {"action": "click", "params": {"node_id": node(&tree_a, "press")?.id}}});
    let mut missing = click.clone();
    missing["target"] = json!({"tabId": a});
    error_code(
        &call(&mut client, AGENT_CONTROL_TOOL, missing).await?,
        "targetIdentityRequired",
    )?;
    let mut stale = guarded(&tree_a, click.clone());
    stale["expectedGeneration"] = tree_a.context.generation.saturating_add(1).into();
    error_code(
        &call(&mut client, AGENT_CONTROL_TOOL, stale).await?,
        "staleGeneration",
    )?;
    let mut stale = guarded(&tree_a, click.clone());
    stale["expectedDocumentId"] = tree_b.context.document_id.into();
    error_code(
        &call(&mut client, AGENT_CONTROL_TOOL, stale).await?,
        "staleDocument",
    )?;
    let mut unknown = guarded(&tree_a, click.clone());
    unknown["target"] = json!({"tabId": u64::MAX});
    error_code(
        &call(&mut client, AGENT_CONTROL_TOOL, unknown).await?,
        "unknownTarget",
    )?;

    let mut observer_a = observe(socket, Some(a), false).await?;
    let mut observer_a2 = observe(socket, Some(a), false).await?;
    let mut observer_all = observe(socket, None, true).await?;
    let mut observer_chrome = observe(socket, None, false).await?;
    checked(
        &mut client,
        AGENT_CONTROL_TOOL,
        guarded(&tree_a, click.clone()),
    )
    .await?;
    wait_name(&mut client, a, "count", "count 1").await?;
    wait_name(&mut client, b, "count", "count 0").await?;
    let current_a = tree(&mut client, Some(a)).await?;
    let field = node(&current_a, "field")?.id;
    act(
        &mut client,
        &current_a,
        "setValue",
        json!({"node_id": field, "value": "typed-a"}),
    )
    .await?;
    wait_name(&mut client, a, "typed", "typed typed-a").await?;
    key(&mut client, &current_a, "Enter", "Enter").await?;
    wait_name(&mut client, a, "key", "key typed-a").await?;
    wait_name(&mut client, b, "typed", "typed empty").await?;

    for (id, tree) in [(a, &current_a), (b, &tree_b)] {
        let snapshot = checked(&mut client, DIAGNOSTICS_TOOL, guarded(tree, json!({"command": "snapshot", "params": {
            "includeDom": true, "includeLayout": true, "includeComputedStyle": true, "nodeIds": [node(tree, "press")?.id]
        }}))).await?;
        let snapshot: DebugSnapshot = serde_json::from_value(snapshot["value"].clone())?;
        ensure!(
            snapshot
                .dom
                .as_ref()
                .and_then(Value::as_array)
                .is_some_and(|nodes| nodes.iter().any(|node| node["domId"] == "press")),
            "tab {id} has no DOM button row"
        );
        ensure!(
            snapshot
                .layout
                .as_ref()
                .is_some_and(|rows| rows.iter().any(|row| row.node_id
                    == node(tree, "press").map(|node| node.id).unwrap_or_default())),
            "tab {id} has no local layout row"
        );
        for command in ["metrics", "waitForIdle"] {
            let response = checked(
                &mut client,
                DIAGNOSTICS_TOOL,
                json!({"target": {"tabId": id}, "command": command}),
            )
            .await?;
            ensure!(
                response["context"]["targetId"] == id,
                "{command} lost tab context"
            );
        }
        let capture = checked(&mut client, DIAGNOSTICS_TOOL, guarded(tree, json!({"command": "capture", "params": {"nodeId": node(tree, "press")?.id, "scale": 1.0}}))).await?;
        let image: CapturedImage = serde_json::from_value(capture["value"].clone())?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(&image.rgba_base64)?;
        ensure!(
            image.width > 0
                && image.height > 0
                && bytes.len() as u64 == u64::from(image.width) * u64::from(image.height) * 4,
            "tab {id} capture is invalid"
        );
        ensure!(
            bytes.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0),
            "tab {id} capture is blank"
        );
        ensure!(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel != &bytes[..4]),
            "tab {id} control capture is uniformly painted"
        );
    }

    // A request on an observed connection must preserve notifications before its reply.
    targets(&mut observer_a).await?;
    let events_a = observer_a.drain(0.5).await?;
    let events_a2 = observer_a2.drain(0.1).await?;
    let events_all = observer_all.drain(0.1).await?;
    let events_chrome = observer_chrome.drain(0.1).await?;
    validate_records(&events_a, Some(&tree_a.context))?;
    validate_records(&events_a2, Some(&tree_a.context))?;
    validate_records(&events_all, None)?;
    validate_records(&events_chrome, Some(&chrome.context))?;
    ensure!(
        event_sequence(&events_a, "startup A")? == event_sequence(&events_a2, "startup A")?,
        "observers do not replay independently"
    );
    event_sequence(&events_a, "click A")?;
    event_sequence(&events_all, "startup B")?;
    for message in ["startup-error A", "timer-error A", "handler-error A"] {
        ensure!(
            records(&events_a)
                .iter()
                .any(|record| record["event"] == "runtimeError"
                    && record["value"]["message"]
                        .as_str()
                        .is_some_and(|text| text.contains(message))),
            "missing tagged {message}"
        );
    }
    one_socket(descriptor_path, &descriptor)?;

    navigate(
        &mut client,
        a,
        &format!("{fixture_url}{separator}name=A&replacement=1"),
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = mounted(&mut client, a).await?;
        if current.generation != tree_a.context.generation {
            ensure!(
                current.document_id != Some(tree_a.context.document_id),
                "navigation retained the old document identity"
            );
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "navigation never replaced the mounted generation"
        );
        nagoya::sleep(Duration::from_millis(25)).await;
    }
    error_code(
        &call(&mut client, AGENT_CONTROL_TOOL, guarded(&tree_a, click)).await?,
        "staleGeneration",
    )?;

    let info_b = mounted(&mut client, b).await?;
    let chrome = tree(&mut client, None).await?;
    let close = chrome
        .snapshot
        .nodes
        .iter()
        .find(|node| node.role == "button" && node.name == format!("Close tab: {}", info_b.title))
        .ok_or_else(|| eyre!("missing B close button"))?
        .id;
    act(&mut client, &chrome, "click", json!({"node_id": close})).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while targets(&mut client)
        .await?
        .iter()
        .any(|target| target.id == TargetId::Tab(b))
    {
        ensure!(Instant::now() < deadline, "closed tab remains discoverable");
        nagoya::sleep(Duration::from_millis(25)).await;
    }
    error_code(&call(&mut client, AGENT_CONTROL_TOOL, guarded(&tree_b, json!({"command": "inspect", "params": {"root": node(&tree_b, "press")?.id, "max_depth": 1}}))).await?, "unknownTarget")?;
    error_code(&call(&mut client, DIAGNOSTICS_TOOL, json!({"target": {"tabId": b}, "command": "observe", "params": {"streams": ["console"]}})).await?, "unknownTarget")?;
    one_socket(descriptor_path, &descriptor)?;
    println!(
        "PAGE TARGETS PASS: inactive input, colliding IDs, DOM/layout/capture, independent replay, tagged errors, navigation/close rejection, legacy chrome, one socket"
    );
    Ok(())
}

pub async fn headless(client: &mut Client) -> Result<()> {
    client.select_target(None);
    let discovery = targets(client).await?;
    ensure!(
        discovery.len() == 1 && discovery[0].id == TargetId::Tab(0),
        "headless discovery changed its sole-page scope"
    );
    let legacy = checked(
        client,
        AGENT_CONTROL_TOOL,
        json!({"command": "inspect", "params": {"root": null, "max_depth": 40}}),
    )
    .await?;
    let selected = checked(client, AGENT_CONTROL_TOOL, json!({"target": {"tabId": 0}, "command": "inspect", "params": {"root": null, "max_depth": 40}})).await?;
    let legacy: AgentSnapshot = serde_json::from_value(legacy["value"].clone())?;
    let selected: AgentSnapshot = serde_json::from_value(selected["value"].clone())?;
    ensure!(
        legacy.nodes == selected.nodes,
        "headless omitted target no longer addresses its sole page"
    );
    error_code(&call(client, AGENT_CONTROL_TOOL, json!({"target": "chrome", "command": "inspect", "params": {"root": null, "max_depth": 40}})).await?, "unknownTarget")?;
    println!("HEADLESS TARGET PASS: sole-page discovery, legacy dispatch, no chrome redirection");
    Ok(())
}

pub async fn inspection_off(socket: &Path, descriptor_path: &Path) -> Result<()> {
    let mut client = Client::connect(socket).await?;
    let chrome = tree(&mut client, None).await?;
    act(
        &mut client,
        &chrome,
        "click",
        json!({"node_id": node(&chrome, "chuzz-settings")?.id}),
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let off = loop {
        let chrome = tree(&mut client, None).await?;
        if let Ok(off) = node(&chrome, "chuzz-agent-control-off") {
            ensure!(
                off.enabled,
                "inspection is locked on; use a disposable GUI profile with the persisted setting enabled"
            );
            break (off.id, chrome);
        }
        ensure!(
            Instant::now() < deadline,
            "inspection-off settings control did not appear"
        );
        nagoya::sleep(Duration::from_millis(25)).await;
    };
    // Closing the transport can win over the final Ack; filesystem cleanup is the assertion.
    let _ = act(&mut client, &off.1, "click", json!({"node_id": off.0})).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while descriptor_path.exists() || socket.exists() {
        if Instant::now() >= deadline {
            bail!("inspection-off did not remove its socket and descriptor");
        }
        nagoya::sleep(Duration::from_millis(25)).await;
    }
    println!("INSPECTION OFF PASS: socket and descriptor removed");
    Ok(())
}
