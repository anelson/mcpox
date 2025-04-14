use std::collections::HashMap;
use std::sync::Arc;

use futures::lock::Mutex;
use mcpox_jsonrpc::{
    Client, ErrorDetails, Id, JsonValue, MethodResponse, Params, Result, Router, Server,
    ServiceConnectionHandle, State, Transport,
};
use serde::{Deserialize, Serialize};

/// A test service built using the JSON-RPC crate. Its internal implementation matches the
/// expected behavior reflected in the `testdata/` folder, and thus it can be used to exercise the
/// JSON RPC implementation end-to-end
#[derive(Debug, Default)]
pub struct TestState {
    pub counter: i32,
    pub values: HashMap<String, JsonValue>,
    pub last_notification: Option<String>,
}

pub type SharedState = Arc<Mutex<TestState>>;

/// Create a router that has methods and notification handlers that make up the "test service".
///
/// This router can be the input to either a client or a server, although a server is a more
/// realistic use case.
pub fn test_service_router() -> Router<SharedState> {
    let mut router = Router::new_with_state(Arc::new(Mutex::new(TestState::default())));

    router.register_handler("echo", echo);
    router.register_handler("increment_counter", increment_counter);
    router.register_handler("get_counter", get_counter);
    router.register_handler("get_value", get_value);
    router.register_handler("set_value", set_value);
    router.register_handler("record_notification", record_notification);
    router.register_handler("get_last_notification", get_last_notification);
    router.register_handler("fail_with_error", fail_with_error);
    router.register_handler("fail_with_panic", fail_with_panic);
    router.register_handler("call_caller_method", call_caller_method);
    router.register_handler("raise_caller_notification", raise_caller_notification);

    router
}

/// Create a client connected to some transport that locally uses the test service router
#[allow(dead_code)] // TODO: remove once this is used in a test
pub fn test_service_client(transport: impl Transport) -> Client<SharedState> {
    Client::builder()
        .with_router(test_service_router())
        .bind(transport)
        .unwrap()
}

/// Create a server that serves the test service operations
pub fn test_service_server() -> Server<SharedState> {
    Server::builder().with_router(test_service_router()).build()
}

async fn echo(params: JsonValue) -> JsonValue {
    params
}

async fn increment_counter(State(state): State<SharedState>) -> MethodResponse<i32> {
    let mut state = state.lock().await;
    state.counter += 1;
    state.counter.into()
}

async fn get_counter(State(state): State<SharedState>) -> MethodResponse<i32> {
    let state = state.lock().await;
    state.counter.into()
}

#[derive(Serialize, Deserialize)]
pub struct SetValueParams {
    pub key: String,
    pub value: JsonValue,
}

async fn set_value(
    State(state): State<SharedState>,
    Params(SetValueParams { key, value }): Params<SetValueParams>,
) {
    let mut state = state.lock().await;
    state.values.insert(key, value);
}

#[derive(Serialize, Deserialize)]
pub struct GetValueParams {
    pub key: String,
}

async fn get_value(
    State(state): State<SharedState>,
    Params(GetValueParams { key }): Params<GetValueParams>,
) -> JsonValue {
    let state = state.lock().await;
    state.values.get(&key).cloned().unwrap_or(JsonValue::Null)
}

#[derive(Serialize, Deserialize)]
pub struct RecordNotificationParams {
    pub message: String,
}

async fn record_notification(
    method_id: Option<Id>,
    State(state): State<SharedState>,
    Params(RecordNotificationParams { message }): Params<RecordNotificationParams>,
) {
    assert!(method_id.is_none(), "Notification should not have an ID",);
    let mut state = state.lock().await;
    state.last_notification = Some(message);
}

async fn get_last_notification(State(state): State<SharedState>) -> JsonValue {
    if let Some(message) = state.lock().await.last_notification.clone() {
        JsonValue::String(message)
    } else {
        JsonValue::Null
    }
}

/// Always fail with a specific error that the caller can verify
async fn fail_with_error() -> Result<(), ErrorDetails> {
    Err(ErrorDetails::server_error(1, "Test error", None))
}

/// Always panic to exercise the panic handler
async fn fail_with_panic() -> Result<(), ErrorDetails> {
    panic!("OMG we're all gonna die!")
}

#[derive(Serialize, Deserialize)]
pub struct CallCallerMethodParams {
    pub method: String,
    pub params: Option<JsonValue>,
}

/// While inside the handler, callback to the caller, calling a method and passing parameters
/// specified by the caller.  Whatever the caller returned from that method, is returned back to
/// the caller of this one.  It's like echo inception.
///
/// This isn't something that would normally be useful but it is great for testing this
/// bidirectional call capability which is an important part of the MCP spec.
async fn call_caller_method(
    connection_handle: ServiceConnectionHandle,
    Params(CallCallerMethodParams { method, params }): Params<CallCallerMethodParams>,
) -> Result<JsonValue> {
    connection_handle.call_raw(&method, params).await
}

#[derive(Serialize, Deserialize)]
pub struct RaiseCallerNotificationParams {
    pub method: String,
    pub params: Option<JsonValue>,
}

/// While inside the handler, fire a notification to the caller, passing parameters
/// specified by the caller.
///
/// This isn't something that would normally be useful but it is great for testing this
/// bidirectional notification capability which is an important part of the MCP spec.
async fn raise_caller_notification(
    connection_handle: ServiceConnectionHandle,
    Params(RaiseCallerNotificationParams { method, params }): Params<RaiseCallerNotificationParams>,
) -> Result<()> {
    connection_handle.raise_raw(&method, params).await
}
