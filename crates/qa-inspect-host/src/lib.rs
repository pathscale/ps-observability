//! Host a Blitz document over the inspection socket, with no window.
//!
//! # Why this exists
//!
//! A component sweep drives one component at a time and asks what happens when
//! a control is pressed. Answering that needs a live document on the other end
//! of a socket: a screenshot says only that something painted, and a semantic
//! tree written to a file says only what was on screen at one instant, so every
//! check involving a click is undecidable against either.
//!
//! Hosting that socket used to require opening a window, because
//! `AgentControlServer::start` was private to the runtime. A sweep of 71
//! components then meant 71 windows over whatever the person at the machine was
//! doing. Nothing about the server needs a window, and this is what that fact
//! buys: a process that owns a document, serves inspection and only paints
//! offscreen when a visual assertion explicitly asks for pixels.
//!
//! # Why it is not part of ps-qa
//!
//! `ps-qa` is forbidden from depending on blitz, tauri, winit or wgpu, so that
//! driving a control does not build a browser engine. A host has to link a
//! renderer. They are two crates for that reason, talking over the socket.
//!
//! # Use
//!
//! ```sh
//! QA_INSPECT_PAGE=/path/to/one/components/dist qa-inspect-host
//! ```
//!
//! It prints its descriptor path on stdout when it is ready, then serves until
//! killed. `ps-qa sweep-components` launches one of these per component and
//! waits for that line.

use blitz_dom::Document;
use blitz_dom::DocumentConfig;
use blitz_script::{DefaultScriptFetcher, FetchError, ScriptDocument, ScriptFetcher};
use brotli::Decompressor;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use url::Url;

const MAX_DECOMPRESSED_ASSET_BYTES: u64 = 32 * 1024 * 1024;

fn trace(message: &str) {
    eprintln!("qa-inspect-host: {message}");
}

struct DistScriptFetcher {
    url: String,
    javascript: String,
}

impl ScriptFetcher for DistScriptFetcher {
    fn fetch(&self, url: &Url) -> Result<String, FetchError> {
        if url.as_str() == self.url {
            Ok(self.javascript.clone())
        } else {
            DefaultScriptFetcher.fetch(url)
        }
    }
}

fn decompress_utf8(compressed: &[u8], label: &str) -> Result<String, String> {
    let mut decoder = Decompressor::new(compressed, 4096);
    let mut decoded = Vec::new();
    decoder
        .by_ref()
        .take(MAX_DECOMPRESSED_ASSET_BYTES + 1)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("could not decompress embedded {label}: {error}"))?;
    if decoded.len() as u64 > MAX_DECOMPRESSED_ASSET_BYTES {
        return Err(format!(
            "decompressed {label} exceeds the {} MiB safety limit",
            MAX_DECOMPRESSED_ASSET_BYTES / (1024 * 1024)
        ));
    }
    String::from_utf8(decoded)
        .map_err(|error| format!("decompressed {label} is not UTF-8: {error}"))
}

fn asset_path(root: &Path, reference: &str) -> Result<PathBuf, String> {
    let reference = reference.split('?').next().unwrap_or(reference);
    let relative = Path::new(reference.trim_start_matches('/'));
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!(
            "asset path escapes the page directory: {reference:?}"
        ));
    }
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| format!("could not resolve asset root {}: {error}", root.display()))?;
    let candidate = fs::canonicalize(canonical_root.join(relative)).map_err(|error| {
        format!(
            "could not resolve asset {} below {}: {error}",
            relative.display(),
            canonical_root.display()
        )
    })?;
    if !candidate.starts_with(&canonical_root) {
        return Err(format!(
            "asset path escapes the page directory: {reference:?}"
        ));
    }
    Ok(candidate)
}

fn create_dist_document(dist: &std::path::Path, url: &str) -> Result<ScriptDocument, String> {
    fn asset_url<'a>(html: &'a str, attribute: &str) -> Result<&'a str, String> {
        let marker = format!("{attribute}=\"");
        let start = html
            .find(&marker)
            .map(|index| index + marker.len())
            .ok_or_else(|| format!("the page has no {attribute} asset"))?;
        let end = html[start..]
            .find('"')
            .map(|index| start + index)
            .ok_or_else(|| format!("the page has an unterminated {attribute} asset"))?;
        Ok(&html[start..end])
    }

    /*
     * Brotli or plain, decided by the bytes rather than by configuration.
     *
     * The capture path is fed a Brotli dist, and AgencyZero's own `dist` is
     * plain text; a harness dist is whatever its bundler emitted. Requiring one
     * of the two produced `could not decompress embedded external CSS: Invalid
     * Data` on a perfectly good stylesheet, and the page then rendered with no
     * styles at all, which reads as broken components rather than a rejected
     * asset.
     */
    fn read_brotli_asset(dist: &std::path::Path, url: &str, label: &str) -> Result<String, String> {
        let path = asset_path(dist, url)?;
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        match decompress_utf8(&bytes, label) {
            Ok(text) => Ok(text),
            Err(compressed_error) => String::from_utf8(bytes).map_err(|_| compressed_error),
        }
    }

    /*
     * A page, or a directory holding one.
     *
     * Pointing this at a directory and demanding `index.html` inside it forced
     * every consumer to reshape its build first: a bundler that emits one page
     * per component (`button.html` beside `button.js`) has no `index.html` at
     * all, so the QA harness carried a `stage.ts` whose whole job was copying
     * one page into a throwaway directory under a different name. Accepting the
     * page directly deletes that step from every project.
     *
     * Assets resolve against the page's own directory, which is where a
     * bundler's relative `src=` and `href=` already point.
     */
    let (page_path, asset_root) = if dist.is_dir() {
        (dist.join("index.html"), dist.to_path_buf())
    } else {
        let parent = dist
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", dist.display()))?;
        (dist.to_path_buf(), parent.to_path_buf())
    };
    let dist = asset_root.as_path();

    trace(&format!("loading page: {}", page_path.display()));
    let index = fs::read_to_string(&page_path)
        .map_err(|error| format!("could not read {}: {error}", page_path.display()))?;
    let javascript_url = asset_url(&index, "src")?;
    let stylesheet_url = asset_url(&index, "href")?;
    let css = read_brotli_asset(dist, stylesheet_url, "external CSS")?;
    let javascript = read_brotli_asset(dist, javascript_url, "external JavaScript")?;
    /*
     * `data-theme` rides along from the source document. Every design token in
     * `@pathscale/ui` is defined under a `[data-theme=...]` selector, so a body
     * without one leaves `var(--color-base-100)` and friends unresolved: the
     * page renders, and every component in it is transparent and unconstrained.
     * That reads as broken components rather than a dropped attribute.
     */
    let theme = index
        .find("data-theme=\"")
        .map(|start| start + "data-theme=\"".len())
        .and_then(|start| {
            index[start..]
                .find('"')
                .map(|end| &index[start..start + end])
        })
        .unwrap_or("dark");
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><style>{css}</style></head><body data-theme=\"{theme}\"><div id=\"root\"></div><script src=\"{javascript_url}\"></script></body></html>"
    );
    let base_url = Url::parse(url).map_err(|error| format!("invalid base URL: {error}"))?;
    let script_url = base_url
        .join(javascript_url)
        .map_err(|error| format!("invalid JavaScript asset URL: {error}"))?
        .to_string();
    let config = DocumentConfig {
        base_url: Some(url.into()),
        ..DocumentConfig::default()
    };
    Ok(
        ScriptDocument::from_html(&html, config).with_fetcher(DistScriptFetcher {
            url: script_url,
            javascript,
        }),
    )
}

pub fn serve() -> Result<(), String> {
    use blitz_traits::events::{BlitzImeEvent, UiEvent};
    use blitz_traits::shell::{ColorScheme, Viewport};
    use std::sync::mpsc;
    #[cfg(feature = "diagnostics")]
    use tauri_runtime_blitz::control_protocol::DiagnosticsRequest;
    use tauri_runtime_blitz::control_protocol::{
        AgentAction, AgentControlRequest, DebugError, DebugEvent, DebugResponse, InputCommand,
        KeyPhase,
    };
    use tauri_runtime_blitz::{
        AgentControlServer, ControlBridgeRequest, DocumentCapture, click_agent_node,
        focus_agent_node, hover_agent_node, inspect_document, press_agent_key, snapshot_document,
    };

    fn dimension(variable: &str, default: u32) -> Result<u32, String> {
        let Some(value) = std::env::var_os(variable) else {
            return Ok(default);
        };
        let text = value
            .into_string()
            .map_err(|_| format!("{variable} is not valid UTF-8"))?;
        text.parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{variable} must be a positive integer, got {text:?}"))
    }

    // Drain synchronous script and reactive work without imposing a timer on
    // every control. Delayed outcomes are polled by ps-qa against the exact
    // declared verdict, so sleeping here only makes fast controls slow and
    // duplicates the caller's timeout.
    struct SettleFailure {
        error: DebugError,
        painted: bool,
    }

    fn settle_immediate(
        document: &mut ScriptDocument,
        clock: &std::time::Instant,
        deadline: std::time::Duration,
    ) -> Result<bool, SettleFailure> {
        let before = document.inner().paint_damage().generation;
        let started = std::time::Instant::now();
        let mut iterations = 0_u32;
        loop {
            if !document.poll(None) {
                break;
            }
            iterations = iterations.saturating_add(1);
            if started.elapsed() >= deadline {
                document.inner_mut().resolve(clock.elapsed().as_secs_f64());
                return Err(SettleFailure {
                    painted: document.inner().paint_damage().generation != before,
                    error: DebugError {
                        code: "documentNotQuiescent".into(),
                        message: format!(
                            "the document still had immediate work after {iterations} settle iterations and {}ms",
                            deadline.as_millis()
                        ),
                    },
                });
            }
        }
        document.inner_mut().resolve(clock.elapsed().as_secs_f64());
        Ok(document.inner().paint_damage().generation != before)
    }

    fn settle_response(
        document: &mut ScriptDocument,
        clock: &std::time::Instant,
        deadline: std::time::Duration,
        painted: &mut bool,
    ) -> DebugResponse {
        match settle_immediate(document, clock, deadline) {
            Ok(did_paint) => {
                *painted = did_paint;
                DebugResponse::Ack
            }
            Err(failure) => {
                *painted = failure.painted;
                DebugResponse::Error(failure.error)
            }
        }
    }

    fn commit_render(events: &tokio::sync::watch::Sender<Option<DebugEvent>>, revision: &mut u64) {
        *revision = revision.saturating_add(1);
        events.send_replace(Some(DebugEvent::PaintCommitted {
            revision: *revision,
        }));
    }

    let width = dimension("QA_HOST_WIDTH", 1344)?;
    let height = dimension("QA_HOST_HEIGHT", 900)?;
    let settle_deadline =
        std::time::Duration::from_millis(u64::from(dimension("QA_HOST_SETTLE_MS", 100)?));

    trace("inspection host started");
    let dist = std::env::var_os("QA_INSPECT_PAGE")
        .ok_or_else(|| "QA_INSPECT_PAGE is not set; point it at one built page".to_owned())?;
    let mut document = create_dist_document(std::path::Path::new(&dist), "tauri://localhost/")?;
    document
        .inner_mut()
        .set_viewport(Viewport::new(width, height, 1.0, ColorScheme::Dark));
    document.inner_mut().set_paint_damage_tracking(true);
    document.execute_scripts();

    // Script execution is synchronous; drain the reactive work it queued
    // before announcing the socket instead of sleeping for a fixed 800 ms.
    let animation_clock = std::time::Instant::now();
    if let Err(failure) = settle_immediate(&mut document, &animation_clock, settle_deadline) {
        trace(&format!(
            "initial document reached the settle deadline: {}",
            failure.error.message
        ));
    }
    trace("document ready");

    /*
     * The bridge hands a request to this thread and waits for the answer.
     *
     * A `SyncSender` with a zero-capacity channel would rendezvous, but the
     * server thread must not block indefinitely if this loop has gone away, so
     * the reply travels on a per-request oneshot the caller owns.
     */
    let (request_tx, request_rx) = mpsc::channel::<(
        ControlBridgeRequest,
        tokio::sync::oneshot::Sender<DebugResponse>,
    )>();

    let bridge: tauri_runtime_blitz::ControlBridge = std::sync::Arc::new(move |request| {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        match request_tx.send((request, response_tx)) {
            Ok(()) => response_rx,
            Err(error) => {
                let (_, response_tx) = error.0;
                let _ = response_tx.send(DebugResponse::Error(DebugError {
                    code: "documentUnavailable".into(),
                    message: "the document is no longer serving".into(),
                }));
                response_rx
            }
        }
    });

    let (render_events, render_event_receiver) = tokio::sync::watch::channel(None);
    let server = AgentControlServer::start_with_events(bridge, render_event_receiver)
        .map_err(|error| format!("could not host the control socket: {error}"))?;
    trace(&format!(
        "inspection socket listening: {}",
        server.descriptor_path().display()
    ));
    // The descriptor path on stdout, so a caller can attach without guessing
    // it. `ps-qa --app` takes a descriptor, and a sweep that has to search a
    // directory races every other instance on the machine.
    println!("{}", server.descriptor_path().display());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let mut revision = 0_u64;
    let mut render_revision = 0_u64;
    #[cfg(feature = "diagnostics")]
    let mut capture = DocumentCapture::new();
    while let Ok((request, reply)) = request_rx.recv() {
        let mut painted = false;
        let response = match request {
            ControlBridgeRequest::Agent(request) => match request {
                AgentControlRequest::Inspect { root, max_depth } => {
                    revision += 1;
                    inspect_document(&mut document, root, max_depth, revision)
                }
                AgentControlRequest::Act(AgentAction::Focus { node_id }) => {
                    let node_id = blitz_dom::NodeId::from_u64(node_id);
                    match focus_agent_node(&mut document, node_id) {
                        Ok(()) => settle_response(
                            &mut document,
                            &animation_clock,
                            settle_deadline,
                            &mut painted,
                        ),
                        Err(error) => DebugResponse::Error(error),
                    }
                }
                AgentControlRequest::Act(AgentAction::Click { node_id }) => {
                    match click_agent_node(&mut document, node_id, 1) {
                        Ok(_) => settle_response(
                            &mut document,
                            &animation_clock,
                            settle_deadline,
                            &mut painted,
                        ),
                        Err(error) => DebugResponse::Error(error),
                    }
                }
                AgentControlRequest::Act(AgentAction::ScrollIntoView { .. }) => {
                    /*
                     * Acknowledged rather than refused. A driver scrolls a control
                     * into view before hovering it, which is right for an
                     * application with a scrolling region and a no-op on a page
                     * holding one component: everything is already in view.
                     *
                     * Refusing it failed every hovering check with "unsupported"
                     * before the hover was ever attempted, which reads as a host
                     * that cannot hover rather than one that cannot scroll.
                     */
                    DebugResponse::Ack
                }
                AgentControlRequest::Act(AgentAction::Hover { node_id }) => {
                    /*
                     * A control revealed on hover is unreachable without this, and
                     * a defect that only shows on the second entry is unreachable
                     * even with one hover: a pill whose hover appends a shadow
                     * layer and never removes it looks right once.
                     */
                    match hover_agent_node(&mut document, node_id) {
                        Ok(_) => settle_response(
                            &mut document,
                            &animation_clock,
                            settle_deadline,
                            &mut painted,
                        ),
                        Err(error) => DebugResponse::Error(error),
                    }
                }
                AgentControlRequest::Act(AgentAction::DoubleClick { node_id }) => {
                    match click_agent_node(&mut document, node_id, 2) {
                        Ok(_) => settle_response(
                            &mut document,
                            &animation_clock,
                            settle_deadline,
                            &mut painted,
                        ),
                        Err(error) => DebugResponse::Error(error),
                    }
                }
                AgentControlRequest::Act(AgentAction::SetValue { node_id, value }) => {
                    let node_id = blitz_dom::NodeId::from_u64(node_id);
                    if !document
                        .inner()
                        .get_node(node_id)
                        .and_then(|node| node.element_data())
                        .is_some_and(|element| element.text_input_data().is_some())
                    {
                        DebugResponse::Error(DebugError {
                            code: "notEditable".into(),
                            message: "node is not a text input".into(),
                        })
                    } else {
                        document.inner_mut().set_focus_to(node_id);
                        document
                            .inner_mut()
                            .with_text_input(node_id, |mut editor| editor.select_all());
                        document.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Commit(value)));
                        settle_response(
                            &mut document,
                            &animation_clock,
                            settle_deadline,
                            &mut painted,
                        )
                    }
                }
                AgentControlRequest::Act(AgentAction::Input(InputCommand::Key {
                    key,
                    code,
                    phase,
                    ..
                })) => {
                    /*
                     * One press per Down, and nothing on the matching Up.
                     *
                     * `press_agent_key` sends both halves, because a control that
                     * acts on keyup never fires if only a keydown arrives. A client
                     * that sends the pair would otherwise press the key twice, and
                     * Escape pressed twice closes a menu and then whatever was
                     * behind it.
                     */
                    if matches!(phase, KeyPhase::Up) {
                        DebugResponse::Ack
                    } else {
                        match press_agent_key(&mut document, &key, &code) {
                            Ok(()) => settle_response(
                                &mut document,
                                &animation_clock,
                                settle_deadline,
                                &mut painted,
                            ),
                            Err(error) => DebugResponse::Error(error),
                        }
                    }
                }
                // Everything else needs runtime state this host does not have, and
                // saying so is better than a plausible-looking Ack: a check that
                // silently did nothing reports the component as broken.
                _ => DebugResponse::Error(DebugError {
                    code: "unsupported".into(),
                    message: "this host serves Inspect, Focus, Hover, Click, DoubleClick, SetValue and Key only".into(),
                }),
            },
            #[cfg(feature = "diagnostics")]
            ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Capture(request)) => {
                if !request.scale.is_finite() || !(0.25..=8.0).contains(&request.scale) {
                    DebugResponse::Error(DebugError {
                        code: "invalidArgument".into(),
                        message: "capture scale must be finite and between 0.25 and 8".into(),
                    })
                } else {
                    match capture.capture(&mut document, request) {
                        Ok(captured) => DebugResponse::Captured(captured),
                        Err(error) => DebugResponse::Error(error),
                    }
                }
            }
            #[cfg(feature = "diagnostics")]
            ControlBridgeRequest::Diagnostics(DiagnosticsRequest::Snapshot(request)) => {
                revision += 1;
                match snapshot_document(&mut document, request, revision) {
                    Ok(snapshot) => DebugResponse::Snapshot(snapshot),
                    Err(error) => DebugResponse::Error(error),
                }
            }
            #[cfg(feature = "diagnostics")]
            ControlBridgeRequest::Diagnostics(DiagnosticsRequest::WindowComposition) => {
                DebugResponse::WindowComposition(
                    tauri_runtime_blitz::control_protocol::WindowComposition::default(),
                )
            }
            #[cfg(feature = "diagnostics")]
            ControlBridgeRequest::Diagnostics(_) => DebugResponse::Error(DebugError {
                code: "unsupported".into(),
                message: "the headless host serves diagnostics Capture, Snapshot and WindowComposition only".into(),
            }),
        };
        if painted {
            commit_render(&render_events, &mut render_revision);
        }
        if reply.send(response).is_err() {
            break;
        }
    }

    trace("inspection host finished");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::asset_path;

    #[test]
    fn assets_cannot_escape_the_page_directory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!(
                "qa-host-assets-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is after the epoch")
                    .as_nanos()
            ));
        std::fs::create_dir(&root).expect("create fixture root");
        std::fs::write(root.join("inside.js"), "fixture").expect("write fixture asset");
        assert_eq!(
            asset_path(&root, "inside.js?cache=1").expect("local asset"),
            std::fs::canonicalize(root.join("inside.js")).unwrap()
        );
        assert!(asset_path(&root, "../outside.js").is_err());
        assert!(asset_path(&root, "/../../outside.js").is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
