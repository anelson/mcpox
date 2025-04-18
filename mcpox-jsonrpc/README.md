# mcpox-jsonrpc

A focused JSON-RPC 2.0 implementation for the MCP (Machine Control Protocol) project.

## Overview

`mcpox-jsonrpc` provides a robust implementation of the [JSON-RPC 2.0 specification](https://www.jsonrpc.org/specification), designed specifically for the needs of the MCP project. While not intended as a general-purpose JSON-RPC library, it offers a clean, modular architecture that could potentially serve as the foundation for one.

This crate focuses on:

- Full compliance with the JSON-RPC 2.0 specification
- Bi-directional communication (both sides can send requests)
- Transport-agnostic design (works over any communication channel)
- Type-safe request and response handling
- Async/await support throughout

## Key Features

- **Transport Abstraction**: Implement the `Transport` trait to use any communication protocol
- **Bi-directional Communication**: Both client and server can send requests to each other
- **Type-safe Handlers**: Automatic parameter extraction and type conversion
- **Builder Pattern**: Simple, fluent API for configuration
- **Cancellation Support**: Proper handling of shutdown and cancellation scenarios
- **Batch Support**: Efficient handling of batched requests and responses

## Basic Usage

This crate provides a JSON-RPC implementation with client and server components. Clients can make calls to servers, and both can handle incoming requests. For complete and working examples, see the test modules in the source code.

Key components:

- `Client`: Connects to a server and sends requests
- `Server`: Listens for connections and processes requests
- `Router`: Routes incoming methods to appropriate handlers
- `Transport`: Abstraction for the underlying transport mechanism

Handlers are registered using a simple pattern that automatically extracts parameters and converts return values to the appropriate JSON-RPC format.

## Transport Implementation

To use this crate with your own transport mechanism, implement the `Transport` trait, which requires methods for sending and receiving messages.

The transport layer is intentionally minimal, allowing you to implement it for any protocol (HTTP, WebSockets, Unix sockets, stdio, etc.) as needed for your specific use case.

## Peer-to-Peer Communication

A key feature of this implementation is the ability for both sides of a connection to send requests to each other, reflecting the peer-to-peer nature of JSON-RPC. This is especially important in the MCP protocol where:

- A "client" doesn't just make calls; it also implements handlers for methods the server can call
- A "server" doesn't just respond to calls; it can also initiate method calls to the client

## Architecture

For a detailed overview of the architecture, see [ARCHITECTURE.md](./ARCHITECTURE.md).

## Acknowledgments

This library is inspired by the [jsonrpsee](https://github.com/paritytech/jsonrpsee) project, which offers a more general-purpose JSON-RPC framework in Rust.