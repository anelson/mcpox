//! In this crate, the concept of a "transport" abstracts away the details of how JSON-RPC messages are
//! sent and received.  In the MCP case specifically, the [specification on transports](https://spec.modelcontextprotocol.io/specification/2025-03-26/basic/transports/)
//! describes two, one using stdio and one using a streamable HTTP connection.
//!
//! Abstracting away two such different transports makes for some awkward abstractions, but it's a
//! necessary complexity.
use crate::Result;
use async_trait::async_trait;

/// Anything that can be made into a transport can implement this trait to control the
/// creation of the transport itself.
///
/// Out of the box, this is implemented on some tokio and async types where it makes sense.
#[async_trait]
pub trait IntoTransport {
    type Transport: Transport;
    type Error: std::error::Error;

    fn try_into_transport(self) -> Result<Self::Transport, Self::Error>;
}

/// A transport that can send and receive JSON-RPC messages over some medium.
#[async_trait]
pub trait Transport {}

pub trait Server {
    fn serve() -> Result<()>;
    // fn serve_abortable(ct: CancellationToken) -> Result<()>;
}
