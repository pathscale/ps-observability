//! The in-process transport: a request in, a response out, no socket.
//!
//! # Why there are two transports
//!
//! The core in [`crate::document`] answers a request against a document. How
//! the request arrives is a separate question, and it has two answers.
//!
//! [`crate::server`] is one of them: a Unix socket, MCP framing, a listener on
//! its own thread. That is how a QA harness and an agent outside the process
//! reach a running application, and it is the transport eleven fleet sites are
//! driven over.
//!
//! This is the other. An embedder that already holds the document calls
//! straight in. There is no socket to bind, no descriptor to publish, no
//! serialisation, and no second thread. AgencyZero is about to be both at once:
//! agents drive it over the socket while it drives a browser it embeds, in
//! process, through this.
//!
//! # What this adds over calling the core directly
//!
//! State that one request leaves for the next, and which the core deliberately
//! does not hold: the document revision an `Inspect` reports, the pointer
//! position an injected wheel event needs, which buttons an injected pointer is
//! holding down, and the reusable offscreen surface a capture draws into.
//!
//! It also holds the action dispatch itself. That existed twice, once in the
//! Tauri runtime and once in the headless browser, and the two had already
//! diverged: one replaced a text field's contents by selecting them and the
//! other by counting bytes, and only the second works with no font catalogue
//! installed. See [`DocumentControl::act`].

use blitz_dom::Document as _;
use blitz_script::ScriptDocument;
use blitz_traits::events::{
    BlitzImeEvent, BlitzInputEvent, BlitzWheelDelta, BlitzWheelEvent, DomEvent, DomEventData,
    MouseEventButton, MouseEventButtons, Point, UiEvent,
};
use keyboard_types::{Code, Key};

use crate::document::{
    activate_agent_node, control_error, debug_error, element_attr, hover_agent_node,
    inspect_document, key_event, keyboard_modifiers, resolve_agent_node,
};
use crate::{
    AgentAction, AgentControlRequest, DebugError, DebugResponse, InputCommand, KeyPhase,
    PointerPhase,
};

/// How many polls an action gets to settle before it is reported as unsettled.
///
/// DOM event handlers enqueue a reactive framework.s work on the document poll
/// hook. Acknowledging before that hook runs makes the next `Inspect` observe
/// the tree from before the click, and a tight harness loop keeps reading that
/// stale tree until it disconnects.
///
/// Timers and animation frames remain asynchronous and are observed normally.
/// A poll hook that stays runnable is different: reporting the exhaustion lets
/// a caller fail the interaction instead of compensating with a sleep.
pub(crate) const MAX_SETTLE_POLLS: usize = 100;

/// One document, driven in process.
///
/// Not `Send`: a `blitz-dom` document is not, and neither is anything that
/// carries one. The socket transport crosses that boundary with a channel
/// rather than by making the document shareable, which is why
/// [`crate::server::ControlBridge`] is a closure returning a receiver.
#[derive(Default)]
pub struct DocumentControl {
    revision: u64,
    /// Where the last action left the pointer.
    ///
    /// A wheel event carries no coordinates and is delivered to whatever the
    /// document last saw hovered. Without this, injected scrolling had no
    /// target and moved nothing, which reads as an accepted command that did
    /// not happen.
    pointer: (f32, f32),
    buttons: MouseEventButtons,
    #[cfg(feature = "capture")]
    capture: crate::document::DocumentCapture,
}

impl DocumentControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// The revision the last `Inspect` or action reported.
    ///
    /// A host that also serves diagnostics stamps its snapshots with this, so
    /// the two surfaces agree about which version of the document a client is
    /// looking at.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Advance the revision without answering a request.
    ///
    /// A diagnostic snapshot is a second reader of the same document and
    /// carries the same counter.
    pub fn next_revision(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    /// Where an injected pointer currently is.
    pub fn pointer(&self) -> (f32, f32) {
        self.pointer
    }

    /// Answer one agent-control request.
    ///
    /// `Inspect` reads the tree. `Act` performs the action, settles the
    /// synchronous script work it caused, and acknowledges. `Relaunch` and
    /// `Quit` are refused here on purpose: they are about the process, not the
    /// document, and only the embedder that started it can honour them.
    pub fn agent(
        &mut self,
        document: &mut ScriptDocument,
        request: AgentControlRequest,
    ) -> DebugResponse {
        match request {
            AgentControlRequest::Inspect { root, max_depth } => {
                self.revision += 1;
                inspect_document(document, root, max_depth, self.revision)
            }
            AgentControlRequest::Act(action) => match self.act(document, action) {
                Ok(()) => match settle(document) {
                    Ok(()) => {
                        self.revision += 1;
                        DebugResponse::Ack
                    }
                    Err(error) => DebugResponse::Error(error),
                },
                Err(error) => DebugResponse::Error(error),
            },
            AgentControlRequest::Relaunch | AgentControlRequest::Quit => control_error(
                "unsupportedRequest",
                "the process lifecycle belongs to the embedder, not to the document",
            ),
        }
    }

    /// Perform one action, without settling and without acknowledging.
    ///
    /// Separate from [`Self::agent`] because settling is the host's business:
    /// a windowed runtime drains the poll hook, and a headless host that owns
    /// an animation clock has to advance that too before the document is
    /// stable. Both want the same actions.
    pub fn act(
        &mut self,
        document: &mut ScriptDocument,
        action: AgentAction,
    ) -> Result<(), DebugError> {
        match action {
            AgentAction::Focus { node_id } => {
                crate::document::focus_agent_node(document, blitz_dom::NodeId::from_u64(node_id))?;
            }
            AgentAction::Click { node_id } => {
                self.pointer = activate_agent_node(document, node_id, 1)?;
            }
            AgentAction::DoubleClick { node_id } => {
                self.pointer = activate_agent_node(document, node_id, 2)?;
            }
            AgentAction::Hover { node_id } => {
                // Resolved first so the pointer is recorded even though the
                // move is dispatched at the node rather than by coordinate.
                self.pointer = resolve_agent_node(document, node_id)?.1;
                hover_agent_node(document, node_id)?;
            }
            AgentAction::SetValue { node_id, value } => {
                set_node_value(document, blitz_dom::NodeId::from_u64(node_id), value)?;
            }
            AgentAction::ScrollIntoView { node_id } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                if document.inner().get_node(node_id).is_none() {
                    return Err(debug_error("unknownNode", "node does not exist"));
                }
                let mut events = Vec::new();
                document
                    .inner_mut()
                    .scroll_to_node_centered_with_events(node_id, |event| events.push(event));
                for event in events {
                    document.dispatch_dom_event(event);
                }
            }
            AgentAction::ScrollBy {
                node_id,
                delta_x,
                delta_y,
            } => {
                let node_id = blitz_dom::NodeId::from_u64(node_id);
                if document.inner().get_node(node_id).is_none() {
                    return Err(debug_error("unknownNode", "node does not exist"));
                }
                let mut events = Vec::new();
                document
                    .inner_mut()
                    .scroll_nearest_container_by_with_events(node_id, delta_x, delta_y, |event| {
                        events.push(event)
                    });
                for event in events {
                    document.dispatch_dom_event(event);
                }
            }
            AgentAction::Input(input) => self.input(document, input)?,
        }
        Ok(())
    }

    fn input(
        &mut self,
        document: &mut ScriptDocument,
        input: InputCommand,
    ) -> Result<(), DebugError> {
        match input {
            InputCommand::Key {
                phase,
                key,
                code,
                modifiers,
            } => {
                let parsed_key = key.parse::<Key>().unwrap_or(Key::Character(key));
                let parsed_code = code.parse::<Code>().unwrap_or(Code::Unidentified);
                let event = key_event(
                    phase,
                    parsed_key,
                    parsed_code,
                    keyboard_modifiers(modifiers),
                );
                document.handle_ui_event(match phase {
                    KeyPhase::Down => UiEvent::KeyDown(event),
                    KeyPhase::Up => UiEvent::KeyUp(event),
                });
            }
            InputCommand::Pointer {
                phase,
                x,
                y,
                button,
                modifiers,
            } => {
                let button = mouse_button(button)?;
                self.pointer = (x as f32, y as f32);
                match phase {
                    PointerPhase::Down => self.buttons.insert(button.into()),
                    PointerPhase::Up | PointerPhase::Cancel => self.buttons.remove(button.into()),
                    PointerPhase::Move => {}
                }
                let event = crate::document::pointer_event_for_document(
                    document,
                    self.pointer,
                    button,
                    self.buttons,
                    keyboard_modifiers(modifiers),
                );
                document.handle_ui_event(match phase {
                    PointerPhase::Move => UiEvent::PointerMove(event),
                    PointerPhase::Down => UiEvent::PointerDown(event),
                    PointerPhase::Up => UiEvent::PointerUp(event),
                    PointerPhase::Cancel => UiEvent::PointerCancel(event),
                });
            }
            InputCommand::Wheel {
                delta_x,
                delta_y,
                modifiers,
                ..
            } => {
                let event = BlitzWheelEvent {
                    delta: BlitzWheelDelta::Pixels(delta_x, delta_y),
                    coords: crate::document::pointer_coords_for_document(document, self.pointer),
                    buttons: self.buttons,
                    mods: keyboard_modifiers(modifiers),
                    element: Point::default(),
                };
                // A wheel event targets the hovered node, and hover is resolved
                // by the shell from real cursor movement. An injected pointer
                // move never touches it, so an injected wheel had no target and
                // scrolled nothing, making remote wheel input look accepted
                // while the document remained unchanged.
                document
                    .inner_mut()
                    .set_hover_to(self.pointer.0, self.pointer.1);
                document.handle_ui_event(UiEvent::Wheel(event));
            }
        }
        Ok(())
    }

    /// Draw the document offscreen, reusing this control's surface.
    #[cfg(feature = "capture")]
    pub fn capture(
        &mut self,
        document: &mut ScriptDocument,
        request: crate::CaptureRequest,
    ) -> Result<crate::CapturedImage, DebugError> {
        request.validate().map_err(|why| DebugError {
            code: "invalidArgument".into(),
            message: why.into(),
        })?;
        self.capture.capture(document, request)
    }
}

/// Drain the synchronous script work an action caused.
pub fn settle(document: &mut ScriptDocument) -> Result<(), DebugError> {
    for _ in 0..MAX_SETTLE_POLLS {
        if !document.poll(None) {
            document.inner_mut().resolve(0.0);
            return Ok(());
        }
    }
    document.inner_mut().resolve(0.0);
    Err(debug_error(
        "actionDidNotSettle",
        "the action kept synchronous script work runnable past the settlement budget",
    ))
}

/// Replace a text input's contents.
///
/// # Why this counts bytes instead of selecting the text
///
/// The other implementation of this called `select_all` first. That builds its
/// selection with `move_lines(&layout, isize::MAX)` and resolves its ends
/// through `Cursor::from_byte_index(&layout, ..)`, so both halves read the laid
/// out text and both depend on a font catalogue being present. With none
/// registered every glyph shapes to nothing, the selection comes back
/// collapsed, and the commit below inserts at the caret instead of replacing:
/// the typed string lands after the old one.
///
/// `delete_bytes_before_selection` and `delete_bytes_after_selection` clamp to
/// the ends of the buffer, so between them they empty it from wherever the
/// caret is, with no layout involved. A host that behaves differently depending
/// on which fonts the machine has is a harness that reports different verdicts
/// on CI and on a laptop.
pub(crate) fn set_node_value(
    document: &mut ScriptDocument,
    node_id: blitz_dom::NodeId,
    value: String,
) -> Result<(), DebugError> {
    let current = document
        .inner()
        .get_node(node_id)
        .and_then(|node| node.element_data())
        .and_then(|element| element.text_input_data())
        .map(|input| input.editor.text().to_string());
    let Some(current) = current else {
        let is_value_input = document
            .inner()
            .get_node(node_id)
            .and_then(|node| node.element_data())
            .is_some_and(|element| {
                element.name.local.as_ref() == "input"
                    && matches!(
                        element_attr(element, "type").unwrap_or("text"),
                        "date" | "datetime-local" | "month" | "time" | "week" | "color"
                    )
            });
        if !is_value_input {
            return Err(debug_error("notEditable", "node is not a text input"));
        }

        document.inner_mut().set_focus_to(node_id);
        document.inner_mut().mutate().set_attribute(
            node_id,
            blitz_dom::qual_name!("value"),
            &value,
        );
        document.dispatch_dom_event(DomEvent::new(
            node_id,
            DomEventData::Input(BlitzInputEvent { value }),
        ));
        return Ok(());
    };
    document.inner_mut().set_focus_to(node_id);
    if let Some(len) = std::num::NonZeroUsize::new(current.len()) {
        document.inner_mut().with_text_input(node_id, |mut editor| {
            editor.delete_bytes_before_selection(len);
            editor.delete_bytes_after_selection(len);
        });
    }
    document.handle_ui_event(UiEvent::Ime(BlitzImeEvent::Commit(value)));
    Ok(())
}

fn mouse_button(button: u16) -> Result<MouseEventButton, DebugError> {
    match button {
        0 => Ok(MouseEventButton::Main),
        1 => Ok(MouseEventButton::Auxiliary),
        2 => Ok(MouseEventButton::Secondary),
        3 => Ok(MouseEventButton::Fourth),
        4 => Ok(MouseEventButton::Fifth),
        _ => Err(debug_error(
            "unsupportedButton",
            "pointer button must be 0 through 4",
        )),
    }
}

#[cfg(test)]
mod tests {
    use blitz_dom::{Document as _, DocumentConfig};

    use super::*;
    use crate::{AgentSnapshot, Modifiers};

    /// A page with something to press, something to type into, and something
    /// to scroll, driven with no socket anywhere.
    fn document() -> ScriptDocument {
        let mut document = ScriptDocument::from_html(
            r#"<style>
                 #scroller { height: 40px; overflow: auto; }
                 #tall { height: 400px; }
               </style>
               <button id="press">Press me</button>
               <input id="field" value="before">
               <input id="date" type="date" value="2025-01-01">
               <div id="scroller"><div id="tall">tall</div></div>
               <output id="log"></output>
               <output id="date-log"></output>
               <script>
                 document.getElementById('press').addEventListener('click', () => {
                   document.getElementById('log').textContent = 'pressed';
                 });
                 document.getElementById('date').addEventListener('input', event => {
                   document.getElementById('date-log').textContent = event.target.value;
                 });
               </script>"#,
            DocumentConfig::default(),
        );
        // A viewport, or every box lays out at zero width and a pointer has
        // nowhere to land.
        document
            .inner_mut()
            .set_viewport(blitz_traits::shell::Viewport::new(
                800,
                600,
                1.0,
                blitz_traits::shell::ColorScheme::Light,
            ));
        document.inner_mut().resolve(0.0);
        settle(&mut document).expect("the fixture settles");
        document
    }

    fn node(control: &mut DocumentControl, document: &mut ScriptDocument, dom_id: &str) -> u64 {
        let DebugResponse::AgentSnapshot(AgentSnapshot { nodes, .. }) = control.agent(
            document,
            AgentControlRequest::Inspect {
                root: None,
                max_depth: 0,
            },
        ) else {
            panic!("inspection did not answer with a tree")
        };
        nodes
            .iter()
            .find(|node| node.dom_id.as_deref() == Some(dom_id))
            .unwrap_or_else(|| panic!("the fixture has no #{dom_id}"))
            .id
    }

    /// What the document says an element holds, read straight from the tree
    /// rather than through the semantic surface. The point of these tests is
    /// that an action happened, not what the surface calls the result.
    fn text(document: &ScriptDocument, selector: &str) -> String {
        let id = document
            .inner()
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("the fixture has no {selector}"));
        document.inner().get_node(id).unwrap().text_content()
    }

    /// The whole point of the second transport: a full request and response
    /// cycle with no listener, no descriptor and no framing.
    #[test]
    fn an_embedder_drives_a_document_with_no_socket() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let press = node(&mut control, &mut document, "press");

        let response = control.agent(
            &mut document,
            AgentControlRequest::Act(AgentAction::Click { node_id: press }),
        );

        assert_eq!(response, DebugResponse::Ack);
        // The click ran the page's handler and the response waited for it, so
        // the very next read sees the consequence rather than the tree from
        // before.
        assert_eq!(text(&document, "#log"), "pressed");
    }

    #[test]
    fn a_revision_advances_with_every_answer() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let before = control.revision();
        let press = node(&mut control, &mut document, "press");
        control.agent(
            &mut document,
            AgentControlRequest::Act(AgentAction::Click { node_id: press }),
        );
        assert!(
            control.revision() > before,
            "a client watching the revision must see that something happened"
        );
    }

    /// Replacing a field's contents must not depend on the machine's fonts.
    ///
    /// This document has no font catalogue registered, which is the condition
    /// under which `select_all` silently selects nothing and the typed string
    /// is appended rather than substituted.
    #[test]
    fn setting_a_value_replaces_what_was_there_with_no_fonts_installed() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let field = node(&mut control, &mut document, "field");

        assert_eq!(
            control.agent(
                &mut document,
                AgentControlRequest::Act(AgentAction::SetValue {
                    node_id: field,
                    value: "after".into(),
                }),
            ),
            DebugResponse::Ack
        );

        let value = document
            .inner()
            .get_node(blitz_dom::NodeId::from_u64(field))
            .and_then(|node| node.element_data())
            .and_then(|element| element.text_input_data())
            .map(|input| input.editor.text().to_string())
            .expect("the field is a text input");
        assert_eq!(
            value, "after",
            "the old contents were appended to rather than replaced"
        );
    }

    /// Date and time controls expose values to accessibility clients even
    /// though Blitz does not give them a text editor. They still need the same
    /// observable SetValue contract as a text box: update the DOM value and
    /// deliver an input event to the application.
    #[test]
    fn setting_a_date_value_updates_the_dom_and_dispatches_input() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let date = node(&mut control, &mut document, "date");

        assert_eq!(
            control.agent(
                &mut document,
                AgentControlRequest::Act(AgentAction::SetValue {
                    node_id: date,
                    value: "2025-06-24".into(),
                }),
            ),
            DebugResponse::Ack
        );

        let value = document
            .inner()
            .get_node(blitz_dom::NodeId::from_u64(date))
            .and_then(|node| node.element_data())
            .and_then(|element| element_attr(element, "value").map(str::to_owned));
        assert_eq!(value.as_deref(), Some("2025-06-24"));
        assert_eq!(text(&document, "#date-log"), "2025-06-24");
    }

    /// Focus is a state change the protocol reports back, so this asserts the
    /// action landed rather than that it was acknowledged.
    ///
    /// Keyboard setup must not be a click: focusing a submit, delete or fork
    /// button by clicking it performs the action before the key under test is
    /// ever delivered.
    #[test]
    fn focus_moves_the_focused_node_without_activating_it() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let field = node(&mut control, &mut document, "field");

        assert_eq!(
            control.agent(
                &mut document,
                AgentControlRequest::Act(AgentAction::Focus { node_id: field }),
            ),
            DebugResponse::Ack
        );

        let DebugResponse::AgentSnapshot(snapshot) = control.agent(
            &mut document,
            AgentControlRequest::Inspect {
                root: None,
                max_depth: 0,
            },
        ) else {
            panic!("inspection did not answer with a tree")
        };
        assert_eq!(snapshot.focused_node, Some(field));
        assert_eq!(
            text(&document, "#log"),
            "",
            "focusing a control must not activate it"
        );
    }

    /// The refusals are part of the contract.
    ///
    /// A host that answered `Ack` to a request it did not perform is worse
    /// than one that says so: a check then reports the page as broken.
    #[test]
    fn the_process_lifecycle_is_refused_rather_than_acknowledged() {
        let mut document = document();
        let mut control = DocumentControl::new();
        for request in [AgentControlRequest::Relaunch, AgentControlRequest::Quit] {
            let DebugResponse::Error(error) = control.agent(&mut document, request) else {
                panic!("a document cannot restart its own process")
            };
            assert_eq!(error.code, "unsupportedRequest");
        }
    }

    #[test]
    fn an_unknown_node_is_an_error_and_not_a_panic() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let DebugResponse::Error(error) = control.agent(
            &mut document,
            AgentControlRequest::Act(AgentAction::ScrollIntoView {
                node_id: u64::MAX - 1,
            }),
        ) else {
            panic!("a node that is not there cannot be scrolled to")
        };
        assert_eq!(error.code, "unknownNode");
    }

    #[test]
    fn a_pointer_button_outside_the_range_is_refused() {
        let mut document = document();
        let mut control = DocumentControl::new();
        let DebugResponse::Error(error) = control.agent(
            &mut document,
            AgentControlRequest::Act(AgentAction::Input(InputCommand::Pointer {
                phase: PointerPhase::Down,
                x: 1.0,
                y: 1.0,
                button: 9,
                modifiers: Modifiers::default(),
            })),
        ) else {
            panic!("button 9 does not exist")
        };
        assert_eq!(error.code, "unsupportedButton");
    }
}
