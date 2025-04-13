//! Integration tests to exercise the JSON-RPC crate using it's public interface.

/// Fun hack to avoid duplication: the `testing` module in the `jsonrpc` crate is only enabled in
/// `test` config, and when running integration tests like this one the crate is compiled without
/// `test` enabled.  But we need to use the same test helpers, so just include that module in this
/// test program as well.
#[path = "../src/testing.rs"]
mod test_helpers;
mod test_service;

use std::sync::{Arc, Mutex};

use mcpox_jsonrpc::{Client, MethodResponse, Params, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use tokio::io::duplex;
use tokio_util::codec::{self, Framed, LinesCodec};

#[tokio::test]
async fn interact_with_test_service() {
    test_helpers::init_test_logging();

    let server = test_service::test_service_server();

    struct ClientState {
        callback_method_count: usize,
        callback_notification_count: usize,
    }

    #[derive(Serialize, Deserialize, PartialEq, Hash, Debug)]
    struct TestCallbackParams {
        foo: String,
        bar: i32,
    }

    impl Default for TestCallbackParams {
        fn default() -> Self {
            TestCallbackParams {
                foo: "foo".to_string(),
                bar: 42,
            }
        }
    }

    async fn client_test_callback(
        State(state): State<Arc<Mutex<ClientState>>>,
        Params(TestCallbackParams { foo, bar }): Params<TestCallbackParams>,
    ) -> MethodResponse<TestCallbackParams> {
        assert_eq!(foo, "foo");
        assert_eq!(bar, 42);
        let mut state = state.lock().unwrap();
        state.callback_method_count += 1;

        TestCallbackParams::default().into()
    }

    async fn client_test_notification(
        State(state): State<Arc<Mutex<ClientState>>>,
        Params(TestCallbackParams { foo, bar }): Params<TestCallbackParams>,
    ) {
        let mut state = state.lock().unwrap();
        state.callback_notification_count += 1;
    }

    // Create a pair of connected pipes that will serve as the transport between client and server
    let (client, server) = duplex(1024);

    // Create framed transports
    let client_transport = Framed::new(client, LinesCodec::new());
    let server_transport = Framed::new(server, LinesCodec::new());

    // Start the server and serve the connection made up by the duplex channel
    let server = test_service::test_service_server();
    let _server_connection_handle = server.serve_connection(server_transport).unwrap();

    // Bind a client to this connection
    let client = Client::builder()
        .with_state(Arc::new(Mutex::new(ClientState {
            callback_method_count: 0,
            callback_notification_count: 0,
        })))
        .with_handler("test_callback", client_test_callback)
        .with_handler("test_notification", client_test_notification)
        .bind(client_transport)
        .unwrap();

    // Start calling into the server and verify we get the behavior we expect
    let response: String = client.call_with_params("echo", "Hello, world!").await.unwrap();
    assert_eq!(response, "Hello, world!");
}
