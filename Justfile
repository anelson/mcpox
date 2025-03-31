# Do a Rust "vibe check" (*cringe*) on the codebase
# This is helpful for humans but it's mainly intended to provide a deterministic way for coding agents
# to get feedback on their almost certainly shitty changes before wasting a human's time with their garbage code.
vibecheck:
	@cargo check --all-targets --all-features --workspace
	@cargo clippy --all-targets --all-features -- -D warnings
	@cargo +nightly fmt -- --config-path rustfmt-nightly.toml
	@cargo fmt

# Run all of the tests in all of the crates
test:
	@cargo test --all-features --workspace

