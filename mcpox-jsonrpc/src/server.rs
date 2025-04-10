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

    pub fn with_handler<H, HackT>(mut self, method: impl Into<String>, handler: H) -> Self
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

    fn new_stateless() -> Server<()> {
        let router = router::Router::new_stateless();
        let service = service::Service::new(router);

        Self::from_service(service)
    }
}

impl<S: Clone + Send + Sync + 'static> Server<S> {
    fn new(state: S) -> Self {
        let router = router::Router::new_with_state(state);
        let service = service::Service::new(router);

        Self::from_service(service)
    }

    fn from_service(service: service::Service<S>) -> Self {
        Self { service }
    }

    pub fn router(&mut self) -> &mut router::Router<S> {
        self.service.router_mut()
    }

    pub fn serve_connection(self, transport: impl transport::Transport) -> service::ServiceConnectionHandle {
        let peer = transport::Peer::new(transport);
        self.service.service_connection(peer)
    }
}
