use mcpox_core::models;
use mcpox_jsonrpc as jsonrpc;
use thiserror::Error;

pub type Result<T, E = McpClientError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("Error spawning MCP server '{program}' with args [{args}] as child process",
        args = .args.join(" ")
    )]
    SpawnServer {
        source: std::io::Error,
        program: String,
        args: Vec<String>,
    },
    #[error(transparent)]
    JsonRpcBind { source: mcpox_jsonrpc::JsonRpcError },

    #[error("Error invoking '{method}' on MCP server")]
    JsonRpcCall {
        method: jsonrpc::Method,
        source: mcpox_jsonrpc::JsonRpcError,
    },

    #[error("The MCP server speaks MCP protocol version '{version}' but this client only supports the following protocol versions: {}",
        models::ProtocolVersion::supported_versions()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )]
    UnsupportedProtoVersion { version: String },
}

impl McpClientError {
    /// Report a JSON RPC error associated with a specific method.
    ///
    /// Over time this might get smarter and raise different errors based on what specificially is
    /// in the error
    pub(crate) fn json_rpc_call_error(
        method: impl Into<jsonrpc::Method>,
        source: jsonrpc::JsonRpcError,
    ) -> Self {
        McpClientError::JsonRpcCall {
            method: method.into(),
            source,
        }
    }
}
