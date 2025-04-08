//! In this crate, the concept of a "transport" abstracts away the details of how JSON-RPC messages are
//! sent and received.  In the MCP case specifically, the [specification on transports](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/)
//! describes two, one using stdio and one using a streamable HTTP connection.
//!
//! In this particular implementation, the transport complexity is up-stack in the MCP
//! implementation crates.  At the JSON RPC level, a transport is very simply a source of and sink
//! of JSON RPC messages.  This makes for a deceptively simple transport abstraction.
//!
//! You will note that there is no use of axum, warp, rocket, or any other Rust HTTP framework.  At
//! the level of the JSON RPC implementation, this is not necessary, because the JSON RPC framework
//! presumes that you have already established a TCP or stdio or HTTP SEE or whatever connection,
//! and can plug it into the JSON RPC client or server via the transport interface.
//!
//! For much more complexity at the transport level, see the MCP implementation crates.
use std::pin::Pin;

use crate::types;
use crate::{JsonRpcError, Result};
use async_trait::async_trait;
use futures::{FutureExt, TryFutureExt};

/// Anything that can be made into a transport can implement this trait to control the
/// creation of the transport itself.
///
/// Out of the box, this is implemented on some tokio and async types where it makes sense.
pub trait IntoTransport {
    type Transport: Transport;
    type Error: std::error::Error;

    /// Consume this object and return a transport that can be used to send and receive JSON-RPC
    fn try_into_transport(self) -> impl Future<Output = Result<Self::Transport, Self::Error>>;
}

/// A transport is a source and sink of JSON-RPC messages, that interfaces with some underlying
/// tranport layer provided by the caller.
///
/// At the transport layer, it's dealing with raw bytes which the framework will provide.  From the
/// transport's perspective these should be considered opaque.
pub trait Transport: Send + Sync + Sized + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Send a message to the transport.  Should not complete until the message has been handed off
    /// to the transport layer and transmitted to the remote peer, whatever that means for the
    /// partciular transport.
    ///
    /// This does not need to be cancelation-safe because the framework will wait until this future
    /// finishes before considering any messages "sent"
    fn send_message(
        &self,
        message: Vec<u8>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static;

    /// Receive a message from the transport.
    ///
    /// The transport can implement a timeout such that the request fails after some time, if that
    /// makes sense.  The framework does not implement any timeout of its own since timeouts are
    /// often transport-dependent.
    ///
    /// This future should be cancelation-safe, such that if the returned future is dropped before
    /// it completes, it will not result in incomplete messages being received or in missed
    /// messages
    ///
    /// This should return `Ok(None)` if the transport is closed and no more messages can be
    /// received.
    fn receive_message(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send + 'static;
}

/// Internal dyn-compatible wrapper trate around [`Transport`] to erase the types and allow dynamic
/// dispatch, hopefully without dire performance conseqsuences
trait BoxedTransport {
    fn send_message(&self, message: Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
    fn receive_message(&self) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>>> + Send + 'static>>;
}

impl<T> BoxedTransport for T
where
    T: Transport + 'static,
{
    fn send_message(&self, message: Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        <Self as Transport>::send_message(self, message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }

    fn receive_message(&self) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>>> + Send + 'static>> {
        <Self as Transport>::receive_message(self)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }
}

pub struct Peer {
    transport: Box<dyn BoxedTransport>,
}

impl Peer {
    pub(crate) fn new(transport: impl Transport) -> Self {
        Self {
            transport: Box::new(transport),
        }
    }

    async fn send_message(self, message: types::Message) -> Result<()> {
        self.transport.send_message(message.into_bytes()?).await
    }

    async fn receive_message(self) -> Result<Option<types::Message>> {
        let message = self.transport.receive_message().await?;

        if let Some(message) = message {
            Ok(Some(types::Message::from_bytes(&message)?))
        } else {
            Ok(None)
        }
    }
}
