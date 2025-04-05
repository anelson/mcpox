use serde_json::Value as JsonValue;
use thiserror::Error;

pub type Result<T, E = JsonRpcError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum JsonRpcError {
    #[error("Invalid JSON request")]
    DeserRequest {
        source: serde_json::Error,
        request: JsonValue,
    },
    #[error("Error serializing response {type_name} to JSON")]
    SerResponse {
        source: serde_json::Error,
        type_name: &'static str
    },
}
