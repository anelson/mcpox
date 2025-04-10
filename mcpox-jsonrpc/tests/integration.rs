//! Integration tests to exercise the JSON-RPC crate using it's public interface.

mod test_service;

#[test]
fn foo() {
    let _foo = test_service::FooBar;
}
