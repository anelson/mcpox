//! In JSON RPC servers and clients both, peers can expose methods that can be called in an RPC
//! style by the remote peer, or invoked in a fire-and-forget style as notifications.
//!
//! Either way, the code that runs in rseponse to methods/notification is called a "handler".
//! All such code implements [`Handler`], although it's not intended that most users will
//! implement that directly.
//!
//! Instead, [`HandlerFn`] provides an implementation that can be wrapped around a variety of
//! different types of functions, for convenients.
use crate::{InvocationRequest, types};
use crate::{JsonRpcError, Result};
use futures::FutureExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::pin::Pin;

/// Inspirted by axum's `FromRequest` trait.
///
/// Axum uses two traits, `FromRequestParts` and `FromRequest`, the important difference being that
/// there can be only one `FromRequest`-impementing parameter in a handler and it must be the last
/// parameter, because `FromRequest` consumes they entire request (including the body).
///
/// TODO: Is this still true?  If we had two traits like axum, the `Params` extractor wouldn't have
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
    fn from_request(request: &InvocationRequest, state: &S) -> Result<Self, Self::Rejection>;
}

// Extractors that pull information from the request and make it available to a handler

/// A Rust struct deserialized from JSON containing the method parameters.
///
/// This is a wrapper around a type that implements [`Serialize`].
pub struct Params<T>(pub T);

impl<T: DeserializeOwned, S> FromRequest<S> for Params<T> {
    type Rejection = JsonRpcError;

    fn from_request(request: &InvocationRequest, _state: &S) -> Result<Self, Self::Rejection> {
        // The params field is optional, so we need to handle the case where it's None
        // A None value means that the caller does not think that the method is expecting any
        // parameters.  If that is wrong, then the deserializatoin of T will be indicate that.
        // There is no need to test for the absence of `params` and fail especially for that.  For
        // all we know, `T` might have a bunch of default values and only needs to be provided with
        // concrete values if the caller is overriding those.
        serde_json::from_value(request.params.clone().unwrap_or_default())
            .map(Self)
            .map_err(|e| JsonRpcError::DeserRequest {
                source: e,
                type_name: std::any::type_name::<T>(),
                request: request.params.clone().unwrap_or_default(),
            })
    }
}

/// Extractor that exposes state to the handler.
pub struct State<S: Clone>(pub S);

impl<S: Clone> FromRequest<S> for State<S> {
    type Rejection = Infallible;

    fn from_request(_request: &InvocationRequest, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(state.clone()))
    }
}

pub struct MethodName(pub String);

impl<S> FromRequest<S> for MethodName {
    type Rejection = Infallible;

    fn from_request(request: &InvocationRequest, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(request.method.clone()))
    }
}

/// Methods that need the request ID for some reason but want to be able to be called as a
/// notification as well can use this extractor to get the ID from the request.
impl<S> FromRequest<S> for Option<types::Id> {
    type Rejection = Infallible;

    fn from_request(request: &InvocationRequest, _state: &S) -> Result<Self, Self::Rejection> {
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

    fn from_request(request: &InvocationRequest, _state: &S) -> Result<Self, Self::Rejection> {
        // The remote peer will never see this error, but at least it will be logged by the
        // framework
        request.id.clone().ok_or_else(|| {
            types::ErrorDetails::invalid_params("This method cannot be invoked as a notification", None)
        })
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
                types::ResponsePayload::serialize_to_success(value)
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

/// Encapsulates a serializable response from a method handler to be sent back to the caller as
/// JSON
pub struct MethodResponse<T: Serialize>(pub T);

impl<T: Serialize> IntoResponse for MethodResponse<T> {
    fn into_response(self) -> types::ResponsePayload {
        types::ResponsePayload::serialize_to_success(self.0)
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
///   with type parameters, those type parameters have to be constrained to the trait itself or the
///   implementing type.  When we make blanket impls for all async functions that take certain types
///   of args, those arg types need to be partof the trait signature.  This type parameter will be
///   erased once the handler is passed to the router, so try to ignore it mentally.
///
/// - `S`: The type of the state that is passed to the handler.  Even handlers that don't care about
///   state get the state parameter, so they have to all agree on the same state type.  The compiler
///   should be able to deduce this so `Handler` implementatoins just need to take an `S` parameter
///   and not put constraints on it.
pub trait Handler<HackT, S>: Clone + Send + Sync + Sized + 'static {
    type MethodFuture: Future<Output = types::ResponsePayload> + Send;
    type NotificationFuture: Future<Output = ()> + Send;

    /// Handle the method when it's invoked as a request, returning a response or an error.
    fn handle_method(self, state: S, request: InvocationRequest) -> Self::MethodFuture;

    /// Handle the method when it's invoked as a notification.
    fn handle_notification(self, state: S, request: InvocationRequest) -> Self::NotificationFuture;
}

/// A trait that is used to wrap [`Handler`] with the `HackT` type parameter erased.
///
/// To wrap a regular [`Handler`] in this trait, use the [`erase_handler`] function.
pub(crate) trait ErasedHandler<S>: Sync + Send + 'static {
    /// Handle the method when it's invoked as a request, returning a response or an error.
    fn handle_method(
        &self,
        state: S,
        request: InvocationRequest,
    ) -> Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;

    /// Handle the method when it's invoked as a notification.
    fn handle_notification(
        &self,
        state: S,
        request: InvocationRequest,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

    /// Make it possible to implement `Clone` on `Box<dyn ErasedHandler<S>>`
    fn clone_boxed(&self) -> Box<dyn ErasedHandler<S>>;
}

impl<S> Clone for Box<dyn ErasedHandler<S>>
where
    S: 'static,
{
    fn clone(&self) -> Self {
        self.clone_boxed()
    }
}

/// Erase the `HackT` type parameter from a [`Handler`] and return a boxed version of it in a dyn
/// compatible package.
pub(crate) fn erase_handler<H, HackT, S>(handler: H) -> Box<dyn ErasedHandler<S>>
where
    H: Handler<HackT, S>,
    // Sadly these are required because the compiler doesn't care that the struct doesn't actually
    // hold any values of these types
    HackT: Send + Sync + 'static,
    S: Send + Sync + 'static,
{
    /// A container for a boxed handler that erases the `HackT` type parameter.
    struct MakeErasedHandler<H, HackT, S> {
        handler: H,
        // Note the use of `fn ...` here instead of the usual direct use of the type param.  This
        // prevents the compiler from considering `HackT` and `S` when deciding if this struct is Send
        // and Sync.
        _phantom: PhantomData<fn() -> (HackT, S)>,
    }

    impl<H, HackT, S> ErasedHandler<S> for MakeErasedHandler<H, HackT, S>
    where
        H: Handler<HackT, S> + 'static,
        // Sadly these are required because the compiler doesn't care that the struct doesn't actually
        // hold any values of these types
        HackT: Send + Sync + 'static,
        S: Send + Sync + 'static,
    {
        fn handle_method(
            &self,
            state: S,
            request: InvocationRequest,
        ) -> Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>> {
            self.handler.clone().handle_method(state, request).boxed()
        }

        fn handle_notification(
            &self,
            state: S,
            request: InvocationRequest,
        ) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
            self.handler.clone().handle_notification(state, request).boxed()
        }

        fn clone_boxed(&self) -> Box<dyn ErasedHandler<S>> {
            Box::new(Self {
                handler: self.handler.clone(),
                _phantom: PhantomData,
            })
        }
    }

    Box::new(MakeErasedHandler {
        handler,
        _phantom: PhantomData,
    })
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
    type MethodResponse;
    type MethodFuture: Future<Output = Self::MethodResponse> + Send;

    fn extract_method_args(
        state: S,
        request: InvocationRequest,
    ) -> Result<Self::MethodArgsTupl, types::ResponsePayload>;

    fn call_impl_func(self, args: Self::MethodArgsTupl) -> Self::MethodFuture;
}

/// Implement handler for parameterless async functions, which are a special case that doesn't fit
/// the macro that generates all of the other impls.  If the async func returns () then it's a
/// notification handler, otherwise if it returns a value that implements IntoResponse then it's a
/// method handler.
impl<F, Fut, S, Res> Handler<(Res,), S> for F
where
    F: FnOnce() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Res> + Send,
    S: Send + Sync + 'static,
    Res: IntoResponse,
{
    type MethodFuture = Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;
    type NotificationFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

    fn handle_method(self, _state: S, _request: InvocationRequest) -> Self::MethodFuture {
        async move {
            let response = self().await;
            response.into_response()
        }
        .boxed()
    }

    fn handle_notification(self, _state: S, _request: InvocationRequest) -> Self::NotificationFuture {
        async move {
            let _response = self().await;
        }
        .boxed()
    }
}

impl<F, Fut, S> Handler<((),), S> for F
where
    F: FnOnce() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
    S: Send + Sync + 'static,
{
    type MethodFuture = Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;
    type NotificationFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

    fn handle_method(self, _state: S, _request: InvocationRequest) -> Self::MethodFuture {
        async move {
            self().await;
            types::ResponsePayload::success(JsonValue::Null)
        }
        .boxed()
    }

    fn handle_notification(self, _state: S, _request: InvocationRequest) -> Self::NotificationFuture {
        async move {
            self().await;
        }
        .boxed()
    }
}

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
        $name!([T1, T2], T3);
        $name!([T1, T2, T3], T4);
        $name!([T1, T2, T3, T4], T5);
        $name!([T1, T2, T3, T4, T5], T6);
        $name!([T1, T2, T3, T4, T5, T6], T7);
        $name!([T1, T2, T3, T4, T5, T6, T7], T8);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8], T9);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9], T10);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10], T11);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11], T12);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12], T13);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13], T14);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14], T15);
        $name!([T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15], T16);
    };
}

/// Generate an impl of [`Handler`] for any async function that returns some value that implements
/// `IntoResponse` and whose arguments can be extracted from a request.
macro_rules! impl_method_handler {
    (
        [$($ty:ident),*], $last:ident
    ) => {
        // Put as much logic as possible in this impl helper, so it gets compiled and monomorphized
        // just once.  The separate impls for method and notification handling can just call into
        // this impl.
        #[allow(non_snake_case)]
        impl<F, Fut, S, Res, $($ty,)* $last> HandlerImplHelper<(($($ty,)* $last,), Res), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = Res> + Send,
            Res: IntoResponse,
            S: Send + Sync + 'static,
            $( $ty: FromRequest<S> + Send, )*
            $last: FromRequest<S> + Send {
            type MethodArgsTupl = ($($ty,)* $last,);
            type MethodResponse = Res;
            type MethodFuture = Fut;

            fn extract_method_args(state: S, request: InvocationRequest) -> Result<Self::MethodArgsTupl, types::ResponsePayload> {
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
        impl<F, Fut, S, Res, $($ty,)* $last> Handler<(($($ty,)* $last,), Res), S> for F
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

            fn handle_method(self, state: S, request: InvocationRequest) -> Self::MethodFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(err) => return err,
                    };

                    let response = self.call_impl_func(args).await;
                    response.into_response()
                }.boxed()
            }

            fn handle_notification(self, state: S, request: InvocationRequest) -> Self::NotificationFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(_) => return,
                    };

                    let _ = self.call_impl_func(args).await;
                }.boxed()
            }
        }
    };
}

/// Generate an impl of [`Handler`] for any async function that returns ()
/// and whose arguments can be extracted from a request.
macro_rules! impl_notification_handler {
    (
        [$($ty:ident),*], $last:ident
    ) => {
        // Put as much logic as possible in this impl helper, so it gets compiled and monomorphized
        // just once.  The separate impls for method and notification handling can just call into
        // this impl.
        //
        // TODO: This is almost identical to the method impl of [`HandlerImplHelper`], with the
        // exception that there is no `Res` type parameter because the async function must return
        // `()`.  A clever use of Rust macros might be able to avoid this almost-duplicated
        // implementation.
        #[allow(non_snake_case)]
        impl<F, Fut, S, $($ty,)* $last> HandlerImplHelper<(($($ty,)* $last,), ()), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = ()> + Send,
            S: Send + Sync + 'static,
            $( $ty: FromRequest<S> + Send, )*
            $last: FromRequest<S> + Send {
            type MethodArgsTupl = ($($ty,)* $last,);
            type MethodResponse = ();
            type MethodFuture = Fut;

            fn extract_method_args(state: S, request: InvocationRequest) -> Result<Self::MethodArgsTupl, types::ResponsePayload> {
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
        impl<F, Fut, S, $($ty,)* $last> Handler<(($($ty,)* $last,), ()), S> for F
        where
            F: FnOnce($($ty,)* $last,) -> Fut + Clone + Send + Sync + 'static,
            Fut: Future<Output = ()> + Send,
            S: Send + Sync + 'static,
            $( $ty: FromRequest<S> + Send, )*
            $last: FromRequest<S> + Send {
            /// TODO: The machinations extracting from request and dealing with potential errors
            /// could all be packaged into a custom future and then we can avoid boxing here.  But it
            /// will be boxed anyway in the Dyn wrapper so maybe not worth it.
            type MethodFuture = Pin<Box<dyn Future<Output = types::ResponsePayload> + Send + 'static>>;
            type NotificationFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

            fn handle_method(self, state: S, request: InvocationRequest) -> Self::MethodFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(err) => return err,
                    };

                    self.call_impl_func(args).await;

                    // This is a notification handler, it doesn't produce a return value
                    types::ResponsePayload::success(JsonValue::Null)
                }.boxed()
            }

            fn handle_notification(self, state: S, request: InvocationRequest) -> Self::NotificationFuture {
                async move {
                    let args = match Self::extract_method_args(state, request) {
                        Ok(args) => args,
                        Err(_) => return,
                    };

                    self.call_impl_func(args).await;
                }.boxed()
            }
        }

    };
}

all_the_tuples!(impl_method_handler);
all_the_tuples!(impl_notification_handler);

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use super::*;
    use serde::{Deserialize, Serialize};

    #[test]
    fn test_compile_time_handler_impls_exist() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct MyRequestStruct {
            foo: String,
        }
        #[derive(Serialize)]
        #[allow(dead_code)]
        struct MyResponseStruct {
            bar: String,
        }
        #[allow(dead_code)]
        struct AppState {
            baz: usize,
        }

        // This is a compile-time test to make sure that the `Handler` impl is available where it
        // should be.
        fn assert_handler<HackT, T, S>(_: T)
        where
            T: Handler<HackT, S>,
        {
        }

        // An async function that takes no arguments and returns nothing
        // (obviously useful as a notification)
        async fn unit_func() {}
        assert_handler::<_, _, ()>(unit_func);

        // Async function that takes no arguments but returns a JSON response
        async fn no_args_jsonvalue_retval() -> JsonValue {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(no_args_jsonvalue_retval);

        // Async function that takes no arguments but returns a serializable struct response
        async fn no_args_serde_retval() -> MethodResponse<MyResponseStruct> {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(no_args_serde_retval);

        // Async function that takes no arguments but returns a `Result` where T is a valid
        // response and E can be converted into ErrorDetails
        async fn no_args_fallible_retval() -> Result<JsonValue, JsonRpcError> {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(no_args_fallible_retval);

        // Async function that takes method arguments as a deserializable struct and has no return
        #[allow(unused_variables)]
        async fn params_args_no_retval(Params(MyRequestStruct { foo }): Params<MyRequestStruct>) {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(params_args_no_retval);

        // Async function that takes method arguments as a deserializable struct and also returns a
        // serializable struct response
        #[allow(unused_variables)]
        async fn params_args_serde_retval(
            Params(MyRequestStruct { foo }): Params<MyRequestStruct>,
        ) -> MethodResponse<MyResponseStruct> {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(params_args_serde_retval);

        // Async function that takes the optional request ID, and method arguments as a deserializable
        // struct and also returns a serializable struct response
        #[allow(unused_variables)]
        async fn opt_id_and_params_args_serde_retval(
            id: Option<types::Id>,
            Params(MyRequestStruct { foo }): Params<MyRequestStruct>,
        ) -> MethodResponse<MyResponseStruct> {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(opt_id_and_params_args_serde_retval);

        // Async function that takes the non-optional request ID, and method arguments as a deserializable
        // struct and also returns a serializable struct response
        #[allow(unused_variables)]
        async fn id_and_params_args_serde_retval(
            id: types::Id,
            Params(MyRequestStruct { foo }): Params<MyRequestStruct>,
        ) -> MethodResponse<MyResponseStruct> {
            unimplemented!()
        }
        assert_handler::<_, _, ()>(id_and_params_args_serde_retval);

        // Async function that takes the non-optional request ID, state, and method arguments as a
        // deserializable struct and also returns a serializable struct response
        #[allow(unused_variables)]
        async fn id_state_and_params_args_serde_retval(
            id: types::Id,
            State(state): State<Arc<AppState>>,
            Params(MyRequestStruct { foo }): Params<MyRequestStruct>,
        ) -> MethodResponse<MyResponseStruct> {
            unimplemented!()
        }
        assert_handler(id_state_and_params_args_serde_retval);
    }
}
