//! The service and related types that form the beating heart of both JSON RPC clients and servers.
//!
//! See the [`Service`] struct for more details and usage.
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{JsonRpcError, Result};
use crate::{handler, router, transport, types};

/// The size of the mpsc channel that is used to send messages to an existing connection.
/// After this many messages are queued and not yet transmitted, backpressure is excerted on the
/// senders
const CONNECTION_CHANNEL_BOUNDS: usize = 16;

/// Service which receives JSON RPC requests, dispatches them to the appropriate handler, and also
/// processes responses from the remote peer.
///
/// Both JSON RPC clients and servers are implemented using an instance of Service.  This is
/// because under the JSON RPC spec, both client and server are peers, each can send requests to
/// the other and receive responses.  The disctinction between client and server is mainly who
/// makes the first move, and also at the transport level the distinction becomes more pronounced.
/// But this crate doesn't deal with that complexity, so this single type is most all that is
/// needed to implement both client and server.
#[derive(Clone)]
pub struct Service<S: Clone + Send + Sync + 'static> {
    router: router::Router<S>,

    custom_event_loop: Arc<dyn EventLoop<S>>,

    /// Signal to abort and exit the loop
    cancellation_token: CancellationToken,
}

/// The type containing an outgoing message that is to be sent via the peer, and the one-shot
/// channel to which the result should be sent.
enum OutboundMessage {
    /// This is a method invocation, so the response channel will receive the response from the
    /// remote peer
    Method {
        request: types::Request,
        response_tx: oneshot::Sender<Result<types::Response>>,
    },
    /// This is a notification, so the response channel will receive a result simply confirming
    /// that the notification was successfully passed to the transport
    Notification {
        notification: types::Notification,
        send_confirmation_tx: oneshot::Sender<Result<()>>,
    },
}

impl<S: Clone + Send + Sync + 'static> Service<S> {
    pub fn new(router: router::Router<S>) -> Self {
        Self {
            router,
            custom_event_loop: Arc::new(NoOpEventLoop(Default::default())),
            cancellation_token: CancellationToken::new(),
        }
    }

    pub fn with_custom_event_loop(mut self, custom_event_loop: impl EventLoop<S>) -> Self {
        self.custom_event_loop = Arc::new(custom_event_loop);
        self
    }

    pub(crate) fn router_mut(&mut self) -> &mut router::Router<S> {
        &mut self.router
    }

    /// Start the service connection for a remote peer, and return a handle that can be used to
    /// interact with the service connection.
    ///
    /// This will spawn an async task that will run for the life of the peer connection, constantly
    /// polling the peer, optionally polling the custom event loop if one was provided, and
    /// periodically performing housekeeping tasks.
    pub(crate) fn service_connection(&self, peer: transport::Peer) -> ServiceConnectionHandle {
        // This is all fucked up.  I had the idea that maybe the Service is long-lived and youc all
        // `service_peer` for each peer, but now it's kind of a mess.  Where is the event loop?  Is
        // "handle" the right abstraction now for ServiceHandle, it seems more like it's a logical
        // connection.  So what's the point of this design?  A client will need to hold on to the
        // service handle because that's how it communicates with the server.  But what would a
        // server do with this?  I suppose it would be needed to send proactive messages to the
        // client.  Can the server's handlers get a copy of this service handle via an extractor?
        //
        // The list of pending requests seems like it should not be global, right?  According to
        // the spec and Claude's summary of it, responses must be scoped to the connection on which
        // they were received, so it's right to scope the pending requests to the connection.

        let (tx, rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);
        let conn = ServiceConnection::new(
            self.router.clone(),
            peer,
            self.custom_event_loop.clone(),
            self.cancellation_token.clone(),
            rx,
        );

        tokio::spawn(async move {
            if let Err(e) = conn.event_loop().await {
                tracing::error!("Error in JSON RPC event loop: {}", e);
            }
        });

        ServiceConnectionHandle {
            outbound_messages: tx,
            cancellation_token: self.cancellation_token.clone(),
        }
    }
}

/// Type alias for pending requests map to simplify complex type
type PendingRequestsMap = Arc<Mutex<HashMap<types::Id, oneshot::Sender<Result<types::Response>>>>>;

struct ServiceConnection<S: Clone + Send + Sync + 'static> {
    router: router::Router<S>,

    custom_event_loop: Arc<dyn EventLoop<S>>,

    peer: transport::Peer,

    cancellation_token: CancellationToken,

    /// Requests that have been sent, keyed by the request ID that was passed to the remote peer.
    /// Responses will come in with this ID specified.
    pending_requests: PendingRequestsMap,

    /// Operations that are running now, processing inbound messages received on this connection.
    /// These futures are polled as part of the event loop, and when they complete they yield the
    /// result of the operation.
    pending_operations: JoinSet<Option<types::Message>>,

    outbound_messages: mpsc::Receiver<OutboundMessage>,
}

impl<S: Clone + Send + Sync + 'static> ServiceConnection<S> {
    fn new(
        router: router::Router<S>,
        peer: transport::Peer,
        custom_event_loop: Arc<dyn EventLoop<S>>,
        cancellation_token: CancellationToken,
        outbound_messages: mpsc::Receiver<OutboundMessage>,
    ) -> Self {
        Self {
            router,
            custom_event_loop,
            peer,
            cancellation_token,
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_operations: JoinSet::new(),
            outbound_messages,
        }
    }

    async fn event_loop(mut self) -> Result<()> {
        let result = loop {
            tokio::select! {
                outbound_message = self.outbound_messages.recv() => {
                    match outbound_message {
                        Some(outbound_message) => {
                            self.handle_outbound_message(outbound_message).await;
                        },
                        None => {
                            // The peer has closed the connection
                            tracing::debug!("Peer closed connection");
                            break Ok(());
                        }
                    }
                },
                result = self.peer.receive_message() => {
                    match result {
                        Ok(Some(transport::TransportMessage { metadata, message})) => {
                            self.handle_inbound_message(Arc::new(metadata), message).await;
                        }
                        Ok(None) => {
                            // The peer has closed the connection
                            tracing::debug!("Peer closed connection");
                            break Ok(());
                        }
                        Err(e) => {
                            tracing::error!("Error receiving message: {}", e);
                            break Err(e);
                        }
                    }
                },
                next = self.pending_operations.join_next(), if !self.pending_operations.is_empty() => {
                    // One of the pending futures handling a previous inbound message has
                    // completed.  If it produced an output, then send it to the peer
                    if let Some(result) = next {
                        match result {
                            Ok(Some(message)) => {
                                // Future ran to completion, and produced a response message.
                                // Note that this is technically also "outbound", but we use that
                                // term to mean messages that are being sent by some caller via the
                                // service handle to the remote peer, not messages that are sent in
                                // response to previously received requests.  Therefore this
                                // message we can pass directly to the peer
                                if let Err(e) = self.peer.send_message(message).await {
                                    tracing::error!("Error sending message: {}", e);
                                    break Err(e);
                                }
                            },
                            Ok(None) => {
                                // Future ran to completion, but there is no response to send.
                                // Nothing further to do
                            },
                            Err(join_err) => {
                                // This is JoinError from tokio, it could mean that the task
                                // paniced, or that it was cancelled.  Let's not let this kill the
                                // whole event loop
                                tracing::error!("JoinError prevented handling of incoming message: {}", join_err);
                            }
                        }
                    }
                },
                result = self.custom_event_loop.run(self.router.state(), &self.peer) => {
                    match result {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::error!("Error in custom event loop: {}", e);
                            break Err(JsonRpcError::CustomEventLoopError {
                                source: e
                            });
                        }
                    }
                }
            }
        };

        // Event loop is over, for whatever reason.  Any remainding pending requests are never
        // going to complete
        let mut pending_requests = self.pending_requests.lock().unwrap();
        for (_, tx) in pending_requests.drain() {
            // Send will fail if the receiver is already dropped, which could very well be the case
            // if the connection itself is dropped.  But if it's still around, let it down easy.
            let _ = tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
        }

        // TODO: Also deal with the remaining pending operations which are also not going to
        // complete.  Should we keep track of the batch and request messages so that we can
        // generate generic error responses for them?

        result
    }

    /// Pass an outbound message to the transport
    ///
    /// This is infallible because any errors from the transport layer are reported back to the
    /// outbound message's oneshot channel.
    async fn handle_outbound_message(&self, outbound_message: OutboundMessage) {
        match outbound_message {
            OutboundMessage::Method { request, response_tx } => {
                // Attempt to send the method call request, and if sucessful then add it to the
                // pending requests list.
                // Note that this is not a race condition because this event loop is
                // the only place where the peer is stored, so we know that no other
                // threads or async tasks can receive a response for this request while
                // we're still trying to add it to the pending requests list
                let id = request.id.clone();
                let result = self.peer.send_message(types::Message::Request(request)).await;
                if let Err(e) = result {
                    // Failed to send, which means this won't go in the pending
                    // requests list and we may as well inform the caller now about the
                    // failure
                    tracing::error!("Error sending method request: {}", e);
                    let _ = response_tx.send(Err(e));
                } else {
                    // Request was sent
                    self.pending_requests.lock().unwrap().insert(id, response_tx);
                }
            }
            OutboundMessage::Notification {
                notification,
                send_confirmation_tx,
            } => {
                // Notifications are easier because there is no response expected.  The
                // only reason there's a oneshot channel at all is just to confirm that
                // the notification was passed off to the transport successfully.
                if let Err(e) = self
                    .peer
                    .send_message(types::Message::Notification(notification))
                    .await
                {
                    // Failed to send, which means this won't go in the pending
                    // requests list and we may as well inform the caller now about the
                    // failure
                    tracing::error!("Error sending notification: {}", e);
                    let _ = send_confirmation_tx.send(Err(e));
                } else {
                    // Request was sent, that's the best result we can hope for
                    let _ = send_confirmation_tx.send(Ok(()));
                }
            }
        }
    }

    /// Process an inbound message that just came in from the transport layer
    ///
    /// Metadata needs to be wrapped in an `Arc` because it is potentially cloned extensively
    /// during message processing.
    ///
    /// If there are any responses to this inbound message, they are produced by the spawned async
    /// task which the event loop will poll separately from this call.
    async fn handle_inbound_message(
        &mut self,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: types::Message,
    ) {
        self.spawn_operation({
            let router = self.router.clone();
            let pending_requests = self.pending_requests.clone();
            async move {
                Self::handle_inbound_message_task(&router, &pending_requests, metadata, inbound_message).await
            }
        })
    }

    /// Handler of inbound messages suitable for running as a 'static async task representing a
    /// pending operation.
    async fn handle_inbound_message_task(
        router: &router::Router<S>,
        pending_requests: &PendingRequestsMap,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: types::Message,
    ) -> Option<types::Message> {
        match inbound_message {
            crate::Message::Batch(messages) => {
                // Batch messages are handled in parallel, with their results combined at the end
                let batch_futures = messages.into_iter().map(|message| {
                    let metadata = metadata.clone();

                    Self::handle_inbound_message_task(router, pending_requests, metadata, message)
                });

                // Await all of those futures.  If even one of them produced some output message,
                // then the result of the batch is a batch with all of those output messages.  If
                // there are no outputs (meaning the entire batch was notifications or responses)
                // then the spec says that there should be no response at all to the batch message
                //
                // TODO: there should be some upper limit on how many messages can be in a batch,
                // just as there should be some upper limit on the total size of a request.  If
                // huge batch requests are going to be a thing, instead of join_all we should use
                // FuturesUnordered to process them with a reasonable degree of parallelism.
                let batch_results = futures::future::join_all(batch_futures)
                    .await
                    .into_iter()
                    .flatten();

                // Filtered out all `None` results, there are only `Some(_)`
                let batch_results: Vec<_> = batch_results.collect();

                if !batch_results.is_empty() {
                    // There is at least one response message, so package all such messages up into
                    // a batch
                    Some(types::Message::Batch(batch_results))
                } else {
                    None
                }
            }
            crate::Message::Request(request) => {
                // This is a request, so we need to find the handler for it and invoke it
                // The router is literally built to do that very thing
                let invocation_request = handler::InvocationRequest::from_request_message(metadata, request);

                // The actual invocation is infallible, because any errors will be reported as a
                // response type with error information, or just ignored in the case of
                // notifications
                let output = router.handle_invocation(invocation_request).await;

                // UGLY: I hate to enforce this at runtime, but there are reasons we have a unifued
                // request type for both notifications and method calls.  We know this is a
                // method call, we know that there *MUST* be a response payload, even if an empty
                // one, but we can't make the compiler enforce that at compile time
                debug_assert!(output.is_some(), "BUG: Method call produced no response");
                output.map(types::Message::Response)
            }
            crate::Message::Notification(notification) => {
                // Process this notification in a simpler version of the request handler workflow
                // All comments there apply here as well, except as noted below
                let invocation_request =
                    handler::InvocationRequest::from_notification_message(metadata, notification);

                // The actual invocation is infallible, because any errors will be reported as a
                // response type with error information, or just ignored in the case of
                // notifications
                let output = router.handle_invocation(invocation_request).await;

                // For notifications our assertion is exactly the opposite, since notifications
                // MUST NOT have a response
                debug_assert!(output.is_none(), "BUG: Invocation call produced a response");
                None
            }
            crate::Message::Response(response) => {
                // The remote peer has sent us a response to a method call request we sent
                // previously.  Exciting.
                //
                // Assuming both sides are adhering to the spec, the ID of this response should be
                // in our list of pending requests, so we just look it up and forward the response
                // to the oneshot channel for that pending request.
                if let Some(tx) = pending_requests.lock().unwrap().remove(&response.id) {
                    // This is a response to a method call, so we need to send the result back to
                    // the caller
                    let _ = tx.send(Ok(response));
                } else {
                    // This is a response to a method call that we don't know about.  This could be
                    // because the request was never sent, or because it was sent but the
                    // connection closed before we got the response.  Either way, we can't do
                    // anything with this response.
                    tracing::warn!("Received response for unknown request ID: {}", response.id);
                }

                // Either way, we do not generate a response to this response; the cycle ends here
                None
            }
            crate::Message::InvalidRequest(invalid_request) => {
                // Lob this back to the remote peer with an error about this invalid request
                // At least it has an ID, otherwise we wouldn't have anything to anchor this to and
                // the error would be generated
                Some(types::Message::Response(types::Response::error(
                    invalid_request.id,
                    types::ErrorCode::InvalidRequest,
                    "Invalid request".to_string(),
                    None,
                )))
            }
        }
    }

    /// Spawn a future that is processing an incoming message and may or may not produce some
    /// output.
    ///
    /// Any output that the future produces will be written to the remote peer when the future
    /// completes.
    fn spawn_operation(&mut self, operation: impl Future<Output = Option<types::Message>> + Send + 'static) {
        // Spawn the operation and add it to the pending operations list
        self.pending_operations.spawn(operation);
    }
}

/// Lightweight and cheaply-clonable handle to a service connection and its associated background
/// task.
///
/// This handle is used to send notifications and method call requests to the remote peer at the
/// other end of the service connection.  It is also able to signal the connection to shutdown if
/// needed.
///
/// Finally, the service connection background task will shutdown if there are no service
/// connection handles left alive.  TODO: is this a good design?  a service connection can be
/// useful without any handles, in a server context where the server is waiting for requests and
/// responding to them.  A good reason why we should *not* do this: I want an extractor that can
/// provide a handler with the handle to the connection the request is received on.  That means the
/// event loop must have a handle to itself so that it can vend handles to handlers.
#[derive(Clone)]
pub struct ServiceConnectionHandle {
    outbound_messages: mpsc::Sender<OutboundMessage>,
    cancellation_token: CancellationToken,
}

impl ServiceConnectionHandle {
    /// Send a request to invoke a method, expecting a response.
    pub async fn call_method<Req, Resp>(&self, method: &str, params: Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let (tx, rx) = oneshot::channel();
        let request_id = types::Id::Str(Uuid::now_v7().to_string());

        // Submit this request to the connection's event loop for processing
        if self
            .outbound_messages
            .send(OutboundMessage::Method {
                request: types::Request::new(
                    request_id.clone(),
                    method,
                    serde_json::to_value(params).map_err(|e| JsonRpcError::SerRequest {
                        source: e,
                        type_name: std::any::type_name::<Req>(),
                    })?,
                ),
                response_tx: tx,
            })
            .await
            .is_err()
        {
            // The event loop is no longer running, so the connection must be closed
            return Err(JsonRpcError::PendingRequestConnectionClosed);
        }

        // Wait for the event loop to send the request and pass the response back to this task via
        // the oneshot channel
        let result = rx.await;
        match result {
            Ok(Ok(response)) => {
                // Decode this Response struct into either the response type Resp or an error
                match response.payload {
                    types::ResponsePayload::Success(success_response) => {
                        // This is a successful response, so decode it into the expected type
                        let response =
                            serde_json::from_value(success_response.result.clone()).map_err(|e| {
                                JsonRpcError::DeserResponse {
                                    source: e,
                                    type_name: std::any::type_name::<Resp>(),
                                    request: success_response.result,
                                }
                            })?;
                        Ok(response)
                    }
                    types::ResponsePayload::Error(error_response) => {
                        // This is an error response, so return the error
                        Err(JsonRpcError::MethodError {
                            method_name: method.to_string(),
                            error: error_response.error,
                        })
                    }
                }
            }
            Ok(Err(e)) => {
                // The event loop sent an error, so pass that error back to the caller
                Err(e)
            }
            Err(_) => {
                // The sender side of the one-shot channel was dropped.  That actually shouldn't
                // happen absent a panic in the event loop, since it contains logic to drain
                // pending requests when the loop exists
                tracing::error!(
                    "BUG: One-shot channel was dropped before the event loop could send a response"
                );
                Err(JsonRpcError::PendingRequestConnectionClosed)
            }
        }
    }

    /// Send a notification, expecting no response.
    ///
    /// TODO: helpers for serialization
    pub async fn raise_notification<Req>(&self, method: &str, params: Req) -> Result<()>
    where
        Req: Serialize,
    {
        // Skip the registration of a pending request since this is just a notification.  Pass it
        // to the event loop and let the chips fall where they may.
        let (tx, rx) = oneshot::channel();

        if self
            .outbound_messages
            .send(OutboundMessage::Notification {
                notification: types::Notification::new(
                    method,
                    serde_json::to_value(params).map_err(|e| JsonRpcError::SerRequest {
                        source: e,
                        type_name: std::any::type_name::<Req>(),
                    })?,
                ),
                send_confirmation_tx: tx,
            })
            .await
            .is_err()
        {
            // The event loop is no longer running, so the connection must be closed
            return Err(JsonRpcError::PendingRequestConnectionClosed);
        }

        // Wait for the oneshot channel to receive confirmation that the notification was sent.
        // This is unlike a method call which is waiting for a response from the remote peer.  In
        // this case, we just want to give the caller some visibility into whether or not the
        // notification went out to the transport layer or not

        let result = rx.await;
        match result {
            Ok(result) => {
                // Either sent or failed to send with an error.  Either way pass the result back to
                // the caller
                result
            }
            Err(_) => {
                // The sender side of the one-shot channel was dropped.  That actually shouldn't
                // happen absent a panic in the event loop, since it contains logic to drain
                // pending requests when the loop exists
                tracing::error!(
                    "BUG: One-shot channel was dropped before the event loop could send a response"
                );
                Err(JsonRpcError::PendingRequestConnectionClosed)
            }
        }
    }

    // TODO: What about batch support?  Right now the event loop assumes each outgoing request is a
    // method call with an ID that can be matched to a response, but if we support batches then we
    // need special-case logic to handle that
}

/// Type alias for event loop result to simplify complex type
pub type EventLoopResult = Result<(), Box<dyn std::error::Error + Sync + Send + 'static>>;

/// A custom event loop that optionally attends to other event sources besides the peer, and
/// dispatches events when that's necessary
pub trait EventLoop<S>: Send + Sync + 'static {
    /// Run a single iteration of the custom event loop, then return.
    ///
    /// This should do quick things like pooling event queues, checking for cancellation, etc.
    ///
    /// A successful result means that the event loop keeps running.  A failure result terminates
    /// the session
    fn run(
        &self,
        state: &S,
        peer: &transport::Peer,
    ) -> Pin<Box<dyn Future<Output = EventLoopResult> + Send + 'static>>;
}

struct NoOpEventLoop<S>(std::marker::PhantomData<S>);

// Make a no-op impl of this trait to use by default
impl<S: Send + Sync + 'static> EventLoop<S> for NoOpEventLoop<S> {
    fn run(
        &self,
        _state: &S,
        _peer: &transport::Peer,
    ) -> Pin<Box<dyn Future<Output = EventLoopResult> + Send + 'static>> {
        // No-op, just a future that never completes
        std::future::pending().boxed()
    }
}
