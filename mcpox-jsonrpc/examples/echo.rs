//! Very simple example in which the JSON-RPC "server" has a method `echo`, which returns in a
//! response any JSON content sent to it by the client.
use mcpox_jsonrpc::{Client, Server};
use serde_json::{Value as JsonValue, json};
use tokio::io::duplex;
use tokio_util::codec::{Framed, LinesCodec};

#[tokio::main]
async fn main() {
    // Create a pair of connected pipes that will serve as the transport between client and server
    let (client, server) = duplex(1024);

    // Create framed transports with a reasonable max size to avoid DoS vulns
    let client_transport = Framed::new(client, LinesCodec::new_with_max_length(1024 * 1024));
    let server_transport = Framed::new(server, LinesCodec::new_with_max_length(1024 * 1024));

    let server = Server::builder()
        .without_state()
        .with_handler("echo", |params: JsonValue| async move { params })
        .build();

    let server_connection = server.serve_connection(server_transport).unwrap();

    let client = Client::builder().bind(client_transport).unwrap();

    let request = json!("Hello, world!");
    let response: JsonValue = client.call_with_params("echo", request.clone()).await.unwrap();
    assert_eq!(response, request);

    let request = json!({ "foo": "bar" });
    let response: JsonValue = client.call_with_params("echo", request.clone()).await.unwrap();
    assert_eq!(response, request);

    let request = json!([1, 2, 3]);
    let response: JsonValue = client.call_with_params("echo", request.clone()).await.unwrap();
    assert_eq!(response, request);

    // Shutdown the server connection
    server_connection.shutdown().await.unwrap();

    // No more requests should be accepted
    let request = json!("Hello, world!");
    assert!(
        client
            .call_with_params::<_, JsonValue>("echo", request.clone())
            .await
            .is_err()
    );
}
