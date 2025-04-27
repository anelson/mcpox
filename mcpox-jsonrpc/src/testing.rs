//! Helpers for testing the JSON RPC implementation.
//!
//! This module is only compiled when `test` is enabled
use crate::Transport;
use tokio::io::duplex;
use tokio_util::codec::{Framed, LinesCodec};

/// Initialize tracing with a subscriber and some reasonable defaults suitable for enabling log
/// output in tests.
///
/// This is idempotent; it can be called from multiple tests in multiple threads but will only
/// initialize tracing once.
pub fn init_test_logging() {
    use std::sync::OnceLock;

    const DEFAULT_LOG_FILTER: &str = "trace";
    static INIT_LOGGING: OnceLock<()> = OnceLock::new();

    INIT_LOGGING.get_or_init(|| {
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()))
            .with_test_writer()
            .try_init()
            .unwrap()
    });
}

/// The lines codec used in [`setup_test_channel`] will fail if a line is longer than this.
pub const TEST_CHANNEL_MAX_LENGTH: usize = 1024 * 1024;

/// Create a pair of [`Transport`] implementations that are connected to each other, suitable for
/// use hooking up a client and a server without using HTTP or some other "real" transport
///
/// Return value is a tupl, `(client_transport, server_transport)`.
pub fn setup_test_channel() -> (impl Transport, impl Transport) {
    // Create a pair of connected pipes that will serve as the transport between client and server
    let (client, server) = duplex(TEST_CHANNEL_MAX_LENGTH * 2);

    // Create framed transports with a reasonable max size to avoid DoS vulns
    let client_transport = Framed::new(client, LinesCodec::new_with_max_length(TEST_CHANNEL_MAX_LENGTH));
    let server_transport = Framed::new(server, LinesCodec::new_with_max_length(TEST_CHANNEL_MAX_LENGTH));

    (client_transport, server_transport)
}
