//! After JSON RPC messages have been decoded, those that represent method calls or notifications
//! need to be routed to the corresponding handler, and that handler invoked.  The logic to perform
//! this is called "routing", and is implemented in the [`Router`] type in this module.
use futures::FutureExt;

use crate::{InvocationRequest, handler, types};
use std::borrow::Cow;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// Router inspired loosely by the `axum` crate router, but simplified and specialized for
/// JSON-RPC.
///
/// This router is much simpler than the `axum` router because it only has to route requests based
/// on the JSON-RPC method name, and the requests that it is routing are not HTTP requests but
/// complete, already-decoded JSON RPC requests.
#[derive(Clone)]
pub struct Router<S: Clone + Send + Sync + 'static = ()> {
    state: S,
    handlers: Arc<RwLock<HashMap<String, Box<dyn handler::ErasedHandler<S>>>>>,
}

impl Router {
    /// Create a new router that doesn't need any state.
    pub fn new_stateless() -> Router<()> {
        Router {
            state: (),
            handlers: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl<S: Clone + Send + Sync + 'static> Router<S> {
    /// Create a new router whose handlers can access the provided shared state
    pub fn new_with_state(state: S) -> Self {
        Router {
            state,
            handlers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Return the current state in the router.
    pub fn state(&self) -> &S {
        &self.state
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
        request: InvocationRequest,
    ) -> Pin<Box<dyn Future<Output = Option<types::ResponsePayload>> + Send + 'static>> {
        let handlers = self.handlers.read().unwrap();
        match handlers.get(&request.method) {
            Some(handler) => {
                // Invoke as an method or notification depending on whether the request has an ID
                if request.id.is_some() {
                    handler
                        .handle_method(self.state.clone(), request)
                        .map(|response| Some(response))
                        .boxed()
                } else {
                    handler
                        .handle_notification(self.state.clone(), request)
                        .map(|_response| None)
                        .boxed()
                }
            }
            None => {
                if request.id.is_some() {
                    std::future::ready(Some(types::ResponsePayload::error(
                        types::ErrorDetails::method_not_found(
                            format!("Method {} not found", request.method),
                            None,
                        ),
                    )))
                    .boxed()
                } else {
                    std::future::ready(None).boxed()
                }
            }
        }
    }
}
