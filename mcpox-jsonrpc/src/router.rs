//! After JSON RPC messages have been decoded, those that represent method calls or notifications
//! need to be routed to the corresponding handler, and that handler invoked.  The logic to perform
//! this is called "routing", and is implemented in the [`Router`] type in this module.
use crate::{handler, types};
use futures::{FutureExt, TryFutureExt};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tracing::Instrument;

/// Type alias for the handlers map to simplify the type signature
type HandlersMap<S> = Arc<RwLock<HashMap<String, Box<dyn handler::ErasedHandler<S>>>>>;

/// Router inspired loosely by the `axum` crate router, but simplified and specialized for
/// JSON-RPC.
///
/// This router is much simpler than the `axum` router because it only has to route requests based
/// on the JSON-RPC method name, and the requests that it is routing are not HTTP requests but
/// complete, already-decoded JSON RPC requests.
#[derive(Clone)]
pub struct Router<S: Clone + Send + Sync + 'static = ()> {
    state: S,
    fallback_handler: Box<dyn handler::ErasedHandler<S>>,
    handlers: HandlersMap<S>,
}

impl Router {
    /// Create a new router that doesn't need any state.
    pub fn new_stateless() -> Router<()> {
        Router {
            state: (),
            fallback_handler: handler::erase_handler(Self::fallback_handler),
            handlers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn fallback_handler(
        id: Option<types::Id>,
        handler::MethodName(method_name): handler::MethodName,
    ) -> types::ErrorDetails {
        if id.is_some() {
            // Methods have request IDs
            tracing::error!(method = %method_name, "Unknown method");
            types::ErrorDetails::method_not_found(format!("Unknown method '{method_name}'"), None)
        } else {
            // Notifications do not
            tracing::error!(notification = %method_name, "Unknown notification");

            // This error will never be seen since this is a notification
            types::ErrorDetails::method_not_found("Unknown notification".to_string(), None)
        }
    }
}

impl<S: Clone + Send + Sync + 'static> Router<S> {
    /// Create a new router whose handlers can access the provided shared state
    pub fn new_with_state(state: S) -> Self {
        Router {
            state,
            fallback_handler: handler::erase_handler(Router::fallback_handler),
            handlers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Return the current state in the router, which is also accessible to all handlers
    pub fn state(&self) -> &S {
        &self.state
    }

    pub fn register_fallback_handler<HackT, H>(&mut self, handler: H)
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        self.fallback_handler = handler::erase_handler(handler);
    }

    pub fn register_handler<H, HackT>(&mut self, method: impl Into<String>, handler: H)
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        let method = method.into();
        let mut handlers = self.handlers.write().unwrap();
        handlers.insert(method, handler::erase_handler(handler));
    }

    /// Handle a invocation request, passing it to the matching handler, or running the
    /// default handler (which returns an error indicating that no such method was found).
    pub(crate) fn handle_invocation(
        &self,
        request: handler::InvocationRequest,
    ) -> Pin<Box<dyn Future<Output = Option<types::Response>> + Send + 'static>> {
        // Construct a span for both the code in the handlers before any future is created (if
        // any), and also to apply to the handler's future itself to ensure that this additional
        // context is present in the logs.  This is also used in case of a panic so that when the
        // panic handler logs the panic we can tell what request caused it.
        let span = if let Some(id) = request.id.as_ref() {
            tracing::debug_span!("handle_method", id = %id, method = %request.method)
        } else {
            tracing::debug_span!("handle_notification", notification = %request.method)
        };
        let local_span = span.clone();
        let _guard = local_span.enter();

        let handlers = self.handlers.read().unwrap();
        match handlers.get(&request.method) {
            Some(handler) => {
                // Invoke as an method or notification depending on whether the request has an ID
                if let Some(id) = request.id.clone() {
                    catch_unwind_closure(request.id.clone(), || {
                        handler
                            .handle_method(self.state.clone(), request)
                            .map(|response_payload| {
                                let response = types::Response::new(id, response_payload);
                                Some(response)
                            })
                            .instrument(span)
                    })
                    .boxed()
                } else {
                    catch_unwind_closure(request.id.clone(), || {
                        handler
                            .handle_notification(self.state.clone(), request)
                            .map(|_response| None)
                            .instrument(span)
                    })
                    .boxed()
                }
            }
            None => {
                if let Some(id) = request.id.clone() {
                    catch_unwind_closure(request.id.clone(), || {
                        self.fallback_handler
                            .handle_method(self.state.clone(), request)
                            .map(|response_payload| {
                                let response = types::Response::new(id, response_payload);
                                Some(response)
                            })
                            .instrument(span)
                    })
                    .boxed()
                } else {
                    catch_unwind_closure(request.id.clone(), || {
                        self.fallback_handler
                            .handle_method(self.state.clone(), request)
                            .map(|_| None)
                            .instrument(span)
                    })
                    .boxed()
                }
            }
        }
    }
}

/// Wrap a closure that calls a handler to return a future, to add catch/unwind in case one of the
/// naughty handlers should have the audacity to panic.
///
/// Handle either a panic in the closure that is supposed to return the future, or a panic in the
/// future itself when it is polled.
///
/// In case of panic this will log it and return a future that evaluates to an empty response (if
/// this was a method call), or an empty response (if this was a notification)
fn catch_unwind_closure<Func, Fut>(
    request_id: Option<types::Id>,
    handler_func: Func,
) -> impl Future<Output = Option<types::Response>> + Send + 'static
where
    Func: FnOnce() -> Fut + Send,
    Fut: Future<Output = Option<types::Response>> + Send + 'static,
{
    // Call the closure within `catch_unwind` to catch panics in the closure, and if it
    // successfully returns a future than use another helper function to wrap the future in the
    // async equivalent of `catch_unwind`
    match std::panic::catch_unwind(AssertUnwindSafe(handler_func)) {
        Ok(handler_fut) => catch_unwind_future(request_id, handler_fut),
        Err(err) => {
            let err_string = panic_err_to_string(err);
            tracing::error!("Handler function panicked: {}", err_string);
            futures::future::ready(request_id.map(make_panic_error_response)).boxed()
        }
    }
}

/// Wrap a future that is invoking one of the handlers, to add catch/unwind in case one of the
/// naughty handlers should have the audacity to panic.
///
/// If the handler *does* panic, log the error and construct a generic server error response to
/// send back to the caller.  If this is a notification, the caller will never see the response,
/// but we will at least have logged the error.
fn catch_unwind_future(
    request_id: Option<types::Id>,
    f: impl Future<Output = Option<types::Response>> + Send + 'static,
) -> Pin<Box<dyn Future<Output = Option<types::Response>> + Send + 'static>> {
    AssertUnwindSafe(f)
        .catch_unwind()
        .unwrap_or_else(|err| {
            let err_string = panic_err_to_string(err);
            tracing::error!("Handler future panicked: {}", err_string);
            request_id.map(make_panic_error_response)
        })
        .boxed()
}

fn make_panic_error_response(request_id: types::Id) -> types::Response {
    types::Response::error(
        request_id,
        types::ErrorCode::InternalError,
        "The requested handler encountered a serious error",
        None,
    )
}

/// Try to make something useful from the panic for logging purposes
fn panic_err_to_string(err: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        // Get the type name using the Any::type_id method
        let type_id = (*err).type_id();
        let type_name = std::any::type_name_of_val(&*err);

        format!("Panic of type {} (type ID {:?})", type_name, type_id)
    }
}

#[cfg(test)]
mod tests {
    use crate::Handler;
    use crate::handler;
    use crate::testing;

    use super::*;

    use serde_json::{Value as JsonValue, json};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Tests the different handler invocation cases, including successfully finding a handler
    /// and falling back to the fallback handler when a method is not found.
    #[tokio::test]
    async fn test_handler_invocation_cases() {
        testing::init_test_logging();

        // Define an enum to track what kind of invocation was received
        #[derive(Debug, PartialEq, Clone)]
        enum InvocationType {
            Method(String),
            Notification(String),
            FallbackMethod(String),
            FallbackNotification(String),
        }

        // State for our test to track invocations
        #[derive(Debug, Clone)]
        struct TestState {
            invocations: Arc<RwLock<Vec<InvocationType>>>,
            counter: Arc<AtomicU32>,
        }

        // Initialize test state
        let test_state = TestState {
            invocations: Arc::new(RwLock::new(Vec::new())),
            counter: Arc::new(AtomicU32::new(0)),
        };

        // Handler that logs method invocations and returns increasing values
        async fn test_handler(
            handler::State(state): handler::State<TestState>,
            handler::MethodName(method_name): handler::MethodName,
            typ: handler::InvocationType,
        ) -> JsonValue {
            let invocation = match typ {
                handler::InvocationType::Method => InvocationType::Method(method_name.clone()),
                handler::InvocationType::Notification => InvocationType::Notification(method_name.clone()),
            };

            // Record that this was called
            {
                let mut invocations = state.invocations.write().unwrap();
                invocations.push(invocation);
            }

            // Return a counter value as the response
            let counter_value = state.counter.fetch_add(1, Ordering::SeqCst) + 1;
            json!(counter_value)
        }

        // Custom fallback handler that records invocations
        async fn test_fallback_handler(
            handler::State(state): handler::State<TestState>,
            handler::MethodName(method_name): handler::MethodName,
            typ: handler::InvocationType,
        ) -> JsonValue {
            let invocation = match typ {
                handler::InvocationType::Method => InvocationType::FallbackMethod(method_name.clone()),
                handler::InvocationType::Notification => {
                    InvocationType::FallbackNotification(method_name.clone())
                }
            };

            // Record that this was called
            {
                let mut invocations = state.invocations.write().unwrap();
                invocations.push(invocation);
            }

            // Return a counter value as the response
            let counter_value = state.counter.fetch_add(1, Ordering::SeqCst) + 1;
            json!(counter_value)
        }

        // Create router with our test state
        let mut router = Router::new_with_state(test_state.clone());

        // Register our custom fallback handler
        router.register_fallback_handler(test_fallback_handler);

        // Register handlers for specific methods
        router.register_handler("test_method", test_handler);

        // 1. Test method invocation
        let request = handler::InvocationRequest::new_test_request(
            Some(types::Id::Number(1)),
            "test_method",
            json!("test_param"),
        );
        let response = router.handle_invocation(request).await;

        // Verify response is as expected
        match response {
            Some(types::Response {
                id: types::Id::Number(n),
                payload: types::ResponsePayload::Success(types::SuccessResponse { result }),
                ..
            }) => {
                assert_eq!(n, 1);
                assert_eq!(result, json!(1));
            }
            _ => panic!("Response didn't match expected pattern for test_method"),
        }

        // 2. Test notification invocation
        let request =
            handler::InvocationRequest::new_test_request(None, "test_method", json!("notification_param"));
        let response = router.handle_invocation(request).await;
        assert!(response.is_none(), "Notification should not produce a response");

        // 3. Test fallback handler for unknown method
        let request = handler::InvocationRequest::new_test_request(
            Some(types::Id::Number(2)),
            "unknown_method",
            json!("unknown_param"),
        );
        let response = router.handle_invocation(request).await;

        // Verify fallback handler response is as expected
        match response {
            Some(types::Response {
                id: types::Id::Number(n),
                payload: types::ResponsePayload::Success(types::SuccessResponse { result }),
                ..
            }) => {
                assert_eq!(n, 2);
                assert_eq!(result, json!(3)); // skip 2 because that was a notification
            }
            _ => panic!("Response didn't match expected pattern for test_method"),
        }
        // 4. Test fallback handler for unknown notification
        let request = handler::InvocationRequest::new_test_request(
            None,
            "unknown_notification",
            json!("unknown_notification_param"),
        );
        let response = router.handle_invocation(request).await;
        assert!(
            response.is_none(),
            "Unknown notification should not produce a response"
        );

        // Get the final state of invocations
        let invocations = test_state.invocations.read().unwrap();

        // Verify the invocation log matches expectations
        assert_eq!(invocations.len(), 4);
        assert_eq!(invocations[0], InvocationType::Method("test_method".to_string()));
        assert_eq!(
            invocations[1],
            InvocationType::Notification("test_method".to_string())
        );
        assert_eq!(
            invocations[2],
            InvocationType::FallbackMethod("unknown_method".to_string())
        );
        assert_eq!(
            invocations[3],
            InvocationType::FallbackNotification("unknown_notification".to_string())
        );
    }

    /// If the `handle_*` method in `Handler` itself panics, before returning a future, that should
    /// be caught and handled
    #[tokio::test]
    async fn handler_closure_panics() {
        testing::init_test_logging();

        #[derive(Clone)]
        struct NaughtyHandler;

        impl Handler<(), ()> for NaughtyHandler {
            type MethodFuture = futures::future::Ready<types::ResponsePayload>;
            type NotificationFuture = futures::future::Ready<()>;

            fn handle_method(self, _state: (), _request: handler::InvocationRequest) -> Self::MethodFuture {
                tracing::debug!("I am about to panic");
                panic!("I am a naughty method");
            }

            fn handle_notification(
                self,
                _state: (),
                _request: handler::InvocationRequest,
            ) -> Self::NotificationFuture {
                tracing::debug!("I am about to panic");
                panic!("I am a naughty notification");
            }
        }

        handler_panic_test_impl(NaughtyHandler).await
    }

    /// If the `handle_*` method in `Handler` itself returns a future, but that future panics when
    /// polled, that should be caught and handled
    #[tokio::test]
    async fn handler_future_panics() {
        testing::init_test_logging();

        #[derive(Clone)]
        struct NaughtyHandler;

        impl Handler<(), ()> for NaughtyHandler {
            type MethodFuture = Pin<Box<dyn Future<Output = types::ResponsePayload> + Send>>;
            type NotificationFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

            fn handle_method(self, _state: (), _request: handler::InvocationRequest) -> Self::MethodFuture {
                tracing::debug!("I am about to panic");
                async move { panic!("I am a naughty method") }.boxed()
            }

            fn handle_notification(
                self,
                _state: (),
                _request: handler::InvocationRequest,
            ) -> Self::NotificationFuture {
                tracing::debug!("I am about to panic");
                async move { panic!("I am a naughty notification") }.boxed()
            }
        }

        handler_panic_test_impl(NaughtyHandler).await
    }

    /// Actual body of the tests that make sure the router handles panics in handlers.
    /// The panics can come from different places so multiple Handler impls are needed, but the
    /// assertions and test setup is otherwise the same
    async fn handler_panic_test_impl(naughty_handler: impl Handler<(), ()>) {
        let mut router = Router::new_stateless();
        router.register_handler("naughty_method", naughty_handler.clone());
        router.register_handler("naughty_notification", naughty_handler);

        // Invoke the method; the result should be an error response and not propagation of the
        // panic
        let request = handler::InvocationRequest::new_test_request(
            Some(types::Id::Number(1)),
            "naughty_method",
            json!("Hola!"),
        );
        let response = router.handle_invocation(request).await;
        // The method, being a method, should have an error response
        match response {
            Some(types::Response {
                id: types::Id::Number(n),
                payload:
                    types::ResponsePayload::Error(types::ErrorResponse {
                        error: types::ErrorDetails { code, .. },
                    }),
                ..
            }) => {
                assert_eq!(n, 1);
                assert_eq!(code, types::ErrorCode::InternalError);
            }
            _ => panic!("Response didn't match expected pattern"),
        }

        // Invoke the notification; there is no response expected but there should also be no panic
        let request =
            handler::InvocationRequest::new_test_request(None, "naughty_notification", json!("Hola!"));
        let response = router.handle_invocation(request).await;
        assert!(response.is_none());
    }
}
