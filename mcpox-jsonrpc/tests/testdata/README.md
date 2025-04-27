Test data for exercising the JSON RPC implementation.

We use YAML instead of JSON because of its more civilized syntax and readability. Any YAML document can be
unambigiously represented as JSON, which we use to our advantage here.

Each YAML file is a conversation, thus an array of messages.

Messages with a `c` are from the client to the server; `s` are, not surprisingly, from the server back to the client.

If played against the test server implemented in the tests, the server responses in each test case should exactly match
what's actually returned. Thus, entire conversations can be played back, which each step verified for consistency with
the test data.
