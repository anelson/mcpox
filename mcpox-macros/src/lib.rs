extern crate proc_macro;

use proc_macro::TokenStream;

/// A simple example proc macro that adds two numbers at compile time
#[proc_macro]
pub fn add(_input: TokenStream) -> TokenStream {
    // This is a simplified example - in a real proc macro,
    // you would parse the input tokens using syn and generate
    // output using quote
    "2 + 2".parse().unwrap()
}

#[cfg(test)]
mod tests {
    // Note: proc macros can't be tested directly in the same crate
    // Real tests would be in a separate integration test crate
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn it_compiles() {
        // This just verifies the crate compiles
        assert!(true);
    }
}
