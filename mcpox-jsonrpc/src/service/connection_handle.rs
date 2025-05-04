//! Handle to a service connection event loop and related types.
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, atomic::AtomicBool};
use std::task::{Context, Poll};

use pin_project::pin_project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::instrument;
use uuid::Uuid;

use super::connection::{Command, OutboundMessage, ServiceConnectionEventLoopFuture};
use crate::{JsonRpcError, Result, types};

/// Lightweight and cheaply-clonable handle to a service connection and its associated background
/// task.
///
/// This handle is used to send notifications and method call requests to the remote peer at the
/// other end of the service connection.  It is also able to signal the connection to shutdown if
/// needed.
///
/// This handle is obtained by calling [`crate::ClientBuilder::bind`] or
/// [`crate::Server::serve_connection`].
#[derive(Clone)]
pub struct ServiceConnectionHandle {
    commands: mpsc::Sender<Command>,
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
        use super::connection::CONNECTION_CHANNEL_BOUNDS;
        use futures::FutureExt;
        let (command_tx, _command_rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);
        let (outbound_tx, _outbound_rx) = mpsc::channel(CONNECTION_CHANNEL_BOUNDS);
        let cancellation_token = CancellationToken::new();
        let event_loop_fut = futures::future::ready(Result::Err(
            "this is a fake handle it's not connected!".to_string(),
        ))
        .boxed()
        .shared();

        Self {
            commands: command_tx,
            outbound_messages: outbound_tx,
            cancellation_token: cancellation_token.clone(),
            event_loop_fut,
        }
    }

    pub(super) fn new(
        commands: mpsc::Sender<Command>,
        outbound_messages: mpsc::Sender<OutboundMessage>,
        cancellation_token: CancellationToken,
        event_loop_fut: ServiceConnectionEventLoopFuture,
    ) -> Self {
        Self {
            commands,
            outbound_messages,
            cancellation_token,
            event_loop_fut,
        }
    }

    /// Some of the unit tests in [`super::connection`] need access to the event loop future
    #[cfg(test)]
    pub(super) fn event_loop_fut(&self) -> &ServiceConnectionEventLoopFuture {
        &self.event_loop_fut
    }

    /// Signal the connection cancellation token to shutdown the event loop, and wait until the
    /// event loop background task has finished running
    ///
    /// Note that this will immediately shutdown the connection's event loop which will impact all
    /// other connection handles as well.
    ///
    /// This doesn't complete until the event loop has exited.
    ///
    /// Any pending outgoing requests will produce immediate results, an error code indicating that
    /// they were cancelled.
    /// Any pending incoming requests will be signaled for cancellation.  Unless the handlers are
    /// written to check for the cancellation signal and abort early, they will continue running.
    /// The configurable graceful shutdown timeout will be applied, after which all remaining
    /// handler async tasks will be forcibly aborted.
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
    ///
    /// If you need to be able to cancel the call or have access to the request ID for the pending
    /// call, use [`Self::start_call`] instead.
    pub async fn call<Resp>(&self, method: impl Into<types::Method>) -> Result<Resp>
    where
        Resp: DeserializeOwned,
    {
        let handle = self.start_call(method).await?;
        handle.await
    }

    /// Send a request to invoke a method with parameters, awaiting a response.
    ///
    /// If you need to be able to cancel the call or have access to the request ID for the pending
    /// call, use [`Self::start_call_with_params`] instead.
    pub async fn call_with_params<Req, Resp>(
        &self,
        method: impl Into<types::Method>,
        params: Req,
    ) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let handle = self.start_call_with_params(method, params).await?;

        handle.await
    }

    /// Start a method call without parameters and immediately return a request handle without
    /// waiting for a response from the remote peer.
    ///
    /// This creates and sends a method call request, but instead of waiting for the response,
    /// it returns a [`RequestHandle`] that can be used to await the response or cancel the request.
    ///
    /// If you don't need a request handle, use [`Self::call`] instead.
    pub async fn start_call<Resp>(&self, method: impl Into<types::Method>) -> Result<RequestHandle<Resp>>
    where
        Resp: DeserializeOwned,
    {
        let raw_handle = self.start_call_raw(method, None).await?;

        Ok(RequestHandle {
            raw_handle,
            _type: PhantomData,
        })
    }

    /// Start a method call with parameters and immediately return a request handle without
    /// waiting for a response from the remote peer.
    ///
    /// This creates and sends a method call request, but instead of waiting for the response,
    /// it returns a [`RequestHandle`] that can be used to await the response or cancel the request.
    ///
    /// If you don't need a request handle, use [`Self::call_with_params`] instead.
    pub async fn start_call_with_params<Req, Resp>(
        &self,
        method: impl Into<types::Method>,
        params: Req,
    ) -> Result<RequestHandle<Resp>>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        let params = serde_json::to_value(params).map_err(|e| JsonRpcError::SerRequest {
            source: e,
            type_name: std::any::type_name::<Req>(),
        })?;

        let raw_handle = self.start_call_raw(method, Some(params)).await?;

        Ok(RequestHandle {
            raw_handle,
            _type: PhantomData,
        })
    }

    /// Initiate an outbound method call request with optional raw JSON params, and expect an
    /// arbitrary JSON response without any deserializtion
    ///
    /// This is the basis for all method call functions, and is not generally used directly.  See
    /// [`Self::start_call`], [`Self::start_call_with_params`], [`Self::call`], or
    /// [`Self::call_with_params`] for higher-level functions that are more likely what you
    /// require.
    #[instrument(skip_all, fields(method))]
    pub async fn start_call_raw(
        &self,
        method: impl Into<types::Method>,
        params: Option<JsonValue>,
    ) -> Result<RawRequestHandle> {
        let method = method.into();
        let (tx, rx) = oneshot::channel();
        let request_id = types::Id::Str(Uuid::now_v7().to_string());
        let request = types::Request::new(request_id.clone(), method.clone(), params);

        // Submit this request to the connection's event loop for processing
        if self
            .outbound_messages
            .send(OutboundMessage::Method {
                request: request.clone(),
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

        Ok(RawRequestHandle {
            receiver: rx,
            request_id,
            method: method.clone(),
            handle: self.clone(),
        })
    }

    /// Send a notification to the remote peer, without any parameters, neither expecting nor
    /// waiting for a response.
    ///
    /// A successful completion of this call merely means that the notification message was formed
    /// and written over the wire successfully.  There is no way to know how the remote peer
    /// processed the notification, if at all.
    pub async fn raise(&self, method: impl Into<types::Method>) -> Result<()> {
        self.raise_raw(method, None).await
    }

    /// Send a notification to the remote peer, including some notification arguments,
    /// neither expecting nor waiting for a response.
    ///
    /// A successful completion of this call merely means that the notification message was formed
    /// and written over the wire successfully.  There is no way to know how the remote peer
    /// processed the notification, if at all.
    pub async fn raise_with_params<Req>(&self, method: impl Into<types::Method>, params: Req) -> Result<()>
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
    pub async fn raise_raw(
        &self,
        method: impl Into<types::Method>,
        params: impl Into<Option<JsonValue>>,
    ) -> Result<()> {
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
        // this case, we just want to give the caller some visibility into whether the
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
                // pending requests when the loop exits
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

    /// Cancel a pending request to the remote peer, identified by the request ID.
    ///
    /// Stop waiting for this outbound request sent to the remote peer and currently pending a
    /// response.  Remove it from the pending outbound requests list, and send a cancellation error
    /// on its oneshot response channel if it has one.
    ///
    /// Meant to be called from the [`RequestHandle`]
    async fn cancel_client_request(&self, request_id: types::Id) {
        // Send a command to the event loop to cancel the request
        let _ = self
            .commands
            .send(Command::CancelPendingOutboundRequest { request_id })
            .await;
    }

    /// Cancel a currently running async task that is in response to a request received from the
    /// remote peer.
    ///
    /// Cancellation here is not guaranteed as not all handlers will monitor the request
    /// cancellation token.  Even after this is called, it's entirely possible that the task keeps
    /// running and eventually produces a success response.
    pub async fn cancel_server_request(&self, request_id: types::Id) {
        // Send a command to the event loop to cancel the request
        let _ = self
            .commands
            .send(Command::CancelPendingInboundRequest { request_id })
            .await;
    }
}

/// Handle to an outbound JSON-RPC method call request that is "raw", meaning there is no
/// particualr expected return type, just arbitrary JSON.
///
/// This low-level handle can be obtained using [`ServiceConnectionHandle::start_call_raw`].
///
/// In most cases this should not be used directly; see [`RequestHandle`] instead.
#[pin_project]
pub struct RawRequestHandle {
    #[pin]
    receiver: oneshot::Receiver<Result<types::Response>>,
    request_id: types::Id,
    method: types::Method,
    handle: ServiceConnectionHandle,
}

impl RawRequestHandle {
    /// Cancel the future that's waiting for a response to this request.
    ///
    /// See [`RequestHandle::cancel`] which behaves identically to this method
    pub async fn cancel(self) {
        // Signal to the event loop to cancel this request
        self.handle.cancel_client_request(self.request_id.clone()).await;

        // At some point that cancellation will happen and an error response will be placed in the
        // receiver.  Wait for that as the signal that the cancellation has happened.
        let _ = self.receiver.await;
    }

    /// The request ID assigned to this outstanding request.
    ///
    /// See [`RequestHandle::request_id`] for more.
    pub fn request_id(&self) -> types::Id {
        self.request_id.clone()
    }
}

impl Future for RawRequestHandle {
    type Output = Result<JsonValue>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match futures::ready!(this.receiver.poll(cx)) {
            Ok(Ok(response)) => {
                // Decode this Response struct into either the response type Resp or an error
                match response.payload {
                    types::ResponsePayload::Success(success_response) => {
                        // This is a successful response, in parsed JSON
                        Poll::Ready(Ok(success_response.result))
                    }
                    types::ResponsePayload::Error(error_response) => {
                        // This is an error response, so return the error
                        Poll::Ready(Err(JsonRpcError::MethodError {
                            method: this.method.clone(),
                            error: error_response.error,
                        }))
                    }
                }
            }
            Ok(Err(e)) => {
                // The event loop sent an error, so pass that error back to the caller
                Poll::Ready(Err(e))
            }
            Err(_) => {
                // The sender side of the one-shot channel was dropped.  That actually shouldn't
                // happen absent a panic in the event loop, since it contains logic to drain
                // pending requests when the loop exists
                tracing::error!(
                    request_id = %this.request_id,
                    "BUG: One-shot channel was dropped before the event loop could send a response"
                );
                Poll::Ready(Err(JsonRpcError::PendingRequestConnectionClosed))
            }
        }
    }
}

/// Handle to an outbound JSON-RPC method call request that is expected to produce some specific
/// response type `T`.
///
/// To obtain this handle when calling a remote method, use [`ServiceConnectionHandle::start_call`]
/// or [`ServiceConnectionHandle::start_call_with_params`].
///
/// This allows you to wait for the response or cancel the request, and exposes the JSON RPC
/// request ID assigned to the request.
/// It implements [`Future`] so you can simply await it to get the response, although if that's
/// all you want to do then you should use the [`ServiceConnectionHandle::call`] or
/// [`ServiceConnectionHandle::call_with_params`] instead.
#[pin_project]
pub struct RequestHandle<T> {
    #[pin]
    raw_handle: RawRequestHandle,

    _type: PhantomData<T>,
}

impl<T> RequestHandle<T>
where
    T: DeserializeOwned,
{
    /// Cancel this request by no longer waiting for a response.
    ///
    /// Note that when this async function returns, it indicates that the pending request was
    /// marked for cancellation on the client side and that cancellation was performed
    /// successfully, so that the event loop is no longer waiting for a response to this request.
    /// If such a response does come, it will be disregarded as it will not match a known
    /// outstanding request ID.
    ///
    /// This does *not* send a message to the remote peer telling it to cancel the request, because
    /// the JSON-RPC spec does not provide a mechanism for cancelling requests.  MCP does, but that
    /// is outside of the scope of this crate.  At this level, cancellation here just indicates
    /// that we are no longer interested in the result; the server will continue to process the
    /// request and send a response unless somehow it is also instructed to cancel.
    ///
    /// TODO: Is there any value in having this function?  How will it differ from just dropping
    /// the future?  Either way the response still comes in from the remote peer, maybe it doesn't
    /// make sense to have this extra functionality?  On the other hand it is more explicit and
    /// lets us log the cancellation...
    pub async fn cancel(self) {
        self.raw_handle.cancel().await;
    }

    /// The request ID assigned to this outstanding request.
    ///
    /// This is not useful for anything except in cases where the caller needs to communicate with
    /// the server about a specific operation.  Such as, for example, at the MCP implementation
    /// level dealing with progress and cancellation.
    pub fn request_id(&self) -> types::Id {
        self.raw_handle.request_id()
    }
}

impl<T> Future for RequestHandle<T>
where
    T: DeserializeOwned,
{
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match futures::ready!(this.raw_handle.poll(cx)) {
            Ok(json_response) => {
                // Response was successful and got JSON back.  Deserialize it and return
                Poll::Ready(serde_json::from_value(json_response.clone()).map_err(|e| {
                    JsonRpcError::DeserResponse {
                        source: e,
                        type_name: std::any::type_name::<T>(),
                        response: json_response,
                    }
                }))
            }
            Err(e) => {
                // Some kind of error, maybe local or maybe sent back from the remote peer.
                Poll::Ready(Err(e))
            }
        }
    }
}

/// Future returned from batch method calls.
///
/// This future represents the result of a single method call in a batch request.
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
    method: types::Method,
    request_id: types::Id,
    _type: PhantomData<T>,
}

impl<T> BatchMethodFuture<T> {
    // TODO: Does a `cancel` method make sense here?  It exists on the request handle types,
    // although of dubious utility there.  It would not be posible here unless the future also
    // holds a clone of the connection handle, which seems a bit...excessive.

    /// The ID of this request.
    ///
    /// This is only useful in cases where you need to communicate with the server about a pending
    /// request, which in our case is at the MCP level implementation dealing with cancellation and
    /// progress.
    pub fn request_id(&self) -> types::Id {
        self.request_id.clone()
    }
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

        match futures::ready!(this.receiver.poll(cx)) {
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
                    method: this.method.clone(),
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
    pub fn call<Resp>(&mut self, method: impl Into<types::Method>) -> BatchMethodFuture<Resp>
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
    pub fn call_with_params<Req, Resp>(
        &mut self,
        method: impl Into<types::Method>,
        params: Req,
    ) -> BatchMethodFuture<Resp>
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
                    method: method.into(),
                    request_id: types::Id::Null,
                };
            }
        };

        self.call_raw(method, params_json)
    }

    /// Add a method call with raw JSON parameters to the batch.
    ///
    /// In most cases you should prefer [`BatchBuilder::call`] or [`BatchBuilder::call_with_params`]
    pub fn call_raw<Resp>(
        &mut self,
        method: impl Into<types::Method>,
        params: Option<JsonValue>,
    ) -> BatchMethodFuture<Resp>
    where
        Resp: DeserializeOwned,
    {
        let (tx, rx) = oneshot::channel();

        let method = method.into();

        let request_id = types::Id::Str(Uuid::now_v7().to_string());
        let request = types::Request::new(request_id.clone(), method.clone(), params);

        self.requests.push((request, tx));

        BatchMethodFuture {
            receiver: rx,
            _type: PhantomData,
            batch_sent: self.batch_sent.clone(),
            method: method.clone(),
            request_id,
        }
    }

    /// Add a notification without parameters to the batch.
    ///
    /// Unlike method calls, notifications do not return a future because there is no
    /// response expected from the server
    pub fn raise(&mut self, method: impl Into<types::Method>) {
        self.raise_raw(method, None);
    }

    /// Add a notification with parameters to the batch.
    ///
    /// Unlike method calls, notifications do not return a future because there is no
    /// response expected from the server
    pub fn raise_with_params<Req>(&mut self, method: impl Into<types::Method>, params: Req)
    where
        Req: Serialize,
    {
        let method = method.into();

        let params_json = match serde_json::to_value(params) {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::error!(
                    %method,
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
    /// This is a raw version that operates on the raw JSON types that are mapped directly into the
    /// JSON RPC messages.  Most callers should use [`Self::raise`] or [`Self::raise_with_params`]
    /// instead.
    fn raise_raw(&mut self, method: impl Into<types::Method>, params: Option<JsonValue>) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        router,
        service::{self, test_helpers::*},
        testing, transport,
    };
    use futures::FutureExt;
    use std::time::Duration;

    // For our unit tests, we'll create simple handlers that simulate the test service's
    // cancellable handlers instead of trying to import the integration test code

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
                method,
                error: types::ErrorDetails { code, message, .. },
            } => {
                assert_eq!(method, "slow".into());
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
