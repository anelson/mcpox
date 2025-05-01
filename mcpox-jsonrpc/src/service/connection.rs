//! A single connection to a remote peer is serviced by an event loop, in this module as part of
//! [`ServiceConnection`].
//!
//! This is not part of the public API, but this can be thought of as the beating heart of either a
//! JSON-RPC client or server.
//!
//! See [`ServiceConnectionHandle`] for the cheaply-clonable handle to a single connection's event
//! loop with which calls to the remote peer can be made.
use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;

use futures::{FutureExt, TryFutureExt};
use itertools::{Either, Itertools};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{ServiceConfig, connection_handle::ServiceConnectionHandle};
use crate::{JsonRpcError, Result};
use crate::{error, handler, router, transport, types};

/// The size of the mpsc channel that is used to send messages to an existing connection.
/// After this many messages are queued and not yet transmitted, backpressure is excerted on the
/// senders
pub(super) const CONNECTION_CHANNEL_BOUNDS: usize = 16;

/// The future used outside of this module to represent the running event loop servicing a
/// connection.
///
/// This is boxed and wrapped in [`futures::future::Shared`] to make is clonable for maximum
/// flexibility.
pub(super) type ServiceConnectionEventLoopFuture =
    futures::future::Shared<Pin<Box<dyn futures::Future<Output = Result<(), String>> + Send>>>;

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

/// [`EventLoop`] impl used by default if the caller doesn't provide one.
/// It does nothing and its returned future never completes.
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

/// Start the service connection for a remote peer, and return a handle that can be used to
/// interact with the service connection.
///
/// This will create a future that will run for the life of the peer connection, constantly
/// polling the peer, optionally polling the custom event loop if one was provided, and
/// periodically performing housekeeping tasks.
///
/// NOTE: The caller is responsible for polling the returned future to ensure that the
/// connection is serviced in a timely fashion.  In most cases this should be spawned into a
/// new async task using [`tokio::spawn`] or something equivalent.
pub(crate) fn service_connection<S: Clone + Send + Sync + 'static>(
    config: ServiceConfig,
    router: router::Router<S>,
    custom_event_loop: Option<Arc<dyn EventLoop<S>>>,
    cancellation_token: CancellationToken,
    peer: transport::Peer,
) -> Result<(ServiceConnectionEventLoopFuture, ServiceConnectionHandle)> {
    // Preserve the tracing context in the event loop
    let callers_span = tracing::Span::current();

    if cancellation_token.is_cancelled() {
        // That's odd.. cancelation requested even as we're just getting started
        let _guard = callers_span.enter();
        tracing::warn!(
            "Cancellation already signaled when starting to service a new connection; connection will be \
             dropped"
        );
        return Err(JsonRpcError::Cancelled);
    }

    // Create a channel that connection handles can use to send commands to the
    // connection's event loop
    let (command_tx, command_rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);

    // Create a channel that connection handles can use to send outgoing messages to the
    // connection's event loop
    let (outgoing_tx, outgoing_rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);

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

    // Define the future that constitutes the event loop.
    // NOTE: It's not been polled yet, so none of this code is actually executing until the caller
    // of this function either spawns the future into a dedicated async task or starts to poll it
    // some other way.
    let event_loop_fut = {
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
                config,
                router,
                peer,
                custom_event_loop.unwrap_or_else(|| Arc::new(NoOpEventLoop(std::marker::PhantomData))),
                cancellation_token,
                grip_on_myself,
                command_rx,
                outgoing_rx,
            );
            conn.run_loop().await
        }
    };

    // Transform the regular event loop future we just created into one that is clonable and
    // connection handles have the ability to wait for the loop to finish and thereby get a
    // clean shutdown.
    //
    // To do this the output of the future has to be `Clone`, which rules out `Result` with a
    // non-clonable error type.  So hack the result into one that just returns a string error
    // message
    //
    // While we're at it, add some extra protective wrapping to catch panics in the event loop.
    // This is hopefully excessive since each handler invocation is wrapped in similar protective
    // handling, and of course we do not make mistrakes in our own code.  Furthermore, if this
    // future is then spawned into a new async task with `tokio::spawn` or its ilk that will add
    // its own panic handling.  But even in that case, we want our event loop futures that we wait
    // on to be able to tell us about panics, rather than relying on the user to be responsible.
    let event_loop_fut = async move {
        let result = AssertUnwindSafe(event_loop_fut).catch_unwind().await;
        match result {
            Ok(Ok(())) => Result::<(), String>::Ok(()),
            Ok(Err(e)) => {
                // This is an error returned by the event loop itself.  It was logged already
                Result::<(), String>::Err(e.to_string())
            }
            Err(e) => {
                // This is an error returned by `catch_unwind` because the async task paniced.
                // That seems unlikely in our case because
                // we're professionals and we don't make mistrakes, but to appease the lawyers
                // we'll capture the message anyway
                Result::<(), String>::Err(error::panic_err_to_string(e))
            }
        }
    }
    .instrument(callers_span)
    .boxed()
    .shared();

    let handle = ServiceConnectionHandle::new(
        command_tx,
        outgoing_tx,
        cancellation_token.clone(),
        event_loop_fut.clone(),
    );

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
            message: "oneshot channel dropped immediately after spawning connection event loop".to_string(),
        })
    } else {
        Ok((event_loop_fut, handle))
    }
}

/// Command types that are sent to the event loop over the command channel.
///
/// We could just hack this stuff into `OutboundMessage` but there is an important semantic
/// difference: [`OutboundMessage`] is always somethign that needs to get sent out to the remote
/// peer, while commands do not cross the wire and are used to send instructions to the event loop
/// via a queue.
#[derive(Clone, Debug)]
pub(super) enum Command {
    /// See [`ServiceConnectionHandle::cancel_client_request`]
    CancelPendingOutboundRequest { request_id: types::Id },
    /// See [`ServiceConnectionHandle::cancel_server_request`]
    CancelPendingInboundRequest { request_id: types::Id },
}

/// The type containing an outgoing message that is to be sent via the peer, and the one-shot
/// channel to which the result should be sent.
pub(super) enum OutboundMessage {
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
    /// This is a batch of method calls and/or notifications
    Batch {
        requests: Vec<(types::Request, oneshot::Sender<Result<types::Response>>)>,
        notifications: Vec<types::Notification>,
        send_confirmation_tx: oneshot::Sender<Result<()>>,
    },
}

/// A message inbound, received from the remote peer to be processed by the event loop.
/// Depending on what kind of message it is, it can be very quickly processed inline in the
/// event loop, or it needs to be processed in a separately spawned async task so as not to
/// block the event loop.
#[derive(Clone, Debug)]
enum InboundMessage {
    Inline(InlineInboundMessage),
    Async(AsyncInboundMessage),

    /// In the case of a Batch message that has a mix of inline (ie, responses) and async (ie,
    /// method calls) messages, the result is a hybrid with components that are processed
    /// separately.
    Hybrid(InlineInboundMessage, AsyncInboundMessage),
}

/// A message that is processed in the event loop async task because processing it is very fast
/// and tightly bounded
#[derive(Clone, Debug)]
enum InlineInboundMessage {
    /// Responses are very fast to process; we look them up in the pending responses list and
    /// dispatch them.
    Response(types::Response),

    /// Batches of inline messages are themselves processed inline
    Batch(Vec<InlineInboundMessage>),
}
/// A message that is processed in a dedicated async task spawned for that purpose, because its
/// processing is unbounded and could otherwise block the event loop
#[derive(Clone, Debug)]
enum AsyncInboundMessage {
    /// A notification handler can run in unbounded time
    Notification(types::Notification),
    /// A method invocation request can run in unbounded time
    Request(types::Request),
    /// Invalid requests are trivial, but the spec only allows a single batch response to a batch
    /// request, so if it contains an invalid request and possibly other valid requests, the
    /// invalid request error needs to be included in the batch with the valid responses
    InvalidRequest(types::InvalidRequest),
    /// A batch of async messages is of course itself unbounded
    Batch(Vec<AsyncInboundMessage>),
}

/// Decode a JSON-RPC message into the inbound message enum to determine how it should be handled.
impl TryFrom<types::Message> for InboundMessage {
    type Error = types::ErrorDetails;

    fn try_from(message: types::Message) -> Result<Self, Self::Error> {
        fn flatten_batch(messages: Vec<crate::Message>) -> Vec<crate::Message> {
            messages
                .into_iter()
                .flat_map(|message| match message {
                    crate::Message::Batch(messages) => flatten_batch(messages),
                    _ => vec![message],
                })
                .collect()
        }

        match message {
            crate::Message::Batch(messages) => {
                // This is an interesting case.  According to the spec, a batch can have a mix of
                // any value messages in it (except for other batches).  So it could be some
                // notifications, some method requests, ands ome responses to previous requests,
                // all mixed up.  In our case those are processed in two different ways depending
                // upon the message type.  So we need to divide them up by individual request type.
                //
                // What makes this workable is that the spec also says that the response to a batch
                // message should contain only responses to method calls, so it's legal in the spec
                // that we process responses separately from calls and notifications.

                // Step one is to decode the message into an `InboundMessage` enum which indicates
                // how the message is to be processed
                //
                // That conversion is fallible, so get it done and return any errors before we
                // proceed.
                let inbound_messages = flatten_batch(messages)
                    .into_iter()
                    .map(|message| {
                        // The spec is not clear on whether or not a batch request can itself
                        // contain nested batch requests.  Seems like a rather rude thing to do in
                        // any case.
                        //
                        // We have flattened the messages to remove any nested batches and just
                        // have one batch with all of the messages; this is just another level of
                        // check
                        debug_assert!(!matches!(message, crate::Message::Batch(_)));
                        InboundMessage::try_from(message)
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                let (inline_messages, async_messages): (Vec<_>, Vec<_>) =
                    inbound_messages.into_iter().partition_map(|message| {
                        match message {
                            InboundMessage::Inline(inline) => {
                                // This is a response or an invalid request, so it can be processed
                                // inline
                                Either::Left(inline)
                            }
                            InboundMessage::Async(async_message) => {
                                // This is a notification or a method call, so it needs to be
                                // processed in a separate async task
                                Either::Right(async_message)
                            }
                            InboundMessage::Hybrid(_, _) => {
                                // This can't happen because we have already flattened the batch so
                                // each message is either inline or async
                                unreachable!("Somehow a nested batch image escaped our flattening logic")
                            }
                        }
                    });

                match (inline_messages.is_empty(), async_messages.is_empty()) {
                    (true, true) => {
                        // Empty batches are a violation of the JSON RPC spec, in the section on
                        // 6, "batch":
                        // > If the batch rpc call itself fails to be recognized as an valid JSON or
                        // > as an Array with at least one value, the response from the Server MUST
                        // > be a single Response object. If there are no Response objects contained
                        // > within the Response array as it is to be sent to the client, the server
                        // > MUST NOT return an empty Array and should return nothing at all.
                        //
                        // In this case the batch is not an array with at least one value, so the
                        // response is a single Response object.
                        tracing::error!(
                            "Received empty batch message; this is not valid under the JSON RPC spec"
                        );
                        Err(types::ErrorDetails::invalid_request(
                            "Empty batch message is not valid",
                            None,
                        ))
                    }
                    (false, true) => Ok(InboundMessage::Inline(InlineInboundMessage::Batch(
                        inline_messages,
                    ))),
                    (true, false) => Ok(InboundMessage::Async(AsyncInboundMessage::Batch(async_messages))),
                    (false, false) => Ok(InboundMessage::Hybrid(
                        InlineInboundMessage::Batch(inline_messages),
                        AsyncInboundMessage::Batch(async_messages),
                    )),
                }
            }
            crate::Message::Request(request) => {
                Ok(InboundMessage::Async(AsyncInboundMessage::Request(request)))
            }
            crate::Message::Notification(notification) => Ok(InboundMessage::Async(
                AsyncInboundMessage::Notification(notification),
            )),
            crate::Message::Response(response) => {
                Ok(InboundMessage::Inline(InlineInboundMessage::Response(response)))
            }
            crate::Message::InvalidRequest(invalid_request) => Ok(InboundMessage::Async(
                AsyncInboundMessage::InvalidRequest(invalid_request),
            )),
        }
    }
}

/// Map between a request ID that was sent to the remote peer and the one-shot channel to which
/// the eventual response from the remote peer to that request should be sent.
type PendingOutboundRequestsMap = HashMap<types::Id, oneshot::Sender<Result<types::Response>>>;

/// Information about a pending inbound method call
/// which is pending because an async task was spawned to handle it and that task is still running.
///
/// Only method calls are of interest because method calls 1) have request IDs, and 2) can be
/// cancelled with a cancellation token.  Notifications are also handled by spawning async tasks
/// but those are fire-and-forget and don't have a cancellation mechanism.
#[derive(Debug)]
struct PendingInboundMethodCall {
    task_id: tokio::task::Id,
    request_id: types::Id,
    cancellation_token: CancellationToken,
}

/// A logical connection to a remote peer (in either the client or server role)
///
/// This is used to implement the event loop for the connection, processing messages back and
/// forth.  Note that it's not Sync and there are no mutexes; a single connection is serviced in a
/// single async task without any need for locking.  Code outside of the event loop communicates
/// with it using [`ServiceConnectionHandle`] which uses channels and cancellation tokens to
/// interact with the event loop.
struct ServiceConnection<S: Clone + Send + Sync + 'static> {
    config: ServiceConfig,

    router: router::Router<S>,

    custom_event_loop: Arc<dyn EventLoop<S>>,

    peer: transport::Peer,

    cancellation_token: CancellationToken,

    handle_oneself: ServiceConnectionHandle,

    /// Requests that have been sent, keyed by the request ID that was passed to the remote peer.
    /// Responses will come in with this ID specified.
    pending_outbound_requests: PendingOutboundRequestsMap,

    /// Operations that are running now, processing inbound messages received on this connection.
    /// These futures are polled as part of the event loop, and when they complete they yield the
    /// result of the operation.
    pending_inbound_operations: JoinSet<Option<types::Message>>,

    /// Subset of pending operations in [`Self::pending_inbound_operations`] that are for method
    /// calls, keyed by the async task ID that is processing the request.
    pending_inbound_operations_by_task_id: HashMap<tokio::task::Id, Arc<PendingInboundMethodCall>>,

    /// Subset of pending operations in [`Self::pending_inbound_operations`] that are for method
    /// calls, keyed by the request ID used by the remote peer to identify the request.
    pending_inbound_operations_by_request_id: HashMap<types::Id, Arc<PendingInboundMethodCall>>,

    commands: mpsc::Receiver<Command>,

    outbound_messages: mpsc::Receiver<OutboundMessage>,
}

impl<S: Clone + Send + Sync + 'static> ServiceConnection<S> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: ServiceConfig,
        router: router::Router<S>,
        peer: transport::Peer,
        custom_event_loop: Arc<dyn EventLoop<S>>,
        cancellation_token: CancellationToken,
        handle_oneself: ServiceConnectionHandle,
        commands: mpsc::Receiver<Command>,
        outbound_messages: mpsc::Receiver<OutboundMessage>,
    ) -> Self {
        Self {
            config,
            router,
            custom_event_loop,
            peer,
            cancellation_token,
            pending_outbound_requests: HashMap::new(),
            pending_inbound_operations: JoinSet::new(),
            pending_inbound_operations_by_task_id: HashMap::new(),
            pending_inbound_operations_by_request_id: HashMap::new(),
            handle_oneself,
            commands,
            outbound_messages,
        }
    }

    /// Entry point for the event loop running in a dedicated async task
    async fn run_loop(self) -> Result<()> {
        // The run loop is already in the span propgated from the caller to service_connection.
        // Add to that the span with information about the transport.
        let transport_span = self.peer.span().await;
        self.event_loop()
            .inspect_err(|e| {
                // If this is an interesting error, log it
                match e {
                    JsonRpcError::Cancelled => {
                        // This is expected, so don't log it
                    }
                    _ => {
                        // This is unexpected, so log it
                        tracing::error!(err = %e,
                            "Event loop terminated due to an error");
                    }
                }
            })
            .instrument(transport_span)
            .await
    }

    /// Implementation of the event loop for the service connection
    async fn event_loop(mut self) -> Result<()> {
        tracing::debug!(state = std::any::type_name::<S>(), "Event loop is starting");

        let result = loop {
            tokio::select! {
                command = self.commands.recv() => {
                    match command {
                        Some(command) => {
                            self.handle_command(command).await;
                        },
                        None => {
                            // There are no more handles to this connection with which commands
                            // can be sent.  This shouldn't happen since the connection
                            // holds a handle to itself...
                            tracing::error!(
                                "BUG: Handle closed connection to command channel; \
                                event loop terminating");
                            break Err(JsonRpcError::Bug {
                                message: "Handle closed connection to command channel".to_string()
                            });
                        }
                    }
                },
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
                                "BUG: Handle closed connection to outbound messages channel; \
                                event loop terminating");
                            break Err(JsonRpcError::Bug {
                                message: "Handle closed connection to outbound messages channel".to_string()
                            });
                        }
                    }
                },
                result = self.peer.receive_message() => {
                    match result {
                        Ok(Some(transport_message)) => {
                            if let Err(error_details) = self.handle_inbound_message(transport_message).await {
                                // Some kind of immediate error handling this message, so report the
                                // error right away
                                let _ = self.send_message(types::Message::Response(
                                    types::Response::error_detail(types::Id::Null, error_details)
                                )).await;
                            }
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
        // them to some conclusion.  *IF* the peer is already closed, then responses to requests
        // can't actually be sent to the remote peer, but we can still cancel requests initiated
        // from our connection handles, and shutdown the async tasks still in flight.

        // First, close the receiver for outbound messages and commands, so no more can be sent by anyone
        // holding a handle to this service connection
        self.commands.close();
        self.outbound_messages.close();

        // Signal our cancellation token.  It might already be signaled, but there are other
        // reasons the event loop might exit.  All per-request cancellation tokens are children of
        // the connection cancellation token, so if we have any running handlers that support
        // cancellation this will give them a chance to abort gracefully.
        if !self.cancellation_token.is_cancelled() {
            tracing::debug!("Cancelling connection cancellation token after event loop exit");
            self.cancellation_token.cancel();
        }

        // All commands in the command queue will be ignored now since the loop is shutting down
        while let Some(command) = self.commands.recv().await {
            tracing::debug!(?command, "Command is ignored due to shutdown");
        }

        // If there are any messages left in the outbound message queue, retrieve them and immediately
        // inform whoever send them that the connection is closed.
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
                OutboundMessage::Batch {
                    requests,
                    notifications,
                    send_confirmation_tx,
                } => {
                    if !notifications.is_empty() {
                        tracing::debug!(
                            num_notifications = notifications.len(),
                            "Cancelling outbound batch notifications due to shutdown"
                        );
                    }

                    if !requests.is_empty() {
                        tracing::debug!(
                            num_requests = requests.len(),
                            "Cancelling outbound batch requests due to shutdown"
                        );

                        // Send error to all method response channels
                        for (request, response_tx) in requests {
                            tracing::debug!(request_id = %request.id,
                                method = %request.method,
                                "Cancelling outbound batch request due to shutdown");
                            let _ = response_tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
                        }
                    }

                    // Send error to the batch confirmation channel
                    let _ = send_confirmation_tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
                }
            }
        }

        // If there are any async tasks running as a result of previously-received messages, give
        // those a chance to complete before we forcibly abort them.  The duration is configurable.
        {
            let deadline: Option<tokio::time::Instant> = self
                .config
                .graceful_connection_shutdown_timeout
                .and_then(|timeout| tokio::time::Instant::now().checked_add(timeout));
            tracing::debug!(
                num_pending_inbound_operations = self.pending_inbound_operations.len(),
                graceful_connection_shutdown_timeout = ?self.config.graceful_connection_shutdown_timeout,
                "Waiting for pending inbound operations to complete"
            );

            let result = loop {
                // This is a bit more complex that it otherwise would be, because the mere
                // existence of any timeout at all is conditional, so we have to support the case
                // where there is no timeout.  That's why the non-timeout branch must produce the
                // same `Result` that the timeout one does, albeit one that is always Ok.
                tracing::trace!(
                    graceful_connection_shutdown_timeout = ?self.config.graceful_connection_shutdown_timeout,
                    "Waiting for the next pending inbound operations to complete"
                );
                let result = match deadline {
                    Some(deadline) => {
                        tokio::time::timeout_at(deadline, self.pending_inbound_operations.join_next_with_id())
                            .await
                    }
                    None => Ok(self.pending_inbound_operations.join_next_with_id().await),
                };
                match result {
                    Ok(Some(result)) => {
                        self.handle_pending_operation_completion(result).await;
                    }
                    Ok(None) => {
                        // All pending operations have completed
                        tracing::trace!("All pending inbound operations have completed");
                        break Ok(());
                    }
                    Err(e) => {
                        // Timed out; there are one or more pending operations still running in the
                        // joinset and we are out of time to wait for them to finish.
                        break Err(e);
                    }
                }
            };

            if result.is_err() {
                tracing::warn!(
                    graceful_connection_shutdown_timeout = ?self.config.graceful_connection_shutdown_timeout,
                    num_pending_inbound_operations = self.pending_inbound_operations.len(),
                    "Timed out waiting for pending inbound operations to complete; \
                        their async tasks will now be aborted");
                self.pending_inbound_operations.abort_all();
                while let Some(result) = self.pending_inbound_operations.join_next_with_id().await {
                    self.handle_pending_operation_completion(result).await;
                }
            }
        }

        // Any outbound messages that were actually sent to the remote peer and are awaiting
        // response, will obviously not get one now.  Let them all know the connection is closed.
        {
            tracing::debug!(
                num_pending_outbound_requests = self.pending_outbound_requests.len(),
                "Responding with an error to all pending outbound requests"
            );
            for (id, tx) in self.pending_outbound_requests.drain() {
                // Send will fail if the receiver is already dropped, which could very well be the case
                // if the connection itself is dropped.  But if it's still around, let it down easy.
                tracing::debug!(request_id = %id,
                    "Responding to pending outbound request with error due to shutdown");
                let _ = tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
            }
        }

        #[cfg(debug_assertions)]
        {
            // The pending operation lists should be empty
            assert!(self.pending_inbound_operations_by_task_id.is_empty());
            assert!(self.pending_inbound_operations_by_request_id.is_empty());
        }

        // That's it; the end of an era.  The event loop has reached its sudden but inevitable
        // end.  What will be inscribed upon the book of life for this connection?
        let (result, termination_reason) = match result {
            Err(JsonRpcError::Cancelled) => (Ok(()), "cancelled_token_triggered"),
            Err(e) => (Err(e), "error"),
            Ok(()) => (Ok(()), "remote_peer_closed_connection"),
        };

        tracing::debug!(termination_reason, "Event loop is exiting");
        result
    }

    /// Send a message to the remote peer, handling serialization and error logging.
    ///
    /// This method centralizes JSON serialization and transport error handling with
    /// structured logging that includes context about the message being sent.
    async fn send_message(&mut self, message: types::Message) -> Result<()> {
        // Extract contextual metadata for logging
        let (message_type, request_id, method) = message.diagnostic_context();

        // Serialize message to JSON string
        let message_str = match message.serialize_to_string() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    err = %e,
                    message_type,
                    method = method.unwrap_or_default(),
                    request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                    "Failed to serialize message to JSON"
                );
                return Err(e);
            }
        };

        // Send via transport layer
        match self.peer.send_message(message_str).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    message_type,
                    method = method.unwrap_or_default(),
                    request_id = %request_id.map(|id| id.to_string()).unwrap_or_default(),
                    "Failed to send message to peer; likely the connection was closed by the remote peer"
                );
                Err(e)
            }
        }
    }

    /// Handle a command sent from a service connection handle.
    ///
    /// This is infallible because there is not a mechanism to report errors back to senders of
    /// commands.  If we introduce a command in the future that needs bidirectional comms that will
    /// be implemented.
    async fn handle_command(&mut self, command: Command) {
        match command {
            Command::CancelPendingOutboundRequest { request_id: id } => {
                // Find and remove any pending outbound request with this ID
                if let Some(tx) = self.pending_outbound_requests.remove(&id) {
                    tracing::debug!(?id, "Cancelling pending outbound request");
                    // Send a cancellation error to the oneshot response channel
                    let _ = tx.send(Err(JsonRpcError::Cancelled));
                } else {
                    tracing::debug!(?id, "Tried to cancel non-existent outbound request");
                }
            }
            Command::CancelPendingInboundRequest { request_id: id } => {
                // Find the task ID for this request
                let request = self.pending_inbound_operations_by_request_id.get(&id).cloned();

                if let Some(request) = request {
                    // We found the task, abort it
                    tracing::debug!(request_id = ?id, "Cancelling pending inbound request");

                    if !request.cancellation_token.is_cancelled() {
                        // Signal the task to abort by signalling its cancellation token
                        // Unless the handler is written specifically to check the cancellation token,
                        // this won't actually stop the task.
                        request.cancellation_token.cancel();
                    } else {
                        // Task was already signaled for cancellation, can't double-tap these
                        tracing::warn!(request_id = ?id,
                            "Request was already cancelled; ignoring this duplicate request");
                    }
                } else {
                    tracing::debug!(request_id = ?id,
                        "Tried to cancel non-existent inbound request");
                }
            }
        }
    }

    /// Pass an outbound message to the transport
    ///
    /// This is infallible because any errors from the transport layer are reported back to the
    /// outbound message's oneshot channel.
    async fn handle_outbound_message(&mut self, outbound_message: OutboundMessage) {
        match outbound_message {
            OutboundMessage::Method { request, response_tx } => {
                // Attempt to send the method call request, and if sucessful then add it to the
                // pending requests list.
                // Note that this is not a race condition because this event loop is
                // the only place where the peer is stored, so we know that no other
                // threads or async tasks can receive a response for this request while
                // we're still trying to add it to the pending requests list
                let id = request.id.clone();
                let result = self.send_message(types::Message::Request(request)).await;
                if let Err(e) = result {
                    // Failed to send, which means this won't go in the pending
                    // requests list and we may as well inform the caller now about the
                    // failure
                    let _ = response_tx.send(Err(e));
                } else {
                    // Request was sent
                    self.pending_outbound_requests.insert(id, response_tx);
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
                    .send_message(types::Message::Notification(notification))
                    .await
                {
                    // Failed to send, which means this won't go in the pending
                    // requests list and we may as well inform the caller now about the
                    // failure
                    let _ = send_confirmation_tx.send(Err(e));
                } else {
                    // Request was sent, that's the best result we can hope for
                    let _ = send_confirmation_tx.send(Ok(()));
                }
            }
            OutboundMessage::Batch {
                requests,
                notifications,
                send_confirmation_tx,
            } => {
                // Create batch message
                let mut batch_messages = Vec::with_capacity(requests.len() + notifications.len());

                // Separate out the requests themselves from their response channels, but preserve
                // the association of the request ID with the response channel for matching up the
                // response later
                let (requests, response_txs): (Vec<_>, Vec<_>) = requests
                    .into_iter()
                    .map(|(request, response_tx)| {
                        let id = request.id.clone();
                        (request, (id, response_tx))
                    })
                    .unzip();

                // Add requests to batch
                batch_messages.extend(requests.into_iter().map(types::Message::Request));

                // Add notifications to batch
                batch_messages.extend(notifications.into_iter().map(types::Message::Notification));

                // Send batch
                let result = self.send_message(types::Message::Batch(batch_messages)).await;

                if let Err(e) = result {
                    // Failed to send batch

                    // Notify all request senders.
                    //
                    // Sadly errors are generally not clonable, so we can't report the actual error
                    // back to every request's future.  Only the actual `send` operation sending
                    // the batch itself will see the reason for the failure.
                    for (_, response_tx) in response_txs {
                        let _ = response_tx.send(Err(JsonRpcError::BatchSendFailed));
                    }

                    // Notify batch sender of the actual underlying error
                    let _ = send_confirmation_tx.send(Err(e));
                } else {
                    // Batch sent successfully

                    // Add requests to pending_outbound_requests
                    for (request_id, response_tx) in response_txs {
                        self.pending_outbound_requests.insert(request_id, response_tx);
                    }

                    // Confirm successful batch send
                    let _ = send_confirmation_tx.send(Ok(()));
                }
            }
        }
    }

    /// Process an inbound message that just came in from the transport layer
    ///
    /// If there are any responses to this inbound message, with the exception of things like parse
    /// errors that are sent immediately, then they will be generated as part of some async process
    /// and sent back to the remote peer as part of the event loop.
    async fn handle_inbound_message(
        &mut self,
        transport::TransportMessage { metadata, message }: transport::TransportMessage,
    ) -> Result<(), types::ErrorDetails> {
        // Parse the string message from the transport layer into a JSON-RPC
        // message payload
        let message = match message.parse::<types::Message>() {
            Ok(message) => {
                // Successfully parsed the message
                message
            }
            Err(e) => {
                // Failed to parse the message as valid JSON or valid JSON-RPC
                tracing::error!(err = %e, "Error parsing JSON message from peer");
                return Err(types::ErrorDetails::new(
                    types::ErrorCode::ParseError,
                    "Parse error",
                    None,
                ));
            }
        };

        let (message_type, request_id, method) = message.diagnostic_context();
        let span = tracing::debug_span!("inbound_message",
            message_type,
            method = %method.unwrap_or_default(),
            request_id = %request_id.map(|id| id.to_string()).unwrap_or_default());
        let _guard = span.enter();

        tracing::trace!("About to process an inbound message");

        match InboundMessage::try_from(message)? {
            InboundMessage::Inline(inline_inbound_message) => {
                self.handle_inbound_message_inline(metadata, inline_inbound_message);
            }
            InboundMessage::Async(async_inbound_message) => {
                self.handle_inbound_message_async(metadata, async_inbound_message)
                    .instrument(span.clone())
                    .await?;
            }
            InboundMessage::Hybrid(inline_inbound_message, async_inbound_message) => {
                // Some batch message with a mix.  Get the inline ones out of the way immediatley,
                // then spawn the async work separately
                self.handle_inbound_message_inline(metadata.clone(), inline_inbound_message);
                self.handle_inbound_message_async(metadata, async_inbound_message)
                    .instrument(span.clone())
                    .await?;
            }
        }

        Ok(())
    }

    /// Handle the kind of inbound message that should be processed inline in the event loop,
    /// without spawning an async task.
    ///
    /// None of this processing involves blocking, or invoking some handler that could block
    fn handle_inbound_message_inline(
        &mut self,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: InlineInboundMessage,
    ) {
        match inbound_message {
            InlineInboundMessage::Response(response) => {
                // The remote peer has sent us a response to a method call request we sent
                // previously.  Exciting.
                //
                // Assuming both sides are adhering to the spec, the ID of this response should be
                // in our list of pending requests, so we just look it up and forward the response
                // to the oneshot channel for that pending request.
                if let Some(tx) = self.pending_outbound_requests.remove(&response.id) {
                    // This is a response to a method call, so we need to send the result back to
                    // the caller
                    let _ = tx.send(Ok(response));
                } else {
                    // This is a response to a method call that we don't know about.  This could be
                    // because the request was never sent, or because it was sent but the
                    // connection closed before we got the response.  Either way, we can't do
                    // anything with this response.
                    tracing::warn!(request_id = %response.id,
                        "Received response for unknown request ID.  \
                            Perhaps this request was previously cancelled.");
                }
            }
            InlineInboundMessage::Batch(inline_inbound_messages) => {
                // just recursively process this.  Earlier in the decoding of this we ensured that
                // the batch is not empty so that error will have already been checked for, and
                // they  aren't nested so there's no risk of stack overflow here
                // Make a local future for each message to process
                for message in inline_inbound_messages {
                    self.handle_inbound_message_inline(metadata.clone(), message)
                }
            }
        }
    }

    /// Handle the kind of inbound messages that can take an unbounded amount of time and thus need
    /// to be run in a dedicated async task
    async fn handle_inbound_message_async(
        &mut self,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: AsyncInboundMessage,
    ) -> Result<(), types::ErrorDetails> {
        struct MethodCall {
            request_id: types::Id,
            cancellation_token: CancellationToken,
            method: String,
        }

        // If this is a method call request with a request ID we have some additional checks to do
        let method_call = if let AsyncInboundMessage::Request(request) = &inbound_message {
            // Hold onto this request, and we'll need a cancellation token for it
            Some(MethodCall {
                request_id: request.id.clone(),
                // Use a child token of our connection-level cancellation token so that connection
                // shutdown will automatically trigger cancellation of any running requests
                cancellation_token: self.cancellation_token.child_token(),
                method: request.method.clone(),
            })
        } else {
            None
        };

        // If this has a request ID, make sure that request ID isn't already in use by some pending
        // operation as that would lead to hard-to-diagnose bugs.
        //
        // The MCP spec (which this JSON-RPC impl exists to implement) states that the ID "MUST
        // NOT" have been used before in the same session, and the JSON RPC spec certainly implies
        // that the IDs must be unique.  So enforce that here
        if let Some(method_call) = &method_call {
            if let Some(duplicate_call) = self
                .pending_inbound_operations_by_request_id
                .get(&method_call.request_id)
            {
                tracing::error!(request_id = ?method_call.request_id,
                    method = %method_call.method,
                    existing_task_id = ?duplicate_call.task_id,
                    "Remote peer sent a duplicate request ID!"
                );

                // NOTE: The request ID is deliberately omitted here, because there is already a
                // request pending with this ID and it will, presumably, produce a result at some
                // point.  Instead we respond to this the same way that we would respond to
                // something that was invalid JSON or missing a critical field.
                return Err(types::ErrorDetails::new(
                    types::ErrorCode::InvalidRequest,
                    "Duplicate request ID",
                    Some(json!({
                        "duplicated_request_id": method_call.request_id,
                        "method": method_call.method.clone()
                    })),
                ));
            }
        }

        let task_id = self.spawn_operation({
            let router = self.router.clone();
            let handle_oneself = self.handle_oneself.clone();
            let cancellation_token = method_call
                .as_ref()
                .map(|method_call| method_call.cancellation_token.clone());

            async move {
                tracing::trace!("Inside inbound message handler task");
                Self::handle_inbound_message_async_task(
                    &router,
                    handle_oneself,
                    metadata,
                    inbound_message,
                    cancellation_token,
                )
                .await
            }
        });

        if let Some(method_call) = method_call {
            // Store a record of this method call both by tokio async task ID and by caller request ID
            tracing::trace!(request_id = ?method_call.request_id,
                ?task_id,
                method = %method_call.method,
                "Inbound method call spawned a new task"
            );

            let pending_record = Arc::new(PendingInboundMethodCall {
                task_id,
                request_id: method_call.request_id.clone(),
                cancellation_token: method_call.cancellation_token,
            });

            // We already know that this request ID is not in use, because we checked it before
            // spawning the task.
            self.pending_inbound_operations_by_request_id
                .insert(method_call.request_id.clone(), pending_record.clone());

            // It's exceedingly unlikely that tokio has a bug that duplicates task IDs, but we
            // might want to check...
            let old_request_id = self
                .pending_inbound_operations_by_task_id
                .insert(task_id, pending_record.clone());

            #[cfg(debug_assertions)]
            if let Some(old_request_id) = old_request_id {
                // This should never happen.  It suggests that two tasks got the same task ID.
                // That will lead to some very confusing behavior!
                tracing::error!(
                    ?task_id,
                    ?old_request_id,
                    "BUG: Task ID overwriting request ID with new request ID; this cannot ever happen!",
                );
            }
            #[cfg(not(debug_assertions))]
            let _ = old_request_id;
        }

        Ok(())
    }

    /// Invoke within a newly spawned async task to handle a request that has unbounded execution
    /// time.  This will block until the request has been handled completely
    async fn handle_inbound_message_async_task(
        router: &router::Router<S>,
        handle_oneself: ServiceConnectionHandle,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: AsyncInboundMessage,
        cancellation_token: Option<CancellationToken>,
    ) -> Option<types::Message> {
        match inbound_message {
            AsyncInboundMessage::Request(request) => {
                // This is a request, so we need to find the handler for it and invoke it
                // The router is literally built to do that very thing
                let mut invocation_request =
                    handler::InvocationRequest::from_request_message(handle_oneself, metadata, request);

                // If we have a cancellation token for this request, use it instead of the default
                if let Some(token) = cancellation_token {
                    invocation_request.cancellation_token = token;
                }

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
            AsyncInboundMessage::Notification(notification) => {
                // Process this notification in a simpler version of the request handler workflow
                // All comments there apply here as well, except as noted below
                let mut invocation_request = handler::InvocationRequest::from_notification_message(
                    handle_oneself,
                    metadata,
                    notification,
                );

                // If we have a cancellation token for this request, use it instead of the default
                if let Some(token) = cancellation_token {
                    invocation_request.cancellation_token = token;
                }

                // The actual invocation is infallible, because any errors will be reported as a
                // response type with error information, or just ignored in the case of
                // notifications
                let output = router.handle_invocation(invocation_request).await;

                // For notifications our assertion is exactly the opposite, since notifications
                // MUST NOT have a response
                debug_assert!(output.is_none(), "BUG: Invocation call produced a response");
                None
            }
            AsyncInboundMessage::Batch(messages) => {
                // Batch messages are handled in parallel, with their results combined at the end
                let batch_futures = messages.into_iter().map(|message| {
                    let metadata = metadata.clone();

                    Self::handle_inbound_message_async_task(
                        router,
                        handle_oneself.clone(),
                        metadata,
                        message,
                        None,
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
            AsyncInboundMessage::InvalidRequest(invalid_request) => {
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
                // Remove this from the map of pending tasks to method calls (if this was a method
                // call) because it's not pending anymore
                let method_call = self.pending_inbound_operations_by_task_id.remove(&task_id);

                if let Some(method_call) = method_call {
                    // Remove the corresponding entry for this call keyed by request ID
                    let request_id_found = self
                        .pending_inbound_operations_by_request_id
                        .remove(&method_call.request_id)
                        .is_some();

                    tracing::trace!(%task_id,
                        request_id = %method_call.request_id,
                        cancelled = method_call.cancellation_token.is_cancelled(),
                        "Pending method call completed");

                    #[cfg(debug_assertions)]
                    debug_assert!(
                        request_id_found,
                        "BUG: Found pending operation by task ID {task_id:?}, but not by request ID {:?}",
                        method_call.request_id
                    );
                    #[cfg(not(debug_assertions))]
                    let _ = request_id_found;
                } else {
                    tracing::trace!(%task_id,
                        "Pending operation completed");
                }

                if let Some(message) = response {
                    // Future ran to completion, and produced a response message.
                    // Note that this is technically also "outbound", but we use that
                    // term to mean messages that are being sent by some caller via the
                    // service handle to the remote peer, not messages that are sent in
                    // response to previously received requests.
                    let _ = self.send_message(message).await;
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
                let method_call = self.pending_inbound_operations_by_task_id.remove(&task_id);

                let request_id_string = method_call
                    .as_ref()
                    .map(|method_call| method_call.request_id.to_string())
                    .unwrap_or("None".to_string());

                if let Some(method_call) = method_call {
                    // If this was a method call, remove the entry by request ID also
                    self.pending_inbound_operations_by_request_id
                        .remove(&method_call.request_id);

                    // If this task ID is associated with a JSON RPC method call (meaning that is has a
                    // request ID) send a response back to the remote peer indicating that
                    // the request failed
                    let _ = self
                        .send_message(types::Message::Response(types::Response::error(
                            method_call.request_id.clone(),
                            types::ErrorCode::InternalError,
                            "Task was cancelled or panicked",
                            None,
                        )))
                        .await;
                }

                if join_err.is_panic() {
                    tracing::error!(%task_id,
                                    request_id = %request_id_string,
                                    join_err = %join_err,
                                    "Pending operation panicked");
                } else if join_err.is_cancelled() {
                    tracing::warn!(%task_id,
                        request_id = %request_id_string,
                        join_err = %join_err,
                        "Pending operation was cancelled (presumably due to connection shutdown)");
                } else {
                    #[cfg(debug_assertions)]
                    unreachable!("BUG: How can a join error be neither panic nor cancellation?");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ErrorDetails, handler,
        service::{Service, test_helpers::*},
        testing,
    };
    use serde_json::{Value as JsonValue, json};
    use std::sync::Mutex;
    use std::time::Duration;

    // For our unit tests, we'll create simple handlers that simulate the test service's
    // cancellable handlers instead of trying to import the integration test code

    /// Test what happens when we cancel a connection that has an in-progress request
    /// This simulates a handler that takes a long time to process
    #[tokio::test]
    async fn test_cancel_with_pending_request() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let (fut, server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create another service as a client so we can send requests
        let client_service = Service::new(router::Router::new_stateless());
        let (fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Spawn a task that will call the slow handler
        let slow_task = tokio::spawn(async move {
            let result = client_handle.call::<JsonValue>("slow").await;
            (result, client_handle)
        });

        // Give the slow handler a little time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now cancel the server connection while the slow handler is still running
        server_handle.shutdown().await.unwrap();

        // The call should fail with an error sent by the server as part of its graceful shutdown
        // sequence, indicating that the call failed because it was cancelled.
        let (result, _) = slow_task.await.unwrap();
        assert!(result.is_err());
        match result.unwrap_err() {
            JsonRpcError::MethodError {
                method_name,
                error: types::ErrorDetails { code, message, .. },
            } => {
                assert_eq!(method_name, "slow");
                assert_eq!(code, types::ErrorCode::InternalError);
                assert!(
                    message.contains("cancel"),
                    "Unexpected error message: {}",
                    message
                );
            }
            e => panic!("Unexpected error type: {:?}", e),
        }
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
        let (fut, handle) = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Spawn task to call a method - this will never be answered since there's no server
        let call_fut = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call::<JsonValue>("echo").await }
        });

        // Give the request some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now shut down the client's connection.
        handle.shutdown().await.unwrap();

        // The call should fail with a PendingRequestConnectionClosed error generated by the client
        // as part of its shutdown sequence, since the client event loop is shutting down and thus
        // will never receive a response from the server to this call
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
        let (fut, handle) = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Spawn task to call a method
        let call_fut = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call::<JsonValue>("echo").await }
        });

        // Give the request some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Close the server stream which will make the client's receive_message return None
        drop(server_transport);

        // The call should fail with a PendingRequestConnectionClosed error since the connection
        // itself is closed so no proper response will come.
        let result = call_fut.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));

        // The handle's event loop future should complete successfully
        assert!(handle.event_loop_fut().clone().await.is_ok());
    }

    /// Test that notifications are properly handled during shutdown
    #[tokio::test]
    async fn test_shutdown_with_pending_notifications() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, _) = testing::setup_test_channel();

        // Create handle with no server
        let (fut, handle) = service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Spawn a task to send a notification
        let notification_task = tokio::spawn({
            let handle_clone = handle.clone();
            async move { handle_clone.raise("hi").await }
        });

        // Give the notification some time to be sent
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now shut down the connection
        handle.shutdown().await.unwrap();

        // Sending the notification should fail with a PendingRequestConnectionClosed error,
        // because at the moment of sending, the connection's event loop is already shutdown, so
        // the notification cannot possibly be sent over the wire
        let result = notification_task.await.unwrap();
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            JsonRpcError::PendingRequestConnectionClosed
        ));
    }

    /// Test multiple concurrent pending requests and their cancellation behavior when the server
    /// is shutdown.
    #[tokio::test]
    async fn test_multiple_concurrent_pending_requests() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let (fut, server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create another service as a client so we can send requests
        let client_service = Service::new(router::Router::new_stateless());
        let (fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

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

        // All calls should fail with an error response received from the server indicating that
        // they were cancelled
        for task in tasks {
            let result = task.await.unwrap();
            assert!(result.is_err());
            match result.unwrap_err() {
                JsonRpcError::MethodError {
                    method_name,
                    error: types::ErrorDetails { code, message, .. },
                } => {
                    assert_eq!(method_name, "slow");
                    assert_eq!(code, types::ErrorCode::InternalError);
                    assert!(
                        message.contains("cancel"),
                        "Unexpected error message: {}",
                        message
                    );
                }
                e => panic!("Unexpected error type: {:?}", e),
            }
        }
    }

    /// Test server-side cancellation using the cancellable_sleep test service method
    #[tokio::test]
    async fn test_server_side_cancellation() {
        testing::init_test_logging();

        // Set up server with our test service
        let (client_transport, server_transport) = testing::setup_test_channel();
        let server = make_test_service();
        let (server_fut, server_handle) = server
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(server_fut);

        // Create client connection
        let client_service = Service::new(router::Router::new_stateless());
        let (client_fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(client_fut);

        // Start a cancellable_sleep that should run for 10 seconds
        let request_handle = client_handle
            .start_call_with_params::<_, ()>("cancellable_sleep", serde_json::json!({ "seconds": 10 }))
            .await
            .unwrap();

        // Wait for the sleep to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Cancel the request server-side
        server_handle
            .cancel_server_request(request_handle.request_id())
            .await;

        // Wait for the result - should be an error with the message from cancellable_sleep
        let result = request_handle.await;
        assert!(result.is_err());

        match result.unwrap_err() {
            JsonRpcError::MethodError {
                method_name,
                error: types::ErrorDetails { code, message, data },
            } => {
                assert_eq!(method_name, "cancellable_sleep");
                assert_eq!(
                    code,
                    types::ErrorCode::ServerError(ErrorDetails::SERVER_ERROR_CODE_MIN + 1)
                );
                assert!(message.contains("cancelled by client request"));
                // Verify the data contains information about the cancellation
                assert!(data.is_some());
                if let Some(data) = data {
                    let data_obj = data.as_object().unwrap();
                    assert!(data_obj.contains_key("cancelled_after_seconds"));
                }
            }
            e => panic!("Unexpected error type: {:?}", e),
        }
    }

    /// Test that the regular 'sleep' method doesn't respond to cancellation
    #[tokio::test]
    async fn test_sleep_ignores_cancellation() {
        testing::init_test_logging();

        // Set up server with our test service
        let (client_transport, server_transport) = testing::setup_test_channel();
        let server = make_test_service();
        let (server_fut, server_handle) = server
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(server_fut);

        // Create client connection
        let client_service = Service::new(router::Router::new_stateless());
        let (client_fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(client_fut);

        // Start a cancellable_sleep that should run for 10 seconds
        let request_handle = client_handle
            .start_call_with_params::<_, ()>("sleep", serde_json::json!({ "seconds": 10 }))
            .await
            .unwrap();

        // Wait for the sleep to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Cancel the request server-side
        server_handle
            .cancel_server_request(request_handle.request_id())
            .await;

        // Wait for the result - should complete successfully despite cancellation
        let result = request_handle.await;
        assert!(result.is_ok());
    }

    /// Test client-side cancellation using the sleep test service method
    #[tokio::test]
    async fn test_client_side_cancellation() {
        testing::init_test_logging();

        // Set up server with our test service
        let (client_transport, server_transport) = testing::setup_test_channel();
        let server = make_test_service();
        let (server_fut, _server_handle) = server
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(server_fut);

        // Create client connection
        let client_service = Service::new(router::Router::new_stateless());
        let (client_fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(client_fut);

        // Start a sleep that should run for 1 second
        let request_handle = client_handle
            .start_call_with_params::<_, ()>("sleep", serde_json::json!({ "seconds": 1 }))
            .await
            .unwrap();

        // Pause for a moment to make sure the request has gone out
        tokio::time::sleep(Duration::from_millis(200)).await;

        request_handle.cancel().await;

        // Wait for a bit longer to ensure the server finishes processing
        tokio::time::sleep(Duration::from_secs(2)).await;

        // There's not a good way to assert that the cancellation happened.  Mainly we're testing
        // that it doesn't break the client event loop in some way.

        // Try to make another call using the same client to ensure it's still functional
        let result = client_handle.call::<JsonValue>("echo").await;
        assert!(result.is_ok());
    }

    /// Verify that when a service shuts down, the request cancellation tokens are all signaled so
    /// that cancellable handlers have a chance to handle cancellation before being aborted.
    #[tokio::test]
    async fn test_connection_cancellation_token_propagation() {
        testing::init_test_logging();

        // Set up server with our test service
        let (client_transport, server_transport) = testing::setup_test_channel();
        let server = make_test_service();
        let (server_fut, server_handle) = server
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(server_fut);

        // Create client connection
        let client_service = Service::new(router::Router::new_stateless());
        let (client_fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(client_fut);

        // Start a cancellable_sleep that should run for 10 seconds
        let request_handle = client_handle
            .start_call_with_params::<_, ()>("cancellable_sleep", serde_json::json!({ "seconds": 10 }))
            .await
            .unwrap();
        // Wait for the sleep operation to start
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now shut down the server-side connection
        // This SHOULD cascade cancellation to all in-progress operations
        server_handle.shutdown().await.unwrap();

        // Wait for the result from the cancellable_sleep
        let result = request_handle.await;
        assert!(result.is_err());

        match result.unwrap_err() {
            JsonRpcError::MethodError {
                method_name,
                error: types::ErrorDetails { code, message, data },
            } => {
                assert_eq!(method_name, "cancellable_sleep");

                assert_eq!(
                    code,
                    types::ErrorCode::ServerError(ErrorDetails::SERVER_ERROR_CODE_MIN + 1),
                    "Bug: Expected handler's custom error code, got: {:?}",
                    code
                );

                assert!(
                    message.contains("cancelled by client request"),
                    "Bug: Expected handler's custom error message, got: {}",
                    message
                );

                assert!(
                    data.is_some()
                        && data
                            .as_ref()
                            .unwrap()
                            .as_object()
                            .unwrap()
                            .contains_key("cancelled_after_seconds"),
                    "Bug: Expected handler-specific cancellation data with 'cancelled_after_seconds'"
                );
            }
            e => panic!("Unexpected error type: {:?}", e),
        }
    }

    /// Test what happens when the connection is closed while a cancellable task is running
    ///
    /// This test checks if the cancellation token is properly triggered when a connection
    /// is unexpectedly closed by the remote peer, rather than by an explicit shutdown.
    #[tokio::test]
    async fn test_connection_close_cancellation_detection() {
        testing::init_test_logging();

        // Define an enum to represent the possible outcomes of our cancellable task
        #[derive(Debug, Clone, PartialEq)]
        enum CancellableTaskResult {
            CompletedNormally,
            DetectedCancellation,
        }

        // Create a oneshot channel to receive the result or detect abortion
        let (result_sender, result_receiver) = oneshot::channel::<CancellableTaskResult>();

        // Because handlers have to be clone and send and sync, wrap the result sender in an arc mutex even
        // though that's utterly silly for a oneshot sender
        let result_sender = Arc::new(Mutex::new(Some(result_sender)));

        // Define a custom router with instrumented cancellable handler
        let mut router = router::Router::new_stateless();

        // Add our custom instrumented cancellable handler
        router.register_handler("instrumented_cancellable", {
            move |handler::Params(params): handler::Params<JsonValue>,
                  handler::RequestCancellationToken(token): handler::RequestCancellationToken| {
                let result_sender = result_sender.clone();
                async move {
                    let seconds = params.get("seconds").and_then(|s| s.as_u64()).unwrap_or(10);
                    let sleep_future = tokio::time::sleep(Duration::from_secs(seconds));

                    // We control the test scenario, we know this handler only runs once, so we can safetly
                    // assume the oneshot sender is there.
                    let result_sender = result_sender.lock().unwrap().take().unwrap();

                    // The key part: use tokio::select to detect cancellation via the token
                    tokio::select! {
                        _ = sleep_future => {
                            // If we reach here, the sleep completed normally
                            let _ = result_sender.send(CancellableTaskResult::CompletedNormally);
                            Ok(())
                        }
                        _ = token.cancelled() => {
                            // If we reach here, the cancellation token was triggered

                            let _ = result_sender.send(CancellableTaskResult::DetectedCancellation);

                            // Return a custom error like the original cancellable_sleep
                            Err(types::ErrorDetails::server_error(
                                ErrorDetails::SERVER_ERROR_CODE_MIN + 1,
                                "Instrumented task was cancelled by client request",
                                Some(json!({ "cancelled_after_seconds": seconds })),
                            ))
                        }
                    }
                }
            }
        });

        // Create service and setup the connections
        let service = Service::new(router);
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Set up the server connection
        let (server_fut, _server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(server_fut);

        // Set up the client connection
        let client_service = Service::new(router::Router::new_stateless());
        let (client_fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(client_fut);

        // Start the instrumented cancellable task on the server that will run for a long time
        let _request_handle = client_handle
            .start_call_with_params::<_, ()>(
                "instrumented_cancellable",
                serde_json::json!({ "seconds": 100 }),
            )
            .await
            .unwrap();

        // Wait briefly for the handler to start running
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Now deliberately close the client by dropping the handle and its future
        // This should close the connection to the server, which should make the server shutdown.
        client_handle.shutdown().await.unwrap();

        // Check the result:
        // - If the task was aborted, the sender will be dropped and we'll get a Closed error
        // - If cancellation was detected, we'll get DetectedCancellation
        // - If task completed normally (unlikely), we'll get CompletedNormally
        match result_receiver.await {
            Ok(CancellableTaskResult::DetectedCancellation) => {
                // Success case: the handler detected cancellation via the token
                println!("Success: Handler properly detected cancellation via token");
            }
            Ok(CancellableTaskResult::CompletedNormally) => {
                // Unexpected case: the task somehow completed the full sleep
                panic!("Task completed normally when it should have been cancelled");
            }
            Err(_) => {
                // The sender was dropped without sending, meaning the task was aborted
                // without being able to detect cancellation - this indicates our bug
                panic!("Task was aborted without detecting cancellation");
            }
        }
    }
}
