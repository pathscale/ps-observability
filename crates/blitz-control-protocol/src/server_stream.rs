//! Concrete futures for the server task, preserving MessageStream semantics.

use crate::server::MessageStream;
use endpoint_libs::libs::ws::transport::{Transport, TransportStream};
use endpoint_libs::libs::ws::{StreamError, WireMessage};
use futures::{SinkExt, StreamExt};

// MessageStream uses async_trait(?Send) for UI callers. The shared nagoya pool
// needs concrete Send futures; use the same underlying Sink and Stream calls
// as TransportStream's implementation, without that trait's boxed future.
pub(crate) struct ServerStream<T: MessageStream>(pub(crate) T);

impl<T> ServerStream<TransportStream<T>>
where
    T: Transport<WireMessage, WireMessage> + Unpin + Send,
{
    pub(crate) async fn send(&mut self, message: WireMessage) -> Result<(), StreamError> {
        self.0
            .0
            .send(message)
            .await
            .map_err(|error| StreamError::Other(error.into()))
    }

    pub(crate) async fn recv(&mut self) -> Option<Result<WireMessage, StreamError>> {
        self.0
            .0
            .next()
            .await
            .map(|result| result.map_err(|error| StreamError::Other(error.into())))
    }
}
