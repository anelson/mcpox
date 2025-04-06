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

/// Re-export the exact async-trait macro we use, for compatibility
#[doc(hidden)]
pub use async_trait::async_trait;

mod error;
mod handler;
mod router;
mod transport;
mod types;

pub use error::*;
pub use handler::*;
pub use router::*;
pub use transport::*;
pub use types::*;

/// A request to invoke a method or fire a notification from the remote peer.
///
/// Somewhat confusingly, the JSON RPC spec calls the message that requests to invoke a method
/// "request", and the message that fires a notification "notification".  In this implementation,
/// both of those are handled very similarly, and in fact the only difference is that a "request"
/// must have an ID (it's the presence of the ID that makes it a request in fact), and a
/// notification does not produce any response message while a request should get a response.
///
/// This type represents both kinds of messages, and the [`Handler`] trait is responsible for
/// handling it.
pub struct InvocationRequest {
    id: Option<types::Id>,
    method: String,

    /// Optional parameters from the caller.
    ///
    /// The JSON RPC spec is clear that this can be omitted when there are no parameters to be
    /// passed.
    params: Option<JsonValue>,
}
