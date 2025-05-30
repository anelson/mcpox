//! The service and related types that form the beating heart of both JSON RPC clients and servers.
//!
//! See the [`Service`] struct for more details and usage.
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::{CancellationToken, DropGuard};

use crate::Result;
use crate::{router, transport};

mod connection;
mod connection_handle;

pub use connection::EventLoop;
pub use connection_handle::{BatchBuilder, RequestHandle, ServiceConnectionHandle};

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    /// The maximum number of concurrent notifications and method calls requests that can be
    /// processed by the service at any given time.
    ///
    /// This is used to limit the number of concurrent requests that can be processed by the
    /// service.  If this limit is reached, new requests will be rejected with an error.
    ///
    /// `None` indicates no limit
    ///
    /// TODO: Implement this
    pub max_concurrent_requests: Option<usize>,

    /// The maximum amount of time to allow a method call or notification to execute before
    /// considering them to be timed out, aborting the async task, and responding back to the
    /// caller with an error.
    ///
    /// `None` indicates no limit.
    ///
    /// TODO: Implement this
    pub inbound_request_timeout: Option<Duration>,

    /// The amount of time after a cancellation token or connection closure causes a connection to
    /// shutdown, to wait for any in-flight requests to complete before aborting them.
    ///
    /// `None` indicates no limit, which is probably not a good idea.
    pub graceful_connection_shutdown_timeout: Option<Duration>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            max_concurrent_requests: None,
            inbound_request_timeout: None,
            graceful_connection_shutdown_timeout: Some(Duration::from_secs(5)),
        }
    }
}

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
    config: ServiceConfig,

    router: router::Router<S>,

    custom_event_loop: Option<Arc<dyn connection::EventLoop<S>>>,

    /// Signal to abort and exit the loop
    cancellation_token: CancellationToken,

    /// Guard that will cancel all connections when the service itself is dropped.
    ///
    /// This is not set unless the [`Self::new`] constructor is used.  Otherwise cancelation is
    /// entirely under the control of the caller's cancellation token.
    #[allow(dead_code)] // This is a drop guard, it's not meant to be used it's just for detecting drop
    drop_guard: Option<Arc<DropGuard>>,
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
            config: ServiceConfig::default(),
            router,
            custom_event_loop: None,
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
            config: ServiceConfig::default(),
            router,
            custom_event_loop: None,
            cancellation_token,
            drop_guard: None,
        }
    }

    /// Use this custom event loop as part of the connection-specific event loop, to allow custom
    /// logic to participate in the event loop.
    pub fn with_custom_event_loop(mut self, custom_event_loop: impl connection::EventLoop<S>) -> Self {
        self.custom_event_loop = Some(Arc::new(custom_event_loop));
        self
    }

    /// Set configuration options for this service which will apply to all service connections in
    /// the future.
    pub fn with_config(mut self, config: ServiceConfig) -> Self {
        self.config = config;
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
    /// This will create the event loop future that will run for the life of the peer connection,
    /// constantly polling the peer, optionally polling the custom event loop if one was
    /// provided, and periodically performing housekeeping tasks.
    ///
    /// NOTE: The caller is responsible for polling the returned future to ensure that the
    /// connection is serviced in a timely fashion.  In most cases this should be spawned into a
    /// new async task using [`tokio::spawn`] or something equivalent.
    pub(crate) fn service_connection(
        &self,
        peer: transport::Peer,
    ) -> Result<(
        connection::ServiceConnectionEventLoopFuture,
        connection_handle::ServiceConnectionHandle,
    )> {
        // Each connection gets its own child cancellation token, that can be signaled separately,
        // but is also signaled whenever the service-level cancellation token is signaled.
        let cancellation_token = self.cancellation_token.child_token();

        connection::service_connection(
            self.config.clone(),
            self.router.clone(),
            self.custom_event_loop.clone(),
            cancellation_token,
            peer,
        )
    }
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use crate::{handler, types};
    use serde_json::{Value as JsonValue, json};

    // Construct a dummy service with a single method "echo" and a single notification "hi"
    pub(super) fn make_test_service() -> Service<()> {
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

        // A regular sleep handler that doesn't respond to cancellation
        async fn sleep_handler(
            handler::Params(params): handler::Params<JsonValue>,
        ) -> Result<(), types::ErrorDetails> {
            let seconds = params.get("seconds").and_then(|s| s.as_u64()).unwrap_or(1);

            tracing::debug!(seconds, "Sleep method called");
            let duration = Duration::from_secs(seconds);
            tokio::time::sleep(duration).await;
            tracing::debug!("Sleep completed normally");

            Ok(())
        }

        // A cancellable sleep handler that checks for cancellation
        async fn cancellable_sleep_handler(
            handler::Params(params): handler::Params<JsonValue>,
            handler::RequestCancellationToken(token): handler::RequestCancellationToken,
        ) -> Result<(), types::ErrorDetails> {
            let seconds = params.get("seconds").and_then(|s| s.as_u64()).unwrap_or(1);

            tracing::debug!(seconds, "Cancellable sleep method called");
            let duration = Duration::from_secs(seconds);

            let sleep_future = tokio::time::sleep(duration);

            tokio::select! {
                _ = sleep_future => {
                    tracing::debug!("Cancellable sleep completed normally");
                    Ok(())
                }
                _ = token.cancelled() => {
                    tracing::debug!("Cancellable sleep was cancelled");
                    Err(types::ErrorDetails::server_error(
                        types::ErrorDetails::SERVER_ERROR_CODE_MIN + 1,
                        "Cancellable sleep was cancelled by client request",
                        Some(json!({ "cancelled_after_seconds": seconds })),
                    ))
                }
            }
        }

        let mut router = router::Router::new_stateless();
        router.register_handler("echo", echo_handler);
        router.register_handler("hi", hi_handler);
        router.register_handler("slow", slow_handler);
        router.register_handler("sleep", sleep_handler);
        router.register_handler("cancellable_sleep", cancellable_sleep_handler);

        Service::new(router)
    }
}
