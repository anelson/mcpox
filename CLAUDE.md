# CLAUDE.md - Guidelines for Claude in mcpox

## Build & Run Commands
- Build: `cargo build`
- Run: `cargo run`
- Release build: `cargo build --release`

## Test Commands
- Run all tests: `cargo test`
- Run specific test: `cargo test test_name`
- Run tests in a specific module: `cargo test module_name`

## Lint Commands
- Lint code: `cargo clippy`
- Format code: `cargo fmt`

## Code Style Guidelines
- **Formatting**: Follow Rust style guide (`rustfmt`)
- **Naming**: Use snake_case for variables/functions, CamelCase for types/traits
- **Imports**: Group imports (std first, then external crates, then internal)
- **Error Handling**: Use Result<T, E> for recoverable errors, panic for unrecoverable
- **Comments**: Doc comments with `///` for public items, regular `//` for implementation
- **Types**: Favor strong typing, use type aliases for complex types
- **Functions**: Keep functions small and focused on a single task

Remember to run lint and format checks before committing changes.