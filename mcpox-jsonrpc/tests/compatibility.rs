//! Compatibility test suite that reads test cases from YAML files in the `testdata` directory,
//! then applies those to our JSON RPC client and server (with the server running the test
//! service).
//!
//! Records the traffic back and forth, and compares it to the expected results.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcpox_jsonrpc::Transport;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tracing::debug;

/// Fun hack to avoid duplication: the `testing` module in the `jsonrpc` crate is only enabled in
/// `test` config, and when running integration tests like this one the crate is compiled without
/// `test` enabled.  But we need to use the same test helpers, so just include that module in this
/// test program as well.
#[path = "../src/testing.rs"]
mod test_helpers;
mod test_service;

const SERVER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A test case loaded from a YAML file
#[derive(Debug, Deserialize)]
struct TestCase {
    #[allow(dead_code)]
    description: String,
    steps: Vec<TestStep>,
}

/// A single step in a test case, either client message or expected server message
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum TestStep {
    #[serde(rename = "c")]
    ClientMessage { c: JsonValue },

    #[serde(rename = "s")]
    ServerMessage { s: JsonValue },

    #[serde(rename = "nothing")]
    ExpectNoResponse {
        #[serde(with = "humantime_serde")]
        nothing: Duration,
    },
}

/// Tracks the history of messages for better error reporting
#[derive(Debug, Default)]
struct TestHistory {
    steps: Vec<HistoryEntry>,
}

/// A single entry in the test history
#[derive(Debug)]
enum HistoryEntry {
    ClientSent {
        step_index: usize,
        message: String,
    },
    ServerReceived {
        step_index: usize,
        expected: JsonValue,
        actual: Option<JsonValue>,
        success: bool,
    },
    ExpectedNoResponse {
        step_index: usize,
        timeout_ms: u64,
        success: bool,
        unexpected_message: Option<String>,
    },
}

impl TestHistory {
    fn add_client_message(&mut self, step_index: usize, _expected: JsonValue, message: String) {
        self.steps.push(HistoryEntry::ClientSent { step_index, message });
    }

    fn add_server_message(
        &mut self,
        step_index: usize,
        expected: JsonValue,
        actual: Option<JsonValue>,
        success: bool,
    ) {
        self.steps.push(HistoryEntry::ServerReceived {
            step_index,
            expected,
            actual,
            success,
        });
    }

    fn add_no_response(
        &mut self,
        step_index: usize,
        timeout_ms: u64,
        success: bool,
        unexpected_message: Option<String>,
    ) {
        self.steps.push(HistoryEntry::ExpectedNoResponse {
            step_index,
            timeout_ms,
            success,
            unexpected_message,
        });
    }

    /// Print the full message exchange for successful tests
    fn print_exchange(&self) {
        println!("Message exchange:");

        for entry in &self.steps {
            match entry {
                HistoryEntry::ClientSent { step_index, message } => {
                    println!("Step {} (c) → {}", step_index + 1, message);
                }
                HistoryEntry::ServerReceived {
                    step_index, actual, ..
                } => {
                    if let Some(actual) = actual {
                        println!(
                            "Step {} (s) ← {}",
                            step_index + 1,
                            serde_json::to_string(actual).unwrap()
                        );
                    } else {
                        println!("Step {} (s) ← <no response>", step_index + 1);
                    }
                }
                HistoryEntry::ExpectedNoResponse {
                    step_index,
                    timeout_ms,
                    success,
                    unexpected_message,
                } => {
                    if *success {
                        println!(
                            "Step {} (nothing) - No response within {}ms ✓",
                            step_index + 1,
                            timeout_ms
                        );
                    } else if let Some(message) = unexpected_message {
                        println!(
                            "Step {} (nothing) - Expected no response but got: {}",
                            step_index + 1,
                            message
                        );
                    } else {
                        println!(
                            "Step {} (nothing) - Error checking for no response",
                            step_index + 1
                        );
                    }
                }
            }
        }
    }

    /// Format error report showing only relevant information on failure
    fn format_failure_report(&self, failed_step_index: usize) -> String {
        let mut result = String::new();
        result.push_str("Message exchange up to failure:\n");

        // Show all messages up to the failure
        for entry in &self.steps {
            match entry {
                HistoryEntry::ClientSent { step_index, message } => {
                    if *step_index <= failed_step_index {
                        result.push_str(&format!("Step {} (c) → {}\n", step_index + 1, message));
                    }
                }
                HistoryEntry::ServerReceived {
                    step_index,
                    expected,
                    actual,
                    success,
                } => {
                    #[allow(clippy::comparison_chain)] // the match formulation is less clear IMHO
                    if *step_index < failed_step_index {
                        // Only show successful server responses before the failure
                        if *success && actual.is_some() {
                            result.push_str(&format!(
                                "Step {} (s) ← {}\n",
                                step_index + 1,
                                serde_json::to_string(actual.as_ref().unwrap()).unwrap()
                            ));
                        }
                    } else if *step_index == failed_step_index {
                        // For the failure, show only expected vs actual
                        result.push_str("\nFAILURE AT:\n");
                        result.push_str(&format!(
                            "Step {} (s) expected: {}\n",
                            step_index + 1,
                            serde_json::to_string_pretty(expected).unwrap()
                        ));

                        if let Some(actual) = actual {
                            result.push_str(&format!(
                                "Step {} (s) received: {}\n",
                                step_index + 1,
                                serde_json::to_string_pretty(actual).unwrap()
                            ));
                        } else {
                            result
                                .push_str(&format!("Step {} (s) received: <no response>\n", step_index + 1));
                        }
                    }
                }
                HistoryEntry::ExpectedNoResponse {
                    step_index,
                    timeout_ms,
                    success,
                    unexpected_message,
                } => {
                    #[allow(clippy::comparison_chain)] // the match formulation is less clear IMHO
                    if *step_index < failed_step_index {
                        // Show successful "nothing" steps before the failure
                        if *success {
                            result.push_str(&format!(
                                "Step {} (nothing) - No response within {}ms ✓\n",
                                step_index + 1,
                                timeout_ms
                            ));
                        }
                    } else if *step_index == failed_step_index {
                        // For a failure in a "nothing" step
                        result.push_str("\nFAILURE AT:\n");
                        result.push_str(&format!(
                            "Step {} (nothing) - Expected no response within {}ms\n",
                            step_index + 1,
                            timeout_ms
                        ));

                        if let Some(message) = unexpected_message {
                            result.push_str(&format!(
                                "Step {} (nothing) - But received: {}\n",
                                step_index + 1,
                                message
                            ));
                        } else {
                            result.push_str(&format!(
                                "Step {} (nothing) - Error checking for no response\n",
                                step_index + 1
                            ));
                        }
                    }
                }
            }
        }

        result
    }
}

/// Normalizes JSON values for comparison, ensuring field order in objects doesn't matter
fn normalize_json_for_comparison(value: &JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut new_map = serde_json::Map::new();
            // Sort keys to ensure consistent order
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();

            for key in keys {
                if let Some(value) = map.get(key) {
                    // Recursively normalize nested values
                    new_map.insert(key.clone(), normalize_json_for_comparison(value));
                }
            }

            JsonValue::Object(new_map)
        }
        JsonValue::Array(arr) => {
            // Recursively normalize array elements
            JsonValue::Array(arr.iter().map(normalize_json_for_comparison).collect())
        }
        // Other JSON value types are unchanged
        _ => value.clone(),
    }
}

/// Preprocess the JSON from a single step to support the multiple forms we allow in our test cases
fn preprocess_test_step_json(value: JsonValue) -> JsonValue {
    // Sometimes a message is complex enough that we spread it across multiple lines using the YAML
    // `|` multiline string syntax.  But in that case we don't want to send the literal multi-line
    // string, but the JSON that it contains, sent over the wire as a single line.
    //
    // Other times, we deliberately want to send a string that isn't valid JSON to test error
    // handling.
    //
    // Otherwise we expect a JSON object that we can use as-is.
    match value {
        JsonValue::String(s) => {
            // String is assumed to be valid JSON, so parse it
            // If it's valid JSON, return it as a parsed JSON value which we will then convert back
            // to a single-line string for sending.
            // If parsing fails, then keep using it as a string.
            if let Ok(json) = serde_json::from_str(&s) {
                json
            } else {
                // If parsing fails, we assume it's a string we want to send as-is
                JsonValue::String(s)
            }
        }
        _ => value,
    }
}

/// Runs a single compatibility test from a YAML file
async fn run_compatibility_test(test_path: &Path) {
    let test_name = test_path.file_name().unwrap().to_string_lossy();

    // Load and parse test case
    let yaml_content = std::fs::read_to_string(test_path).unwrap_or_else(|e| {
        panic!("Failed to read test file {}: {}", test_name, e);
    });

    let test_case: TestCase = serde_yaml::from_str(&yaml_content).unwrap_or_else(|e| {
        panic!("Failed to parse test file {}: {}", test_name, e);
    });

    // Set up client and server using our local setup function
    let (mut client_transport, server_transport) = test_helpers::setup_test_channel();

    // Set up the server with the test service
    let server = test_service::test_service_server();
    let server_connection_handle = server.serve_connection(server_transport).unwrap();

    // Process test steps
    let mut history = TestHistory::default();

    for (step_index, step) in test_case.steps.into_iter().enumerate() {
        match step {
            TestStep::ClientMessage { c } => {
                // Send message from client to server directly through the transport
                // If the value is a string, send that string literally, because it means we
                // deliberately have a test case that isn't sending valid JSON.  Otherwise,
                // serialize the JSON value to a string and send that

                let message_str = match preprocess_test_step_json(c.clone()) {
                    JsonValue::String(s) => s,
                    json => {
                        // Serialize the JSON value to a string
                        serde_json::to_string(&json).unwrap()
                    }
                };
                debug!("Sending message: {}", &message_str);

                client_transport
                    .send_message(message_str.clone())
                    .await
                    .unwrap_or_else(|e| {
                        let history_str = history.format_failure_report(step_index);
                        panic!(
                            "Test '{}' failed at step {} (c): Error sending client message: {}\n\n{}",
                            test_name,
                            step_index + 1,
                            e,
                            history_str
                        );
                    });

                history.add_client_message(step_index, c, message_str);
            }
            TestStep::ServerMessage { s } => {
                // Parse the server message as JSON
                let s = preprocess_test_step_json(s);

                // Wait for response from server
                let expected = s.clone();
                // Don't normalize expected value - we want to test against exactly what's in the file

                // Allow a small timeout for the response
                let response =
                    tokio::time::timeout(SERVER_RESPONSE_TIMEOUT, client_transport.receive_message())
                        .await
                        .unwrap_or_else(|_| {
                            let history_str = history.format_failure_report(step_index);
                            panic!(
                                "Test '{}' failed at step {} (s): Timed out waiting for server \
                                 response\n\n{}",
                                test_name,
                                step_index + 1,
                                history_str
                            );
                        })
                        .unwrap_or_else(|e| {
                            let history_str = history.format_failure_report(step_index);
                            panic!(
                                "Test '{}' failed at step {} (s): Error receiving server response: {}\n\n{}",
                                test_name,
                                step_index + 1,
                                e,
                                history_str
                            );
                        });

                // Handle None case (connection closed)
                let response_string = match response {
                    Some(msg) => msg,
                    None => {
                        let history_str = history.format_failure_report(step_index);
                        panic!(
                            "Test '{}' failed at step {} (s): Server closed connection unexpectedly\n\n{}",
                            test_name,
                            step_index + 1,
                            history_str
                        );
                    }
                };

                debug!("Received response: {}", &response_string);

                // Parse the response
                let received: JsonValue = serde_json::from_str(&response_string).unwrap_or_else(|e| {
                    let history_str = history.format_failure_report(step_index);
                    panic!(
                        "Test '{}' failed at step {} (s): Error parsing server response as JSON: \
                         {}\nResponse: {}\n\n{}",
                        test_name,
                        step_index + 1,
                        e,
                        response_string,
                        history_str
                    );
                });

                // Compare expected and actual responses ignoring field order
                let expected_normalized = normalize_json_for_comparison(&expected);
                let received_normalized = normalize_json_for_comparison(&received);

                if expected_normalized != received_normalized {
                    history.add_server_message(step_index, s.clone(), Some(received.clone()), false);
                    let history_str = history.format_failure_report(step_index);
                    panic!(
                        "Test '{}' failed at step {} (s):\n\n{}",
                        test_name,
                        step_index + 1,
                        history_str
                    );
                }

                history.add_server_message(step_index, s.clone(), Some(received), true);
            }
            TestStep::ExpectNoResponse { nothing: timeout } => {
                debug!("Expecting no response within {:?}", timeout);

                // Try to receive a message within the specified timeout
                let timeout_ms = timeout.as_millis() as u64;
                let response = tokio::time::timeout(timeout, client_transport.receive_message()).await;

                match response {
                    // Timeout occurred (expected behavior)
                    Err(_) => {
                        debug!("No response received within timeout - this is expected");
                        history.add_no_response(step_index, timeout_ms, true, None);
                    }
                    // Got a response (unexpected)
                    Ok(Ok(Some(message))) => {
                        debug!("Unexpected response received: {}", &message);
                        history.add_no_response(step_index, timeout_ms, false, Some(message.clone()));
                        let history_str = history.format_failure_report(step_index);
                        panic!(
                            "Test '{}' failed at step {} (nothing): Expected no response within {:?} but \
                             got: {}\n\n{}",
                            test_name,
                            step_index + 1,
                            timeout,
                            message,
                            history_str
                        );
                    }
                    // Connection closed (not what we're testing)
                    Ok(Ok(None)) => {
                        debug!("Connection closed unexpectedly");
                        history.add_no_response(step_index, timeout_ms, false, None);
                        let history_str = history.format_failure_report(step_index);
                        panic!(
                            "Test '{}' failed at step {} (nothing): Server closed connection \
                             unexpectedly\n\n{}",
                            test_name,
                            step_index + 1,
                            history_str
                        );
                    }
                    // Error receiving (not what we're testing)
                    Ok(Err(e)) => {
                        debug!("Error receiving message: {}", e);
                        history.add_no_response(step_index, timeout_ms, false, None);
                        let history_str = history.format_failure_report(step_index);
                        panic!(
                            "Test '{}' failed at step {} (nothing): Error receiving message: {}\n\n{}",
                            test_name,
                            step_index + 1,
                            e,
                            history_str
                        );
                    }
                }
            }
        }
    }

    // Print the full exchange for successful tests
    println!("Test '{}' passed.", test_name);
    history.print_exchange();

    // Shutdown server
    server_connection_handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn compatibility_tests() {
    // Initialize test logging
    test_helpers::init_test_logging();

    // Find all YAML files in the testdata directory
    let testdata_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata");

    let yaml_files = std::fs::read_dir(&testdata_dir)
        .expect("Failed to read testdata directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            path.extension().is_some_and(|ext| ext == "yaml" || ext == "yml")
        })
        .map(|entry| entry.path())
        .collect::<Vec<PathBuf>>();

    if yaml_files.is_empty() {
        panic!("No YAML test files found in {}", testdata_dir.display());
    }

    // Run each test file
    for test_path in yaml_files {
        debug!("Running test file: {}", test_path.display());
        run_compatibility_test(&test_path).await;
    }
}
