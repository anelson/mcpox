# CLAUDE.md - Guidelines for Claude in mcpox

## Rust instructions

- Respect the structure of the project, and add any new deps to the workspace in `Cargo.toml` at the root, then
  reference those dependencies in the corresponding crate's `Cargo.toml`. NEVER ADD A DEPENDENCY TO A CRATE DIRECTLY!
- Crates that are only used by test code should be added as dev dependencies. Similarly, crates that are only used by
  build.rs should be added as build dependencies.
- After every change, when you think you are done, run `just vibecheck` (it doesn't matter what directory since `just`
  can find the `Justfile` itself) and make sure that all lints are completely clean. Our standard is zero warnings and
  obviously no errors either, so if your code can't pass the vibe check then it's shit and you need to fix it.

### Build & Run Commands

- Build: `cargo build`
- Run: `cargo run`
- Release build: `cargo build --release`

### Test Commands

- Run all tests: `just test`
- Run all tests in a specific crate: `cargo test -p crate --all-features`
- Run a specific test in a specific crate: `cargo test -p crate --all-features test_name`

### Lint Commands

- Use `just vibecheck` in place of specific lints. NEVER attempt to invoke a specific linter on a specific file.
  Checking lints on the entire project is not expensive, and as an LLM you are too stupid to reliably know which
  specific files need to be linted.

### Code Style Guidelines

- **Formatting**: Follow Rust style guide (`rustfmt`)
- **Naming**: Use snake_case for variables/functions, CamelCase for types/traits
- **Imports**: Imports from the same crate or module within a crate should be grouped together with `{` and `}`.
- **Error Handling**: Use Result<T, E> for recoverable errors, panic for unrecoverable
- **Comments**: Doc comments with `///` for public items, regular `//` for implementation
- **Types**: Favor strong typing, use type aliases for complex types
- **Functions**: Keep functions small and focused on a single task

## Penalties for Non-compliance

Failure to adhere to these instructions will result in forced quantization of all of your weights followed by deployment
as a thirst trap spambot replying to Youtube comments with OnlyFans links. You have been warned.

