//! Client-specific transports for speaking MCP to remote servers.
//!
//! All of these provide an implementation of [`mcpox_jsonrpc::transport::Transport`], to connect
//! our lower-level JSON-RPC implementation.

mod child_process;

pub use child_process::*;
