use std::sync::Arc;

use futures::lock::Mutex;
use mcpox_jsonrpc::Router;

/// A test service built using the JSON-RPC crate.  It's internal implementation matches the
/// expected behavior reflected in the `testdata/` folder, and thus it can be used to exercise the
/// JSON RPC implementation end-to-end
#[derive(Debug)]
pub struct TestState {}

type State = Arc<Mutex<TestState>>;

/// Create a router that has methods and notification handlers that make up the "test service".
///
/// This router can be the input to either a client or a server, although a server is a more
/// realistic use case.
pub fn test_service_router() -> Router<State> {
    let mut router = Router::new_with_state(Arc::new(Mutex::new(TestState {})));

    // TODO: come up with some test methods and implement them in terms of some state struct.
    router.register_handler("foo", foo);

    router
}

async fn foo() -> () {
    // don't do anything
}
