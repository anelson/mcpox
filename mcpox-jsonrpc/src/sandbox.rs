//! A Rust implementation of the JSON-RPC 2.0 protocol, specific to `mcpox`.
//!
//! This implementation isn't intended as a general-purpose JSON RPC library, but rather is
//! purpose-built for use with the MCP implementation elsewhere in this repo.  That said, anything
//! MCP-specific resides in other crates at a higher level of abstraction, so in a pinch this crate
//! could form the basis for a more general JSON-RPC library.
//!
//! This library owes a debt to the [`jsonrpsee`](https://github.com/paritytech/jsonrpsee) project,
//! which is a more general-purpose JSON-RPC framework in Rust and is probably what you should use
//! if you want to talk to JSON-RPC from Rust.

use std::collections::VecDeque;

/// Re-export the exact async-trait macro we use, for compatibility
#[doc(hidden)]
pub use async_trait::async_trait;

mod error;
mod transport;
mod types;

pub use error::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
pub use transport::*;
pub use types::*;

/// A remote peer that speaks the JSON-RPC protocol over some transport
///
/// Must be cheap to clone and work properly when multiple threads send and receive messages.
///
/// This might be all we need for a "transport" at the JSON level, if we can assume that the MCP
/// level will handle the particulars of getting an HTTP request.
#[async_trait]
pub trait Peer: Send + Sync + Clone + 'static {
    async fn send_message(&self, message: Message) -> Result<()>;
    async fn receive_msg(&self) -> Result<Message>;
}

use serde_json::Value;

/// A custom event loop that optionally attends to other event sources besides the peer, and
/// dispatches events when that's necessary
#[async_trait]
pub trait EventLoop<S>: Send + 'static {
    /// Run a single iteration of the custom event loop, then return.
    ///
    /// This should do quick things like pooling event queues, checking for cancellation, etc.
    ///
    /// A successful result means that the event loop keeps running.  A failure result terminates
    /// the session
    async fn run(&self, state: &S, peer: &Box<dyn Peer>) -> Result<(), Box<dyn std::error::Error + Sync + Send + 'static>>;
}


// Implement the method handler on various convenient types

/// Thing that both server and client need, that monitors a connection for messages and dispatches
/// them to the approriate handler/channel
struct JsonRpcThingamajig<S> {
    peer: Box<dyn Peer>,

    router: Router<S>,

    /// Requests that have been sent, keyed by the request ID that was passed to the remote peer.
    /// Responses will come in with this ID specified.
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>,

    custom_event_loop: Option<Box<dyn EventLoop<S>>>,

    /// Signal to abort and exit the loop
    cancellation_token: CancellationToken
}

impl JsonRpcThingamajig {
    fn new() -> Self {
        let (broadcast_tx, broadcast_rx) = broadcast::channel(16);
        Self {
            peer: Box::new(DummyPeer),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            broadcast_rx,
            cancellation_token: CancellationToken::new(),
        }
    }

    pub fn start(self) -> JsonRpcThingamajigHandle {
        let thing = self;

        let handle = JsonRpcThingamajigHandle {
            pending_requests: thing.pending_requests.clone(),
            cancellation_token: thing.cancellation_token.clone(),
            broadcast_rx: thing.broadcast_rx.clone(),
        };

        tokio::spawn(async move {
            if let Err(e) = thing.event_loop().await {
                log::error!("Error in JSON RPC event loop: {}", e);
            }
        });

        handle
    }

    fn event_loop(&self) -> Result<()> {
        loop {
            tokio::select! {
                result = self.peer.receive_msg().await => {
                    match result {
                        Ok(message) => {
                            match message {
                                Message::Request { id, method, params } => {
                                    let context = Context::new();
                                    let result = self.handle_method(&context, method, params);
                                    let response = Message::new_response(id, result);
                                    self.peer.send_message(response).await?;
                                }
                                Message::Notification { method, params } => {
                                    let context = Context::new();
                                    self.handle_method(&context, method, params);
                                }
                                Message::Response { id, result } => {
                                    if let Some(tx) = self.pending_requests.lock().remove(&id) {
                                        tx.send(result).unwrap();
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            log::error!("Error receiving message: {}", e);
                            return Err(e);
                        }
                    }
                },
                result = self.custom_event_loop.run(&self.router.state(), &self.peer) => {
                    match result {
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("Error in custom event loop: {}", e);
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
}

struct JsonRpcThingamajigHandle {
    peer: Box<dyn Peer>,
    pending_requests: Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>,
    cancellation_token: CancellationToken,
    broadcast_rx: broadcast::Receiver<Value>,
}

impl JconRpcThingamajigHandle {
    /// Send a request to invoke a method, expecting a response.
    ///
    /// TODO: helpers to handle serde automatically
    async fn send_request(&self, method: &str, params: Params) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        let request_id = Uuid::new_v4().to_string();

        let message = Message::new_request(request_id, method, params);
        self.pending_requests.lock().insert(request_id, tx);

        // This function isn't itself async so need to be careful about how we approach this.
        // Return a future that will resolve when the response is received.
        self.peer.send_message(message).await?;
        let result = rx.await;
        match result {
            Ok(value) => Ok(value),
            Err(_) => Err(JsonRpcError::RequestCancelled),
        }
    }

    /// Send a notification, expecting no response.
    ///
    /// TODO: helpers for serialization
    fn notify(&self, method: &str, params: Params) -> Result<()> {
        // Skip the registration of a pending request since this is just a notification
        let message = Message::new_notification(method, params);

        self.peer.send_message(message).await?;
    }
}

/// Trait implemented for servers and clients both.
trait JsonRpcService {
    fn handle_method(&self, context: &Context, method: &str, params: Params) -> Result<Value>;
}

/// The actual JSON RPC server impl.
///
/// A single server will service many clients, so when server code wants to call
/// methods/notifications on a client, it will only happen when handling an event related to a
/// specific client.
/// Also servers have the concept of broadcast events that can be sent to all connection handlers.
/// It's not clear how that will work.  Is there some custom app-specific event loop for a given
/// connection that would let us poll a broadcast queue for broadcast events as well as polling the
/// transport?
/// Conceptually that's not even server-specific; the ability of the server to invoke
/// methods/notifications on the client means that a client must have some event loop also.
struct JsonRpcServer<S: JsonRpcService, T: Transport> {
    service: S,
    transport: T,
}

/// The actual client
///
/// Clients differ from servers in that:
/// - An instance of the client is always bound to a specific remote peer over some transport.
/// - Methods/notifications are sent using the client.  On the server, it's possible to send method
/// calls/invocations to the client, but that is not at the server level since servers can service
/// many clients.
struct JsonRpcClient<S: JsonRpcService, T: Transport> {
    service: S,
    transport: T,
}

impl JsonRpcClient {
    async fn call_method(&self, method: &str, params: Params) -> Result<Value> {
        let message = Message::new_request(method, params);
        self.transport.send_message(message).await?;

        let response = self.transport.receive_msg().await?;
        Ok(response.result)
    }
}

/// Context associated with the JSON RPC service.
///
/// Both clients and servers are JRSON RPC services, although clients could not implement any
/// methods or notifications and still be useful, while for servers that would be pointless.
///
/// The service context provides a place for shared state to be stored.
struct ServiceContext {}

impl ServiceContext {
    fn state(&self) -> &ServiceState {
        unimplemented!()
    }
}

/// State.  This would actually be a dedicated type, and use some trickery with `Any` to make this
/// a type-safe store of state information
type ServiceState = Arc<RwLock<HashMap<String, String>>>;
type ConnectionState = Arc<RwLock<HashMap<String, String>>>;
type MessageState = Arc<RwLock<HashMap<String, String>>>;

/// Context specific to a logical connection to a peer.
///
/// For HTTP this survives for the duration of the HTTP connection.
/// For stdio, there's always exactly one connection, and this context survives for the duration of
/// the server process that is connected to it.
struct ConnectionContext {}

impl ConnectionContext {
    /// The remote peer that this context is associated with
    fn peer(&self) -> &dyn Peer {
        unimplemented!()
    }

    fn state(&self) -> &ConnectionState {
        unimplemented!()
    }

    fn service_context(&self) -> &ServiceContext {
        unimplemented!()
    }
}

/// Context associated with a single JSON RPC message
struct MessageContext {}

impl MessageContext {
    fn peer(&self) -> &dyn Peer {
        unimplemented!()
    }

    fn state(&self) -> &MessageState {
        unimplemented!()
    }

    fn connection_context(&self) -> &ConnectionContext {
        unimplemented!()
    }

    fn service_context(&self) -> &ServiceContext {
        unimplemented!()
    }
}

/// Mock up of what the MCP server and client impls might look like

struct McpServer {
    inner: JrpcServer,
    sessions: HashMap<String, McpSession>,

}

struct Handle {
    inner: JrpcServerHandle;
}

impl McpServer {
    fn new() -> Self {
        let server = JrpcServer::new();

        server.add_method("mcp.hello", |params| {
            Ok(Value::String("Hello, world!".to_string()))
        });
    }
    /// Start the MCP server, returning a handle that can be used to stop it
    fn start(&self) -> Result<Handle> {
        Ok(Handle { inner: self.inner.start()? })
    }

    fn handle_connection(&self, client: (), peer: Box<dyn Peer>) -> Result<McpServerConnection> {

    }
}

struct McpServerConnection {
    inner: JrpcServerConnection,
    client: (),
    pending_responses: VecDeque<oneshot::Receiver<Result<Message>>>,
}

impl McpServerConnection {
    fn handle_message(&self, message: Message) -> Result<()> {
        if let Some(rx) = self.inner.handle_message(message)? {
            // This message can expect a response
            self.pending_responses.insert(id, rx);
        }

        Ok(())
    }


    /// Poll the pending requests for any that have completed.
    ///
    /// Return None if the connection is closed.
    /// Return Some(Err) if there was an error, including an abort error if the cancellation token
    /// is signaled
    fn await_message(&self, ct: CancellationToken) -> Result<Option<Message>> {
        self.inner.await_message()
    }
}


