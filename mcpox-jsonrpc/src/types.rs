//! Rust struct definitions that ser/de to/from JSON-RPC messages.
//!
//! Initially this used the `jsonrpsee-types` types, but they were very awkward to use due to the
//! way they were implemented to be able to deserialize into a borrowed value.  That's more
//! efficient to be sure and someday perhaps benchmarking will indicate that we need that
//! optimization as well, but for now this more naive implementation suits our needs.
use serde::{Deserialize, Serialize};

use crate::{JsonRpcError, Result};
/// Re-export the structs and enums that are part of the JSON-RPC protocol
///
/// No need to re-invent this wheel.  The Request and Response types are not suitable for our use
/// case but other basic types are exactly what we would declare ourselves so we can just
/// re-export.
pub use jsonrpsee_types::{error::ErrorCode, params::TwoPointZero};
pub use serde_json::Value as JsonValue;
/// Request Id
#[derive(Debug, PartialEq, Clone, Hash, Eq, Deserialize, Serialize, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Id {
    /// Null
    Null,
    /// Numeric id
    Number(u64),
    /// String id
    Str(String),
}

/// Serializable [JSON-RPC request object](https://www.jsonrpc.org/specification#request-object).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Request ID
    pub id: Id,
    /// Name of the method to be invoked.
    pub method: String,
    /// Parameter values of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Request {
    /// Create a serializable JSON-RPC method call.
    pub fn new(id: Id, method: impl Into<String>, params: impl Into<Option<JsonValue>>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            id,
            method: method.into(),
            params: params.into(),
        }
    }
}

/// JSON-RPC Invalid request as defined in the [spec](https://www.jsonrpc.org/specification#request-object).
///
/// If the request has an invalid shape, but still has an id, we will try to deserialize it into
/// this struct so that when we send back the error response complaining about the invalid request
/// we can at least included the ID of the request for the client to match it up with the request
/// that caused the error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvalidRequest {
    /// Request ID
    pub id: Id,
}

/// Serializable [JSON-RPC notification object](https://www.jsonrpc.org/specification#request-object).
///
/// Differs from the request object in that it does not have an `id` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Name of the method to be invoked.
    pub method: String,
    /// Parameter values of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Notification {
    /// Create a serializable JSON-RPC notification.
    pub fn new(method: impl Into<String>, params: impl Into<Option<JsonValue>>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            method: method.into(),
            params: params.into(),
        }
    }
}

/// JSON-RPC response object as defined in the [spec](https://www.jsonrpc.org/specification#response_object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Payload which can be result or error.
    #[serde(flatten)]
    pub payload: ResponsePayload,
    /// Request ID
    pub id: Id,
}

impl Response {
    /// Create a owned successful response
    pub fn success(id: Id, result: JsonValue) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            payload: ResponsePayload::Success(SuccessResponse { result }),
            id,
        }
    }

    /// Create an error response
    pub fn error(
        id: Id,
        code: ErrorCode,
        message: impl Into<String>,
        data: impl Into<Option<JsonValue>>,
    ) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            payload: ResponsePayload::Error(ErrorResponse {
                error: ErrorDetails {
                    code,
                    message: message.into(),
                    data: data.into(),
                },
            }),
            id,
        }
    }
}

/// Possible payloads of a JSON RPC response
///
/// This is an `untagged` enum so that serde will try to deserialize into each variant in order
/// until one succeeds.  This works because successs responses should have `result` and error
/// responses should have `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    /// Corresponds to failed JSON-RPC response with a error object with the field `error.
    Error(ErrorResponse),
    /// Corresponds to successful JSON-RPC response with the field `result`.
    Success(SuccessResponse),
}

impl ResponsePayload {
    /// Create a successful response payload
    pub fn success(result: JsonValue) -> Self {
        ResponsePayload::Success(SuccessResponse { result })
    }

    /// Serialize our way to a a successful response payload, handling serialization error by
    /// producing an error payload instead
    pub fn serialize_to_success<T: Serialize>(result: T) -> Self {
        match serde_json::to_value(result).map_err(|e| JsonRpcError::SerResponse {
            source: e,
            type_name: std::any::type_name::<T>(),
        }) {
            Ok(json) => Self::Success(json.into()),
            Err(e) => {
                let details: ErrorDetails = e.into();
                Self::Error(details.into())
            }
        }
    }

    /// Create an error response payload
    pub fn error(details: impl Into<ErrorDetails>) -> Self {
        ResponsePayload::Error(details.into().into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub result: JsonValue,
}

impl From<JsonValue> for SuccessResponse {
    fn from(result: JsonValue) -> Self {
        Self { result }
    }
}

/// [Failed JSON-RPC response object](https://www.jsonrpc.org/specification#error_object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorDetails,
}

impl From<ErrorDetails> for ErrorResponse {
    fn from(error: ErrorDetails) -> Self {
        Self { error }
    }
}

/// [JSON-RPC failed response error details](https://www.jsonrpc.org/specification#error_object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDetails {
    /// Code
    pub code: ErrorCode,
    /// Message
    pub message: String,
    /// Optional data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

impl ErrorDetails {
    pub fn new(code: ErrorCode, message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self {
            code,
            message: message.into(),
            data: data.into(),
        }
    }

    pub fn parse_error(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::ParseError, message, data)
    }
    pub fn oversized_request(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::OversizedRequest, message, data)
    }
    pub fn invalid_request(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message, data)
    }
    pub fn method_not_found(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::MethodNotFound, message, data)
    }
    pub fn server_is_busy(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::ServerIsBusy, message, data)
    }
    pub fn invalid_params(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::InvalidParams, message, data)
    }
    pub fn internal_error(message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::InternalError, message, data)
    }
    pub fn server_error(code: i32, message: impl Into<String>, data: impl Into<Option<JsonValue>>) -> Self {
        Self::new(ErrorCode::ServerError(code), message, data)
    }
}

/// Every possible (valid) JSON-RPC message that can be sent over the wire
///
/// Represented as an `untagged` enum so serde will try to deserialize into each variant in order
/// until one succeeds.  This is the most flexible way to represent the JSON-RPC protocol, as it
/// allows for proper handling of each message type.
///
/// The order of variants is important for deserialization since serde will try each variant in
/// order.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    /// A batch of messages
    Batch(Vec<Message>),

    /// A request with method, id, and optional params
    Request(Request),

    /// A notification with method and optional params but no id
    Notification(Notification),

    /// A response containing either a result or error
    Response(Response),

    /// An invalid request that still has an id
    InvalidRequest(InvalidRequest),
}

impl Message {
    /// Attempt to deserialize a message from raw bytes.
    ///
    /// Transport implementations should prefer to use this implementation rather than their own
    /// interpretation.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|e| JsonRpcError::ParseJson {
            source: e,
            json: String::from_utf8_lossy(bytes).to_string(),
        })
    }

    /// Attempt to serialize this message into raw bytes.
    ///
    /// This is fallible but unlikely to fail barring memory issues.
    ///
    /// Transport implementations should prefer to use this implementation rather than their own.
    pub fn into_bytes(self) -> Result<Vec<u8>> {
        serde_json::to_vec(&self).map_err(|e| JsonRpcError::SerResponse {
            source: e,
            type_name: std::any::type_name::<Self>(),
        })
    }
}
// The JSON-RPC protocol talks about servers and clients, but in almost every way that matters
// it's actually a peer-to-peer protocol.  Servers can send messages that the client didn't
// initiate a request for, and clients have to handle those and respond.  There are differences in
// how clients and servers are instantiated, as servers serve but clients connect, however most of
// the JSON RPC protocol is about exchanging messages between what are, in effect, peers.

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use serde_json::{Value, json};

    #[test]
    fn test_request_serialization() {
        // Create known-good JSON-RPC request string manually - this is the format defined by the spec
        let known_good_json = r#"{"jsonrpc":"2.0","method":"test_method","params":[1,"test",true],"id":1}"#;
        let known_good_value: Value = serde_json::from_str(known_good_json).unwrap();

        // Create a Request using our implementation
        let params = json!([1, "test", true]);
        let id = Id::Number(1);
        let our_request = Request::new(id.clone(), "test_method", Some(params));

        // Serialize our request to JSON string
        let our_json = serde_json::to_string(&our_request).unwrap();
        let our_value: Value = serde_json::from_str(&our_json).unwrap();

        // Compare our implementation against the known-good format
        assert_eq!(known_good_value, our_value);

        // Test round-trip serialization/deserialization
        let deserialized: Request = serde_json::from_str(&our_json).unwrap();
        assert_eq!(deserialized.id, our_request.id);
        assert_eq!(deserialized.method, our_request.method);
        assert_eq!(deserialized.jsonrpc, TwoPointZero);

        // Also verify we can deserialize the known-good format
        let known_good_request: Request = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(known_good_request.id, Id::Number(1));
        assert_eq!(known_good_request.method, "test_method");

        // Test deserializing into a Message enum
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(message, Message::Request(_));
    }

    #[test]
    fn test_batch_request_serialization() {
        // Create known-good JSON-RPC request string manually - this is the format defined by the spec
        let known_good_json = concat!(
            r#"[ {"jsonrpc":"2.0","method":"test_method","params":[1,"test",true],"id":1}, "#,
            r#"{"jsonrpc":"2.0","method":"test_method2","params":[1,"test",true],"id":2} ]"#
        );

        // Test round-trip serialization/deserialization
        let deserialized: Vec<Request> = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(deserialized[0].id, Id::Number(1));
        assert_eq!(deserialized[0].method, "test_method");
        assert_eq!(deserialized[0].params.as_ref(), Some(&json!([1, "test", true])));
        assert_eq!(deserialized[1].id, Id::Number(2));
        assert_eq!(deserialized[1].method, "test_method2");
        assert_eq!(deserialized[1].params.as_ref(), Some(&json!([1, "test", true])));

        // Serialize back to JSON
        let _our_json = serde_json::to_string(&deserialized).unwrap();

        // Test deserializing into a Message enum
        println!("{}", known_good_json);
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(message, Message::Batch(_));
    }

    #[test]
    fn test_invalid_request_serialization() {
        // Create known-good JSON-RPC request string manually - this is the format defined by the spec
        let known_good_json = r#"{"jsonrpc":"2.0","methood":"test_method","id":1}"#;

        // Test round-trip serialization/deserialization
        let deserialized: InvalidRequest = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(deserialized.id, Id::Number(1));

        // Test deserializing into a Message enum
        println!("{}", known_good_json);
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(message, Message::InvalidRequest(_));
    }

    #[test]
    fn test_notification_serialization() {
        // Create known-good JSON-RPC notification string manually - this is the format defined by the spec
        let known_good_json = r#"{"jsonrpc":"2.0","method":"notify_method","params":{"key":"value"}}"#;
        let known_good_value: Value = serde_json::from_str(known_good_json).unwrap();

        // Create a Notification using our implementation
        let params = json!({"key": "value"});
        let our_notification = Notification::new("notify_method", Some(params));

        // Serialize our notification to JSON string
        let our_json = serde_json::to_string(&our_notification).unwrap();
        let our_value: Value = serde_json::from_str(&our_json).unwrap();

        // Compare our implementation against the known-good format
        assert_eq!(known_good_value, our_value);

        // Test round-trip serialization/deserialization
        let deserialized: Notification = serde_json::from_str(&our_json).unwrap();
        assert_eq!(deserialized.method, our_notification.method);
        assert_eq!(deserialized.jsonrpc, TwoPointZero);

        // Also verify we can deserialize the known-good format
        let known_good_notification: Notification = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(known_good_notification.method, "notify_method");

        // Test deserializing into a Message enum
        println!("{}", known_good_json);
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(message, Message::Notification(_));
    }

    #[test]
    fn test_success_response_serialization() {
        // Create known-good JSON-RPC success response string manually - this is the format defined by the
        // spec
        let known_good_json = r#"{"jsonrpc":"2.0","result":{"status":"success"},"id":42}"#;
        let known_good_value: Value = serde_json::from_str(known_good_json).unwrap();

        // Create a successful Response using our implementation
        let result_raw = json!({"status": "success"});
        let id = Id::Number(42);
        let our_response = Response::success(id.clone(), result_raw);

        // Serialize our response to JSON string
        let our_json = serde_json::to_string(&our_response).unwrap();
        let our_value: Value = serde_json::from_str(&our_json).unwrap();

        // Compare our implementation against the known-good format
        assert_eq!(known_good_value, our_value);

        // Test round-trip serialization/deserialization
        let deserialized: Response = serde_json::from_str(&our_json).unwrap();
        assert_eq!(deserialized.id, our_response.id);
        assert_matches!(deserialized.payload, ResponsePayload::Success(SuccessResponse { result }) if result == json!({"status":"success"}));

        // Also verify we can deserialize the known-good format
        let known_good_response: Response = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(known_good_response.id, our_response.id);
        assert_matches!(known_good_response.payload, ResponsePayload::Success(SuccessResponse { result }) if result == json!({"status":"success"}));

        // Test deserializing into a Message enum
        println!("{}", known_good_json);
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(
            message,
            Message::Response(Response {
                payload: ResponsePayload::Success(_),
                ..
            })
        );
    }

    #[test]
    fn test_error_response_serialization() {
        // Create known-good JSON-RPC error response string manually - this is the format defined by the
        // spec
        let known_good_json = concat!(
            r#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"Invalid request","#,
            r#""data":{"details":"error details"}},"id":"error-id"}"#
        );
        let known_good_value: Value = serde_json::from_str(known_good_json).unwrap();

        // Create an error Response using our implementation
        let id = Id::Str("error-id".into());
        let error_data = json!({"details": "error details"});
        let our_response = Response::error(
            id.clone(),
            ErrorCode::InvalidRequest,
            "Invalid request",
            error_data,
        );

        // Serialize our error response to JSON string
        let our_json = serde_json::to_string(&our_response).unwrap();
        let our_value: Value = serde_json::from_str(&our_json).unwrap();

        // Compare our implementation against the known-good format
        assert_eq!(known_good_value, our_value);

        // Test deserializing into a Message enum
        println!("{}", known_good_json);
        let message: Message = serde_json::from_str(known_good_json).unwrap();
        assert_matches!(
            message,
            Message::Response(Response {
                payload: ResponsePayload::Error(ErrorResponse { error: ErrorDetails { code, ..}}),
                ..
            }) if code == ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn test_json_rpc_protocol_spec_compliance() {
        // This test verifies our implementation against the JSON-RPC 2.0 spec examples
        // from https://www.jsonrpc.org/specification

        // Test a basic request
        let request_json = r#"{"jsonrpc":"2.0","method":"subtract","params":[42,23],"id":1}"#;
        let request: Request = serde_json::from_str(request_json).unwrap();
        assert_eq!(request.method, "subtract");
        assert_eq!(request.id, Id::Number(1));

        // Test named params request
        let named_request_json =
            r#"{"jsonrpc":"2.0","method":"subtract","params":{"subtrahend":23,"minuend":42},"id":3}"#;
        let named_request: Request = serde_json::from_str(named_request_json).unwrap();
        assert_eq!(named_request.method, "subtract");
        assert_eq!(named_request.id, Id::Number(3));

        // Test notification (no id)
        let notification_json = r#"{"jsonrpc":"2.0","method":"update","params":[1,2,3,4,5]}"#;
        let notification: Notification = serde_json::from_str(notification_json).unwrap();
        assert_eq!(notification.method, "update");

        // Test success response
        let success_json = r#"{"jsonrpc":"2.0","result":19,"id":1}"#;
        let success_parsed: Value = serde_json::from_str(success_json).unwrap();

        // Create our own success response
        let our_success = Response::success(Id::Number(1), json!(19));
        let our_success_json = serde_json::to_string(&our_success).unwrap();
        let our_success_parsed: Value = serde_json::from_str(&our_success_json).unwrap();

        // Compare the JSON structures
        assert_eq!(success_parsed, our_success_parsed);

        // Test error response
        let error_json = r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":"1"}"#;
        let error_parsed: Value = serde_json::from_str(error_json).unwrap();

        // Create our own error response
        let our_error = Response::error(
            Id::Str("1".into()),
            ErrorCode::MethodNotFound,
            "Method not found",
            None,
        );
        let our_error_json = serde_json::to_string(&our_error).unwrap();
        let our_error_parsed: Value = serde_json::from_str(&our_error_json).unwrap();

        // Compare the JSON structures
        assert_eq!(error_parsed, our_error_parsed);

        // Verify that when we serialize our types, they match the spec format
        let our_request = Request::new(Id::Number(1), "subtract", json!([42, 23]));
        let our_request_json = serde_json::to_string(&our_request).unwrap();
        let our_request_value: Value = serde_json::from_str(&our_request_json).unwrap();
        let spec_request_value: Value = serde_json::from_str(request_json).unwrap();
        assert_eq!(our_request_value, spec_request_value);

        let our_success_response = Response::success(Id::Number(1), json!(19));
        let our_success_json = serde_json::to_string(&our_success_response).unwrap();
        let our_success_value: Value = serde_json::from_str(&our_success_json).unwrap();
        let spec_success_value: Value = serde_json::from_str(success_json).unwrap();
        assert_eq!(our_success_value, spec_success_value);
    }

    #[test]
    fn test_special_id_values() {
        // Create request with null ID
        let null_id = Id::Null;
        let null_id_request = Request::new(null_id, "method", None);
        let null_id_json = serde_json::to_string(&null_id_request).unwrap();
        let parsed: Value = serde_json::from_str(&null_id_json).unwrap();

        // Verify JSON serialization works correctly
        assert_eq!(parsed["id"], Value::Null);

        // Create request with string ID
        let str_id = Id::Str("abc123".into());
        let str_id_request = Request::new(str_id, "method", None);
        let str_id_json = serde_json::to_string(&str_id_request).unwrap();
        let parsed: Value = serde_json::from_str(&str_id_json).unwrap();

        // Verify JSON serialization works correctly
        assert_eq!(parsed["id"], json!("abc123"));

        // Create request with numeric ID
        let num_id = Id::Number(42);
        let num_id_request = Request::new(num_id, "method", None);
        let num_id_json = serde_json::to_string(&num_id_request).unwrap();
        let parsed: Value = serde_json::from_str(&num_id_json).unwrap();

        // Verify JSON serialization works correctly
        assert_eq!(parsed["id"], json!(42));

        // Verify deserialization from known-good strings
        let null_id_spec = r#"{"jsonrpc":"2.0","method":"method","params":null,"id":null}"#;
        let str_id_spec = r#"{"jsonrpc":"2.0","method":"method","params":null,"id":"abc123"}"#;
        let num_id_spec = r#"{"jsonrpc":"2.0","method":"method","params":null,"id":42}"#;

        let null_req: Request = serde_json::from_str(null_id_spec).unwrap();
        let str_req: Request = serde_json::from_str(str_id_spec).unwrap();
        let num_req: Request = serde_json::from_str(num_id_spec).unwrap();

        assert_eq!(null_req.id, Id::Null);
        assert_eq!(str_req.id, Id::Str("abc123".into()));
        assert_eq!(num_req.id, Id::Number(42));
    }
}
