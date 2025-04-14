//! Integration tests to exercise the JSON-RPC crate using it's public interface.

/// Fun hack to avoid duplication: the `testing` module in the `jsonrpc` crate is only enabled in
/// `test` config, and when running integration tests like this one the crate is compiled without
/// `test` enabled.  But we need to use the same test helpers, so just include that module in this
/// test program as well.
#[path = "../src/testing.rs"]
mod test_helpers;
mod test_service;

use test_service::{
    CallCallerMethodParams, GetValueParams, RaiseCallerNotificationParams, RecordNotificationParams,
    SetValueParams,
};

use std::sync::{Arc, Mutex};

use mcpox_jsonrpc::{
    Client, ErrorCode, JsonRpcError, JsonValue, MethodResponse, Params, Server, ServiceConnectionHandle,
    State, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::duplex;
use tokio_util::codec::{Framed, LinesCodec};

/// Create a pair of [`Transport`] implementations that are connected to each other, suitable for
/// use hooking up a client and a server without using HTTP or some other "real" transport
///
/// Return value is a tupl, `(client_transport, server_transport)`.
fn setup_test_channel() -> (impl Transport, impl Transport) {
    // Create a pair of connected pipes that will serve as the transport between client and server
    let (client, server) = duplex(1024);

    // Create framed transports
    let client_transport = Framed::new(client, LinesCodec::new());
    let server_transport = Framed::new(server, LinesCodec::new());

    (client_transport, server_transport)
}

/// Create a server that implements the test service from the [`test_service`] module, for use
/// exercising interactions with the client.
fn setup_test_server() -> Server<test_service::SharedState> {
    test_service::test_service_server()
}

/// State maintained by the test client
struct ClientState {
    callback_method_count: usize,
    callback_notification_count: usize,
}

/// Parameters passed to test client's method and notification handlers
#[derive(Serialize, Deserialize, PartialEq, Hash, Debug)]
struct TestCallbackParams {
    name: String,
    value: i32,
}

#[derive(Serialize, Deserialize, PartialEq, Hash, Debug)]
struct TestCallbackResponse {
    message: String,
    callback_method_count: usize,
}

/// Setup a test client that has a local method and notification which records its activity in
/// shared state to help exercise that behavior.
fn setup_test_client(transport: impl Transport) -> Client<Arc<Mutex<ClientState>>> {
    async fn client_test_callback(
        State(state): State<Arc<Mutex<ClientState>>>,
        Params(TestCallbackParams { name, value }): Params<TestCallbackParams>,
    ) -> MethodResponse<TestCallbackResponse> {
        assert_eq!(name, "test");
        assert_eq!(value, 42);
        let mut state = state.lock().unwrap();
        state.callback_method_count += 1;

        TestCallbackResponse {
            message: "Hello from Client!".to_string(),
            callback_method_count: state.callback_method_count,
        }
        .into()
    }

    async fn client_test_notification(
        State(state): State<Arc<Mutex<ClientState>>>,
        Params(TestCallbackParams { name, value }): Params<TestCallbackParams>,
    ) {
        assert_eq!(name, "test");
        assert_eq!(value, 42);

        let mut state = state.lock().unwrap();
        state.callback_notification_count += 1;
    }

    println!("Launching client!");
    let client = Client::builder()
        .with_state(Arc::new(Mutex::new(ClientState {
            callback_method_count: 0,
            callback_notification_count: 0,
        })))
        .with_handler("test_callback", client_test_callback)
        .with_handler("test_notification", client_test_notification)
        .bind(transport)
        .unwrap();
    println!("Client is now running");

    client
}

/// Construct a server that serves the test service operations, an in-memory channel-based
/// Transport, and a client connectedto the server.  Return all of it
fn setup_client_and_server() -> (
    Server<test_service::SharedState>,
    ServiceConnectionHandle,
    Client<Arc<Mutex<ClientState>>>,
) {
    let (client_transport, server_transport) = setup_test_channel();

    let server = setup_test_server();
    let server_connection_handle = server.serve_connection(server_transport).unwrap();

    let client = setup_test_client(client_transport);

    (server, server_connection_handle, client)
}

/// End-to-end test of client talking to server, doing a simple echo operation
#[tokio::test]
async fn echo_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Test the echo method
    let response: String = client.call_with_params("echo", "Hello, world!").await.unwrap();
    assert_eq!(response, "Hello, world!");
}

/// Exercise the test service with a variety of operations that exercise various aspects of the
/// JSON RPC impl, in the form of conventional RPC calls and notifications initiated by the client
/// and received by the server.
#[tokio::test]
async fn client_server_service_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Test counter operations
    let initial_counter: i32 = client.call("get_counter").await.unwrap();
    assert_eq!(initial_counter, 0);

    let incremented_counter: i32 = client.call("increment_counter").await.unwrap();
    assert_eq!(incremented_counter, 1);

    let counter_value: i32 = client.call("get_counter").await.unwrap();
    assert_eq!(counter_value, 1);

    // Test value storage and retrieval
    let test_key = "test_key".to_string();
    let test_value = json!("test_value");

    let set_value_params = SetValueParams {
        key: test_key.clone(),
        value: test_value.clone(),
    };

    client
        .call_with_params::<_, ()>("set_value", set_value_params)
        .await
        .unwrap();

    let get_value_params = GetValueParams {
        key: test_key.clone(),
    };

    let retrieved_value: JsonValue = client
        .call_with_params("get_value", get_value_params)
        .await
        .unwrap();
    assert_eq!(retrieved_value, test_value);

    // Test non-existent key returns null
    let nonexistent_key_params = GetValueParams {
        key: "nonexistent".to_string(),
    };

    let nonexistent_value: JsonValue = client
        .call_with_params("get_value", nonexistent_key_params)
        .await
        .unwrap();
    assert_eq!(nonexistent_value, JsonValue::Null);

    // Test notification
    assert_eq!(
        JsonValue::Null,
        client.call::<JsonValue>("get_last_notification").await.unwrap()
    );
    let notification_message = "test notification".to_string();
    let notification_params = RecordNotificationParams {
        message: notification_message.clone(),
    };

    client
        .raise_with_params("record_notification", notification_params)
        .await
        .unwrap();

    // Verify that the notification was processed by the server
    assert_eq!(
        JsonValue::String(notification_message),
        client.call::<JsonValue>("get_last_notification").await.unwrap()
    );
}

/// Call a method that doesn't exist and note the error
#[tokio::test]
async fn method_not_found_error_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Call a method that doesn't exist
    let error_result = client.call::<()>("nonesistent_method").await;
    assert!(error_result.is_err());
    if let Err(err) = error_result {
        match err {
            JsonRpcError::MethodError { method_name, error } => {
                assert_eq!(method_name, "nonesistent_method");
                assert!(matches!(error.code, ErrorCode::MethodNotFound));
            }
            _ => panic!("Unexpected error type: {:?}", err),
        }
    }
}

/// Raise a notification that doesn't exist and note that...nothing happens
#[tokio::test]
async fn notification_not_found_error_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Raise a notification that doesn't exist
    // The 'unwrap` on the result here will only fail if there was some kind of transport error;
    // otherwise there's no way to know how the server handled things.
    client.raise("nonesistent_notification").await.unwrap();
}

/// Call a method that we know fails with an error
#[tokio::test]
async fn method_error_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Raise a notification that doesn't exist
    // The 'unwrap` on the result here will only fail if there was some kind of transport error;
    // otherwise there's no way to know how the server handled things.
    let result = client.call::<()>("fail_with_error").await.unwrap_err();

    assert!(matches!(result, JsonRpcError::MethodError { method_name, error }
        if method_name == "fail_with_error" && matches!(error.code, ErrorCode::ServerError(1))));
}

/// Call a method that we know panics; that should be reported back to the client as an internal
/// error
#[tokio::test]
async fn method_panic_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // Raise a notification that doesn't exist
    // The 'unwrap` on the result here will only fail if there was some kind of transport error;
    // otherwise there's no way to know how the server handled things.
    let result = client.call::<()>("fail_with_panic").await.unwrap_err();

    assert!(matches!(result, JsonRpcError::MethodError { method_name, error }
        if method_name == "fail_with_panic" && matches!(error.code, ErrorCode::InternalError)));
}

/// Test the interactions in which the test service on the server side initiates a method call and
/// a notification passed back to the client.
#[tokio::test]
async fn server_initiated_callback_test() {
    test_helpers::init_test_logging();

    let (_server, _server_connection_handle, client) = setup_client_and_server();

    // The client is already setup to have a notification and a method of its own.  Just need to
    // call them.  It will assert that the params have these exact values
    let callback_params = TestCallbackParams {
        name: "test".to_string(),
        value: 42,
    };

    // call `call_caller_method`, which will pass whatever is given to it in `params`, and call the
    // method whose name is `method`, back on the client.  `setup_client_and_server` creates the
    // client with a router that has a method `test_callback` and a notification
    // `test_notification`.  The method returns `TestCallbackParams`
    assert_eq!(0, client.state().lock().unwrap().callback_method_count);
    let callback_result: TestCallbackResponse = client
        .call_with_params(
            "call_caller_method",
            CallCallerMethodParams {
                method: "test_callback".to_string(),
                params: Some(serde_json::to_value(&callback_params).unwrap()),
            },
        )
        .await
        .unwrap();

    // The client's own `test_callback` method returns a hard-coded message and the call count
    assert_eq!("Hello from Client!", callback_result.message);
    assert_eq!(callback_result.callback_method_count, 1);
    assert_eq!(1, client.state().lock().unwrap().callback_method_count);

    // Now do the same with a notification.  Notifications don't produce responses but we can
    // inspect the client's state and ascertain that it was called
    //
    // The notification also asserts the same callback values that the method does
    assert_eq!(0, client.state().lock().unwrap().callback_notification_count);
    client
        .raise_with_params(
            "raise_caller_notification",
            RaiseCallerNotificationParams {
                method: "test_notification".to_string(),
                params: Some(serde_json::to_value(&callback_params).unwrap()),
            },
        )
        .await
        .unwrap();
    assert_eq!(1, client.state().lock().unwrap().callback_notification_count);
}
