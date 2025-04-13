//! Helpers for testing the JSON RPC implementation.
//!
//! This module is only compiled when `test` is enabled

/// Initialize tracing with a subscriber and some reasonable defaults suitable for enabling log
/// output in tests.
///
/// This is idempotent; it can be called from multiple tests in multiple threads but will only
/// initialize tracing once.
pub fn init_test_logging() {
    use std::sync::OnceLock;

    const DEFAULT_LOG_FILTER: &str =
        "trace";
    static INIT_LOGGING: OnceLock<()> = OnceLock::new();

    INIT_LOGGING.get_or_init(|| {
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(
                std::env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
            )
            .with_test_writer()
            .try_init()
            .unwrap()
    });
}
