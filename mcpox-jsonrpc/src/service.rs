//! The service and related types that form the beating heart of both JSON RPC clients and servers.
//!
//! See the [`Service`] struct for more details and usage.
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures::{FutureExt, TryFutureExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{Instrument, instrument};
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

    /// Guard that will cancel all connections when the service itself is dropped.
    ///
    /// This is not set unless the [`Self::new`] constructor is used.  Otherwise cancelation is
    /// entirely under the control of the caller's cancellation token.
    #[allow(dead_code)] // This is a drop guard, it's not meant to be used it's just for detecting drop
    drop_guard: Option<Arc<DropGuard>>,
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

impl<S: Clone + Send + Sync + 'static> Drop for Service<S> {
    fn drop(&mut self) {
        if let Some(drop_guard) = self.drop_guard.take() {
            tracing::debug!(
                state = std::any::type_name::<S>(),
                drop_guard_strong_ref_count = Arc::strong_count(&drop_guard),
                drop_guard_weak_ref_count = Arc::weak_count(&drop_guard),
                "Dropping service and drop guard"
            );
        }
    }
}

impl<S: Clone + Send + Sync + 'static> Service<S> {
    /// Create a new Service instance that uses a router to handle incoming requests.
    ///
    /// NOTE: This creates a service that will automatically cancel all service connections when
    /// the `Service` itself and all of its clones are dropped.  For servers, this is usually what
    /// you want, but for clients it's probably not.  For clients, use [`Self::new_cancellable`]
    /// instead.
    pub fn new(router: router::Router<S>) -> Self {
        // Make our own cancellation token, and a drop guard for it that we will carry around.
        // That way as soon as this service is dropped, it will trigger all connections to abort as
        // well
        let cancellation_token = CancellationToken::new();
        let drop_guard = cancellation_token.clone().drop_guard();
        Self {
            router,
            custom_event_loop: Arc::new(NoOpEventLoop(Default::default())),
            cancellation_token,
            drop_guard: Some(Arc::new(drop_guard)),
        }
    }

    /// Create a new Service instance that uses a router to handle incoming requests, and a
    /// cancellation token that will be used to cancel all service connection handlers if it's
    /// triggered.
    pub fn new_cancellable(cancellation_token: CancellationToken, router: router::Router<S>) -> Self {
        // The caller provided a cancellation token, so we won't use a drop guard here.
        Self {
            router,
            custom_event_loop: Arc::new(NoOpEventLoop(Default::default())),
            cancellation_token,
            drop_guard: None,
        }
    }

    pub fn with_custom_event_loop(mut self, custom_event_loop: impl EventLoop<S>) -> Self {
        self.custom_event_loop = Arc::new(custom_event_loop);
        self
    }

    /// The current state of the service, which is managed by the internal router and accessible to
    /// handlers
    pub fn state(&self) -> &S {
        self.router.state()
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
    pub(crate) fn service_connection(&self, peer: transport::Peer) -> Result<ServiceConnectionHandle> {
        // Preserve the tracing context in the event loop
        let callers_span = tracing::Span::current();

        // Each connection gets its own child cancellation token, that can be signaled separately,
        // but is also signaled whenever the service-level cancellation token is signaled.
        let cancellation_token = self.cancellation_token.child_token();

        if cancellation_token.is_cancelled() {
            // That's odd.. cancelation requested even as we're just getting started
            let _guard = callers_span.enter();
            tracing::warn!(
                "Cancellation already signaled when starting to service a new connection; connection will \
                 be dropped"
            );
            return Err(JsonRpcError::Cancelled);
        }

        // Create a channel that connection handles can use to send outgoing messages to the
        // connection's event loop
        let (tx, rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);

        // Create another one-shot channel that is a bit of a hack.  Buckle up:
        //
        // Each connection handle also holds a (clonable) future that reflects the end result of
        // the event loop, so that connection handles can trigger cancellation and then wait until
        // the event loop has properly shutdown, getting a string error back if anything went
        // wrong.
        //
        // Furthermore, handlers which are called from inside the event loop have the ability to
        // get a connection handle to the connection that they are being called from, so that those
        // handlers can raise notifications or call methods on the remote peers.
        //
        // But if connection handlers hold a future to the event loop, and inside the event loop it
        // must be able to vend connection handles to handlers, then that means the event loop must
        // have its own future!  :mind-blown:
        //
        // This one-shot channel is used so that we can spawn the event loop future, and make it
        // immediately block until we then pass a handle to itself via the channel.
        let (self_handle_tx, self_handle_rx) = oneshot::channel();

        let event_loop_handle = tokio::spawn({
            let router = self.router.clone();
            let custom_event_loop = self.custom_event_loop.clone();
            let cancellation_token = cancellation_token.clone();

            async move {
                // Get a grip on ourself, waiting for our progenitor to send us our own handle...
                let grip_on_myself = self_handle_rx.await.map_err(|_| {
                    // oneshot channels fail to read only when the sender is dropped.  The way the code
                    // is written it seems impossible for that to happen.  Hence the decision here
                    // to panic
                    tracing::error!("BUG: oneshot channel dropped immediately after spawn");
                    panic!("BUG: oneshot channel dropped immediately after spawn");
                })?;

                let conn = ServiceConnection::new(
                    router,
                    peer,
                    custom_event_loop,
                    cancellation_token,
                    grip_on_myself,
                    rx,
                );

                // Run the event loop, preserving the caller's span and also running it inside the
                // transport span.  To do that, enter the caller's span, and call the transport to
                // get its span (which is is assumed to construct on the fly with a span!-like
                // macro), so that the transport span will be a child of the current span.
                let transport_span = {
                    let _guard = callers_span.enter();
                    conn.peer.span().await
                };
                conn.event_loop()
                    .inspect_err(|e| {
                        // If this is an interesting error, log it
                        match e {
                            JsonRpcError::Cancelled => {
                                // This is expected, so don't log it
                            }
                            _ => {
                                // This is unexpected, so log it
                                tracing::error!("Event loop terminated due to an error: {}", e);
                            }
                        }
                    })
                    .instrument(transport_span)
                    .await
            }
        });

        // Transform the JoinHandle returned by `tokio::spawn` into a clonable future so that the
        // connection handles have the ability to wait for the loop to finish and thereby get a
        // clean shutdown.
        //
        // To do this the output of the future has to be `Clone`, which rules out `Result` with a
        // non-clonable error type.  So hack the result into one that just returns a string error
        // message
        let event_loop_fut = async move {
            let result = event_loop_handle.await;
            match result {
                Ok(Ok(())) => Result::<(), String>::Ok(()),
                Ok(Err(e)) => {
                    // This is an error returned by the event loop itself.  It was logged already
                    Result::<(), String>::Err(e.to_string())
                }
                Err(e) => {
                    // This is an error returned by the Tokio runtime.  Usually this happens
                    // because the async task paniced.  That seems unlikely in our case because
                    // we're professionals and we don't make mistrakes, but to appease the lawyers
                    // we'll capture the message anyway
                    Result::<(), String>::Err(e.to_string())
                }
            }
        }
        .boxed()
        .shared();

        let handle = ServiceConnectionHandle {
            outbound_messages: tx,
            cancellation_token: cancellation_token.clone(),
            event_loop_fut: event_loop_fut.boxed().shared(),
        };

        // Help the event loop future find itself, by sending it a handle to its own future via the
        // oneshot channel made for this purposee.
        //
        // Sometimes async Rust gets very philosophical...
        if self_handle_tx.send(handle.clone()).is_err() {
            // Almost certainly a bug.  Send fails because the receiver was dropped, but the
            // receiver should be running in a newly-spawned tokio task waiting for this exact
            // handle to be sent.  If it's dropped that must mean the task didn't start for some
            // reason.
            Err(JsonRpcError::Bug {
                message: "oneshot channel dropped immediately after spawning connection event loop"
                    .to_string(),
            })
        } else {
            Ok(handle)
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

    handle_oneself: ServiceConnectionHandle,

    /// Requests that have been sent, keyed by the request ID that was passed to the remote peer.
    /// Responses will come in with this ID specified.
    pending_outbound_requests: PendingRequestsMap,

    /// Operations that are running now, processing inbound messages received on this connection.
    /// These futures are polled as part of the event loop, and when they complete they yield the
    /// result of the operation.
    pending_inbound_operations: JoinSet<Option<types::Message>>,

    /// Subset of pending operations in [`Self::pending_inbound_operations`] that are for method
    /// calls, keyed by the async task ID that is processing the request.  In case that async
    /// task panics or is cancelled, this is used to communicate the panic back to the caller.
    pending_inbound_operation_request_ids: Arc<Mutex<HashMap<tokio::task::Id, types::Id>>>,

    outbound_messages: mpsc::Receiver<OutboundMessage>,
}

impl<S: Clone + Send + Sync + 'static> ServiceConnection<S> {
    fn new(
        router: router::Router<S>,
        peer: transport::Peer,
        custom_event_loop: Arc<dyn EventLoop<S>>,
        cancellation_token: CancellationToken,
        handle_oneself: ServiceConnectionHandle,
        outbound_messages: mpsc::Receiver<OutboundMessage>,
    ) -> Self {
        Self {
            router,
            custom_event_loop,
            peer,
            cancellation_token,
            pending_outbound_requests: Arc::new(Mutex::new(HashMap::new())),
            pending_inbound_operations: JoinSet::new(),
            pending_inbound_operation_request_ids: Arc::new(Mutex::new(HashMap::new())),
            handle_oneself,
            outbound_messages,
        }
    }

    async fn event_loop(mut self) -> Result<()> {
        tracing::debug!(state = std::any::type_name::<S>(), "Event loop is starting");

        let result = loop {
            tokio::select! {
                outbound_message = self.outbound_messages.recv() => {
                    match outbound_message {
                        Some(outbound_message) => {
                            self.handle_outbound_message(outbound_message).await;
                        },
                        None => {
                            // There are no more handles to this connection with which outbound
                            // messages can be sent.  This shouldn't happen since the connection
                            // holds a handle to itself...
                            tracing::error!(
                                "BUG: Handle closed connection to outbound connection channel; \
                                event loop terminating");
                            break Err(JsonRpcError::Bug {
                                message: "Handle closed connection to outbound connection channel".to_string()
                            });
                        }
                    }
                },
                result = self.peer.receive_message() => {
                    match result {
                        Ok(Some(transport::TransportMessage { metadata, message})) => {
                            self.handle_inbound_message(metadata, message).await;
                        }
                        Ok(None) => {
                            // The peer has closed the connection
                            tracing::debug!("Peer closed connection; event loop terminating");
                            break Ok(());
                        }
                        Err(e) => {
                            tracing::error!("Error receiving message from peer: {}", e);
                            break Err(e);
                        }
                    }
                },
                next = self.pending_inbound_operations.join_next_with_id(),
                    if !self.pending_inbound_operations.is_empty() => {
                    // One of the pending futures handling a previous inbound message has
                    // completed (successfully or due to a panic or cancellation)
                    if let Some(result) = next {
                        self.handle_pending_operation_completion(result).await;
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
                _ = self.cancellation_token.cancelled() => {
                        // The cancellation token for this connection has been triggered, either as
                        // part of a shutdown of the entire service or this particular connection.
                        tracing::info!("Cancellation signal received, shutting down connection");
                        break Err(JsonRpcError::Cancelled)
                    }
            }
        };

        // Event loop is over, maybe due to an error or maybe cancellation was signaled or the peer
        // connection is closed.
        //
        // To perform an orderly shutdown, operate on the remaining in-flight requests and bring
        // them to come conclusion.  *IF* the peer is already closed, then responses to requests
        // can't actually be sent to the remote peer, but we can still cancel requests initiated
        // from our connection handles, and shutdown the async tasks still in flight.

        // First, close the receiver for outbound messages, so no more can be sent to us.
        self.outbound_messages.close();

        // If there are any left in the outbound message queue, retrieve them and immediately
        // inform them that the connection is closed.
        while let Some(outbound_message) = self.outbound_messages.recv().await {
            match outbound_message {
                OutboundMessage::Method { request, response_tx } => {
                    tracing::debug!(request_id = %request.id,
                        method = %request.method,
                        "Cancelling outbound request due to shutdown");
                    let _ = response_tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
                }
                OutboundMessage::Notification {
                    notification,
                    send_confirmation_tx,
                } => {
                    tracing::debug!(notification = %notification.method,
                        "Cancelling outbound notification due to shutdown");
                    // Notifications are easier because there is no response expected.  The
                    // only reason there's a oneshot channel at all is just to confirm that
                    // the notification was passed off to the transport successfully.
                    let _ = send_confirmation_tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
                }
            }
        }

        // Any outbound messages that were actually sent to the remote peer and are awaiting
        // response, will obviously not get one now.  Let them all know the connection is closed.
        {
            let mut pending_outbound_requests = self.pending_outbound_requests.lock().unwrap();
            tracing::debug!(
                num_pending_outbound_requests = pending_outbound_requests.len(),
                "Responding with an error to all pending outbound requests"
            );
            for (id, tx) in pending_outbound_requests.drain() {
                // Send will fail if the receiver is already dropped, which could very well be the case
                // if the connection itself is dropped.  But if it's still around, let it down easy.
                tracing::debug!(request_id = %id,
                    "Responding to pending outbound request with error due to shutdown");
                let _ = tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
            }
        }

        // Any remaining pending incoming requests being processed as async tasks are also not
        // going to complete.  Abort them, and for the ones that are handling method call requests
        // from the remote peer, try to send an error response back.
        //
        // Note that if the event loop ended because the peer reported the connection dropped,
        // these responses will of course never be received, but we have to abort the tasks anyway,
        // may as well try to send the error response
        //
        // TODO: A future enhancement would be to have some configurable wait timeout and give
        // these pending incoming requests that much time to complete before hard aborting them.
        tracing::debug!(
            num_pending_inbound_operations = self.pending_inbound_operations.len(),
            "Cancelling all pending async tasks"
        );

        self.pending_inbound_operations.abort_all();
        while let Some(result) = self.pending_inbound_operations.join_next_with_id().await {
            self.handle_pending_operation_completion(result).await;
        }

        let (result, termination_reason) = match result {
            Err(JsonRpcError::Cancelled) => (Ok(()), "cancelled_token_triggered"),
            Err(e) => (Err(e), "error"),
            Ok(()) => (Ok(()), "remote_peer_closed_connection"),
        };

        tracing::debug!(termination_reason, "Event loop is exiting");
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
                    self.pending_outbound_requests
                        .lock()
                        .unwrap()
                        .insert(id, response_tx);
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
        let request_id = if let types::Message::Request(request) = &inbound_message {
            Some(request.id.clone())
        } else {
            None
        };
        tracing::trace!(?request_id, "About to spawn an inbound message handler task");

        let task_id = self.spawn_operation({
            let router = self.router.clone();
            let pending_requests = self.pending_outbound_requests.clone();
            let handle_oneself = self.handle_oneself.clone();
            async move {
                tracing::trace!("Inside inbound message handler task");
                Self::handle_inbound_message_task(
                    &router,
                    &pending_requests,
                    handle_oneself,
                    metadata,
                    inbound_message,
                )
                .await
            }
        });
        tracing::trace!(%task_id,
            ?request_id,
            "Inbound message handler task spawned");

        // If this is a request to call a method, that means the caller will be waiting for a
        // response.
        // Store the request ID along side the task ID, so that if the task panics or if we have to
        // abort it, we can still communicate the error back to the waiting client
        if let Some(request_id) = request_id {
            let old_request_id = self
                .pending_inbound_operation_request_ids
                .lock()
                .unwrap()
                .insert(task_id, request_id);
            #[cfg(debug_assertions)]
            if let Some(old_request_id) = old_request_id {
                // This should never happen.  It suggests that two tasks got the same task ID.
                // That will lead to some very confusing behavior!
                tracing::error!(
                    "BUG: Task ID {} overwriting request ID {} with new request ID; this cannot ever happen!",
                    task_id,
                    old_request_id,
                );
            }
            #[cfg(not(debug_assertions))]
            let _ = old_request_id;
        }
    }

    /// Handler of inbound messages suitable for running as a 'static async task representing a
    /// pending operation.
    async fn handle_inbound_message_task(
        router: &router::Router<S>,
        pending_requests: &PendingRequestsMap,
        handle_oneself: ServiceConnectionHandle,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: types::Message,
    ) -> Option<types::Message> {
        match inbound_message {
            crate::Message::Batch(messages) => {
                // Batch messages are handled in parallel, with their results combined at the end
                let batch_futures = messages.into_iter().map(|message| {
                    let metadata = metadata.clone();

                    Self::handle_inbound_message_task(
                        router,
                        pending_requests,
                        handle_oneself.clone(),
                        metadata,
                        message,
                    )
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
                let invocation_request =
                    handler::InvocationRequest::from_request_message(handle_oneself, metadata, request);

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
                let invocation_request = handler::InvocationRequest::from_notification_message(
                    handle_oneself,
                    metadata,
                    notification,
                );

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

    /// An async task that was started to handle a request from the remote peer (invoke a method,
    /// raise a notification) has completed, either successfully or not.
    async fn handle_pending_operation_completion(
        &mut self,
        result: Result<(tokio::task::Id, Option<types::Message>), tokio::task::JoinError>,
    ) {
        match result {
            Ok((task_id, response)) => {
                // Remove this from the map of pending tasks to request IDs since
                // it's not pending anymore
                let request_id = self
                    .pending_inbound_operation_request_ids
                    .lock()
                    .unwrap()
                    .remove(&task_id);
                let request_id_string = request_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .unwrap_or("None".to_string());
                tracing::trace!(%task_id,
                                    request_id = %request_id_string,
                                    "Pending operation completed");

                if let Some(message) = response {
                    // Future ran to completion, and produced a response message.
                    // Note that this is technically also "outbound", but we use that
                    // term to mean messages that are being sent by some caller via the
                    // service handle to the remote peer, not messages that are sent in
                    // response to previously received requests.  Therefore this
                    // message we can pass directly to the peer
                    if let Err(e) = self.peer.send_message(message).await {
                        tracing::error!(%task_id,
                            request_id = %request_id_string,
                            err = %e,
                            "Error sending completed task response to remote peer");
                    }
                } else {
                    // Future ran to completion, but there is no response to send.
                    // Nothing further to do
                }
            }
            Err(join_err) => {
                // This is JoinError from tokio, it could mean that the task
                // paniced, or that it was cancelled.  Let's not let this kill the
                // whole event loop
                let task_id = join_err.id();
                let request_id = self
                    .pending_inbound_operation_request_ids
                    .lock()
                    .unwrap()
                    .remove(&task_id);
                let request_id_string = request_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .unwrap_or("None".to_string());

                tracing::error!(is_panic = join_err.is_panic(),
                                    is_cancelled = join_err.is_cancelled(),
                                    %task_id,
                                    request_id = %request_id_string,
                                    join_err = %join_err,
                                    "Pending operation panicked or was cancelled");

                // If this task ID is associated with a JSON RPC request ID,
                // that means it was supposed to be handling a method request,
                // so send a response back to the remote peer indicating that
                // the request failed
                if let Some(request_id) = request_id {
                    if let Err(e) = self
                        .peer
                        .send_message(types::Message::Response(types::Response::error(
                            request_id,
                            types::ErrorCode::InternalError,
                            "Task was cancelled or panicked",
                            None,
                        )))
                        .await
                    {
                        tracing::error!(%task_id,
                            request_id = %request_id_string,
                            err = %e,
                            "Error sending failed task error response to remote peer");
                    }
                }
            }
        }
    }

    /// Spawn a future that is processing an incoming message and may or may not produce some
    /// output.
    ///
    /// Any output that the future produces will be written to the remote peer when the future
    /// completes.
    ///
    /// Returns the Tokio task ID assigned to the spawned task
    fn spawn_operation(
        &mut self,
        operation: impl Future<Output = Option<types::Message>> + Send + 'static,
    ) -> tokio::task::Id {
        // Spawn the operation and add it to the pending operations list
        //
        // Always propagate whatever span we're in now, to the spawned future
        let span = tracing::Span::current();
        self.pending_inbound_operations
            .spawn(operation.instrument(span))
            .id()
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
    #[allow(clippy::type_complexity)] // It's more clear here than in a separate type alias used just once
    event_loop_fut: futures::future::Shared<
        Pin<Box<dyn futures::Future<Output = Result<(), String>> + std::marker::Send>>,
    >,
}

impl ServiceConnectionHandle {
    /// Create a new handle that is just for use in tests that need this handle in order to
    /// construct a request.  This handle is not connected to anything, and will fail if it is used
    /// to invoke anything.
    #[cfg(test)]
    pub(crate) fn new_test_handle() -> Self {
        let (tx, _rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);
        let cancellation_token = CancellationToken::new();
        let event_loop_fut = futures::future::ready(Result::Err(
            "this is a fake handle it's not connected!".to_string(),
        ))
        .boxed()
        .shared();

        Self {
            outbound_messages: tx,
            cancellation_token: cancellation_token.clone(),
            event_loop_fut,
        }
    }

    /// A clone of the cancellation token for the connection that this handle corresponds to.
    ///
    /// Triggering this token will cause the connection's event loop to perform an orderly
    /// shutdown, returning an error response for any pending requests still outstanding with the
    /// remote peer, and cancelling the execution of any pending operations
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    /// Signal the connection cancellation token to shutdown the event loop, and wait until the
    /// event loop background task has finished running
    ///
    /// Note that this will immediately shutdown the connection's event loop which will impact all
    /// other connection handles as well.
    ///
    /// This thread is cancel safe in that, once the cancellation token is triggered, the
    /// connection shutdown will proceed whether or not this future is polled to completion.
    /// However if the future is dropped it completes but after the cancellation token is
    /// triggered, the caller will have no way of knowing when the event loop task has finished.
    ///
    /// The result of this async operation is the result of the event loop itself.  If this returns
    /// Ok or Err, either way, the event loop has stopped.
    pub async fn shutdown(self) -> Result<(), String> {
        self.cancellation_token.cancel();

        self.event_loop_fut.await
    }

    /// Send a request to invoke a method without any parameters, awaiting a response.
    pub async fn call<Resp>(&self, method: &str) -> Result<Resp>
    where
        Resp: DeserializeOwned,
    {
        let response = self.call_raw(method, None).await?;

        serde_json::from_value(response.clone()).map_err(|e| JsonRpcError::DeserResponse {
            source: e,
            type_name: std::any::type_name::<Resp>(),
            response,
        })
    }

    /// Send a request to invoke a method with parameters, awaiting a response.
    pub async fn call_with_params<Req, Resp>(&self, method: &str, params: Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let response = self
            .call_raw(
                method,
                serde_json::to_value(params).map_err(|e| JsonRpcError::SerRequest {
                    source: e,
                    type_name: std::any::type_name::<Req>(),
                })?,
            )
            .await?;

        serde_json::from_value(response.clone()).map_err(|e| JsonRpcError::DeserResponse {
            source: e,
            type_name: std::any::type_name::<Resp>(),
            response,
        })
    }

    /// Send a request to invoke a method, awaiting a response, using the raw JSON types that are
    /// mapped directly into the JSON RPC messages.
    ///
    /// In most cases callers should prefer [`Self::call`] or [`Self::call_with_params`]
    #[instrument(skip_all, fields(method))]
    pub async fn call_raw(&self, method: &str, params: impl Into<Option<JsonValue>>) -> Result<JsonValue> {
        let (tx, rx) = oneshot::channel();
        let request_id = types::Id::Str(Uuid::now_v7().to_string());

        // Submit this request to the connection's event loop for processing
        if self
            .outbound_messages
            .send(OutboundMessage::Method {
                request: types::Request::new(request_id.clone(), method, params.into()),
                response_tx: tx,
            })
            .await
            .is_err()
        {
            // The event loop is no longer running, so the connection must be closed
            tracing::debug!(
                %request_id,
                "Outbound messages channel closed when trying to send method call; connection is presumably \
                 closed or event loop is terminated"
            );
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
                        // This is a successful response
                        Ok(success_response.result)
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
                    %request_id,
                    "BUG: One-shot channel was dropped before the event loop could send a response"
                );
                Err(JsonRpcError::PendingRequestConnectionClosed)
            }
        }
    }

    /// Send a notification to the remote peer, without any parameters, neither expecting nor
    /// waiting for a response.
    ///
    /// A successful completion of this call merely means that the notification message was formed
    /// and written over the wire successfully.  There is no way to know how the remote peer
    /// processed the notification, if at all.
    pub async fn raise(&self, method: &str) -> Result<()> {
        self.raise_raw(method, None).await
    }

    /// Send a notification to the remote peer, including some notification arguments,
    /// neither expecting nor waiting for a response.
    ///
    /// A successful completion of this call merely means that the notification message was formed
    /// and written over the wire successfully.  There is no way to know how the remote peer
    /// processed the notification, if at all.
    pub async fn raise_with_params<Req>(&self, method: &str, params: Req) -> Result<()>
    where
        Req: Serialize,
    {
        self.raise_raw(
            method,
            serde_json::to_value(params).map_err(|e| JsonRpcError::SerRequest {
                source: e,
                type_name: std::any::type_name::<Req>(),
            })?,
        )
        .await
    }

    /// Send a notification to the remote peer, neither expecting nor waiting for a response.
    ///
    /// This is a raw version that operates on the raw JSON types that are mapped directly into the
    /// JSON RPC messages.  Most callers should use [`Self::raise`] or [`Self::raise_with_params`]
    /// instead.
    ///
    /// A successful completion of this call merely means that the notification message was formed
    /// and written over the wire successfully.  There is no way to know how the remote peer
    /// processed the notification, if at all.
    #[instrument(skip_all, fields(method))]
    pub async fn raise_raw(&self, method: &str, params: impl Into<Option<JsonValue>>) -> Result<()> {
        let (tx, rx) = oneshot::channel();

        if self
            .outbound_messages
            .send(OutboundMessage::Notification {
                notification: types::Notification::new(method, params.into()),
                send_confirmation_tx: tx,
            })
            .await
            .is_err()
        {
            // The event loop is no longer running, so the connection must be closed
            tracing::debug!(
                "Outbound messages channel closed when trying to send notification; connection is \
                 presumably closed or event loop is terminated"
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{handler, testing};
    use std::time::Duration;

    // Construct a dummy service with a single method "echo" and a single notification "hi"
    fn make_test_service() -> Service<()> {
        async fn echo_handler(handler::Params(params): handler::Params<JsonValue>) -> JsonValue {
            tracing::debug!(?params, "Echo handler called");
            params
        }

        async fn hi_handler(handler::Params(params): handler::Params<JsonValue>) {
            tracing::debug!(?params, "Hi handler called");
        }

        // A handler that sleeps for a long time before responding
        async fn slow_handler(handler::Params(params): handler::Params<JsonValue>) -> JsonValue {
            tracing::debug!(?params, "Slow handler called");
            tokio::time::sleep(Duration::from_secs(60)).await;
            tracing::debug!("Slow handler finally finished");
            JsonValue::String("done".to_string())
        }

        let mut router = router::Router::new_stateless();
        router.register_handler("echo", echo_handler);
        router.register_handler("hi", hi_handler);
        router.register_handler("slow", slow_handler);

        Service::new(router)
    }

    /// Verify the behavior of a handle when the corresponding connection event loop has already
    /// stopped for whatever reason
    #[tokio::test]
    async fn handle_behavior_when_event_loop_stopped() {
        testing::init_test_logging();

        let service = make_test_service();
        let (_client_transport, server_transport) = testing::setup_test_channel();

        let service_connection_handle = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();

        let handle_clone = service_connection_handle.clone();

        // Shutdown this connection
        service_connection_handle.shutdown().await.unwrap();

        // Use the clone handle to do some operations.  They should fail of course, and fail in a
        // friendly way

        let result = handle_clone.call::<JsonValue>("echo").await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));

        let result = handle_clone.raise("hi").await;

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));

        // Without doing anything to signal a shutdown, we know that the event loop is already
        // stopped so the future in this cloned handle should also reflect that
        handle_clone.event_loop_fut.clone().await.unwrap();

        // Finally, signal a shutdown from this handle as well.  Of course the loop has already
        // shutdown but we should still get a future that is completed
        handle_clone.shutdown().await.unwrap();
    }

    /// Test what happens when we cancel a connection that has an in-progress request
    /// This simulates a handler that takes a long time to process
    #[tokio::test]
    async fn test_cancel_with_pending_request() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let server_handle = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();

        // Create another service as a client so we can send requests
        let client_service = Service::new(router::Router::new_stateless());
        let client_handle = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();

        // Spawn a task that will call the slow handler
        let slow_task = tokio::spawn(async move {
            let result = client_handle.call::<JsonValue>("slow").await;
            (result, client_handle)
        });

        // Give the slow handler a little time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now cancel the server connection while the slow handler is still running
        server_handle.shutdown().await.unwrap();

        // The call should fail with a PendingRequestConnectionClosed error
        let (result, _) = slow_task.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));
    }

    /// Test what happens when we have outgoing requests that are never answered
    /// This tests the behavior when the service connection shuts down with pending outbound
    /// requests
    #[tokio::test]
    async fn test_shutdown_with_pending_outbound_requests() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, _server_transport) = testing::setup_test_channel();

        // There's no server for this transport, so any requests will never get a response
        let handle = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();

        // Spawn task to call a method - this will never be answered since there's no server
        let call_fut = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call::<JsonValue>("echo").await }
        });

        // Give the request some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now shut down the connection
        handle.shutdown().await.unwrap();

        // The call should fail with a PendingRequestConnectionClosed error
        let result = call_fut.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));
    }

    /// Test what happens when a remote peer closes the connection while we have pending requests
    #[tokio::test]
    async fn test_remote_close_with_pending_requests() {
        testing::init_test_logging();

        let (client_transport, server_transport) = testing::setup_test_channel();
        let service = make_test_service();

        // Create the client handle
        let handle = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();

        // Spawn task to call a method
        let call_fut = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call::<JsonValue>("echo").await }
        });

        // Give the request some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Close the server stream which will make the client's receive_message return None
        drop(server_transport);

        // The call should fail with a PendingRequestConnectionClosed error
        let result = call_fut.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));

        // The handle's event loop future should complete successfully
        assert!(handle.event_loop_fut.clone().await.is_ok());
    }

    /// Test that notifications are properly handled during shutdown
    #[tokio::test]
    async fn test_shutdown_with_pending_notifications() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, _) = testing::setup_test_channel();

        // Create handle with no server
        let handle = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();

        // Spawn a task to send a notification
        let notification_task = tokio::spawn({
            let handle_clone = handle.clone();
            async move { handle_clone.raise("hi").await }
        });

        // Give the notification some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now shut down the connection
        handle.shutdown().await.unwrap();

        // The notification should fail with a PendingRequestConnectionClosed error
        let result = notification_task.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));
    }

    /// Test multiple concurrent pending requests and their cancellation
    #[tokio::test]
    async fn test_multiple_concurrent_pending_requests() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let server_handle = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();

        // Create another service as a client so we can send requests
        let client_service = Service::new(router::Router::new_stateless());
        let client_handle = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();

        // Spawn multiple tasks that will call the slow handler
        let mut tasks = Vec::new();
        for _ in 0..5 {
            let task = tokio::spawn({
                let handle = client_handle.clone();
                async move { handle.call::<JsonValue>("slow").await }
            });
            tasks.push(task);
        }

        // Give the slow handlers a little time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now cancel the server connection while the slow handlers are still running
        server_handle.shutdown().await.unwrap();

        // All calls should fail with a PendingRequestConnectionClosed error
        for task in tasks {
            let result = task.await.unwrap();
            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                JsonRpcError::PendingRequestConnectionClosed
            ));
        }
    }
}
