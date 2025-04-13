use std::collections::HashMap;
use std::sync::Arc;

use futures::lock::Mutex;
use mcpox_jsonrpc::{ErrorDetails, Id, JsonValue, MethodName, MethodResponse, Params, Result, Router, State};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// A test service built using the JSON-RPC crate. Its internal implementation matches the
/// expected behavior reflected in the `testdata/` folder, and thus it can be used to exercise the
/// JSON RPC implementation end-to-end
#[derive(Debug, Default)]
pub struct TestState {
    counter: i32,
    values: HashMap<String, JsonValue>,
    last_notification: Option<String>,
}

type SharedState = Arc<Mutex<TestState>>;

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
    router.register_handler("fail_with_error", fail_with_error);
    router.register_handler("fail_with_panic", fail_with_panic);
    router.register_handler("call_caller_method", call_caller_method);
    router.register_handler("raise_caller_notification", raise_caller_notification);

    router
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
    key: String,
    value: JsonValue,
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
    key: String,
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
    message: String,
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
    method: String,
    params: Option<JsonValue>,
}

/// While inside the handler, callback to the caller, calling a method and passing parameters
/// specified by the caller.  Whatever the caller returned from that method, is returned back to
/// the caller of this one.  It's like echo inception.
///
/// This isn't something that would normally be useful but it is great for testing this
/// bidirectional call capability which is an important part of the MCP spec.
async fn call_caller_method(
    Params(CallCallerMethodParams { method, params }): Params<CallCallerMethodParams>,
) -> Result<JsonValue> {
    todo!()
}

#[derive(Serialize, Deserialize)]
pub struct RaiseCallerNotificationParams {
    method: String,
    params: Option<JsonValue>,
}

/// While inside the handler, fire a notification to the caller, passing parameters
/// specified by the caller.
///
/// This isn't something that would normally be useful but it is great for testing this
/// bidirectional notification capability which is an important part of the MCP spec.
async fn raise_caller_notification(
    Params(RaiseCallerNotificationParams { method, params }): Params<RaiseCallerNotificationParams>,
) -> Result<()> {
    todo!()
}



