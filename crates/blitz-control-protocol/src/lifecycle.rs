//! The embedder's answer to a request about the process, not the document.
//!
//! [`crate::in_process::DocumentControl`] refuses `Relaunch` and `Quit`, and
//! that refusal is correct: a document cannot restart the program it is being
//! displayed by. Somebody above the document has to answer, and which of them
//! it is depends on the application rather than on the transport. AgencyZero
//! drains its persistence layer and hands over to a restart supervisor; a plain
//! host re-executes itself; a headless one has nothing to relaunch and says so.
//!
//! This is where that answer is registered. It lives beside the vocabulary
//! rather than inside a host, because more than one host serves the same
//! request: the Tauri runtime does it for a window today, and an embedder that
//! drives a browser in process will do it for that browser next. It used to
//! live in `tauri-runtime-blitz`, which meant an application registered a
//! handler for an inspection service through a window runtime, and named the
//! protocol's types through that runtime's re-export of this crate. Depending
//! on a renderer to answer "restart yourself" is the edge this removes.
//!
//! Nothing here needs a feature. The handler is one `Arc` and a lock, and the
//! types it names are the vocabulary, so a consumer that registers one pays
//! serde and nothing else: no engine, no socket, no window.
//!
//! # One per process
//!
//! Deliberately global, and deliberately last-writer-wins. A process has one
//! lifecycle, so a second registration is a second opinion about the same
//! question rather than a second subject to ask it about. Registering from a
//! test therefore disturbs any other test in the same binary that reads it;
//! [`clear_lifecycle_handler`] exists so such a test can put the process back.

use std::sync::{Arc, OnceLock, RwLock};

use crate::{AgentControlRequest, DebugResponse};

/// What an embedder installs to answer a lifecycle request.
pub type LifecycleHandler = dyn Fn(AgentControlRequest) -> DebugResponse + Send + Sync + 'static;

static LIFECYCLE_HANDLER: OnceLock<RwLock<Option<Arc<LifecycleHandler>>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<Arc<LifecycleHandler>>> {
    LIFECYCLE_HANDLER.get_or_init(|| RwLock::new(None))
}

/// Install the embedder's answer to `Relaunch` and `Quit`.
///
/// The handler runs on whichever thread the host serves control requests from,
/// which for a windowed host is the UI thread. It must not block: a host that
/// takes its time here stops answering every other request, including the
/// `Inspect` a client is waiting on.
pub fn set_lifecycle_handler(
    handler: impl Fn(AgentControlRequest) -> DebugResponse + Send + Sync + 'static,
) {
    *slot().write().unwrap() = Some(Arc::new(handler));
}

/// The installed handler, if an embedder installed one.
///
/// A host calls this rather than holding the handler, because the embedder may
/// install it after the host has started listening: AgencyZero registers during
/// Tauri's `setup`, which runs after the runtime is built.
pub fn lifecycle_handler() -> Option<Arc<LifecycleHandler>> {
    slot().read().unwrap().clone()
}

/// Forget the installed handler, leaving the host's own behaviour.
pub fn clear_lifecycle_handler() {
    *slot().write().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DebugError;

    /// One process, one lifecycle: these share the static and must not run at
    /// the same time as each other.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn an_embedder_answers_relaunch_in_place_of_the_host() {
        let _guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        clear_lifecycle_handler();
        assert!(lifecycle_handler().is_none());

        set_lifecycle_handler(|request| match request {
            AgentControlRequest::Relaunch => DebugResponse::Ack,
            _ => DebugResponse::Error(DebugError {
                code: "unsupportedEmbedderAction".into(),
                message: "only relaunch is delegated".into(),
            }),
        });

        let handler = lifecycle_handler().expect("a handler was installed");
        assert_eq!(handler(AgentControlRequest::Relaunch), DebugResponse::Ack);
        assert!(matches!(
            handler(AgentControlRequest::Quit),
            DebugResponse::Error(error) if error.code == "unsupportedEmbedderAction"
        ));

        clear_lifecycle_handler();
        assert!(lifecycle_handler().is_none());
    }

    #[test]
    fn the_last_registration_is_the_one_that_answers() {
        let _guard = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        clear_lifecycle_handler();

        set_lifecycle_handler(|_| DebugResponse::Ack);
        set_lifecycle_handler(|_| {
            DebugResponse::Error(DebugError {
                code: "second".into(),
                message: "the later registration wins".into(),
            })
        });

        let handler = lifecycle_handler().expect("a handler was installed");
        assert!(matches!(
            handler(AgentControlRequest::Relaunch),
            DebugResponse::Error(error) if error.code == "second"
        ));

        clear_lifecycle_handler();
    }
}
