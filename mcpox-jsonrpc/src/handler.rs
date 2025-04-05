//! In JSON RPC servers and clients both, peers can expose methods that can be called in an RPC
//! style by the remote peer, or invoked in a fire-and-forget style as notifications.
//!
//! Either way, the code that runs in rseponse to methods/notification is called a "handler".
//! All such code implements [`MethodHandler`], although it's not intended that most users will
//! implement that directly.
//!
//! Instead, [`MethodHandlerFn`] provides an implementation that can be wrapped around a variety of
//! different types of functions, for convenients.
use crate::{JsonRpcError, Result};
use async_trait::async_trait;
use futures::FutureExt;
use pin_project::pin_project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Placeholder
pub struct MessageContext {}

// This adapter wraps a future (producing a Result<T>) and “discards” its Ok value,
// yielding a future that returns Result<()> instead.
#[pin_project]
struct Discard<Fut>(#[pin] Fut);

impl<Fut, T> Future for Discard<Fut>
where
    Fut: Future<Output = Result<T>> + Send,
{
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Poll the inner future and map its output.
        let this = self.project();
        match this.0.poll(cx) {
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A handler for JSON-RPC methods and notifications.
///
/// This trait is implemented by any code that can be invoked by a JSON-RPC server or client.
///
/// You may implement it directly, but it's more likely that you'll use [`MethodHandlerFn`] to wrap
/// it in a more convenient way
///
/// In the JSON RPC spec, the only difference between a method invocation request, and a
/// notification, is that a notification doesn't have an ID and therefore there is no way to send a
/// response back to the remote peer that sent the notification.  In the spec there's no
/// requirement that actual methods be invoked with an invocation request and notifications with a
/// notification request.  So in this library we do not make a distinction between the two.  WHen
/// constructing a client or a server, you register methods, each of which is handled by an
/// implementation of [`MethodHandler`].  Those methods might get called as notifications, or as
/// method requests.
///
/// Your implementation of [`MethodHandler`] may provide a different implementation for the two
/// ways of being called, but by default the helper implementations of this trait implement the
/// notification in terms of the method and just discard the result.  If for some reason this isn't
/// what you want you can override this behavior and return an error for the invocation type that
/// you consider invalid.
trait MethodHandler {
    type MethodFuture: Future<Output = Result<JsonValue>> + Send;
    type NotificationFuture: Future<Output = Result<()>> + Send;

    /// Handle the method when it's invoked as a request, returning a response or an error.
    fn handle_method(&self, context: MessageContext, params: JsonValue) -> Self::MethodFuture;

    /// Handle the method when it's invoked as a notification.
    fn handle_notification(&self, context: MessageContext, params: JsonValue) -> Self::NotificationFuture;
}

/// A dyn compatible version of [`MethodHandler`].  This is more convenient to use elsewhere in
/// the codebase because it erases the type parameters, at the expense of a bit of vtable overhead.
///
/// However this overhead is trivial, and it still internally wraps a [`MethodHandler`] implementation
/// which the compiler is able to monomorphize and (presumably) optimize aggresively.
pub trait DynMethodHandler {
    /// Handle the method when it's invoked as a request, returning a response or an error.
    fn handle_method(
        &self,
        context: MessageContext,
        params: JsonValue,
    ) -> Pin<Box<dyn Future<Output = Result<JsonValue>> + Send + 'static>>;

    /// Handle the method when it's invoked as a notification.
    fn handle_notification(
        &self,
        context: MessageContext,
        params: JsonValue,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
}

impl<T: MethodHandler + Send> DynMethodHandler for T
where
    <T as MethodHandler>::MethodFuture: Send + 'static,
    <T as MethodHandler>::NotificationFuture: Send + 'static,
{
    fn handle_method(
        &self,
        context: MessageContext,
        params: JsonValue,
    ) -> Pin<Box<dyn Future<Output = Result<JsonValue>> + Send + 'static>> {
        MethodHandler::handle_method(self, context, params).boxed()
    }

    fn handle_notification(
        &self,
        context: MessageContext,
        params: JsonValue,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>> {
        MethodHandler::handle_notification(self, context, params).boxed()
    }
}

/// Wraps a function pointer in a struct that implements MethodHandler by calling the function in
/// the [`MethodHandler::handle_method`] method.  Notifications are handled by the same function,
/// with the return value discarded.
///
/// Has constructor methods that wrap function pointers with various types of parameters and return
/// types, for maximum user convenience.
struct MethodHandlerFn<Func, Fut> {
    func: Func,
    _phantom: PhantomData<Fut>,
}

impl<Func, Fut> MethodHandlerFn<Func, Fut>
where
    Func: Fn(MessageContext, JsonValue) -> Fut + Send + 'static,
    Fut: Future<Output = Result<JsonValue>> + Send,
{
    fn new(func: Func) -> Self {
        Self {
            func,
            _phantom: PhantomData,
        }
    }
}

impl<Func, Fut> Clone for MethodHandlerFn<Func, Fut>
where
    Func: Clone,
{
    fn clone(&self) -> Self {
        Self {
            func: self.func.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<Func, Fut> MethodHandler for MethodHandlerFn<Func, Fut>
where
    Func: Fn(MessageContext, JsonValue) -> Fut + Send + 'static,
    Fut: Future<Output = Result<JsonValue>> + Send,
{
    type MethodFuture = Fut;

    type NotificationFuture = Discard<Fut>;

    fn handle_method(&self, context: MessageContext, params: JsonValue) -> Self::MethodFuture {
        (self.func)(context, params)
    }

    fn handle_notification(&self, context: MessageContext, params: JsonValue) -> Self::NotificationFuture {
        Discard(MethodHandler::handle_method(self, context, params))
    }
}

/// Helpers for constructing method handlers from various functions
pub mod method {
    use super::*;

    /// Construct a [`DynMethodHandler`] from a function pointer that has the exact same signature as
    /// the [`DynMethodHandler::handle_method`] method.
    ///
    /// This is only useful if you want complete control over the internals of the handler.  For
    /// most applications a higher-level construction is more convenient
    pub fn raw<RawFunc, RawFut>(f: RawFunc) -> impl DynMethodHandler
    where
        RawFunc: Fn(super::MessageContext, serde_json::Value) -> RawFut + Send + 'static,
        RawFut: Future<Output = Result<serde_json::Value>> + Send + 'static,
    {
        super::MethodHandlerFn::new(f)
    }

    /// Construct a [`DynMethodHandler`] from a function pointer that takes and returns types that
    /// must be serde'd to/from JSON values, and possibly with a custom error type.
    pub fn serde_args<Req, Resp, Func, Fut, Err>(f: Func) -> impl DynMethodHandler
    where
        Func: Fn(MessageContext, Req) -> Fut + Send + 'static,
        Req: DeserializeOwned,
        Resp: Serialize,
        Fut: Future<Output = Result<Resp, Err>> + Send + 'static,
        Err: Into<JsonRpcError>,
    {
        super::MethodHandlerFn::new(move |context, params: JsonValue| {
            // In order to appease the borrow checker, we need to deserialize the request and call
            // the function outside of the `async move` block.  Once inside the `async move` block
            // we will return an error if the deserialization failed, and if not we will await the
            // future returned by the function.
            let result = serde_json::from_value(params.clone())
                .map_err(|e| JsonRpcError::DeserRequest {
                    source: e,
                    request: params,
                })
                .map(|params| f(context, params));

            async move {
                let response = result?.await.map_err(|e| e.into())?;
                let response = serde_json::to_value(response).map_err(|e| JsonRpcError::SerResponse {
                    source: e,
                    type_name: std::any::type_name::<Resp>(),
                })?;

                Ok(response)
            }
        })
    }
}

/// Helpers for constructing notification handlers from various functions
pub mod notification {
    use super::*;
}

async fn bullshit(handler: impl DynMethodHandler + 'static) -> Box<dyn DynMethodHandler> {
    Box::new(handler)
}
// impl<RawFunc, RawFut> MethodHandlerFn<RawFunc, RawFut> {
//     /// Create a new method handler from a function pointer that has the exact same signature as
//     /// [`MethodHandler::handle_method`].
//     fn from_raw_fn(f: RawFunc) -> Self
//     where
//         RawFunc: Fn(&MessageContext, JsonValue) -> RawFut,
//         RawFut: Future<Output = Result<JsonValue>>,
//     {
//         Self {
//             func: f,
//             _phantom: PhantomData,
//         }
//     }
//
//     /// Create a new method handler from a function pointer that takes and returns types that
//     /// should be ser/de'd to/from JSON values, and possibly with a custom error type.
//     fn from_serde_args_fn<Req, Resp, Func, Fut, Err>(f: Func) -> Self
//     where
//         Func: Fn(&MessageContext, Req) -> RawFut + Send + 'static,
//         Req: DeserializeOwned,
//         Resp: Serialize,
//         Fut: Future<Output = Result<Resp, Err>>,
//         Err: Into<anyhow::Error>,
//     {
//         Self::from_raw_fn(move |context, params| async {
//             let params = serde_json::from_value(params)?;
//             let result = f(context, params).await?;
//             Ok(serde_json::to_value(result)?)
//         })
//     }
//
//     /// Create a new method handler from a function pointer to a member function that takes and returns types that
//     /// should be ser/de'd to/from JSON values, and possibly with a custom error type.
//     fn from_serde_args_method<FuncSelf, Req, Resp, Func, Fut, Err>(s: &FuncSelf, f: Func) -> Self
//     where
//         Func: Fn(&FuncSelf, &MessageContext, Req) -> RawFut + Send + 'static,
//         FuncSelf: Clone + Send + 'static,
//         Req: DeserializeOwned,
//         Resp: Serialize,
//         Fut: Future<Output = Result<Resp, Err>>,
//         Err: Into<anyhow::Error>,
//     {
//         let func_self = s.clone();
//         Self::from_raw_fn(move |context, params| async {
//             let params = serde_json::from_value(params)?;
//             let result = f(&s, context, params).await?;
//             Ok(serde_json::to_value(result)?)
//         })
//     }
// }
