//! In JSON RPC servers and clients both, peers can expose methods that can be called in an RPC
//! style by the remote peer, or invoked in a fire-and-forget style as notifications.
//!
//! Either way, the code that runs in rseponse to methods/notification is called a "handler".
//! All such code implements [`Handler`], although it's not intended that most users will
//! implement that directly.
//!
//! Instead, [`HandlerFn`] provides an implementation that can be wrapped around a variety of
//! different types of functions, for convenients.
use crate::types;
use crate::{JsonRpcError, Result};
use async_trait::async_trait;
use futures::FutureExt;
use pin_project::pin_project;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Placeholder.
///
/// In addition to the information about the method being invoked, optional ID, and payload, this
/// needs to carry the HTTP request if this came via HTTP, probably some transport-specific
/// details, headers, etc.
pub struct Request {
    pub id: Option<types::Id>,
    pub method: String,

    /// TODO: Why is this `Option`?  Does the JSON RPC spec allow the `params` field to be omitted?
    /// If so, should it be an error in the Json extractor if it's not present, or should it just
    /// make a default `JsonValue` and try to deserialize from that?
    pub params: Option<JsonValue>,
}

/// Inspirted by axum's `FromRequest` trait.
///
/// Axum uses two traits, `FromRequestParts` and `FromRequest`, the important difference being that
/// there can be only one `FromRequest`-impementing parameter in a handler and it must be the last
/// parameter, because `FromRequest` consumes they entire request (including the body).
///
/// TODO: Is this still true?  If we had two traits like axum, the `Json` extractor wouldn't have
/// to clone the params field.
///
/// JSON RPC is both more generic because we support it over transports that aren't simple HTTP
/// requests, and also simpler because the request body and response body are always JSON and not
/// expected to be large streaming payloads.  So the distinction is not needed.
pub trait FromRequest<S>: Sized {
    /// The error type that indicates the extraction of this type from the request failed for some
    /// reason.
    type Rejection: IntoResponse;

    /// Extract the implementor's type from the request.
    fn from_request(request: &Request, state: &S) -> Result<Self, Self::Rejection>;
}

// Extractors that pull information from the request and make it available to a handler

/// A Rust struct serialized to or from JSON.
///
/// This is a wrapper around a type that implements [`Serialize`].
///
/// Deserialize the parameters of the request into a type that implements [`Deserialize`].
///
/// It can be used as an extractor to deserialize request params from JSON, or in return position
/// to serialize a Rust type to JSON
pub struct Json<T>(pub T);

impl<T: DeserializeOwned, S> FromRequest<S> for Json<T> {
    type Rejection = JsonRpcError;

    fn from_request(request: &Request, _state: &S) -> Result<Self, Self::Rejection> {
        let params = request.params.clone().unwrap_or_default();
        serde_json::from_value(params.clone())
            .map(Json)
            .map_err(|e| JsonRpcError::DeserRequest {
                source: e,
                type_name: std::any::type_name::<T>(),
                request: params,
            })
    }
}

/// Extractor that exposes state to the handler.
pub struct State<S: Clone>(pub S);

impl<S: Clone> FromRequest<S> for State<S> {
    type Rejection = Infallible;

    fn from_request(_request: &Request, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(state.clone()))
    }
}

/// Methods that need the request ID for some reason but want to be able to be called as a
/// notification as well can use this extractor to get the ID from the request.
impl<S> FromRequest<S> for Option<types::Id> {
    type Rejection = Infallible;

    fn from_request(request: &Request, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(request.id.clone())
    }
}

/// Methods that need the request ID and must not be invoked as a notification can use this
/// extractor.
///
/// WARNING: If a method is invoked as a notification and thus has no request ID, if this extractor
/// is used by the method the result will be an error that is not returned to the caller since
/// notifications are fire-and-forget.
impl<S> FromRequest<S> for types::Id {
    type Rejection = types::ErrorDetails;

    fn from_request(request: &Request, _state: &S) -> Result<Self, Self::Rejection> {
        let id = request.id.clone().ok_or_else(|| {
            types::ErrorDetails::invalid_params("This method cannot be invoked as a notification", None)
        });

        // The remote peer will never see this error, but at least it will be logged by the
        // framework
        id
    }
}

/// Type implemented by anything that can be converted into a JSON RPC response.
pub trait IntoResponse {
    /// Convert the implementor's type into a JSON RPC response.
    fn into_response(self) -> types::ResponsePayload;
}

/// Handlers that return a result can be automatically converted into a response payload,
/// provided the success type implements [`Serialize`] and the error type implements
/// [`Into<types::ErrorResponse>`].
impl<T, E> IntoResponse for Result<T, E>
where
    T: Serialize,
    E: Into<types::ErrorDetails>,
{
    fn into_response(self) -> types::ResponsePayload {
        match self {
            Ok(value) => {
                // Try to serialize the type.  If that fails, this suddenly becomes an error
                // response
                match serde_json::to_value(value).map_err(|e| JsonRpcError::SerResponse {
                    source: e,
                    type_name: std::any::type_name::<T>(),
                }) {
                    Ok(json) => types::ResponsePayload::success(json),
                    Err(e) => types::ResponsePayload::error(e),
                }
            }
            Err(e) => types::ResponsePayload::error(e.into()),
        }
    }
}

/// If a handler returns a `JsonValue`, we assume that it's infallible and this is a success
/// response
impl IntoResponse for JsonValue {
    fn into_response(self) -> types::ResponsePayload {
        types::ResponsePayload::Success(types::SuccessResponse { result: self })
    }
}

/// Our internal error type can of course be used as a response type
impl IntoResponse for JsonRpcError {
    fn into_response(self) -> types::ResponsePayload {
        types::ResponsePayload::error(Into::<types::ErrorDetails>::into(self))
    }
}

impl IntoResponse for types::ErrorDetails {
    fn into_response(self) -> types::ResponsePayload {
        types::ResponsePayload::error(self)
    }
}

/// Return `T` in JSON as a success response.
///
/// In the unlikely event that serialization to JSON fails, this will instead produce an eror
/// response.
impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> types::ResponsePayload {
        match serde_json::to_value(self.0) {
            Ok(json) => types::ResponsePayload::success(json),
            Err(e) => {
                types::ResponsePayload::error(Into::<types::ErrorDetails>::into(JsonRpcError::SerResponse {
                    source: e,
                    type_name: std::any::type_name::<T>(),
                }))
            }
        }
    }
}

impl IntoResponse for Infallible {
    fn into_response(self) -> types::ResponsePayload {
        unreachable!("Infallible should never be used as a response type")
    }
}

/// A handler for JSON-RPC methods and notifications.
///
/// This trait is implemented by any code that can be invoked by a JSON-RPC server or client.
///
/// You may implement it directly, but it's more likely that you'll use the existing
/// implementations on `async` functions.  The way that is accomplished is inspired by how Axum
/// implements it's `Handler` trait in a very flexible way to maximize developer ergonomics.
///
/// In the JSON RPC spec, the only difference between a method invocation request, and a
/// notification, is that a notification doesn't have an ID and therefore there is no way to send a
/// response back to the remote peer that sent the notification.  In the spec there's no
/// requirement that actual methods be invoked with an invocation request and notifications with a
/// notification request.  So in this library we do not make a distinction between the two.  WHen
/// constructing a client or a server, you register methods, each of which is handled by an
/// implementation of [`Handler`].  Those methods might get called as notifications, or as
/// method requests.
///
/// Your implementation of [`Handler`] may provide a different implementation for the two
/// ways of being called, but by default the helper implementations of this trait implement the
/// notification in terms of the method and just discard the result.  If for some reason this isn't
/// what you want you can override this behavior and return an error for the invocation type that
/// you consider invalid.
///
/// ## Type Parameters
///
/// - `HackT`: This is a hack to get around Rust's rules for trait implementation.  To `impl Trait`
/// with type parameters, those type parameters have to be constrained to the trait itself or the
/// implementing type.  When we make blanket impls for all async functions that take certain types
/// of args, those arg types need to be partof the trait signature.  This type parameter will be
/// erased once the handler is passed to the router, so try to ignore it mentally.
/// - `S`: The type of the state that is passed to the handler.  Even handlers that don't care
/// about state get the state parameter, so they have to all agree on the same state type.  The
/// compiler should be able to deduce this so `Handler` implementatoins just need to take an `S`
/// parameter and not put constraints on it.
pub trait Handler<HackT, S>: Clone + Send + Sync + Sized + 'static {
    type MethodFuture: Future<Output = types::ResponsePayload> + Send;
    type NotificationFuture: Future<Output = ()> + Send;

    /// Handle the method when it's invoked as a request, returning a response or an error.
    fn handle_method(self, state: S, request: Request) -> Self::MethodFuture;

    /// Handle the method when it's invoked as a notification.
    fn handle_notification(self, state: S, request: Request) -> Self::NotificationFuture;
}

/// Private internal trait for which an impl is defined for every type that gets an
/// auto-implementation of [`Handler`].  This internal impl trait lets us extract some of the
/// implementation logic into one place (meanning one set of monomorphized impls) that can be
/// called from both the method and notification handler.
///
/// If you are implementing your own [`Handler`] this is not relevant to you, although you might
/// look at how it's implemented to get ideas for your own impls.
trait HandlerImplHelper<HackT, S>: Clone + Send + Sync + Sized + 'static {
    type MethodArgsTupl;
    type MethodResponse: IntoResponse;
    type MethodFuture: Future<Output = Self::MethodResponse> + Send;

    fn extract_method_args(
        state: S,
        request: Request,
    ) -> Result<Self::MethodArgsTupl, types::ResponsePayload>;

    fn call_impl_func(self, args: Self::MethodArgsTupl) -> Self::MethodFuture;
}

/// A dyn compatible version of [`Handler`].  This is more convenient to use elsewhere in
/// the codebase because it erases the type parameters, at the expense of a bit of vtable overhead.
///
/// However this overhead is trivial, and it still internally wraps a [`Handler`] implementation
/// which the compiler is able to monomorphize and (presumably) optimize aggresively.
// pub trait DynHandler<S> {
//     /// Handle the method when it's invoked as a request, returning a response or an error.
//     fn handle_method(
//         self,
//         state: S,
//         request: Request,
//     ) -> Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;
//
//     /// Handle the method when it's invoked as a notification.
//     fn handle_notification(
//         self,
//         state: S,
//         request: Request,
//     ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
// }
//
// impl<T: Handler<HackT, S> + Send, HackT, S> DynHandler<S> for T
// where
//     <T as Handler<S>>::MethodFuture: Send + 'static,
//     <T as Handler<S>>::NotificationFuture: Send + 'static,
// {
//     fn handle_method(
//         self,
//         state: S,
//         request: Request,
//     ) -> Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>> {
//         Handler::handle_method(self, state, request)
//             .map(|response| response.into_response())
//             .boxed()
//     }
//
//     fn handle_notification(
//         self,
//         state: S,
//         request: Request,
//     ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
//         Handler::handle_notification(self, state, request).boxed()
//     }
// }

/// Marker trait for [`Handler`] implementations that are implemented as method handlers as opposed
/// to notification handlers.
///
/// Both method handlers and notification handlers can be invoked as either methods or
/// notifications according to the JSON RPC spec, but this implementation makes a distinction
/// between the two based on how the handler is implemented.
pub trait MethodHandler<HackT, S>: Handler<HackT, S> {}

/// Marker trait for [`Handler`] implementations that are implemented as notification handlers as opposed
/// to method handlers.
///
/// Both method handlers and notification handlers can be invoked as either methods or
/// notifications according to the JSON RPC spec, but this implementation makes a distinction
/// between the two based on how the handler is implemented.
pub trait NotificationHandler<HackT, S>: Handler<HackT, S> {}

// This adapter wraps a future (producing a T) and “discards” its value,
// yielding a future that returns () instead.
//
// This is used to wrap method handler futures to make them return a unit type like a notification
// handler.
#[pin_project]
pub struct Discard<Fut>(#[pin] Fut);

impl<Fut, T> Future for Discard<Fut>
where
    Fut: Future<Output = T> + Send,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Poll the inner future and map its output.
        let this = self.project();
        match this.0.poll(cx) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}

// TODO: put this back once I figure out why the compiler thinks it conflics with the impls below
// impl<F, Fut, Res, S> Handler<S> for F
// where
//     F: FnOnce() -> Fut + Clone + Send + Sync + 'static,
//     Fut: Future<Output = Res> + Send,
//     Res: IntoResponse,
// {
//     type MethodResponse = Res;
//     type MethodFuture = Fut;
//     type NotificationFuture = Discard<Self::MethodFuture>;
//
//     fn handle_method(&self, _state: S, _request: Request) -> Self::MethodFuture {
//         (self.clone())()
//     }
//
//     fn handle_notification(&self, state: S, request: Request) -> Self::NotificationFuture {
//         Discard(<Self as Handler<S>>::handle_method(self, state, request))
//     }
// }

/// Stolen verbatim from https://github.com/tokio-rs/axum/blob/170d7d4dcc8a1368e7bea68f517a7791aff89422/axum/src/macros.rs#L49
/// Invoke a macro for all supported tuples up to 16 elements.
///
/// In our case 16 seems excessive since there aren't 16 useful things you can put in a handler
/// function.  If we need to reduce compile time cutting back on this would be a good start.
#[rustfmt::skip]
macro_rules! all_the_tuples {
    ($name:ident) => {
        $name!([], T1);
        $name!([T1], T2);
        // $name!([T1, T2], T3);
        // $name!([T1, T2, T3], T4);
        // $name!([T1, T2, T3, T4], T5);
        // $name!([T1, T2, T3, T4, T5], T6);
        // $name!([T1, T2, T3, T4, T5, T6], T7);
        // $name!([T1, T2, T3, T4, T5, T6, T7], T8);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8], T9);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9], T10);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10], T11);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11], T12);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12], T13);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13], T14);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14], T15);
        // $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15], T16);
    };
}

/// Another macro heavily influenced by axum (see <https://github.com/tokio-rs/axum/blob/170d7d4dcc8a1368e7bea68f517a7791aff89422/axum/src/handler/mod.rs#L206C1-L244C2>)
/// This macro generates the impls for the [`Handler`] and [`MethodHandler`] trait for all async methods that have
/// appropriate signatures.
///
/// Method handlers have a return type
macro_rules! impl_method_handler {
    (
        [$($ty:ident),*], $last:ident
    ) => {
        // Put as much logic as possible in this impl helper, so it gets compiled and monomorphized
        // just once.  The separate impls for method and notification handling can just call into
        // this impl.
        #[allow(non_snake_case)]
        impl<F, Fut, S, Res, $($ty,)* $last> HandlerImplHelper<($($ty,)* $last,), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send,
            S: Send + Sync + 'static,
            Res: IntoResponse,
            $( $ty: FromRequest<S> + Send, )*
            $last: FromRequest<S> + Send {
            type MethodArgsTupl = ($($ty,)* $last,);
            type MethodResponse = Res;
            type MethodFuture = Fut;

            fn extract_method_args(state: S, request: Request) -> Result<Self::MethodArgsTupl, types::ResponsePayload> {
                $(
                    let $ty = match $ty::from_request(&request, &state) {
                        Ok(value) => value,
                        Err(rejection) => return Err(rejection.into_response()),
                    };
                )*

                let $last = match $last::from_request(&request, &state) {
                    Ok(value) => value,
                    Err(rejection) => return Err(rejection.into_response()),
                };

                // All of the args were successfully extracted from the request, so we have the
                // method tuple now
                Ok(($($ty,)* $last,))
            }

            fn call_impl_func(self, ($($ty,)* $last,): Self::MethodArgsTupl) -> Self::MethodFuture {
                self($($ty,)* $last,)
            }
        }

        // Implement `Handler` in terms of `HandlerImplHelper`.
        //
        // Note that the type constraints here are duplicated, rather than just being `where F:
        // HandlerImplHelper...`, because if the user screws up and writes a function that doesn't
        // meet the constraints, at least we want the (probably not very helpful) compiler error to
        // complain about a lack of `Handler`
        #[allow(non_snake_case)]
        impl<F, Fut, S, Res, $($ty,)* $last> Handler<($($ty,)* $last,), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send,
            S: Send + Sync + 'static,
            Res: IntoResponse,
            $( $ty: FromRequest<S> + Send, )*
            $last: FromRequest<S> + Send {
            /// TODO: The machinations extracting from request and dealing with potential errors
            /// could all be packaged into a custom future and then we can avoid boxing here.  But it
            /// will be boxed anyway in the Dyn wrapper so maybe not worth it.
            type MethodFuture = Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;
            type NotificationFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

            fn handle_method(self, state: S, request: Request) -> Self::MethodFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(err) => return err,
                    };

                    let response = self.call_impl_func(args).await;
                    response.into_response()
                }.boxed()
            }

            fn handle_notification(self, state: S, request: Request) -> Self::NotificationFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(_) => return,
                    };

                    let _ = self.call_impl_func(args).await;
                }.boxed()
            }
        }

        #[allow(non_snake_case)]
        impl<F, Fut, S, $($ty,)* $last> MethodHandler<($($ty,)* $last,), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            F: Handler<($($ty,)* $last,), S>,
        {}
    };
}

all_the_tuples!(impl_method_handler);

// TODO: Next step is to actually start generating the `MethodHandler` impls for async functions.
// Axum uses macros for this since they support up to 16 parameters.  I don't think we need that
// many; there aren't even 16 extractors available.  We can start with 8 and see how it goes.
//
// Also because I want to distinguish between methods and notification handlers, I want the async
// functions to be wrapped in a decorator `Method` or `Notification`.  So the macros will need to
// generate two variations, one for `Method` and one for `Notification`. Actually now that I think
// about it, that can be achieved easier by just treating fallible async funtions as methods and
// infallible ones as notifications.  That only determines which of the implementations is native;
// of course either can be invoked as either a method or a notification.
//
// Actually clearer still would be to have two separate traits, `MethodHandler` and
// `NotificationHandler`, and then rename the current `MethodHandler` to `Handler`.
// The macros would implement `MethodHandler` for all types of async funtions, and marker traits
// `MethodHandler` and `NotificationHandler` to distinguishb between the two.  Actually they should
// both inherit from `Handler` so they are truly just marker traits.

///// Wraps a function pointer in a struct that implements MethodHandler by calling the function in
///// the [`MethodHandler::handle_method`] method.  Notifications are handled by the same function,
///// with the return value discarded.
/////
///// Has constructor methods that wrap function pointers with various types of parameters and return
///// types, for maximum user convenience.
//struct MethodHandlerFn<Func, Fut> {
//    func: Func,
//    _phantom: PhantomData<Fut>,
//}
//
//impl<Func, Fut> MethodHandlerFn<Func, Fut>
//where
//    Func: Fn(Request, JsonValue) -> Fut + Send + 'static,
//    Fut: Future<Output = Result<JsonValue>> + Send,
//{
//    fn new(func: Func) -> Self {
//        Self {
//            func,
//            _phantom: PhantomData,
//        }
//    }
//}
//
//impl<Func, Fut> Clone for MethodHandlerFn<Func, Fut>
//where
//    Func: Clone,
//{
//    fn clone(&self) -> Self {
//        Self {
//            func: self.func.clone(),
//            _phantom: PhantomData,
//        }
//    }
//}
//
//impl<Func, Fut> MethodHandler for MethodHandlerFn<Func, Fut>
//where
//    Func: Fn(Request, JsonValue) -> Fut + Send + 'static,
//    Fut: Future<Output = Result<JsonValue>> + Send,
//{
//    type MethodFuture = Fut;
//
//    type NotificationFuture = Discard<Fut>;
//
//    fn handle_method(&self, context: Request, params: JsonValue) -> Self::MethodFuture {
//        (self.func)(context, params)
//    }
//
//    fn handle_notification(&self, context: Request, params: JsonValue) -> Self::NotificationFuture {
//        Discard(MethodHandler::handle_method(self, context, params))
//    }
//}
//
///// Helpers for constructing method handlers from various functions
//pub mod method {
//    use super::*;
//
//    /// Construct a [`DynMethodHandler`] from a function pointer that has the exact same signature as
//    /// the [`DynMethodHandler::handle_method`] method.
//    ///
//    /// This is only useful if you want complete control over the internals of the handler.  For
//    /// most applications a higher-level construction is more convenient
//    pub fn raw<RawFunc, RawFut>(f: RawFunc) -> impl DynMethodHandler
//    where
//        RawFunc: Fn(super::Request, serde_json::Value) -> RawFut + Send + 'static,
//        RawFut: Future<Output = Result<serde_json::Value>> + Send + 'static,
//    {
//        super::MethodHandlerFn::new(f)
//    }
//
//    /// Construct a [`DynMethodHandler`] from a function pointer that takes and returns types that
//    /// must be serde'd to/from JSON values, and possibly with a custom error type.
//    pub fn serde_args<Req, Resp, Func, Fut, Err>(f: Func) -> impl DynMethodHandler
//    where
//        Func: Fn(Request, Req) -> Fut + Send + 'static,
//        Req: DeserializeOwned,
//        Resp: Serialize,
//        Fut: Future<Output = Result<Resp, Err>> + Send + 'static,
//        Err: Into<JsonRpcError>,
//    {
//        super::MethodHandlerFn::new(move |context, params: JsonValue| {
//            // In order to appease the borrow checker, we need to deserialize the request and call
//            // the function outside of the `async move` block.  Once inside the `async move` block
//            // we will return an error if the deserialization failed, and if not we will await the
//            // future returned by the function.
//            let result = serde_json::from_value(params.clone())
//                .map_err(|e| JsonRpcError::DeserRequest {
//                    source: e,
//                    request: params,
//                })
//                .map(|params| f(context, params));
//
//            async move {
//                let response = result?.await.map_err(|e| e.into())?;
//                let response = serde_json::to_value(response).map_err(|e| JsonRpcError::SerResponse {
//                    source: e,
//                    type_name: std::any::type_name::<Resp>(),
//                })?;
//
//                Ok(response)
//            }
//        })
//    }
//}
//
///// Helpers for constructing notification handlers from various functions
//pub mod notification {
//    use super::*;
//}
//
//async fn bullshit(handler: impl DynMethodHandler + 'static) -> Box<dyn DynMethodHandler> {
//    Box::new(handler)
//}
