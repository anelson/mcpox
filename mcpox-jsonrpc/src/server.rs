use crate::{Result, handler, router, service, transport};

pub struct ServerBuilder<Stage> {
    stage: Stage,
}

impl Default for ServerBuilder<Stage1> {
    fn default() -> Self {
        Self { stage: Stage1 }
    }
}

impl ServerBuilder<Stage1> {
    pub fn with_state<S: Clone + Send + Sync + 'static>(self, state: S) -> ServerBuilder<Stage2<S>> {
        ServerBuilder {
            stage: Stage2 {
                router: router::Router::new_with_state(state),
            },
        }
    }

    pub fn without_state(self) -> ServerBuilder<Stage2<()>> {
        ServerBuilder {
            stage: Stage2 {
                router: router::Router::new_stateless(),
            },
        }
    }

    pub fn with_router<S: Clone + Send + Sync + 'static>(
        self,
        router: router::Router<S>,
    ) -> ServerBuilder<Stage2<S>> {
        ServerBuilder {
            stage: Stage2 { router },
        }
    }
}

impl<S: Clone + Send + Sync + 'static> ServerBuilder<Stage2<S>> {
    pub fn with_fallback_handler<H, HackT>(mut self, handler: H) -> Self
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        self.stage.router.register_fallback_handler(handler);
        self
    }

    pub fn with_handler<H, HackT>(mut self, method: impl Into<handler::Method>, handler: H) -> Self
    where
        H: handler::Handler<HackT, S> + 'static,
        HackT: Send + Sync + 'static,
    {
        self.stage.router.register_handler(method, handler);
        self
    }

    pub fn build(self) -> Server<S> {
        let router = self.stage.router;
        let service = service::Service::new(router);

        Server::from_service(service)
    }
}

#[doc(hidden)]
pub struct Stage1;

#[doc(hidden)]
pub struct Stage2<S: Clone + Send + Sync + 'static> {
    router: router::Router<S>,
}

#[derive(Clone)]
pub struct Server<S: Clone + Send + Sync + 'static = ()> {
    service: service::Service<S>,
}

impl Server {
    pub fn builder() -> ServerBuilder<Stage1> {
        ServerBuilder::default()
    }
}

impl<S: Clone + Send + Sync + 'static> Server<S> {
    fn from_service(service: service::Service<S>) -> Self {
        Self { service }
    }

    /// The shared state of the server, managed by the internal router and accessible to handlers
    pub fn state(&self) -> &S {
        self.service.state()
    }

    pub fn router(&mut self) -> &mut router::Router<S> {
        self.service.router_mut()
    }

    pub fn serve_connection(
        &self,
        transport: impl transport::Transport,
    ) -> Result<service::ServiceConnectionHandle> {
        let peer = transport::Peer::new(transport);

        let span = tracing::info_span!("server");
        let _guard = span.enter();
        let (future, handle) = self.service.service_connection(peer)?;

        // TODO: In the future perhaps provide more flexbility in how the future gets spawned
        //
        // Servers in particular should probably keep a list of all of their active connections,
        // know then they finish, and provide an orderly shutdown mechanism that waits some
        // reasonable period of time after signalling the global cancellation token before knocking
        // heads.
        tokio::spawn(future);

        Ok(handle)
    }
}
