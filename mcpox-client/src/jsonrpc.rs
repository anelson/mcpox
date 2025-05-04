//! Low-level implementation of an MCP protocol client layered on top of the JSON-RPC client.
//!
//! This is intended to be a raw mapping of the client part of MCP into Rust on top of our
//! [`mcpox_jsonrpc::Client`] type.  It isn't going to be very ergonomic to use from Rust, which is
//! why it's an internal implementation upon which a more ergonomic and idiomatic Rust layer will
//! be built.

use std::sync::{Arc, RwLock};

use crate::{McpClientError, Result};
use mcpox_jsonrpc::{self as jsonrpc, ErrorDetails, Params, State};
use serde_json::Value as JsonValue;

/// Re-export the MCP protocol types from here, as some of these will leak into the public API and
/// we don't want callers to have to take a dependency on the core crate separately
pub(crate) use mcpox_core::models::*;

#[derive(Debug, Clone)]
pub struct JsonRpcClient {
    /// The underlying JSON-RPC client connected to the remote MCP server
    client: jsonrpc::Client<ClientStateArc>,

    /// Cached init result which has some useful information about the server
    initialize_result: Arc<InitializeResult>,

    /// The MCP protocol version that is negotiated with the server
    mcp_version: ProtocolVersion,
}

impl JsonRpcClient {
    /// Connect to a remote MCP server and perform the initialization handshake.
    ///
    /// Note that this does *not* raise the `initialized` notification to the server, because the
    /// higher-level client should do whatever initializattion it needs to do before we report to
    /// the server.  Therefore, to remain compliant with the MCP spec, make sure the higher-level
    /// MCP client type explicitly raises `initialized` at the appropriate time
    pub async fn initialize(
        transport: impl jsonrpc::Transport,
        capabilities: ClientCapabilities,
        client_info: Implementation,
    ) -> Result<Self> {
        let state = Arc::new(RwLock::new(ClientState {}));
        let mut builder = jsonrpc::Client::builder().with_state(state);

        // Register notification and method handlers for the client.
        //
        // As the client, we have to implement client methods, but we have to handle server
        // notifications
        //
        // Using an enum this way avoid excessively stringly-typed code and also ensures we don't
        // miss any handlers.
        for method in ClientMethod::iter() {
            builder = match method {
                ClientMethod::Ping => builder.with_handler(method, ClientState::ping),
                ClientMethod::CreateMessage => builder.with_handler(method, ClientState::create_message),
                ClientMethod::ListRoots => builder.with_handler(method, ClientState::list_roots),
            }
        }

        for notification in ServerNotification::iter() {
            builder = match notification {
                ServerNotification::Cancelled => {
                    builder.with_handler(notification, ClientState::on_cancelled)
                }
                ServerNotification::Progress => builder.with_handler(notification, ClientState::on_progress),
                ServerNotification::LoggingMessage => {
                    builder.with_handler(notification, ClientState::on_logging_message)
                }
                ServerNotification::ResourceUpdated => {
                    builder.with_handler(notification, ClientState::on_resource_updated)
                }
                ServerNotification::ResourceListChanged => {
                    builder.with_handler(notification, ClientState::on_resource_list_changed)
                }
                ServerNotification::ToolListChanged => {
                    builder.with_handler(notification, ClientState::on_tool_list_changed)
                }
                ServerNotification::PromptListChanged => {
                    builder.with_handler(notification, ClientState::on_prompt_list_changed)
                }
            }
        }

        let client = builder
            .bind(transport)
            .map_err(|e| McpClientError::JsonRpcBind { source: e })?;

        // Call initialize on the server
        let initialize_result: InitializeResult = client
            .call_with_params(
                ServerMethod::Initialize,
                InitializeRequest {
                    capabilities,
                    client_info,
                    protocol_version: ProtocolVersion::LATEST.to_string(),
                },
            )
            .await
            .map_err(|e| McpClientError::json_rpc_call_error(ServerMethod::Initialize, e))?;

        // Validate that the server version is one we support.  The spec says that if the client
        // gets a version back from the server that it doesn't support, it should disconnect
        let remote_protocol_version = ProtocolVersion::try_from_str(&initialize_result.protocol_version)
            .ok_or_else(|| McpClientError::UnsupportedProtoVersion {
                version: initialize_result.protocol_version.clone(),
            })?;

        Ok(Self {
            client,
            initialize_result: Arc::new(initialize_result),
            mcp_version: remote_protocol_version,
        })
    }

    /// Get the capabilities that the server reported at init time.
    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.initialize_result.capabilities
    }
}

/// Define a method for a server method with proper Rust naming conventions
macro_rules! server_method {
    ($rust_method:ident, $server_method:ident, $request_type:ty, $result_type:ty) => {
        pub async fn $rust_method(&self, request: $request_type) -> Result<$result_type> {
            self.client
                .call_with_params(ServerMethod::$server_method, request)
                .await
                .map_err(|e| McpClientError::json_rpc_call_error(ServerMethod::$server_method, e))
        }
    };
}

/// Define a method that raises a client notification to the server
macro_rules! client_notification {
    ($rust_method:ident, $client_notification:ident, $notification_type:ty) => {
        pub async fn $rust_method(&self, request: $notification_type) -> Result<()> {
            self.client
                .raise_with_params(ClientNotification::$client_notification, request)
                .await
                .map_err(|e| McpClientError::json_rpc_call_error(ClientNotification::$client_notification, e))
        }
    };
}

// Define methods for all server methods that should be present according to the MCP spec,
// and functions to raise each of the notifications that clients can raise under the spec
impl JsonRpcClient {
    server_method!(list_tools, ListTools, ListToolsRequest, ListToolsResult);
    server_method!(call_tool, CallTool, CallToolRequest, CallToolResult);
    server_method!(
        list_resources,
        ListResources,
        ListResourcesRequest,
        ListResourcesResult
    );
    server_method!(
        list_resource_templates,
        ListResourceTemplates,
        ListResourceTemplatesRequest,
        ListResourceTemplatesResult
    );
    server_method!(
        read_resource,
        ReadResource,
        ReadResourceRequest,
        ReadResourceResult
    );
    server_method!(subscribe, Subscribe, SubscribeRequest, ());
    server_method!(unsubscribe, Unsubscribe, UnsubscribeRequest, ());
    server_method!(get_prompt, GetPrompt, GetPromptRequest, GetPromptResult);
    server_method!(list_prompts, ListPrompts, ListPromptsRequest, ListPromptsResult);
    server_method!(set_level, SetLevel, SetLevelRequest, ());
    server_method!(complete, Complete, CompleteRequest, CompleteResult);

    client_notification!(cancelled, Cancelled, CancelledNotification);
    client_notification!(progress, Progress, ProgressNotification);
    client_notification!(roots_list_changed, RootsListChanged, RootsListChangedNotification);
}

/// Shared state used by all clones of the JsonRpcClient and also by the handlers for method calls
/// and notifications
#[derive(Debug)]
struct ClientState {}

type ClientStateArc = Arc<RwLock<ClientState>>;

/// The handlers for methods, and notifications, are implemented on the shared state type since
/// that's cleaner than implementing them on the client type itself because of how awkward it is
/// since handlers can't take `self`.
impl ClientState {
    async fn ping(State(_): State<ClientStateArc>) {
        // Do nothing; the empty response will suffice
    }

    async fn create_message(
        State(_): State<ClientStateArc>,
        Params(params): Params<CreateMessageRequest>,
    ) -> Result<CreateMessageResult, ErrorDetails> {
        todo!()
    }

    async fn list_roots(
        State(_): State<ClientStateArc>,
        Params(params): Params<ListRootsRequest>,
    ) -> Result<ListRootsResult, ErrorDetails> {
        todo!()
    }

    async fn on_cancelled(State(_): State<ClientStateArc>, Params(params): Params<CancelledNotification>) {
        todo!()
    }
    async fn on_progress(State(_): State<ClientStateArc>, Params(params): Params<ProgressNotification>) {
        todo!()
    }
    async fn on_logging_message(
        State(_): State<ClientStateArc>,
        Params(params): Params<LoggingMessageNotification>,
    ) {
        todo!()
    }
    async fn on_resource_updated(
        State(_): State<ClientStateArc>,
        Params(params): Params<ResourceUpdatedNotification>,
    ) {
        todo!()
    }
    async fn on_resource_list_changed(
        State(_): State<ClientStateArc>,
        Params(params): Params<ResourceListChangedNotification>,
    ) {
        todo!()
    }
    async fn on_tool_list_changed(
        State(_): State<ClientStateArc>,
        Params(params): Params<ToolListChangedNotification>,
    ) {
        todo!()
    }
    async fn on_prompt_list_changed(
        State(_): State<ClientStateArc>,
        Params(params): Params<PromptListChangedNotification>,
    ) {
        todo!()
    }
}
