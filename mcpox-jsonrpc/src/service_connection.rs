//! The service and related types that form the beating heart of both JSON RPC clients and servers.
//!
//! See the [`service::Service`] struct for more details and usage.
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use std::task::{Context, Poll};

use futures::{FutureExt, TryFutureExt, ready};
use itertools::{Either, Itertools};
use pin_project::pin_project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, instrument};
use uuid::Uuid;

use crate::{JsonRpcError, Result};
use crate::{error, handler, router, service, transport, types};

/// The size of the mpsc channel that is used to send messages to an existing connection.
/// After this many messages are queued and not yet transmitted, backpressure is excerted on the
/// senders
const CONNECTION_CHANNEL_BOUNDS: usize = 16;

/// The future used outside of this module to represent the running event loop servicing a
/// connection.
///
/// This is boxed and wrapped in [`futures::future::Shared`] to make is clonable for maximum
/// flexibility.
pub type ServiceConnectionEventLoopFuture =
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
    config: service::ServiceConfig,
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

    let handle = ServiceConnectionHandle {
        outbound_messages: outgoing_tx,
        cancellation_token: cancellation_token.clone(),
        event_loop_fut: event_loop_fut.clone(),
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
            message: "oneshot channel dropped immediately after spawning connection event loop".to_string(),
        })
    } else {
        Ok((event_loop_fut, handle))
    }
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

/// Type alias for pending requests map to simplify complex type
type PendingRequestsMap = Arc<Mutex<HashMap<types::Id, oneshot::Sender<Result<types::Response>>>>>;

/// Future returned from batch method calls.
///
/// This future represents the result of a method call in a batch request.
/// It will resolve to the response value when the batch is sent and the server responds.
///
/// # Error Handling
///
/// This future will yield a [`JsonRpcError::BatchNotSentYet`] error if polled (awaited)
/// before the batch it belongs to has been sent. You must call [`BatchBuilder::send`]
/// before awaiting any futures returned from the batch builder's methods.
///
/// Other possible errors include:
/// - [`JsonRpcError::DeserResponse`] if the response cannot be deserialized
/// - [`JsonRpcError::MethodError`] if the server returns an error response
/// - [`JsonRpcError::PendingRequestConnectionClosed`] if the connection closes before a response is
///   received
/// - [`JsonRpcError::BatchSendFailed`] if the batch failed to send
#[pin_project]
pub struct BatchMethodFuture<T> {
    #[pin]
    receiver: oneshot::Receiver<Result<types::Response>>,
    /// Whether the batch this future belongs to has been sent
    batch_sent: Arc<AtomicBool>,
    method_name: String,
    _type: PhantomData<T>,
}

impl<T> Future for BatchMethodFuture<T>
where
    T: DeserializeOwned,
{
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // If the batch hasn't been sent yet, yield an error immediately
        if !this.batch_sent.load(std::sync::atomic::Ordering::SeqCst) {
            return Poll::Ready(Err(JsonRpcError::BatchNotSentYet));
        }

        match ready!(this.receiver.poll(cx)) {
            Ok(Ok(response)) => match response.payload {
                types::ResponsePayload::Success(success) => {
                    match serde_json::from_value(success.result.clone()) {
                        Ok(typed_value) => Poll::Ready(Ok(typed_value)),
                        Err(e) => Poll::Ready(Err(JsonRpcError::DeserResponse {
                            source: e,
                            type_name: std::any::type_name::<T>(),
                            response: success.result,
                        })),
                    }
                }
                types::ResponsePayload::Error(error) => Poll::Ready(Err(JsonRpcError::MethodError {
                    method_name: this.method_name.clone(),
                    error: error.error,
                })),
            },
            Ok(Err(e)) => Poll::Ready(Err(e)),
            Err(_) => Poll::Ready(Err(JsonRpcError::PendingRequestConnectionClosed)),
        }
    }
}

/// Builder for creating JSON-RPC batch requests.
///
/// This builder allows you to construct a batch of JSON-RPC method calls and notifications,
/// which will be sent together in a single message when you call [`BatchBuilder::send`].
///
/// Method calls added to the batch using [`BatchBuilder::call`] and
/// [`BatchBuilder::call_with_params`] return futures that you can await to get the responses, but
/// you must call [`BatchBuilder::send`] on the batch before awaiting those futures.
pub struct BatchBuilder {
    requests: Vec<(types::Request, oneshot::Sender<Result<types::Response>>)>,
    notifications: Vec<types::Notification>,
    handle: ServiceConnectionHandle,
    batch_sent: Arc<AtomicBool>,
}

impl BatchBuilder {
    fn new(handle: ServiceConnectionHandle) -> Self {
        Self {
            requests: Vec::new(),
            notifications: Vec::new(),
            handle,
            batch_sent: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Add a method call without parameters to the batch.
    ///
    /// This returns a future that will resolve to the method response when the batch is sent
    /// and the server responds. You must call [`BatchBuilder::send`] on the batch before awaiting
    /// this future, or it will immediately yield an error.
    pub fn call<Resp>(&mut self, method: &str) -> BatchMethodFuture<Resp>
    where
        Resp: DeserializeOwned,
    {
        self.call_raw(method, None)
    }

    /// Add a method call with parameters to the batch.
    ///
    /// This returns a future that will resolve to the method response when the batch is sent
    /// and the server responds. You must call [`BatchBuilder::send`] on the batch before awaiting
    /// this future, or it will immediately yield an error.
    ///
    /// If serialization of the parameters fails, the returned future will immediately yield
    /// an error when awaited
    pub fn call_with_params<Req, Resp>(&mut self, method: &str, params: Req) -> BatchMethodFuture<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let params_json = match serde_json::to_value(params) {
            Ok(value) => Some(value),
            Err(e) => {
                // Create a channel with Response type
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(Err(JsonRpcError::SerRequest {
                    source: e,
                    type_name: std::any::type_name::<Req>(),
                }));
                return BatchMethodFuture {
                    receiver: rx,
                    _type: PhantomData,
                    batch_sent: Arc::new(AtomicBool::new(true)), // Allow immediate polling
                    method_name: method.to_string(),
                };
            }
        };

        self.call_raw(method, params_json)
    }

    /// Add a method call with raw JSON parameters to the batch.
    ///
    /// This is an internal implementation detail that backs the public [`BatchBuilder::call`] and
    /// [`BatchBuilder::call_with_params`] methods
    fn call_raw<Resp>(&mut self, method: &str, params: Option<JsonValue>) -> BatchMethodFuture<Resp>
    where
        Resp: DeserializeOwned,
    {
        let (tx, rx) = oneshot::channel();

        let request = types::Request::new(types::Id::Str(Uuid::now_v7().to_string()), method, params);

        self.requests.push((request, tx));

        BatchMethodFuture {
            receiver: rx,
            _type: PhantomData,
            batch_sent: self.batch_sent.clone(),
            method_name: method.to_string(),
        }
    }

    /// Add a notification without parameters to the batch.
    ///
    /// Unlike method calls, notifications do not return a future because there is no
    /// response expected from the server
    pub fn raise(&mut self, method: &str) {
        self.raise_raw(method, None);
    }

    /// Add a notification with parameters to the batch.
    ///
    /// Unlike method calls, notifications do not return a future because there is no
    /// response expected from the server
    pub fn raise_with_params<Req>(&mut self, method: &str, params: Req)
    where
        Req: Serialize,
    {
        let params_json = match serde_json::to_value(params) {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::error!(
                    method,
                    err = %e,
                    "Error serializing parameters for notification in batch",
                );
                None
            }
        };

        self.raise_raw(method, params_json);
    }

    /// Add a notification with raw JSON parameters to the batch.
    ///
    /// This is an internal implementation detail that backs the public [`BatchBuilder::raise`] and
    /// [`BatchBuilder::raise_with_params`] methods
    fn raise_raw(&mut self, method: &str, params: Option<JsonValue>) {
        self.notifications.push(types::Notification::new(method, params));
    }

    /// Send the batch of requests and notifications to the server.
    ///
    /// This method consumes the batch builder, so it can only be called once.
    /// After calling this method, you can await the futures returned by the
    /// [`BatchBuilder::call`] and [`BatchBuilder::call_with_params`] methods to get the responses.
    pub async fn send(self) -> Result<()> {
        // Mark the batch as sent, so the futures can be polled, even in the event that sending the
        // batch itself fails
        self.batch_sent.store(true, std::sync::atomic::Ordering::SeqCst);

        if self.requests.is_empty() && self.notifications.is_empty() {
            // Empty batch, nothing to do
            return Ok(());
        }

        // Send the batch
        let (send_confirmation_tx, send_confirmation_rx) = oneshot::channel();

        if let Err(e) = self
            .handle
            .outbound_messages
            .send(OutboundMessage::Batch {
                requests: self.requests,
                notifications: self.notifications,
                send_confirmation_tx,
            })
            .await
        {
            // Channel closed, service connection event loop is down, probably because of a
            // disconnect
            //
            // Before returning an error back to the caller, signal to the receivers for any method
            // requests that were part of this batch as well
            let batch = e.0;
            if let OutboundMessage::Batch { requests, .. } = batch {
                for (_, tx) in requests {
                    let _ = tx.send(Err(JsonRpcError::PendingRequestConnectionClosed));
                }
            }
            return Err(JsonRpcError::PendingRequestConnectionClosed);
        }

        // Wait for confirmation
        match send_confirmation_rx.await {
            Ok(result) => result,
            Err(_) => Err(JsonRpcError::PendingRequestConnectionClosed),
        }
    }
}

struct ServiceConnection<S: Clone + Send + Sync + 'static> {
    config: service::ServiceConfig,

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
        config: service::ServiceConfig,
        router: router::Router<S>,
        peer: transport::Peer,
        custom_event_loop: Arc<dyn EventLoop<S>>,
        cancellation_token: CancellationToken,
        handle_oneself: ServiceConnectionHandle,
        outbound_messages: mpsc::Receiver<OutboundMessage>,
    ) -> Self {
        Self {
            config,
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
                        tracing::error!("Event loop terminated due to an error: {}", e);
                    }
                }
            })
            .instrument(transport_span)
            .await
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
        // them to come conclusion.  *IF* the peer is already closed, then responses to requests
        // can't actually be sent to the remote peer, but we can still cancel requests initiated
        // from our connection handles, and shutdown the async tasks still in flight.

        // First, close the receiver for outbound messages, so no more can be sent by anyone
        // holding a handle to this service connection
        self.outbound_messages.close();

        // If there are any left in the outbound message queue, retrieve them and immediately
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
        let context = match &message {
            types::Message::Request(req) => format!("request id={}, method={}", req.id, req.method),
            types::Message::Response(resp) => {
                let status = match &resp.payload {
                    types::ResponsePayload::Success(_) => "success".to_string(),
                    types::ResponsePayload::Error(err) => format!("error code={}", err.error.code),
                };
                format!("response id={}, {}", resp.id, status)
            }
            types::Message::Notification(notif) => format!("notification method={}", notif.method),
            types::Message::Batch(messages) => format!("batch size={}", messages.len()),
            types::Message::InvalidRequest(invalid) => format!("invalid_request id={}", invalid.id),
        };

        // Serialize message to JSON string
        let message_str = match message.into_string() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    err = %e,
                    message_type = %context,
                    "Failed to serialize message to JSON"
                );
                return Err(e);
            }
        };

        // Send via transport layer
        match self.peer.send_message(message_str).await {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::error!(
                    err = %e,
                    message_type = %context,
                    "Failed to send message to peer"
                );
                Err(e)
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
                    let mut pending_requests = self.pending_outbound_requests.lock().unwrap();
                    for (request_id, response_tx) in response_txs {
                        pending_requests.insert(request_id, response_tx);
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

        let request_id = if let types::Message::Request(request) = &message {
            Some(request.id.clone())
        } else {
            None
        };
        tracing::trace!(?request_id, "About to process an inbound message");

        match InboundMessage::try_from(message)? {
            InboundMessage::Inline(inline_inbound_message) => {
                self.handle_inbound_message_inline(metadata, inline_inbound_message);
            }
            InboundMessage::Async(async_inbound_message) => {
                self.handle_inbound_message_async(metadata, async_inbound_message)
                    .await;
            }
            InboundMessage::Hybrid(inline_inbound_message, async_inbound_message) => {
                // Some batch message with a mix.  Get the inline ones out of the way immediatley,
                // then spawn the async work separately
                self.handle_inbound_message_inline(metadata.clone(), inline_inbound_message);
                self.handle_inbound_message_async(metadata, async_inbound_message)
                    .await;
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
                if let Some(tx) = self
                    .pending_outbound_requests
                    .lock()
                    .unwrap()
                    .remove(&response.id)
                {
                    // This is a response to a method call, so we need to send the result back to
                    // the caller
                    let _ = tx.send(Ok(response));
                } else {
                    // This is a response to a method call that we don't know about.  This could be
                    // because the request was never sent, or because it was sent but the
                    // connection closed before we got the response.  Either way, we can't do
                    // anything with this response.
                    tracing::warn!(request_id = %response.id,
                                "Received response for unknown request ID");
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
    ) {
        let request_id = if let AsyncInboundMessage::Request(request) = &inbound_message {
            Some(request.id.clone())
        } else {
            None
        };

        let task_id = self.spawn_operation({
            let router = self.router.clone();
            let pending_requests = self.pending_outbound_requests.clone();
            let handle_oneself = self.handle_oneself.clone();
            async move {
                tracing::trace!("Inside inbound message handler task");
                Self::handle_inbound_message_async_task(
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

    /// Invoke within a newly spawned async task to handle a request that has unbounded execution
    /// time.  This will block until the request has been handled completely
    async fn handle_inbound_message_async_task(
        router: &router::Router<S>,
        pending_requests: &PendingRequestsMap,
        handle_oneself: ServiceConnectionHandle,
        metadata: Arc<transport::TransportMetadata>,
        inbound_message: AsyncInboundMessage,
    ) -> Option<types::Message> {
        match inbound_message {
            AsyncInboundMessage::Request(request) => {
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
            AsyncInboundMessage::Notification(notification) => {
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
            AsyncInboundMessage::Batch(messages) => {
                // Batch messages are handled in parallel, with their results combined at the end
                let batch_futures = messages.into_iter().map(|message| {
                    let metadata = metadata.clone();

                    Self::handle_inbound_message_async_task(
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
                let request_id = self
                    .pending_inbound_operation_request_ids
                    .lock()
                    .unwrap()
                    .remove(&task_id);
                let request_id_string = request_id
                    .as_ref()
                    .map(|id| id.to_string())
                    .unwrap_or("None".to_string());

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

                // If this task ID is associated with a JSON RPC request ID,
                // that means it was supposed to be handling a method request,
                // so send a response back to the remote peer indicating that
                // the request failed
                if let Some(request_id) = request_id {
                    let _ = self
                        .send_message(types::Message::Response(types::Response::error(
                            request_id,
                            types::ErrorCode::InternalError,
                            "Task was cancelled or panicked",
                            None,
                        )))
                        .await;
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
    event_loop_fut: ServiceConnectionEventLoopFuture,
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

    /// Start a new batch of requests and notifications.
    ///
    /// This method returns a [`BatchBuilder`] that allows you to construct a batch of JSON-RPC
    /// method calls and notifications, which can be sent together in a single message.
    ///
    /// # Example
    ///
    /// ```
    /// # use mcpox_jsonrpc::{ServiceConnectionHandle, JsonRpcError};
    /// # use serde_json::Value as JsonValue;
    /// # async fn example(handle: &ServiceConnectionHandle) -> Result<(), JsonRpcError> {
    /// // Create a new batch
    /// let mut batch = handle.start_batch();
    ///
    /// // Add method calls to the batch
    /// let future1 = batch.call::<JsonValue>("method1");
    /// let future2 = batch.call_with_params::<_, JsonValue>("method2", 42);
    ///
    /// // Add notifications to the batch
    /// batch.raise("notification1");
    /// batch.raise_with_params("notification2", "data");
    ///
    /// // Send the batch - this must be done before awaiting any futures
    /// batch.send().await?;
    ///
    /// // Now you can await the futures to get the responses
    /// let result1 = future1.await?;
    /// let result2 = future2.await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Important Notes
    ///
    /// - You must call [`BatchBuilder::send`] on the batch before awaiting any of the futures
    ///   returned by [`BatchBuilder::call`] or [`BatchBuilder::call_with_params`]. Attempting to
    ///   await these futures before the batch is sent will result in an immediate
    ///   [`JsonRpcError::BatchNotSentYet`] error.
    ///
    /// - All method calls in the batch are sent in a single JSON-RPC message, which can
    ///   significantly reduce network overhead when making multiple requests.
    ///
    /// - Method calls in the batch return futures that you can await after sending the batch.
    ///   Notifications do not return futures since they don't expect responses.
    ///
    /// - If sending the batch fails, all method futures will fail with the same error when awaited.
    pub fn start_batch(&self) -> BatchBuilder {
        BatchBuilder::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{handler, service, testing};
    use std::time::Duration;

    // Construct a dummy service with a single method "echo" and a single notification "hi"
    fn make_test_service() -> service::Service<()> {
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

        service::Service::new(router)
    }

    /// Verify the behavior of a handle when the corresponding connection event loop has already
    /// stopped for whatever reason
    #[tokio::test]
    async fn handle_behavior_when_event_loop_stopped() {
        testing::init_test_logging();

        let service = make_test_service();
        let (_client_transport, server_transport) = testing::setup_test_channel();

        let (fut, service_connection_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

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
        assert_eq!(Some(Ok(())), handle_clone.event_loop_fut.clone().now_or_never());

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
        let (fut, server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create another service as a client so we can send requests
        let client_service = service::Service::new(router::Router::new_stateless());
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
            e => panic!("Unexpected error type: {:?}", anyhow::Error::from(e)),
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
        assert!(handle.event_loop_fut.clone().await.is_ok());
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
        let client_service = service::Service::new(router::Router::new_stateless());
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
                e => panic!("Unexpected error type: {:?}", anyhow::Error::from(e)),
            }
        }
    }

    /// Test the behavior of the batch API when the connection is closed or shutdown
    #[tokio::test]
    async fn test_batch_with_connection_shutdown() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let (fut, _server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create another service as a client so we can send requests
        let client_service = service::Service::new(router::Router::new_stateless());
        let (fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Start creating a batch but don't send it yet
        let mut batch = client_handle.start_batch();
        let echo_future = batch.call_with_params::<_, JsonValue>("echo", "test");

        // Shutdown the CLIENT connection before the batch is sent
        // This ensures the outbound channel is immediately closed
        client_handle.shutdown().await.unwrap();

        // Now try to send the batch - it should fail with PendingRequestConnectionClosed
        // because the client connection has been shut down
        let result = batch.send().await;
        let err = result.unwrap_err();
        assert!(matches!(err, JsonRpcError::PendingRequestConnectionClosed));

        // The future should already be marked as sent (to allow polling) but should fail
        // with PendingRequestConnectionClosed when awaited
        let result = echo_future.await;
        let err = result.unwrap_err();
        assert!(matches!(err, JsonRpcError::PendingRequestConnectionClosed));
    }

    /// Test the behavior of BatchMethodFuture when attempting to poll it before the batch is sent
    #[tokio::test]
    async fn test_batch_method_future_not_sent_error() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection handle for the server
        let (fut, _server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create another service as a client so we can send requests
        let client_service = service::Service::new(router::Router::new_stateless());
        let (fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create a batch and a future
        let mut batch = client_handle.start_batch();
        let future = batch.call::<JsonValue>("echo");

        // Create a task to poll the future
        let poll_task = tokio::spawn(async move {
            match future.await {
                Ok(_) => panic!("Future should not complete successfully"),
                Err(e) => e,
            }
        });

        // Wait a short time for the task to poll the future
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now send the batch (this won't affect the already spawned task with the moved future)
        batch.send().await.unwrap();

        // The polling task should have received BatchNotSentYet error
        let error = poll_task.await.unwrap();
        assert!(matches!(error, JsonRpcError::BatchNotSentYet));
    }

    /// Test that an empty batch can be sent successfully
    #[tokio::test]
    async fn test_empty_batch() {
        testing::init_test_logging();

        let service = make_test_service();
        let (client_transport, server_transport) = testing::setup_test_channel();

        // Create the service connection
        let (fut, _server_handle) = service
            .service_connection(transport::Peer::new(server_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create client connection
        let client_service = service::Service::new(router::Router::new_stateless());
        let (fut, client_handle) = client_service
            .service_connection(transport::Peer::new(client_transport))
            .unwrap();
        tokio::spawn(fut);

        // Create an empty batch
        let batch = client_handle.start_batch();

        // Sending an empty batch should succeed and not actually send anything
        let result = batch.send().await;
        assert!(result.is_ok());
    }
}
