use crate::types;
use serde_json::{Value as JsonValue, json};
use thiserror::Error;

pub type Result<T, E = JsonRpcError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum JsonRpcError {
    #[error("Error parsing JSON")]
    ParseJson { source: serde_json::Error, json: String },
    #[error("Error serializing request {type_name} to JSON")]
    SerRequest {
        source: serde_json::Error,
        type_name: &'static str,
    },
    #[error("Error deserializing request JSON into {type_name}")]
    DeserRequest {
        source: serde_json::Error,
        type_name: &'static str,
        request: JsonValue,
    },
    #[error("Error serializing response {type_name} to JSON")]
    SerResponse {
        source: serde_json::Error,
        type_name: &'static str,
    },
    #[error("Error deserializing response JSON into {type_name}")]
    DeserResponse {
        source: serde_json::Error,
        type_name: &'static str,
        request: JsonValue,
    },
    #[error("Transport error")]
    Transport {
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error("Operation canceled because the connection closed before a response was received")]
    PendingRequestConnectionClosed,
    #[error("The method '{method_name}' failed with an error {}: {}", error.code, error.message)]
    MethodError {
        method_name: String,
        error: types::ErrorDetails,
    },
    #[error("Custom event loop handler reported an error")]
    CustomEventLoopError {
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
}

/// Any error that we can encounter should be able to be represented on the wire as a JSON-RPC
/// error
///
/// Note that this is sent over the wire to the remote peer, so we don't want to provide a huge
/// amount of information about the failure that might reveal internal implementation details and
/// help attackers.
impl From<JsonRpcError> for types::ErrorDetails {
    fn from(val: JsonRpcError) -> Self {
        match val {
            JsonRpcError::ParseJson { .. } => {
                types::ErrorDetails::parse_error("Invalid JSON".to_string(), None)
            }
            JsonRpcError::DeserRequest { request, .. } => types::ErrorDetails::invalid_request(
                "Invalid request".to_string(),
                Some(json!({
                    "bad_json": request,
                })),
            ),
            JsonRpcError::SerResponse { .. } => {
                types::ErrorDetails::internal_error("Error serializing response".to_string(), None)
            }
            JsonRpcError::Transport { .. } => {
                types::ErrorDetails::internal_error("Transport error".to_string(), None)
            }
            JsonRpcError::MethodError { error, .. } => error,
            JsonRpcError::PendingRequestConnectionClosed
            | JsonRpcError::CustomEventLoopError { .. }
            | JsonRpcError::SerRequest { .. }
            | JsonRpcError::DeserResponse { .. } => {
                types::ErrorDetails::internal_error(val.to_string(), None)
            }
        }
    }
}
