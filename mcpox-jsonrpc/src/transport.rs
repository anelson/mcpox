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
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;

use crate::types;
use crate::{JsonRpcError, Result};
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};

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
pub trait Transport: Send + Sized + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Transport-specific identifier of the remote peer, useful for logging and debugging.
    fn remote_peer(&self) -> Cow<'static, str>;

    /// Send a message to the transport.  Should not complete until the message has been handed off
    /// to the transport layer and transmitted to the remote peer, whatever that means for the
    /// partciular transport.
    ///
    /// This does not need to be cancelation-safe because the framework will wait until this future
    /// finishes before considering any messages "sent"
    fn send_message(&mut self, message: Vec<u8>)
    -> impl Future<Output = Result<(), Self::Error>> + Send + '_;

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
    fn receive_message(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send + '_;
}

/// Internal dyn-compatible wrapper trate around [`Transport`] to erase the types and allow dynamic
/// dispatch, hopefully without dire performance conseqsuences
trait BoxedTransport: Send + 'static {
    fn remote_peer(&self) -> Cow<'static, str>;
    fn send_message(&mut self, message: Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn receive_message(&mut self) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>>> + Send + '_>>;
}

impl<T> BoxedTransport for T
where
    T: Transport + 'static,
{
    fn remote_peer(&self) -> Cow<'static, str> {
        <Self as Transport>::remote_peer(self)
    }

    fn send_message(&mut self, message: Vec<u8>) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        <Self as Transport>::send_message(self, message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }

    fn receive_message(&mut self) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>>> + Send + '_>> {
        <Self as Transport>::receive_message(self)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }
}

/// Implement transport on some std lib types, mainly for testing.  Production impls will be
/// provided outside of this crate
impl<In, InErr, Out> Transport for (In, Out)
where
    In: Stream<Item = Result<Vec<u8>, InErr>> + Unpin + Send + 'static,
    InErr: std::error::Error + Send + Sync + 'static,
    Out: Sink<Vec<u8>> + Unpin + Send + 'static,
    Out::Error: std::error::Error + Send + Sync + 'static,
{
    type Error = JsonRpcError;

    fn remote_peer(&self) -> Cow<'static, str> {
        format!(
            "({}, {})",
            std::any::type_name::<In>(),
            std::any::type_name::<Out>(),
        )
        .into()
    }

    fn send_message(&mut self, message: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.1
            .send(message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
    }

    fn receive_message(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
        self.0.next().map(|opt_result: Option<Result<_, InErr>>| {
            // Convert this from Option<Result<T>> to Result<Option<T>>
            let result = opt_result.transpose();

            // And wrap the error in a JsonRpcError
            result.map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
        })
    }
}

/// Wrapper around a [`Transport`] implementation which erases some irrelevant type information and
/// provides a slightly higher-level wrapper around the transport.
///
/// Among other things this provides a `Sync` wrapper around the transport, so send and receive do
/// not require a `mut` reference.  It also wraps the low-level byte slice-oriented send/receive
/// operations with higher-level ones that automatically serialize/deserialize the JSON messages,
/// and standardizes the error type for failures to [`JsonRpcError`].
pub struct Peer {
    remote_peer: String,
    transport: tokio::sync::Mutex<Box<dyn BoxedTransport>>,
}

impl Peer {
    /// Wrap a [`Transport`] implementation in a [`Peer`] object.
    pub(crate) fn new(transport: impl Transport) -> Self {
        // Regretably, it's not practical to interact with this wrapper from async code unless it
        // is `Sync`.  I don't want to impose that requirement on the transport itself, so a mutex
        // becomes necessary.
        Self {
            remote_peer: transport.remote_peer().to_string(),
            transport: tokio::sync::Mutex::new(Box::new(transport)),
        }
    }

    /// Return the remote peer's identifier, which is transport-specific.
    fn remote_peer(&self) -> Cow<'static, str> {
        self.remote_peer.clone().into()
    }

    /// Send a message to the remote peer, serializing it to JSON first.
    pub async fn send_message(&self, message: types::Message) -> Result<()> {
        self.transport
            .lock()
            .await
            .send_message(message.into_bytes()?)
            .await
    }

    /// Receive a message from the remote peer, deserializing it from JSON.
    ///
    /// If the message is not valid JSON, or if it's valid JSON but not a valid JSON-RPC message,
    /// this fails with an error.
    ///
    /// If the underlying transport reports that the connection is closed, returns `None`.
    pub async fn receive_message(&self) -> Result<Option<TransportMessage>> {
        let remote_peer = self.remote_peer.clone();

        if let Some(message) = self.transport.lock().await.receive_message().await? {
            let message = types::Message::from_bytes(&message)?;

            Ok(Some(TransportMessage {
                metadata: TransportMetadata {
                    remote_peer,
                    request_headers: Default::default(),
                },
                message,
            }))
        } else {
            Ok(None)
        }
    }
}

/// Message from the transport layer, augmented with additional transport-specific information.
///
/// Not necessarily a method invocation, but a message received from the transport layer with
/// additional metadata provided by the transport.
pub struct TransportMetadata {
    pub remote_peer: String,
    pub request_headers: HashMap<String, String>,
}
pub struct TransportMessage {
    pub metadata: TransportMetadata,
    pub message: types::Message,
}
