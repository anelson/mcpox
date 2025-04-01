use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Re-export the exact async-trait macro we use, for compatibility
#[doc(hidden)]
pub use async_trait::async_trait;

/// Re-export the structs and enums that are part of the JSON-RPC protocol
///
/// No need to re-invent this wheel.  The Request and Response types are not suitable for our use
/// case but other basic types are exactly what we would declare ourselves so we can just
/// re-export.
pub use jsonrpsee_types::{
    error::ErrorCode,
    params::{Id, TwoPointZero},
};

pub use serde_json::value::RawValue;

mod error;

pub use error::{JsonRpcError, Result};

/// Serializable [JSON-RPC request object](https://www.jsonrpc.org/specification#request-object).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request<'a> {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Request ID
    #[serde(borrow)]
    pub id: Id<'a>,
    /// Name of the method to be invoked.
    #[serde(borrow)]
    pub method: Cow<'a, str>,
    /// Parameter values of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Cow<'a, RawValue>>,
}

impl<'a> Request<'a> {
    /// Create a borrowed serializable JSON-RPC method call.
    pub fn borrowed(id: &'a Id<'a>, method: &'a impl AsRef<str>, params: Option<&'a RawValue>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            id: id.clone(),
            method: method.as_ref().into(),
            params: params.map(Cow::Borrowed),
        }
    }

    /// Create a owned serializable JSON-RPC method call.
    pub fn owned(id: Id<'a>, method: impl Into<String>, params: Option<Box<RawValue>>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            id,
            method: method.into().into(),
            params: params.map(Cow::Owned),
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
pub struct InvalidRequest<'a> {
    /// Request ID
    #[serde(borrow)]
    pub id: Id<'a>,
}

/// Serializable [JSON-RPC notification object](https://www.jsonrpc.org/specification#request-object).
///
/// Differs from the request object in that it does not have an `id` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification<'a> {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Name of the method to be invoked.
    #[serde(borrow)]
    pub method: Cow<'a, str>,
    /// Parameter values of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Cow<'a, RawValue>>,
}

impl<'a> Notification<'a> {
    /// Create a borrowed serializable JSON-RPC notification.
    pub fn borrowed(method: &'a impl AsRef<str>, params: Option<&'a RawValue>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            method: method.as_ref().into(),
            params: params.map(Cow::Borrowed),
        }
    }

    /// Create an owned serializable JSON-RPC notification.
    pub fn owned(method: impl Into<String>, params: Option<Box<RawValue>>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            method: method.into().into(),
            params: params.map(Cow::Owned),
        }
    }
}

/// JSON-RPC response object as defined in the [spec](https://www.jsonrpc.org/specification#response_object).
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response<'a> {
    /// JSON-RPC version.
    pub jsonrpc: TwoPointZero,
    /// Payload which can be result or error.
    #[serde(flatten)]
    pub payload: ResponsePayload<'a>,
    /// Request ID
    #[serde(borrow)]
    pub id: Id<'a>,
}

impl<'a> Response<'a> {
    /// Create a owned successful response
    pub fn success(id: Id<'a>, result: Box<RawValue>) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            payload: ResponsePayload::Success(SuccessResponse {
                result: Cow::Owned(result),
            }),
            id,
        }
    }

    /// Create an owned error response
    pub fn owned(
        id: Id<'a>,
        code: ErrorCode,
        message: impl Into<String>,
        data: Option<Cow<'a, RawValue>>,
    ) -> Self {
        Self {
            jsonrpc: TwoPointZero,
            payload: ResponsePayload::Error(ErrorResponse {
                error: ErrorDetails {
                    code,
                    message: message.into().into(),
                    data,
                },
            }),
            id,
        }
    }
}

/// Deserializing the response requires some custom logic because we want to represent the response
/// payload as an enum.  But we also want to store the success payload as a `RawValue` for
/// efficiency and consistency.  This exceeds the abilities of the `#[serde(flatten)]` attribute
/// (see https://github.com/serde-rs/json/issues/1051#issuecomment-1664728009).
///
/// So to work around it we perform deserialization into a dumber version of the struct that just
/// has two optional fields `result` and `error`, then convert that into the payload enum.
impl<'de: 'a, 'a> Deserialize<'de> for Response<'a> {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DumbResponse<'a> {
            /// JSON-RPC version.
            pub jsonrpc: TwoPointZero,
            /// Payload which can be result or error.
            #[serde(borrow)]
            pub result: Option<Cow<'a, RawValue>>,
            #[serde(borrow)]
            pub error: Option<ErrorDetails<'a>>,
            /// Request ID
            #[serde(borrow)]
            pub id: Id<'a>,
        }

        let DumbResponse {
            jsonrpc,
            result,
            error,
            id,
        } = DumbResponse::deserialize(deserializer)?;

        let payload = match (result, error) {
            (Some(result), None) => ResponsePayload::Success(SuccessResponse { result }),
            (None, Some(error)) => ResponsePayload::Error(ErrorResponse { error }),
            _ => {
                return Err(serde::de::Error::custom(
                    "response must have either result or error, but not both or neither",
                ));
            }
        };

        Ok(Self { jsonrpc, payload, id })
    }
}

/// Possible payloads of a JSON RPC response
///
/// This is an `untagged` enum so that serde will try to deserialize into each variant in order
/// until one succeeds.  This works because successs responses should have `result` and error
/// responses should have `error`.
// #[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Debug, Clone)]
// #[serde(untagged)]
pub enum ResponsePayload<'a> {
    /// Corresponds to successful JSON-RPC response with the field `result`.
    // #[serde(borrow)]
    Success(SuccessResponse<'a>),
    /// Corresponds to failed JSON-RPC response with a error object with the field `error.
    // #[serde(borrow)]
    Error(ErrorResponse<'a>),
}

#[doc(hidden)]
#[allow(
    non_upper_case_globals,
    unused_attributes,
    unused_qualifications,
    clippy::absolute_paths
)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'a> _serde::Serialize for ResponsePayload<'a> {
        fn serialize<__S>(&self, __serializer: __S) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            match *self {
                ResponsePayload::Success(ref __field0) => {
                    _serde::Serialize::serialize(__field0, __serializer)
                }
                ResponsePayload::Error(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
            }
        }
    }
};
#[doc(hidden)]
#[allow(
    non_upper_case_globals,
    unused_attributes,
    unused_qualifications,
    clippy::absolute_paths
)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de: 'a, 'a> _serde::Deserialize<'de> for ResponsePayload<'a> {
        fn deserialize<__D>(__deserializer: __D) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            let __content =
                dbg!(<_serde::__private::de::Content as _serde::Deserialize>::deserialize(__deserializer)?);
            let __deserializer = _serde::__private::de::ContentRefDeserializer::<__D::Error>::new(&__content);
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <SuccessResponse<'a> as _serde::Deserialize>::deserialize(__deserializer),
                ResponsePayload::Success,
            )) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <ErrorResponse<'a> as _serde::Deserialize>::deserialize(__deserializer),
                ResponsePayload::Error,
            )) {
                return _serde::__private::Ok(__ok);
            }
            _serde::__private::Err(_serde::de::Error::custom(
                "data did not match any variant of untagged enum ResponsePayload",
            ))
        }
    }
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse<'a> {
    #[serde(borrow)]
    pub result: Cow<'a, RawValue>,
}

/// [Failed JSON-RPC response object](https://www.jsonrpc.org/specification#error_object).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse<'a> {
    #[serde(borrow)]
    pub error: ErrorDetails<'a>,
}

/// [JSON-RPC failed response error details](https://www.jsonrpc.org/specification#error_object).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorDetails<'a> {
    /// Code
    pub code: ErrorCode,
    /// Message
    #[serde(borrow)]
    pub message: Cow<'a, str>,
    /// Optional data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Cow<'a, RawValue>>,
}

/// Every possible (valid) JSON-RPC message that can be sent over the wire
///
/// Represented as an `untagged` enum so serde will try to deserialize into each variant in order
/// until one succeeds.  This is the most flexible way to represent the JSON-RPC protocol, as it
/// allows for proper handling of each message type.
///
/// The order of variants is important for deserialization since serde will try each variant in
/// order.
// #[derive(Debug, Serialize, Deserialize)]
#[derive(Debug)]
// #[serde(untagged)]
pub enum Message<'a> {
    /// A batch of messages
    // #[serde(borrow)]
    Batch(Vec<Message<'a>>),

    /// A request with method, id, and optional params
    // #[serde(borrow)]
    Request(Request<'a>),

    /// A notification with method and optional params but no id
    // #[serde(borrow)]
    Notification(Notification<'a>),

    /// A response containing either a result or error
    // #[serde(borrow)]
    Response(Response<'a>),

    /// An invalid request that still has an id
    // #[serde(borrow)]
    InvalidRequest(InvalidRequest<'a>),
}

#[doc(hidden)]
#[allow(
    non_upper_case_globals,
    unused_attributes,
    unused_qualifications,
    clippy::absolute_paths
)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'a> _serde::Serialize for Message<'a> {
        fn serialize<__S>(&self, __serializer: __S) -> _serde::__private::Result<__S::Ok, __S::Error>
        where
            __S: _serde::Serializer,
        {
            match *self {
                Message::Batch(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
                Message::Request(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
                Message::Notification(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
                Message::Response(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
                Message::InvalidRequest(ref __field0) => _serde::Serialize::serialize(__field0, __serializer),
            }
        }
    }
};
#[doc(hidden)]
#[allow(
    non_upper_case_globals,
    unused_attributes,
    unused_qualifications,
    clippy::absolute_paths
)]
const _: () = {
    #[allow(unused_extern_crates, clippy::useless_attribute)]
    extern crate serde as _serde;
    #[automatically_derived]
    impl<'de: 'a, 'a> _serde::Deserialize<'de> for Message<'a> {
        fn deserialize<__D>(__deserializer: __D) -> _serde::__private::Result<Self, __D::Error>
        where
            __D: _serde::Deserializer<'de>,
        {
            let __content =
                <_serde::__private::de::Content as _serde::Deserialize>::deserialize(__deserializer)?;
            let __deserializer = _serde::__private::de::ContentRefDeserializer::<__D::Error>::new(&__content);
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <Vec<Message<'a>> as _serde::Deserialize>::deserialize(__deserializer),
                Message::Batch,
            )) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <Request<'a> as _serde::Deserialize>::deserialize(__deserializer),
                Message::Request,
            )) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <Notification<'a> as _serde::Deserialize>::deserialize(__deserializer),
                Message::Notification,
            )) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <Response<'a> as _serde::Deserialize>::deserialize(__deserializer),
                Message::Response,
            )) {
                return _serde::__private::Ok(__ok);
            }
            if let _serde::__private::Ok(__ok) = dbg!(_serde::__private::Result::map(
                <InvalidRequest<'a> as _serde::Deserialize>::deserialize(__deserializer),
                Message::InvalidRequest,
            )) {
                return _serde::__private::Ok(__ok);
            }
            _serde::__private::Err(_serde::de::Error::custom(
                "data did not match any variant of untagged enum Message",
            ))
        }
    }
};

// The JSON-RPC protocol talks about servers and clients, but in almost every way that matters
// it's actually a peer-to-peer protocol.  Servers can send messages that the client didn't
// initiate a request for, and clients have to handle those and respond.  There are differences in
// how clients and servers are instantiated, as servers serve but clients connect, however most of
// the JSON RPC protocol is about exchanging messages between what are, in effect, peers.

/// A remote peer that speaks the JSON-RPC protocol over some transport
#[async_trait]
pub trait Peer: Send + Sync {
    async fn send_message(
        &self,
        message: Message<'_>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use serde_json::{Value, json};
    use std::borrow::Cow;

    fn raw_value_from_json(value: Value) -> Box<RawValue> {
        let raw = serde_json::to_string(&value).unwrap();
        serde_json::value::RawValue::from_string(raw).unwrap()
    }

    #[test]
    fn test_request_serialization() {
        // Create known-good JSON-RPC request string manually - this is the format defined by the spec
        let known_good_json = r#"{"jsonrpc":"2.0","method":"test_method","params":[1,"test",true],"id":1}"#;
        let known_good_value: Value = serde_json::from_str(known_good_json).unwrap();

        // Create a Request using our implementation
        let params = raw_value_from_json(json!([1, "test", true]));
        let id = Id::Number(1);
        let our_request = Request::owned(id.clone(), "test_method", Some(params));

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
        let known_good_json = r#"[ {"jsonrpc":"2.0","method":"test_method","params":[1,"test",true],"id":1}, {"jsonrpc":"2.0","method":"test_method2","params":[1,"test",true],"id":2} ]"#;

        // Test round-trip serialization/deserialization
        let deserialized: Vec<Request> = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(deserialized[0].id, Id::Number(1));
        assert_eq!(deserialized[0].method, "test_method");
        assert_eq!(
            deserialized[0].params.as_ref().map(|p| p.get()),
            Some(r#"[1,"test",true]"#)
        );
        assert_eq!(deserialized[1].id, Id::Number(2));
        assert_eq!(deserialized[1].method, "test_method2");
        assert_eq!(
            deserialized[1].params.as_ref().map(|p| p.get()),
            Some(r#"[1,"test",true]"#)
        );

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
        let params = raw_value_from_json(json!({"key": "value"}));
        let our_notification = Notification::owned("notify_method", Some(params));

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
        let result_raw = raw_value_from_json(json!({"status": "success"}));
        let id = Id::Number(42);
        let our_response = Response::success(id.clone(), result_raw);

        // Serialize our response to JSON string
        let our_json = serde_json::to_string(&our_response).unwrap();
        let our_value: Value = serde_json::from_str(&our_json).unwrap();

        // Compare our implementation against the known-good format
        assert_eq!(known_good_value, our_value);

        // Test round-trip serialization/deserialization
        let deserialized: Response = serde_json::from_str(dbg!(&our_json)).unwrap();
        assert_eq!(deserialized.id, our_response.id);
        assert_matches!(deserialized.payload, ResponsePayload::Success(SuccessResponse { ref result }) if result.get() == r#"{"status":"success"}"#);

        // Also verify we can deserialize the known-good format
        let known_good_response: Response = serde_json::from_str(known_good_json).unwrap();
        assert_eq!(known_good_response.id, our_response.id);
        assert_matches!(known_good_response.payload, ResponsePayload::Success(SuccessResponse { ref result }) if result.get() == r#"{"status":"success"}"#);

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
        let error_data = raw_value_from_json(json!({"details": "error details"}));
        let our_response = Response::owned(
            id.clone(),
            ErrorCode::InvalidRequest,
            "Invalid request",
            Some(Cow::Owned(error_data)),
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
        let our_success = Response::success(Id::Number(1), raw_value_from_json(json!(19)));
        let our_success_json = serde_json::to_string(&our_success).unwrap();
        let our_success_parsed: Value = serde_json::from_str(&our_success_json).unwrap();

        // Compare the JSON structures
        assert_eq!(success_parsed, our_success_parsed);

        // Test error response
        let error_json = r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":"1"}"#;
        let error_parsed: Value = serde_json::from_str(error_json).unwrap();

        // Create our own error response
        let our_error = Response::owned(
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
        let our_request = Request::owned(
            Id::Number(1),
            "subtract",
            Some(raw_value_from_json(json!([42, 23]))),
        );
        let our_request_json = serde_json::to_string(&our_request).unwrap();
        let our_request_value: Value = serde_json::from_str(&our_request_json).unwrap();
        let spec_request_value: Value = serde_json::from_str(request_json).unwrap();
        assert_eq!(our_request_value, spec_request_value);

        let our_success_response = Response::success(Id::Number(1), raw_value_from_json(json!(19)));
        let our_success_json = serde_json::to_string(&our_success_response).unwrap();
        let our_success_value: Value = serde_json::from_str(&our_success_json).unwrap();
        let spec_success_value: Value = serde_json::from_str(success_json).unwrap();
        assert_eq!(our_success_value, spec_success_value);
    }

    #[test]
    fn test_borrowed_vs_owned() {
        // Test the borrowed vs owned versions

        // For Request
        let params_json = json!([1, 2, 3]);
        let params_raw = raw_value_from_json(params_json.clone());
        let id = Id::Number(42);

        // Owned request
        let owned_request = Request::owned(id.clone(), "test", Some(params_raw));
        let owned_json = serde_json::to_string(&owned_request).unwrap();

        // Create borrowed request (using local variables)
        let params_raw_ref =
            serde_json::value::RawValue::from_string(serde_json::to_string(&params_json).unwrap()).unwrap();
        let method = "test";
        let borrowed_request = Request::borrowed(&id, &method, Some(&params_raw_ref));
        let borrowed_json = serde_json::to_string(&borrowed_request).unwrap();

        // Both should produce the same JSON
        assert_eq!(owned_json, borrowed_json);

        // For Notification
        let owned_notification = Notification::owned("notify", Some(raw_value_from_json(json!({"a": 1}))));
        let owned_notif_json = serde_json::to_string(&owned_notification).unwrap();

        let method = "notify";
        let params_raw_ref =
            serde_json::value::RawValue::from_string(serde_json::to_string(&json!({"a": 1})).unwrap())
                .unwrap();
        let borrowed_notification = Notification::borrowed(&method, Some(&params_raw_ref));
        let borrowed_notif_json = serde_json::to_string(&borrowed_notification).unwrap();

        assert_eq!(owned_notif_json, borrowed_notif_json);
    }

    #[test]
    fn test_special_id_values() {
        // Create request with null ID
        let null_id = Id::Null;
        let null_id_request = Request::owned(null_id, "method", None);
        let null_id_json = serde_json::to_string(&null_id_request).unwrap();
        let parsed: Value = serde_json::from_str(&null_id_json).unwrap();

        // Verify JSON serialization works correctly
        assert_eq!(parsed["id"], Value::Null);

        // Create request with string ID
        let str_id = Id::Str("abc123".into());
        let str_id_request = Request::owned(str_id, "method", None);
        let str_id_json = serde_json::to_string(&str_id_request).unwrap();
        let parsed: Value = serde_json::from_str(&str_id_json).unwrap();

        // Verify JSON serialization works correctly
        assert_eq!(parsed["id"], json!("abc123"));

        // Create request with numeric ID
        let num_id = Id::Number(42);
        let num_id_request = Request::owned(num_id, "method", None);
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
