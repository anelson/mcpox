use thiserror::Error;

pub type Result<T, E = JsonRpcError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum JsonRpcError {}
