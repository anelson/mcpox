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
#![doc = include_str!("../README.md")]

/// Re-export the exact async-trait macro we use, for compatibility
#[doc(hidden)]
pub use async_trait::async_trait;

mod client;
mod error;
mod handler;
mod router;
mod server;
mod service;
#[cfg(test)]
pub mod testing;
mod transport;
mod typemap;
mod types;

pub use client::{Client, ClientBuilder};
pub use error::{JsonRpcError, Result};
pub use handler::{
    FromRequest, Handler, IntoResponse, MethodResponse, Params, RequestCancellationToken, State,
    TransportMeta,
};
pub use router::Router;
pub use server::{Server, ServerBuilder};
pub use service::{BatchBuilder, EventLoop, RequestHandle, Service, ServiceConfig, ServiceConnectionHandle};
pub use transport::{IntoTransport, Peer, Transport, TransportMessage, TransportMetadata};
pub use typemap::TypeMap;
pub use types::{
    ErrorCode, ErrorDetails, ErrorResponse, Id, InvalidRequest, JsonValue, Message, Method, Notification,
    Request, Response, ResponsePayload, SuccessResponse, TwoPointZero,
};
