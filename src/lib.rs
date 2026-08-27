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
//! buys: a process that owns a document, serves inspection and paints nothing.
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
//! QA_HOST_DIST=/path/to/one/components/dist qa-headless-host
//! ```
//!
//! It prints its descriptor path on stdout when it is ready, then serves until
//! killed. `ps-qa sweep-components` launches one of these per component and
//! waits for that line.

use blitz_dom::Document;
use brotli::Decompressor;
use blitz_dom::DocumentConfig;
use blitz_script::{DefaultScriptFetcher, FetchError, ScriptDocument, ScriptFetcher};
use std::fs;
use std::io::Read;
use url::Url;

fn trace(message: &str) {
    eprintln!("qa-headless-host: {message}");
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
    let mut decoded = String::new();
    decoder
        .read_to_string(&mut decoded)
        .map_err(|error| format!("could not decompress embedded {label}: {error}"))?;
    Ok(decoded)
}

fn create_dist_document(dist: &std::path::Path, url: &str) -> Result<ScriptDocument, String> {
    fn asset_url<'a>(html: &'a str, attribute: &str) -> Result<&'a str, String> {
        let marker = format!("{attribute}=\"");
        let start = html
            .find(&marker)
            .map(|index| index + marker.len())
            .ok_or_else(|| format!("index.html has no {attribute} asset"))?;
        let end = html[start..]
            .find('"')
            .map(|index| start + index)
            .ok_or_else(|| format!("index.html has an unterminated {attribute} asset"))?;
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
        let relative = url.split('?').next().unwrap_or(url).trim_start_matches('/');
        let path = dist.join(relative);
        let bytes = fs::read(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        match decompress_utf8(&bytes, label) {
            Ok(text) => Ok(text),
            Err(compressed_error) => String::from_utf8(bytes).map_err(|_| compressed_error),
        }
    }

    trace(&format!("loading dist: {}", dist.display()));
    let index_path = dist.join("index.html");
    let index = fs::read_to_string(&index_path)
        .map_err(|error| format!("could not read {}: {error}", index_path.display()))?;
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
    use blitz_traits::shell::{ColorScheme, Viewport};
    use std::sync::mpsc;
    use tauri_runtime_blitz::control_protocol::{
        AgentAction, AgentControlRequest, DebugError, DebugResponse, InputCommand, KeyPhase,
    };
    use tauri_runtime_blitz::{
        AgentControlServer, ControlBridgeRequest, click_agent_node, inspect_document,
        press_agent_key,
    };

    fn dimension(variable: &str, default: u32) -> u32 {
        std::env::var(variable)
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    }

    let width = dimension("QA_HOST_WIDTH", 1344);
    let height = dimension("QA_HOST_HEIGHT", 900);

    trace("headless inspection started");
    let dist = std::env::var_os("QA_HOST_DIST")
        .ok_or_else(|| "QA_HOST_DIST is not set; point it at one built page".to_owned())?;
    let mut document = create_dist_document(std::path::Path::new(&dist), "tauri://localhost/")?;
    document
        .inner_mut()
        .set_viewport(Viewport::new(width, height, 1.0, ColorScheme::Dark));
    document.execute_scripts();

    // Let the page settle before anyone can ask about it. The fixture backend
    // resolves commands after 90 ms, so a client that attaches instantly would
    // otherwise inspect a document that has not mounted yet and see an empty
    // page, which reads as a component that renders nothing.
    for _ in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        document.eval("void 0");
        document.poll(None);
    }
    document.inner_mut().resolve(0.0);
    trace("headless document ready");

    /*
     * The bridge hands a request to this thread and waits for the answer.
     *
     * A `SyncSender` with a zero-capacity channel would rendezvous, but the
     * server thread must not block indefinitely if this loop has gone away, so
     * the reply travels on a per-request oneshot the caller owns.
     */
    let (request_tx, request_rx) = mpsc::channel::<(
        AgentControlRequest,
        std::sync::mpsc::Sender<DebugResponse>,
    )>();

    let bridge: tauri_runtime_blitz::ControlBridge = std::sync::Arc::new(move |request| {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        match request {
            ControlBridgeRequest::Agent(agent_request) => {
                let (reply_tx, reply_rx) = mpsc::channel();
                if request_tx.send((agent_request, reply_tx)).is_ok() {
                    if let Ok(reply) = reply_rx.recv() {
                        let _ = response_tx.send(reply);
                        return response_rx;
                    }
                }
                let _ = response_tx.send(DebugResponse::Error(DebugError {
                    code: "documentUnavailable".into(),
                    message: "the headless document is no longer serving".into(),
                }));
            }
            #[cfg(feature = "diagnostics")]
            ControlBridgeRequest::Diagnostics(_) => {
                let _ = response_tx.send(DebugResponse::Error(DebugError {
                    code: "diagnosticsUnavailable".into(),
                    message: "the headless host serves inspection only".into(),
                }));
            }
        }
        response_rx
    });

    let server = AgentControlServer::start(bridge)
        .map_err(|error| format!("could not host the control socket: {error}"))?;
    trace(&format!(
        "headless inspection listening: {}",
        server.descriptor_path().display()
    ));
    // The descriptor path on stdout, so a caller can attach without guessing
    // it. `ps-qa --app` takes a descriptor, and a sweep that has to search a
    // directory races every other instance on the machine.
    println!("{}", server.descriptor_path().display());
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    let mut revision = 0_u64;
    while let Ok((request, reply)) = request_rx.recv() {
        let response = match request {
            AgentControlRequest::Inspect { root, max_depth } => {
                revision += 1;
                inspect_document(&mut document, root, max_depth, revision)
            }
            AgentControlRequest::Act(AgentAction::Click { node_id }) => {
                match click_agent_node(&mut document, node_id, 1) {
                    Ok(_) => {
                        // Settle before acknowledging. A click that opens a menu
                        // needs the effect to have run before the next Inspect,
                        // or the check reads the tree from before its own action
                        // and reports that nothing happened.
                        for _ in 0..6 {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            document.eval("void 0");
                            document.poll(None);
                        }
                        document.inner_mut().resolve(0.0);
                        DebugResponse::Ack
                    }
                    Err(error) => DebugResponse::Error(error),
                }
            }
            AgentControlRequest::Act(AgentAction::DoubleClick { node_id }) => {
                match click_agent_node(&mut document, node_id, 2) {
                    Ok(_) => DebugResponse::Ack,
                    Err(error) => DebugResponse::Error(error),
                }
            }
            AgentControlRequest::Act(AgentAction::Input(InputCommand::Key {
                key, code, phase, ..
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
                        Ok(()) => {
                            for _ in 0..6 {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                document.eval("void 0");
                                document.poll(None);
                            }
                            document.inner_mut().resolve(0.0);
                            DebugResponse::Ack
                        }
                        Err(error) => DebugResponse::Error(error),
                    }
                }
            }
            AgentControlRequest::Quit => {
                let _ = reply.send(DebugResponse::Ack);
                break;
            }
            // Everything else needs runtime state this host does not have, and
            // saying so is better than a plausible-looking Ack: a check that
            // silently did nothing reports the component as broken.
            _ => DebugResponse::Error(DebugError {
                code: "unsupported".into(),
                message: "the headless host serves Inspect and Click only".into(),
            }),
        };
        if reply.send(response).is_err() {
            break;
        }
    }

    trace("headless inspection finished");
    Ok(())
}
