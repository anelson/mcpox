//! In this crate, the concept of a "transport" abstracts away the details of how JSON-RPC messages
//! are sent and received.  In the MCP case specifically, the [specification on transports](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/)
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

    /// Transport-specific identifier of the remote peer, useful for logging and debugging.
    fn remote_peer(&self) -> Cow<'static, str>;

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

/// Internal dyn-compatible wrapper trate around [`Transport`] to erase the types and allow dynamic
/// dispatch, hopefully without dire performance conseqsuences
trait BoxedTransport: Send + 'static {
    fn boxed_remote_peer(&self) -> Cow<'static, str>;
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
    fn boxed_remote_peer(&self) -> Cow<'static, str> {
        <Self as Transport>::remote_peer(self)
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

/// Implement transport on some std lib types, mainly for testing.  Production impls will be
/// provided outside of this crate
impl<In, InErr, Out> Transport for (In, Out)
where
    In: Stream<Item = Result<String, InErr>> + Unpin + Send + 'static,
    InErr: std::error::Error + Send + Sync + 'static,
    Out: Sink<String> + Unpin + Send + 'static,
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

    fn send_message(&mut self, message: String) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.1
            .send(message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
    }

    fn receive_message(&mut self) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send {
        self.0.next().map(|opt_result: Option<Result<_, InErr>>| {
            // Convert this from Option<Result<T>> to Result<Option<T>>
            let result = opt_result.transpose();

            // And wrap the error in a JsonRpcError
            result.map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
        })
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
    type Error = JsonRpcError;

    fn remote_peer(&self) -> Cow<'static, str> {
        Cow::Borrowed(std::any::type_name::<Io>())
    }

    fn send_message(&mut self, message: String) -> impl Future<Output = Result<(), Self::Error>> + Send + '_ {
        self.send(message)
            .map_err(|e| JsonRpcError::Transport { source: Box::new(e) })
    }

    fn receive_message(&mut self) -> impl Future<Output = Result<Option<String>, Self::Error>> + Send + '_ {
        // NOTE: `recieve_message` must be cancel safe.  The docs on `StreamExt::next` don't
        // explicitly say that it is, but it doesn't take ownership of the stream so it seems
        // likely to also be cancel safe.
        self.next()
            .map(|opt_result: Option<Result<_, codec::LinesCodecError>>| {
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
        // becomes necessary.  Most of the transport impls, like a stream or a `Write` impl, also
        // require `&mut self` to send or receive so this works out for the best anyway.
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
            .boxed_send_message(message.into_string()?)
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

        if let Some(message) = self.transport.lock().await.boxed_receive_message().await? {
            let message = message.parse::<types::Message>()?;

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

pub struct TransportMetadata {
    pub remote_peer: String,
    pub request_headers: HashMap<String, String>,
}

/// Message from the transport layer, augmented with additional transport-specific information.
///
/// Not necessarily a method invocation, but a message received from the transport layer with
/// additional metadata provided by the transport.
pub struct TransportMessage {
    pub metadata: TransportMetadata,
    pub message: types::Message,
}

#[cfg(test)]
mod tests {
    use super::Transport as _;
    use super::*;
    use crate::types::{Id, Message, Notification, Request, Response, ResponsePayload};
    use futures::channel::mpsc;
    use serde_json::json;
    use tokio::io::duplex;
    use tokio_util::codec::LinesCodec;

    #[tokio::test]
    async fn test_stream_sink_transport() {
        let (tx1, rx1) = mpsc::channel(10);
        let (tx2, rx2) = mpsc::channel(10);

        // Create transport from stream/sink pairs
        let mut transport1 = (rx2.map(Ok::<String, std::io::Error>), tx1);
        let mut transport2 = (rx1.map(Ok::<String, std::io::Error>), tx2);

        // Test remote_peer
        assert!(transport1.remote_peer().contains("mpsc::Receiver"));
        assert!(transport1.remote_peer().contains("mpsc::Sender"));

        // Test sending message
        let message = "test message".to_string();
        transport1.send_message(message.clone()).await.unwrap();

        // Test receiving message
        let received = transport2.receive_message().await.unwrap();
        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn test_framed_transport() {
        // Create a pair of connected pipes
        let (client, server) = duplex(1024);

        // Create framed transports
        let mut client_transport = Framed::new(client, LinesCodec::new());
        let mut server_transport = Framed::new(server, LinesCodec::new());

        // Test remote_peer contains some type info - not checking exact string as it may vary
        let peer_name = client_transport.remote_peer();
        println!("Framed transport remote_peer: {}", peer_name);
        assert!(!peer_name.is_empty());

        // Test sending message
        let message = "test message".to_string();
        client_transport.send_message(message.clone()).await.unwrap();

        // Test receiving message
        let received = server_transport.receive_message().await.unwrap();
        assert_eq!(received, Some(message));
    }

    #[tokio::test]
    async fn test_peer_wrapper() {
        // Create a pair of connected pipes
        let (client, server) = duplex(1024);

        // Create framed transports
        let client_transport = Framed::new(client, LinesCodec::new());
        let server_transport = Framed::new(server, LinesCodec::new());

        // Create peer wrappers
        let client_peer = Peer::new(client_transport);
        let server_peer = Peer::new(server_transport);

        // Test send_message with JSON-RPC message
        let request = Request::new(Id::Number(1), "test_method", json!(["param1", "param2"]));
        let message = Message::Request(request);

        client_peer.send_message(message).await.unwrap();

        // Test receive_message
        let received = server_peer.receive_message().await.unwrap().unwrap();
        match received.message {
            Message::Request(req) => {
                assert_eq!(req.id, Id::Number(1));
                assert_eq!(req.method, "test_method");
                assert_eq!(req.params, Some(json!(["param1", "param2"])));
            }
            _ => panic!("Expected Request, got different message type"),
        }

        // Test sending a response back
        let response = Response::success(Id::Number(1), json!("result data"));
        let message = Message::Response(response);

        server_peer.send_message(message).await.unwrap();

        // Test receiving the response
        let received = client_peer.receive_message().await.unwrap().unwrap();
        match received.message {
            Message::Response(res) => {
                assert_eq!(res.id, Id::Number(1));
                if let ResponsePayload::Success(success) = res.payload {
                    assert_eq!(success.result, json!("result data"));
                } else {
                    panic!("Expected success response");
                }
            }
            _ => panic!("Expected Response, got different message type"),
        }
    }

    #[tokio::test]
    async fn test_notification_messages() {
        // Create a pair of connected pipes
        let (client, server) = duplex(1024);

        // Create framed transports with peers
        let client_peer = Peer::new(Framed::new(client, LinesCodec::new()));
        let server_peer = Peer::new(Framed::new(server, LinesCodec::new()));

        // Send a notification (no ID)
        let notification = Notification::new("notify", json!({"event": "something_happened"}));
        let message = Message::Notification(notification);

        client_peer.send_message(message).await.unwrap();

        // Receive the notification
        let received = server_peer.receive_message().await.unwrap().unwrap();
        match received.message {
            Message::Notification(notif) => {
                assert_eq!(notif.method, "notify");
                assert_eq!(notif.params, Some(json!({"event": "something_happened"})));
            }
            _ => panic!("Expected Notification, got different message type"),
        }
    }

    #[tokio::test]
    async fn test_batch_messages() {
        // Create a pair of connected pipes
        let (client, server) = duplex(1024);

        // Create framed transports with peers
        let client_peer = Peer::new(Framed::new(client, LinesCodec::new()));
        let server_peer = Peer::new(Framed::new(server, LinesCodec::new()));

        // Create batch message with multiple requests
        let request1 = Request::new(Id::Number(1), "method1", json!(["param1"]));
        let request2 = Request::new(Id::Number(2), "method2", json!(["param2"]));
        let notification = Notification::new("notify", json!({"event": "test"}));

        let batch = Message::Batch(vec![
            Message::Request(request1),
            Message::Request(request2),
            Message::Notification(notification),
        ]);

        client_peer.send_message(batch).await.unwrap();

        // Receive the batch message
        let received = server_peer.receive_message().await.unwrap().unwrap();
        match received.message {
            Message::Batch(messages) => {
                assert_eq!(messages.len(), 3);

                // Verify the batch contents
                match &messages[0] {
                    Message::Request(req) => {
                        assert_eq!(req.id, Id::Number(1));
                        assert_eq!(req.method, "method1");
                    }
                    _ => panic!("Expected Request as first batch item"),
                }

                match &messages[1] {
                    Message::Request(req) => {
                        assert_eq!(req.id, Id::Number(2));
                        assert_eq!(req.method, "method2");
                    }
                    _ => panic!("Expected Request as second batch item"),
                }

                match &messages[2] {
                    Message::Notification(notif) => {
                        assert_eq!(notif.method, "notify");
                    }
                    _ => panic!("Expected Notification as third batch item"),
                }
            }
            _ => panic!("Expected Batch, got different message type"),
        }
    }

    #[tokio::test]
    async fn test_transport_connection_close() {
        // Create a limited channel with capacity for just one message
        let (tx, rx) = mpsc::channel(1);

        // Create a transport that will close after one message
        let mut transport = (rx.map(Ok::<String, std::io::Error>), tx);

        // Send a message (this won't close the transport)
        let message = "test message".to_string();
        transport.send_message(message).await.unwrap();

        // Close the stream/channel
        drop(transport.0);

        // Create a new transport with the closed stream
        let (_, rx1) = mpsc::channel::<String>(1);
        let mut transport = (rx1.map(Ok::<String, std::io::Error>), transport.1);

        // When trying to receive from a closed stream, we should get None
        let result = transport.receive_message().await.unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_transport_metadata() {
        // Create a pair of connected pipes
        let (client, server) = duplex(1024);

        // Create framed transports with peers
        let client_peer = Peer::new(Framed::new(client, LinesCodec::new()));
        let server_peer = Peer::new(Framed::new(server, LinesCodec::new()));

        // Send a request
        let request = Request::new(Id::Number(1), "test_method", json!({"key": "value"}));
        let message = Message::Request(request);

        client_peer.send_message(message).await.unwrap();

        // Receive the message and check metadata
        let received = server_peer.receive_message().await.unwrap().unwrap();

        // Verify transport metadata
        println!(
            "Transport metadata remote_peer: {}",
            received.metadata.remote_peer
        );
        assert!(!received.metadata.remote_peer.is_empty());
        assert!(received.metadata.request_headers.is_empty());

        // Verify the message itself
        match received.message {
            Message::Request(req) => {
                assert_eq!(req.id, Id::Number(1));
                assert_eq!(req.method, "test_method");
            }
            _ => panic!("Expected Request"),
        }
    }

    // Custom transport implementation for testing
    struct MockTransport {
        name: String,
        message_queue: Vec<String>,
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

        fn remote_peer(&self) -> Cow<'static, str> {
            format!("MockTransport({})", self.name).into()
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

        // Check remote_peer implementation
        assert_eq!(transport.remote_peer(), "MockTransport(test-transport)");

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

        // Send JSON-RPC messages between them
        let request1 = Request::new(Id::Number(1), "method1", json!("param1"));
        peer1.send_message(Message::Request(request1)).await.unwrap();

        let request2 = Request::new(Id::Number(2), "method2", json!("param2"));
        peer2.send_message(Message::Request(request2)).await.unwrap();

        // Receive the pre-queued messages
        let received1 = peer1.receive_message().await.unwrap();
        let received2 = peer2.receive_message().await.unwrap();

        assert!(received1.is_some());
        assert!(received2.is_some());
    }
}
