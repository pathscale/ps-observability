//! Finding a running inspector, and talking to it.
//!
//! The implementation is [`blitz_control_protocol::client`]. It was here, and
//! it moved to the crate that defines the protocol so that there is one client
//! rather than one per consumer: the browser had grown a second one that built
//! its requests as untyped JSON by hand.
//!
//! Re-exported under the name this crate has always used, because that name is
//! what the modules below it call the transport, and renaming 300 call sites
//! would bury the move it is meant to make visible.

pub use blitz_control_protocol::client::*;
