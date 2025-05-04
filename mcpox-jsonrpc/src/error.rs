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
        response: JsonValue,
    },
    #[error("Transport error")]
    Transport {
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error("Operation canceled because the connection closed before a response was received")]
    PendingRequestConnectionClosed,
    #[error("The method '{method}' failed with an error {}: {}", error.code, error.message)]
    MethodError {
        method: types::Method,
        error: types::ErrorDetails,
    },
    #[error("Custom event loop handler reported an error")]
    CustomEventLoopError {
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },
    #[error("Operation was cancelled by request")]
    Cancelled,
    #[error("BUG: {message}")]
    Bug { message: String },

    #[error("Batch has not been sent yet")]
    BatchNotSentYet,

    #[error("Call failed because the batch request it was part of failed to send")]
    BatchSendFailed,
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
            JsonRpcError::Bug { .. } => {
                // Keep embarassing things like this in the family, don't tell the remote peer
                types::ErrorDetails::internal_error("Unexpected internal error", None)
            }
            JsonRpcError::PendingRequestConnectionClosed
            | JsonRpcError::Cancelled
            | JsonRpcError::CustomEventLoopError { .. }
            | JsonRpcError::SerRequest { .. }
            | JsonRpcError::DeserResponse { .. }
            | JsonRpcError::BatchNotSentYet
            | JsonRpcError::BatchSendFailed => types::ErrorDetails::internal_error(val.to_string(), None),
        }
    }
}

/// Try to make something useful from the panic for logging purposes
///
/// Informed by tokio's internal `panic_payload_as_str` which only bothers with `String` and
/// `&'static str` panics.
pub(crate) fn panic_err_to_string(err: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = err.downcast_ref::<&'static str>() {
        s.to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        // Get the type name using the Any::type_id method
        let type_id = (*err).type_id();
        let type_name = std::any::type_name_of_val(&*err);

        format!("Panic of type {} (type ID {:?})", type_name, type_id)
    }
}
