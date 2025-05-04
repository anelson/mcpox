use thiserror::Error;

pub type Result<T, E = McpClientError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum McpClientError {
    #[error("Error spawning MCP server '{program}' with args [{args}] as child process",
        args = .args.join(" ")
    )]
    SpawnServer {
        source: std::io::Error,
        program: String,
        args: Vec<String>,
    },
}
