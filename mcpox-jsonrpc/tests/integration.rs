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
    SetValueParams, SleepParams,
};

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcpox_jsonrpc::{
    Client, ErrorCode, ErrorDetails, JsonRpcError, JsonValue, MethodResponse, Params, Server,
    ServiceConnectionHandle, State, Transport,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use test_helpers::setup_test_channel;

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

/// Test that a long-running server-side method gets canceled when the server is shutdown
#[tokio::test]
async fn server_shutdown_cancels_in_progress_calls() {
    test_helpers::init_test_logging();

    let (_server, server_connection_handle, client) = setup_client_and_server();

    // Start a sleep operation that would take a long time to complete
    let sleep_seconds = 60; // Long enough that it won't finish during the test
    let sleep_future = client.call_with_params::<_, bool>(
        "sleep",
        SleepParams {
            seconds: sleep_seconds,
        },
    );

    // Poll that method call future for give seconds before we give up.  That's plenty of time for
    // the request to be sent to the server, but nowhere near enough time for the sleep to finish
    tokio::pin!(sleep_future);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut sleep_future)
            .await
            .is_err(),
        "Sleep operation should not have completed yet"
    );

    // Shutdown the server connection - this should cause all in-progress operations to be cancelled
    tracing::debug!("Shutting down server connection");
    server_connection_handle.shutdown().await.unwrap();

    // The sleep future should complete quickly with an error rather than waiting for the full duration
    match sleep_future.await {
        Ok(_) => panic!("Sleep operation should have been cancelled, but it completed successfully"),
        Err(err) => {
            tracing::debug!("Sleep operation was cancelled with error: {:?}", err);
            // The exact error might vary, but it should be one of the error variants
            // that indicates the connection was closed or the operation was cancelled
            //
            // TODO: this fails because the server sends back the correct response, the client
            // receives that response and spawns a task to process it, then immediately notices
            // that the connection is closed and exits the loop and performs the shutdown
            // sequence...which includes cancelling all running tasks.  FML.
            // Seems like the approach to exiting the event loop and what to do after it's exited
            // needs a bit of a rethink.  Probably there should be a configurable shutdown period
            // during which running tasks should be allowed to run to completion before we perform
            // a cancelation.  This is most acute on the client side where the connection closing
            // after a response is received literally guarantees a pending task that needs to be
            // given some time to run before it completes.
            //
            // Or maybe the spawning of incoming messages to a future should only be for
            // notification and method call requests; everything else should be processed inside of
            // the event loop.  Maybe?
            //
            // Perhaps the event loop should keep running if the connection is closed, as long as
            // the cancellation token isn't triggered and there are either outbound messages to
            // process or or pending tasks to execute.  outbound messages obviously are not going
            // out once the connection is closed, but we can process them inside of the event
            // loop...no actually we still have to process them after the loop in case we exit the
            // loop due to cancelation.  So really the reason to keep running the loop after the
            // connection is closed would just be for pending tasks and also for the custom event
            // loop if any.
            assert!(matches!(
                err,
                JsonRpcError::MethodError { method_name, error: ErrorDetails { code, message, .. } }
                if method_name == "sleep" && code == ErrorCode::InternalError && message.contains("cancelled")
            ));
        }
    }
}
