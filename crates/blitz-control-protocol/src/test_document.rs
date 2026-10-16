use std::task::Context;

use blitz_dom::{
    DocGuard, DocGuardMut, Document, DocumentConfig, EventDriver, EventHandler, NodeId,
};
use blitz_html::HtmlDocument;
use blitz_traits::events::{BlitzPointerEvent, DomEvent, EventState, UiEvent};

type ListenerCallback =
    Box<dyn FnMut(&mut blitz_dom::BaseDocument, &DomEvent, &mut EventState) + 'static>;
type PollHook = Box<dyn FnMut(&mut dyn Document, Option<&Context<'_>>) -> bool>;

struct Listener {
    node: NodeId,
    event_name: String,
    callback: ListenerCallback,
}

/// A native event fixture for control protocol tests.
///
/// This fixture parses HTML and uses the DOM event driver for event ordering
/// and default actions. Its explicitly registered Rust callbacks stand in for
/// page listeners; it does not parse or execute JavaScript.
pub(crate) struct TestDocument {
    document: HtmlDocument,
    listeners: Vec<Listener>,
    poll_hook: Option<PollHook>,
}

impl TestDocument {
    pub(crate) fn from_html(html: &str, config: DocumentConfig) -> Self {
        Self {
            document: HtmlDocument::from_html(html, config),
            listeners: Vec::new(),
            poll_hook: None,
        }
    }

    pub(crate) fn on(
        &mut self,
        selector: &str,
        event_name: &str,
        callback: impl FnMut(&mut blitz_dom::BaseDocument, &DomEvent, &mut EventState) + 'static,
    ) {
        let node = self
            .document
            .inner()
            .query_selector(selector)
            .unwrap()
            .unwrap_or_else(|| panic!("the fixture has no {selector}"));
        self.listeners.push(Listener {
            node,
            event_name: event_name.to_owned(),
            callback: Box::new(callback),
        });
    }

    pub(crate) fn set_poll_hook(
        &mut self,
        hook: impl FnMut(&mut dyn Document, Option<&Context<'_>>) -> bool + 'static,
    ) {
        self.poll_hook = Some(Box::new(hook));
    }

    fn with_event_driver<R>(
        &mut self,
        f: impl FnOnce(&mut EventDriver<'_, NativeEventHandler<'_>>) -> R,
    ) -> R {
        let Self {
            document,
            listeners,
            ..
        } = self;
        let handler = NativeEventHandler { listeners };
        let mut driver = EventDriver::new(document, handler);
        f(&mut driver)
    }
}

struct NativeEventHandler<'a> {
    listeners: &'a mut [Listener],
}

impl EventHandler for NativeEventHandler<'_> {
    fn handle_event(
        &mut self,
        chain: &[NodeId],
        event: &mut DomEvent,
        document: &mut dyn Document,
        event_state: &mut EventState,
    ) {
        for node in chain {
            for listener in self
                .listeners
                .iter_mut()
                .filter(|listener| listener.node == *node && listener.event_name == event.name())
            {
                let mut inner = document.inner_mut();
                (listener.callback)(&mut inner, event, event_state);
            }
            if !event.bubbles || event_state.propagation_is_stopped() {
                break;
            }
        }
    }
}

impl Document for TestDocument {
    fn inner(&self) -> DocGuard<'_> {
        self.document.inner()
    }

    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        self.document.inner_mut()
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        self.with_event_driver(|driver| driver.handle_ui_event(event));
    }

    fn poll(&mut self, context: Option<Context<'_>>) -> bool {
        let Some(mut hook) = self.poll_hook.take() else {
            return false;
        };
        let result = hook(self, context.as_ref());
        self.poll_hook = Some(hook);
        result
    }
}

impl crate::ControlScriptBackend for TestDocument {
    fn dispatch_dom_event(&mut self, event: DomEvent) {
        self.with_event_driver(|driver| driver.handle_dom_event(event));
    }

    fn handle_pointer_move_to_node(&mut self, event: BlitzPointerEvent, node: NodeId) {
        self.with_event_driver(|driver| {
            driver.handle_pointer_move_to_node(&event, node);
        });
    }

    fn handle_ui_event_to_node(&mut self, event: UiEvent, node: NodeId) {
        self.with_event_driver(|driver| driver.handle_ui_event_to_node(event, node));
    }
}
