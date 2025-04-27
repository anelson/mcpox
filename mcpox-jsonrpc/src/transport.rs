//! In this crate, the concept of a "transport" abstracts away the details of how JSON-RPC messages
//! are sent and received.  In the MCP case specifically, the
//! [specification](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/)
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
use std::sync::Arc;

use crate::{JsonRpcError, Result, typemap};
use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{self, Framed};

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

    /// Construct a [`tracing::Span`] with useful metadata about the transport.
    ///
    /// For example an HTTP response would include the remote endpoint and the request URL and
    /// method.
    ///
    /// The code that processes messages from this transport will enter this span whenever it does
    /// so.
    fn span(&self) -> tracing::Span;

    /// Fill a typemap with transport-specific metadata.
    ///
    /// Each transport has to define for itself what its metadata is.  Some simple transports may
    /// not have any metadata.
    fn populate_metadata(&self, metadata: &mut typemap::TypeMap);

    /// Send a message to the transport.  Should not complete until the message has been handed off
    /// to the transport layer and transmitted to the remote peer, whatever that means for the
    /// partciular transport.
    ///
    /// This does not need to be cancelation-safe because the framework will wait until this future
    /// finishes before considering any messages "sent"
    fn send_message(&mut self, message: String) -> impl Future<Output = Result<(), Self::Error>> + Send + '_;

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
    fn receive_message(&mut self) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send + '_;
}

/// Internal dyn-compatible wrapper trait around [`Transport`] to erase the types and allow dynamic
/// dispatch, hopefully without dire performance conseqsuences
trait BoxedTransport: Send + 'static {
    fn boxed_span(&self) -> tracing::Span;
    fn boxed_send_message(
        &mut self,
        message: String,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>;
    fn boxed_receive_message(&mut self) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>>;
}

impl<T> BoxedTransport for T
where
    T: Transport + 'static,
{
    fn boxed_span(&self) -> tracing::Span {
        <Self as Transport>::span(self)
    }

    fn boxed_send_message(
        &mut self,
        message: String,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        <Self as Transport>::send_message(self, message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }

    fn boxed_receive_message(&mut self) -> Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + '_>> {
        <Self as Transport>::receive_message(self)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
            .boxed()
    }
}

/// Implementation of [`Transport`] that is generic over any type that implements
/// [`tokio::io::AsyncRead`] and [`tokio::io::AsyncWrite`].
///
/// Reads and writes messages assuming that each message is UTF-8 text separated by newline
/// characters.
///
/// This is useful for testing, and can also be used to implement the `stdio` transport in MCP by
/// simply using tokio's stdin and stdout wrappers.
impl<Io> Transport for Framed<Io, codec::LinesCodec>
where
    Io: Send + Unpin + AsyncRead + AsyncWrite + 'static,
{
    type Error = codec::LinesCodecError;

    fn span(&self) -> tracing::Span {
        tracing::debug_span!("Framed<Io, LinesCodec>", Io = std::any::type_name::<Io>(),)
    }

    fn populate_metadata(&self, _metadata: &mut typemap::TypeMap) {
        // There's no metadata of any interest for this basic transport
    }

    fn send_message(&mut self, message: String) -> impl Future<Output = Result<(), Self::Error>> + Send + '_ {
        self.send(message)
    }

    fn receive_message(&mut self) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send + '_ {
        // NOTE: `recieve_message` must be cancel safe.  The docs on `StreamExt::next` don't
        // explicitly say that it is, but it doesn't take ownership of the stream so it seems
        // likely to also be cancel safe.
        self.next()
            .map(|opt_result: Option<Result<_, codec::LinesCodecError>>| {
                // Convert this from Option<Result<T>> to Result<Option<T>>
                opt_result.transpose()
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
    metadata: Arc<TransportMetadata>,
    transport: tokio::sync::Mutex<Box<dyn BoxedTransport>>,
}

impl Peer {
    /// Wrap a [`Transport`] implementation in a [`Peer`] object.
    pub(crate) fn new(transport: impl Transport) -> Self {
        let mut metadata = TransportMetadata::new();
        transport.populate_metadata(&mut metadata.map);

        // Regretably, it's not practical to interact with this wrapper from async code unless it
        // is `Sync`.  I don't want to impose that requirement on the transport itself, so a mutex
        // becomes necessary.  Most of the transport impls, like a stream or a `Write` impl, also
        // require `&mut self` to send or receive so this works out for the best anyway.
        Self {
            metadata: Arc::new(metadata),
            transport: tokio::sync::Mutex::new(Box::new(transport)),
        }
    }

    pub async fn span(&self) -> tracing::Span {
        self.transport.lock().await.boxed_span()
    }

    pub fn metadata(&self) -> &TransportMetadata {
        &self.metadata
    }

    /// Send a raw message string to the remote peer.
    pub async fn send_message(&self, message: String) -> Result<()> {
        self.transport.lock().await.boxed_send_message(message).await
    }

    /// Receive a message from the remote peer.
    ///
    /// This returns the raw string message from the transport without attempting to parse it.
    /// The service connection layer is responsible for parsing and processing the message content.
    ///
    /// If the underlying transport reports that the connection is closed, returns `None`.
    pub async fn receive_message(&self) -> Result<Option<TransportMessage>> {
        if let Some(message) = self.transport.lock().await.boxed_receive_message().await? {
            Ok(Some(TransportMessage {
                metadata: self.metadata.clone(),
                message,
            }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug)]
pub struct TransportMetadata {
    /// Metadata that is transport-specific and thus keyed by transport-specific types
    map: typemap::TypeMap,
}

impl TransportMetadata {
    pub fn new() -> Self {
        Self {
            map: typemap::TypeMap::new(),
        }
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        self.map.get::<T>()
    }

    pub fn get_clone<T: Clone + 'static>(&self) -> Option<T> {
        self.map.get_clone::<T>()
    }

    pub fn contains<T: 'static>(&self) -> bool {
        self.map.contains::<T>()
    }
}

impl Default for TransportMetadata {
    fn default() -> Self {
        Self::new()
    }
}

/// Message from the transport layer, augmented with additional transport-specific information.
///
/// Not necessarily a method invocation, but a message received from the transport layer with
/// additional metadata provided by the transport. The message is kept as an unparsed string
/// to allow handling parse errors at the service connection level.
#[derive(Debug)]
pub struct TransportMessage {
    pub metadata: Arc<TransportMetadata>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::Transport as _;
    use super::*;
    use crate::testing;
    use assert_matches::assert_matches;

    #[tokio::test]
    async fn test_framed_transport() {
        // Create framed transports
        let (mut client_transport, mut server_transport) = testing::setup_test_channel();

        // Test sending message
        let message = "test message".to_string();
        client_transport.send_message(message.clone()).await.unwrap();

        // Test receiving message
        let received = server_transport.receive_message().await.unwrap();
        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn test_large_message_exceeds_max_length() {
        // Create framed transports
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Create a message slightly larger than the max
        let large_message = "x".repeat(testing::TEST_CHANNEL_MAX_LENGTH + 100);
        client_peer.send_message(large_message.clone()).await.unwrap();

        let err = server_peer.receive_message().await.unwrap_err();
        if let JsonRpcError::Transport { source } = &err {
            println!("Received error: {}", source);
        }
        assert_matches!(err,
            JsonRpcError::Transport { source } if source.to_string().contains("max line length exceeded"));
    }

    #[tokio::test]
    async fn test_peer_wrapper() {
        // Create framed transports
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Test sending a simple string message
        let test_message = "Test message from client";
        client_peer.send_message(test_message.to_string()).await.unwrap();

        // Test receive_message - now we receive a raw string instead of a parsed message
        let received = server_peer.receive_message().await.unwrap().unwrap();

        // Verify the received message matches what was sent
        assert_eq!(received.message, test_message);

        // Test sending a response string
        let response_message = "Response message from server";
        server_peer
            .send_message(response_message.to_string())
            .await
            .unwrap();

        // Test receiving the response - now we receive a raw string instead of a parsed message
        let received = client_peer.receive_message().await.unwrap().unwrap();

        // Verify the received message matches what was sent
        assert_eq!(received.message, response_message);
    }

    #[tokio::test]
    async fn test_notification_messages() {
        // Create framed transports
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Send a notification string
        let notification_message = "Notification message";
        client_peer
            .send_message(notification_message.to_string())
            .await
            .unwrap();

        // Receive the notification
        let received = server_peer.receive_message().await.unwrap().unwrap();

        // Verify the received message matches what was sent
        assert_eq!(received.message, notification_message);
    }

    #[tokio::test]
    async fn test_batch_messages() {
        // Create framed transports
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Send a batch message string
        let batch_message = "Batch of messages";
        client_peer.send_message(batch_message.to_string()).await.unwrap();

        // Receive the batch message
        let received = server_peer.receive_message().await.unwrap().unwrap();

        // Verify the received message matches what was sent
        assert_eq!(received.message, batch_message);
    }

    #[tokio::test]
    async fn test_transport_metadata() {
        // Create framed transports
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Send a simple text message string
        let test_message = "Simple test message";
        client_peer.send_message(test_message.to_string()).await.unwrap();

        // Receive the message and check metadata
        let received = server_peer.receive_message().await.unwrap().unwrap();

        // Verify transport metadata
        // TODO: once there's a mechanism for transports to provide typed metadata, check that here

        // Verify received message matches what was sent
        assert_eq!(received.message, test_message);
    }

    // Custom transport implementation for testing
    struct MockTransport {
        name: String,
        message_queue: Vec<String>,
    }

    struct MockTransportMetadata {
        foo: usize,
    }

    impl MockTransport {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                message_queue: Vec::new(),
            }
        }

        fn queue_message(&mut self, message: String) {
            self.message_queue.push(message);
        }
    }

    impl Transport for MockTransport {
        type Error = JsonRpcError;

        fn span(&self) -> tracing::Span {
            tracing::debug_span!("MockTransport", name = %self.name)
        }

        fn populate_metadata(&self, metadata: &mut typemap::TypeMap) {
            metadata.insert(MockTransportMetadata { foo: 42 });
        }

        async fn send_message(&mut self, message: String) -> Result<(), Self::Error> {
            self.message_queue.push(message);
            Ok(())
        }

        async fn receive_message(&mut self) -> Result<Option<String>, Self::Error> {
            Ok(self.message_queue.pop())
        }
    }

    #[tokio::test]
    async fn test_custom_transport_impl() {
        // Create a custom transport
        let mut transport = MockTransport::new("test-transport");

        // Test sending a message
        transport.send_message("test message".to_string()).await.unwrap();

        // Test receiving the message
        let received = transport.receive_message().await.unwrap();
        assert_eq!(received, Some("test message".to_string()));

        // Test receiving from empty queue returns None
        let received = transport.receive_message().await.unwrap();
        assert_eq!(received, None);
    }

    #[tokio::test]
    async fn test_boxed_transport_through_peer() {
        // Create two custom transports
        let mut transport1 = MockTransport::new("transport1");
        let mut transport2 = MockTransport::new("transport2");

        // Queue a message to be received
        transport1.queue_message("message for transport1".to_string());
        transport2.queue_message("message for transport2".to_string());

        // Create peers from the transports (this boxes them)
        let peer1 = Peer::new(transport1);
        let peer2 = Peer::new(transport2);

        assert_eq!(42, peer1.metadata().get::<MockTransportMetadata>().unwrap().foo);
        assert_eq!(42, peer2.metadata().get::<MockTransportMetadata>().unwrap().foo);

        // Send raw string messages between the peers
        peer1
            .send_message("message for transport1".to_string())
            .await
            .unwrap();
        peer2
            .send_message("message for transport2".to_string())
            .await
            .unwrap();

        // Receive the pre-queued messages
        let received1 = peer1.receive_message().await.unwrap();
        let received2 = peer2.receive_message().await.unwrap();

        assert!(received1.is_some());
        // Check metadata
        assert_eq!(
            42,
            received1
                .as_ref()
                .unwrap()
                .metadata
                .get::<MockTransportMetadata>()
                .unwrap()
                .foo
        );
        // Check message contents
        assert_eq!("message for transport1", received1.unwrap().message);

        assert!(received2.is_some());
        // Check metadata
        assert_eq!(
            42,
            received2
                .as_ref()
                .unwrap()
                .metadata
                .get::<MockTransportMetadata>()
                .unwrap()
                .foo
        );
        // Check message contents
        assert_eq!("message for transport2", received2.unwrap().message);
    }
}
