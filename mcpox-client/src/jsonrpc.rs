//! Low-level implementation of an MCP protocol client layered on top of the JSON-RPC client.
//!
//! This is intended to be a raw mapping of the client part of MCP into Rust on top of our
//! [`mcpox_jsonrpc::Client`] type.  It isn't going to be very ergonomic to use from Rust, which is
//! why it's an internal implementation upon which a more ergonomic and idiomatic Rust layer will
//! be built.
use std::sync::{Arc, RwLock};

use mcpox_core::models::{
    self, ClientCapabilities, ListResourcesResult, ListToolsRequest, ListToolsResult, ServerCapabilities,
};
use mcpox_jsonrpc::{self as jsonrpc, State};
use serde_json::Value as JsonValue;
use strum::IntoEnumIterator;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct JsonRpcClient {
    client: jsonrpc::Client,
    server_capabilities: ServerCapabilities,
    state: ClientStateArc,
}

impl JsonRpcClient {
    pub async fn initialize(
        transport: impl jsonrpc::Transport,
        capabilities: ClientCapabilities,
    ) -> Result<Self> {
        let state = Arc::new(RwLock::new(ClientState {}));
        let mut builder = jsonrpc::Client::builder().with_state(state);

        // Register notification and method handlers for the client.
        //
        // Using an enum this way avoid excessively stringly-typed code and also ensures we don't
        // miss any handlers.
        for method in models::ClientMethods::iter() {
            let method_name: &'static str = (&method).into();
            match method {
                models::ClientMethods::Ping => {
                    builder = builder.with_handler(method_name, ClientState::ping);
                }
                models::ClientMethods::CreateMessage => todo!(),
                models::ClientMethods::ListRoots => todo!(),
            }
        }

        let client = builder.bind(transport)?;

        // TODO: invoke init method,

        todo!()
    }

    /// Get the capabilities that the server reported at init time.
    pub fn server_capabilities() -> ServerCapabilities {
        todo!()
    }

    /// List all available tools on the remote server right now
    pub async fn list_tools(&self, cursor: Option<String>) -> Result<ListToolsResult> {
        todo!()
    }

    /// Get a handle to the tools list changed event
    pub async fn tools_list_changed_event(&self) -> EventHandle<()> {
        todo!()
    }

    /// Initiate a call to a tool on the remote server.
    ///
    /// Returns as soon as the call is initiated.
    pub async fn call_tool(
        &self,
        tool: &Tool,
        arguments: JsonValue,
        progress_token: impl Into<Option<String>>,
    ) -> Result<ToolCall> {
        todo!()
    }

    /// List all available resources on the remote server right now
    pub async fn list_resources(&self) -> Result<ListResourcesResult> {
        todo!()
    }

    /// Get a handle to the resources list changed event
    pub async fn resources_list_changed_event(&self) -> EventHandle<()> {
        todo!()
    }

    /// Get a handle to an event fired when the specified resource is changed
    pub async fn subscribe_to_resource(&self, resource: &Resource) -> EventHandle<()> {
        todo!()
    }
}

/// Shared state used by all clones of the JsonRpcClient and also by the handlers for method calls
/// and notifications
struct ClientState {}

type ClientStateArc = Arc<RwLock<ClientState>>;

/// The handlers for methods, and notifications, are implemented on the shared state type since
/// that's cleaner than implementing them on the client type itself because of how awkward it is
/// since handlers can't take `self`.
impl ClientState {
    async fn ping(State(me): State<ClientStateArc>, params: JsonValue) -> JsonValue {
        todo!()
    }
}

pub struct Tool;
pub struct Resource;

/// An event fired by the remote peer.
///
/// Provides flexible ways of consuming this event that should be suitable to a variety of
/// application architectures.
pub struct EventHandle<T> {
    _phantom: std::marker::PhantomData<T>,
}

impl<T> EventHandle<T> {
    pub fn new() -> Self {
        EventHandle {
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Implements `Future` for cases where just want to block until the result is ready
pub struct ToolCall;

impl ToolCall {
    // Get a stream that will emit progress reports as they are received.
    //
    // If a progress token was not provided at the time of the tool call then this will fail.
    // pub fn progress(&self) -> Result<impl Stream<Item = ProgressReport>> {
    //     todo!()
    // }
}
