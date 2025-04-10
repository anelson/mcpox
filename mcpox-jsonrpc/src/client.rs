use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{Result, handler, router, service, transport};

pub struct ClientBuilder<Stage> {
    stage: Stage,
}

impl Default for ClientBuilder<Stage1> {
    fn default() -> Self {
        Self { stage: Stage1 }
    }
}

impl ClientBuilder<Stage1> {
    pub fn with_state<S: Clone + Send + Sync + 'static>(self, state: S) -> ClientBuilder<Stage2<S>> {
        ClientBuilder {
            stage: Stage2 {
                router: router::Router::new_with_state(state),
            },
        }
    }

    pub fn without_state(self) -> ClientBuilder<Stage2<()>> {
        ClientBuilder {
            stage: Stage2 {
                router: router::Router::new_stateless(),
            },
        }
    }

    pub fn with_router<S: Clone + Send + Sync + 'static>(
        self,
        router: router::Router<S>,
    ) -> ClientBuilder<Stage2<S>> {
        ClientBuilder {
            stage: Stage2 { router },
        }
    }
}

impl<S: Clone + Send + Sync + 'static> ClientBuilder<Stage2<S>> {
    pub fn with_fallback_handler<H, HackT>(mut self, handler: H) -> Self
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        self.stage.router.register_fallback_handler(handler);
        self
    }

    pub fn with_handler<H, HackT>(mut self, method: impl Into<String>, handler: H) -> Self
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        self.stage.router.register_handler(method, handler);
        self
    }

    pub fn bind(self, transport: impl transport::Transport) -> Client<S> {
        Client::new(transport, self.stage.router)
    }
}

#[doc(hidden)]
pub struct Stage1;

#[doc(hidden)]
pub struct Stage2<S: Clone + Send + Sync + 'static> {
    router: router::Router<S>,
}

pub struct Client<S = ()> {
    state: S,
    connection_handle: service::ServiceConnectionHandle,
}

impl<S: Clone> Clone for Client<S> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            connection_handle: self.connection_handle.clone(),
        }
    }
}

impl Client {
    pub fn builder() -> ClientBuilder<Stage1> {
        ClientBuilder::default()
    }
}

impl<S: Clone + Send + Sync + 'static> Client<S> {
    fn new(transport: impl transport::Transport, router: router::Router<S>) -> Self {
        let state = router.state().clone();
        let service = service::Service::new(router);

        let handle = service.service_connection(transport::Peer::new(transport));
        Client {
            state,
            connection_handle: handle,
        }
    }

    pub async fn call_method<Req, Resp>(&self, method: &str, params: Req) -> Result<Resp>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
    {
        self.connection_handle.call_method(method, params).await
    }

    pub async fn raise_notification<Req, Resp>(&self, method: &str, params: Req) -> Result<()>
    where
        Req: Serialize,
    {
        self.connection_handle.raise_notification(method, params).await
    }
}
